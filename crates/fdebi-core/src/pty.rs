//! 伪终端(PTY)执行:让维护脚本以终端语义运行。
//!
//! debconf 的 readline 前端要求 stdin 是终端;普通管道下它退化为
//! noninteractive(问题按默认值静默处理)。仅当包携带 debconf 模板时走
//! 本模块,其余安装仍用管道 + noninteractive(见 [`crate::install`])。
//!
//! - 输出:从端 stdout/stderr 合并为单流(pty 语义),按行转发;无换行的
//!   片段(提示符)在完整 UTF-8 时立即呈现,前端不会等到换行才看到问题;
//!   转发前去掉 ANSI 转义序列与回车、退格等终端控制字符(前端是纯文本
//!   视图,不解释控制序列)
//! - 输入:前端经 [`InputHandle`] 逐行写入主端,等效于在终端键入
//! - 终端语义:子进程独立成会话并把从端设为控制终端,`/dev/tty` 可用;
//!   从端关闭回显,避免前端已显示输入时再回显一遍
//! - 取消:独立进程组 + SIGTERM(沿用 [`CancelHandle`])

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};

use crate::error::FdebiError;
use crate::install::{CancelHandle, TAIL_LINES};
use crate::ui::InstallProgress;

/// 交互应答句柄:持有当前交互式 dpkg 的 stdin 写端
#[derive(Default)]
pub struct InputHandle {
    tx: Mutex<Option<Sender<String>>>,
}

impl InputHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 提交一行应答;当前没有等待输入的交互式安装时返回 false
    pub fn send(&self, line: &str) -> bool {
        match self.tx.lock().unwrap().as_ref() {
            Some(tx) => tx.send(line.to_string()).is_ok(),
            None => false,
        }
    }
}

/// 在 PTY 中运行命令,返回 (退出码, 输出尾部)。取消时返回
/// [`FdebiError::Cancelled`];运行期间经 [`InstallProgress::input_state`]
/// 通知前端可应答。
pub fn run(
    mut cmd: Command,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
    input: &InputHandle,
) -> Result<(i32, Vec<String>), FdebiError> {
    use std::os::unix::process::CommandExt as _;

    let (master, slave) = open_pty()?;
    // spawn 前完成全部可失败操作:此后没有早退路径,不会遗留未 wait 的子进程
    let mut writer = master.try_clone()?;
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        // 守护进程常运行在无 TERM 的环境(systemd);避免脚本查 terminfo 失败
        .env("TERM", "xterm");
    // 独立会话 + 从端设为控制终端:维护脚本里 `read </dev/tty`、
    // stty/tput 等按终端语义工作的命令才可用(仅 new pgrp 不够)。
    // 新会话的 pgid == 子 pid,取消时仍可对整组 SIGTERM。
    // SAFETY: 闭包只调用异步信号安全的 setsid/ioctl,不做分配或加锁;
    // 此时 std 已把 fd 0/1/2 接到从端。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // 关闭父进程持有的从端副本:否则子进程退出后主端读不到 EOF(EIO),
    // 读线程与输出循环会永远等待
    drop(cmd);
    cancel.register(child.id());

    // 写线程:InputHandle 的行 → 主端(从端已关闭回显,避免输出区重复输入)
    let (tx, rx) = mpsc::channel::<String>();
    *input.tx.lock().unwrap() = Some(tx);
    let writer_thread = std::thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            if writeln!(writer, "{line}")
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });

    // 读线程只把行送进 channel,progress 由本线程调用(与管道路径同构)
    let (line_tx, line_rx) = mpsc::channel::<String>();
    progress.input_state(true);
    let reader_thread = std::thread::spawn(move || read_lines(master, line_tx));

    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    for line in line_rx {
        progress.output_line(&line);
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    // 读线程结束即从端已关闭(子进程退出);等待并收尾。
    // wait 失败也要走完收尾,确保前端收到 input_state(false)
    let status = child.wait();
    cancel.unregister();
    *input.tx.lock().unwrap() = None; // 之后的 send 返回 false
    let _ = reader_thread.join();
    let _ = writer_thread.join();
    progress.input_state(false);
    let status = status?;

    if cancel.is_cancelled() {
        return Err(FdebiError::Cancelled);
    }
    Ok((status.code().unwrap_or(-1), tail.into()))
}

/// 打开一对主/从伪终端;固定 24x100 窗口(0x0 会让按 COLUMNS 折行的脚本
/// 得到反直觉布局)
fn open_pty() -> io::Result<(File, File)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: openpty 成功时写回两个新 fd;其余参数为空表示默认 termios
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty 返回的 fd 由此处独占所有权
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // openpty 不带 O_CLOEXEC;守护进程会并发 spawn ps/dpkg-deb 等子进程,
    // 若不设置,主端(及短暂存在的从端)会被它们继承:从端被继承会让
    // 子进程退出后主端读不到 EIO,交互事务永久挂起
    for file in [&master, &slave] {
        // SAFETY: 对有效 fd 设置 FD_CLOEXEC,不改变其他标志
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: slave 是有效 fd,winsize 为该 ioctl 的入参
    unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    // 回显关闭:输入由前端输入框(或用户自己的终端)呈现,PTY 再回显会在
    // 输出区重复一遍;IUTF8 让行规程按 UTF-8 处理退格(多字节字符不被拆坏)
    // SAFETY: termios 结构由内核填充,tcsetattr 只修改该从端
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut term) < 0 {
            return Err(io::Error::last_os_error());
        }
        term.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
        term.c_iflag |= libc::IUTF8;
        if libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &term) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((master, slave))
}

/// 读主端并按行发送;无换行的片段(提示符)立即发送,半截多字节字符
/// 留待下一轮(避免把 UTF-8 拆成替换字符)
fn read_lines(mut master: File, tx: Sender<String>) {
    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    send_line(&tx, pending.drain(..=pos).collect());
                }
                // 提示符通常无换行:本轮已读完(短读)且剩余是完整 UTF-8 时
                // 立即呈现;读满缓冲说明长行未完,继续累积避免按块切断
                if n < buf.len() && !pending.is_empty() && std::str::from_utf8(&pending).is_ok() {
                    send_line(&tx, std::mem::take(&mut pending));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // 从端关闭时 Linux 返回 EIO:视同 EOF
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        send_line(&tx, pending);
    }
}

/// 去掉行尾换行与 CR(pty 输出把 \n 翻译为 \r\n),并清理纯文本视图
/// 无法解释的终端控制序列(ANSI CSI/OSC、回车、退格等)后发送
fn send_line(tx: &Sender<String>, mut line: Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let _ = tx.send(sanitize(&line));
}

/// 转义序列解析状态
#[derive(Clone, Copy)]
enum EscState {
    /// 普通文本
    Text,
    /// 刚读到 ESC
    Esc,
    /// ESC + 字符集/中间字节,再吞掉一个字符
    EscIntermediate,
    /// CSI(ESC [)
    Csi,
    /// OSC/字符串序列(ESC ] P X ^ _)
    Osc,
    /// OSC 内读到 ESC,等待 ST(ESC \)
    OscEsc,
}

/// 把 PTY 输出清理成适合纯文本视图显示的内容:
/// - 去掉 ANSI CSI(颜色、光标、括号粘贴等)与 OSC(标题等)序列
/// - 回车按终端语义回到行首(清除本行已累积内容),其余 C0 控制字符丢弃
/// - 退格按终端语义删除前一个字符;保留制表符
fn sanitize(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    let mut state = EscState::Text;
    for ch in text.chars() {
        state = match state {
            EscState::Text => match ch {
                '\x1b' => EscState::Esc,
                '\r' => {
                    out.clear();
                    EscState::Text
                }
                '\x08' => {
                    out.pop();
                    EscState::Text
                }
                c if c.is_control() && c != '\t' => EscState::Text,
                c => {
                    out.push(c);
                    EscState::Text
                }
            },
            EscState::Esc => match ch {
                '[' => EscState::Csi,
                ']' | 'P' | 'X' | '^' | '_' => EscState::Osc,
                '(' | ')' | '#' | '%' | '*' | '+' | '-' | '.' | '/' => EscState::EscIntermediate,
                _ => EscState::Text,
            },
            EscState::EscIntermediate => EscState::Text,
            // CSI 以 0x40..=0x7e 的终止字节结束
            EscState::Csi => {
                if ('\x40'..='\x7e').contains(&ch) {
                    EscState::Text
                } else {
                    EscState::Csi
                }
            }
            EscState::Osc => match ch {
                '\x07' => EscState::Text,
                '\x1b' => EscState::OscEsc,
                _ => EscState::Osc,
            },
            EscState::OscEsc => match ch {
                '\\' => EscState::Text,
                '\x1b' => EscState::OscEsc,
                _ => EscState::Osc,
            },
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    #[derive(Default)]
    struct Collector {
        lines: Arc<StdMutex<Vec<String>>>,
        input: Arc<StdMutex<Vec<bool>>>,
    }

    impl InstallProgress for Collector {
        fn output_line(&self, line: &str) {
            self.lines.lock().unwrap().push(line.to_string());
        }

        fn input_state(&self, enabled: bool) {
            self.input.lock().unwrap().push(enabled);
        }
    }

    #[test]
    fn prompt_and_answer_round_trip() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'Ask [y/N]: '; read ans; echo \"answer=$ans\"");
        let progress = Collector::default();
        let lines = progress.lines.clone();
        let flags = progress.input.clone();
        let cancel = Arc::new(CancelHandle::new());
        let input = Arc::new(InputHandle::new());

        // 交互句柄就绪后应答(轮询避免固定延迟造成偶发失败)
        let input_tx = input.clone();
        let sender = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !input_tx.send("yes") {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            true
        });

        // 看门狗:run 若因回归挂起,10s 后取消,让测试失败而不是永久等待
        let watchdog_done = Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            let done = watchdog_done.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(10));
                if !done.load(Ordering::SeqCst) {
                    cancel.cancel();
                }
            });
        }

        let result = run(cmd, &progress, &cancel, &input);
        watchdog_done.store(true, Ordering::SeqCst);
        assert!(sender.join().unwrap(), "runtime never accepted input");
        let (code, _tail) = result.expect("pty run");
        assert_eq!(code, 0);
        let out = lines.lock().unwrap().join("\n");
        assert!(out.contains("Ask [y/N]:"), "prompt missing: {out}");
        assert!(out.contains("answer=yes"), "answer missing: {out}");
        assert_eq!(*flags.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn send_without_running_child_is_rejected() {
        assert!(!InputHandle::new().send("y"));
    }

    #[test]
    fn child_gets_controlling_terminal() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'tty? '; read ans </dev/tty; echo \"tty=$ans\"");
        let progress = Collector::default();
        let lines = progress.lines.clone();
        let cancel = Arc::new(CancelHandle::new());
        let input = Arc::new(InputHandle::new());

        let input_tx = input.clone();
        let sender = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !input_tx.send("ok") {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            true
        });

        let result = run(cmd, &progress, &cancel, &input);
        assert!(sender.join().unwrap(), "runtime never accepted input");
        let (code, _tail) = result.expect("pty run");
        assert_eq!(code, 0);
        let out = lines.lock().unwrap().join("\n");
        // 没有控制终端时 /dev/tty 打不开,read 失败,答案不会出现
        assert!(out.contains("tty=ok"), "/dev/tty unavailable: {out}");
    }

    #[test]
    fn sanitize_strips_terminal_controls() {
        assert_eq!(sanitize(b"plain"), "plain");
        // CSI:下划线 + 重置、括号粘贴开关
        assert_eq!(sanitize(b"\x1b[4mAsk\x1b[24m?"), "Ask?");
        assert_eq!(sanitize(b"\x1b[?2004hask\x1b[?2004l"), "ask");
        // ESC ( B 字符集选择、OSC 标题(BEL / ST 终止)
        assert_eq!(sanitize(b"\x1b(Btext"), "text");
        assert_eq!(sanitize(b"\x1b]0;title\x07text"), "text");
        assert_eq!(sanitize(b"\x1b]0;title\x1b\\text"), "text");
        // 回车按终端语义回到行首;退格删除前一个字符(含多字节)
        assert_eq!(sanitize(b"abc\rdef"), "def");
        assert_eq!(sanitize("ab\x08c".as_bytes()), "ac");
        assert_eq!(sanitize("中\x08文".as_bytes()), "文");
        // 制表符保留,其余 C0 控制字符丢弃
        assert_eq!(sanitize(b"a\tb\x07c"), "a\tbc");
    }
}
