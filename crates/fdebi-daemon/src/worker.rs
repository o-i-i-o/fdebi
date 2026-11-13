//! apt worker 进程驱动。
//!
//! - 请求:序列化到 root 专属 0600 临时文件,worker 读取后立即删除。
//! - 输出:PTY master 逐行读取,去除 ANSI/控制字符后送 `Msg::Output`。
//! - 控制:worker 的 fd 3 是单向管道,每行一个 JSON `Event`。
//! - 输入:`InputHandle` 的行写入 PTY master,等价于终端键入。
//! - 取消:`CancelHandle` 向 worker 进程组发 SIGTERM(连带 dpkg)。

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use fdebi_apt_proto::{Event, Mode, Request};
use fdebi_core::install::CancelHandle;

/// worker 事件与 PTY 输出共用一条 channel,调用方按到达顺序处理。
pub enum Msg {
    Output(String),
    Event(Event),
}

/// 交互应答句柄:daemon 侧持有,spawn 时绑定到当前 worker。
#[derive(Default)]
pub struct InputHandle {
    tx: Mutex<Option<Sender<String>>>,
}

impl InputHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 提交一行应答;当前没有运行中的 worker 时返回 false
    pub fn send(&self, line: &str) -> bool {
        match self.tx.lock().unwrap().as_ref() {
            Some(tx) => tx.send(line.to_string()).is_ok(),
            None => false,
        }
    }

    fn attach(&self, tx: Sender<String>) {
        *self.tx.lock().unwrap() = Some(tx);
    }

    fn detach(&self) {
        *self.tx.lock().unwrap() = None;
    }
}

/// 运行中的 worker:事件流 + 待回收的 IO 线程。
pub struct Worker {
    pub child: Child,
    pub rx: Receiver<Msg>,
    writer: Option<JoinHandle<()>>,
    pty: Option<JoinHandle<()>>,
    control: Option<JoinHandle<()>>,
}

impl Worker {
    /// 停止输入、等待 IO 线程与子进程退出。
    pub fn finish(mut self, input: &InputHandle) -> io::Result<ExitStatus> {
        input.detach();
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        if let Some(h) = self.pty.take() {
            let _ = h.join();
        }
        if let Some(h) = self.control.take() {
            let _ = h.join();
        }
        self.child.wait()
    }
}

static REQ_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 创建 worker、PTY 与控制管道;返回事件流与子进程句柄。
pub fn spawn(
    mode: Mode,
    req: &Request,
    cancel: &CancelHandle,
    input: &Arc<InputHandle>,
) -> Result<Worker, String> {
    let request_path = write_request(req)?;
    match spawn_inner(mode, req, cancel, input, &request_path) {
        Ok(worker) => Ok(worker),
        Err(e) => {
            let _ = std::fs::remove_file(&request_path);
            Err(e)
        }
    }
}

fn spawn_inner(
    mode: Mode,
    _req: &Request,
    cancel: &CancelHandle,
    input: &Arc<InputHandle>,
    request_path: &PathBuf,
) -> Result<Worker, String> {
    // 控制管道:父读子写(子端 dup2 到 fd 3)
    let mut pipe_fds = [0 as RawFd; 2];
    // SAFETY: pipe2 写入两个新 fd
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("pipe2: {}", io::Error::last_os_error()));
    }
    let (control_r, control_w) = (pipe_fds[0], pipe_fds[1]);

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let winsize = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty 成功时写回两个新 fd;termios/winsize 指针有效
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    if rc != 0 {
        close_fd(control_r);
        close_fd(control_w);
        return Err(format!("openpty: {}", io::Error::last_os_error()));
    }

    // 从端关闭回显:前端已经显示了用户输入,避免再回显一遍
    // SAFETY: slave 是 openpty 返回的有效 fd
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave, &mut term) == 0 {
            term.c_lflag &= !(libc::ECHO | libc::ECHONL);
            let _ = libc::tcsetattr(slave, libc::TCSANOW, &term);
        }
    }

    // SAFETY: openpty 返回的 fd 由此处独占所有权
    let master_file = unsafe { File::from_raw_fd(master) };
    let slave_file = unsafe { File::from_raw_fd(slave) };
    let stdin = slave_file
        .try_clone()
        .map_err(|e| format!("clone pty slave: {e}"))?;
    let stdout = slave_file
        .try_clone()
        .map_err(|e| format!("clone pty slave: {e}"))?;

    let exe = worker_path()?;
    let mut cmd = Command::new(&exe);
    cmd.arg(mode.as_arg())
        .arg(request_path)
        .env("TERM", "xterm")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave_file));

    // 子进程:新会话 + PTY 为控制终端 + 控制管道到 fd 3
    // SAFETY: 闭包只调用 async-signal-safe 的 setsid/ioctl/dup2/close/fcntl
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            if control_w != 3 {
                if libc::dup2(control_w, 3) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(control_w);
            } else {
                // dup2 会清除 CLOEXEC;若本来就在 3 则手动清除
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            close_fd(control_r);
            close_fd(control_w);
            return Err(format!("spawn {}: {e}", exe.display()));
        }
    };
    // 关闭父进程持有的从端副本,否则主端读不到 EOF
    drop(cmd);
    close_fd(control_w);
    cancel.register(child.id());

    let control_file = unsafe { File::from_raw_fd(control_r) };
    let master_reader = master_file
        .try_clone()
        .map_err(|e| format!("clone pty master: {e}"))?;
    let mut master_writer = master_file;

    let (tx, rx) = mpsc::channel::<Msg>();

    // 写线程:InputHandle 的行 → PTY master
    let (line_tx, line_rx) = mpsc::channel::<String>();
    input.attach(line_tx);
    let writer = std::thread::spawn(move || {
        for line in line_rx {
            if writeln!(master_writer, "{line}")
                .and_then(|()| master_writer.flush())
                .is_err()
            {
                break;
            }
        }
    });

    // PTY 读线程
    let pty_tx = tx.clone();
    let pty = std::thread::spawn(move || read_pty(master_reader, pty_tx));

    // 控制通道读线程
    let control_tx = tx.clone();
    let control = std::thread::spawn(move || read_control(control_file, control_tx));

    drop(tx);
    Ok(Worker {
        child,
        rx,
        writer: Some(writer),
        pty: Some(pty),
        control: Some(control),
    })
}

/// 把请求写入 root 专属临时文件(0600,create_new 防止抢占)。
fn write_request(req: &Request) -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        ".fdebi-req-{}-{}-{}-{}.json",
        std::process::id(),
        seq,
        nanos,
        // 额外熵:地址随机化下的栈地址
        &req as *const _ as usize
    );
    let path = std::env::temp_dir().join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot create request file {}: {e}", path.display()))?;
    serde_json::to_writer(&mut file, req)
        .map_err(|e| format!("cannot write request file: {e}"))?;
    file.flush()
        .map_err(|e| format!("cannot flush request file: {e}"))?;
    Ok(path)
}

fn worker_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("FDEBI_APT_WORKER") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("fdebi-apt-worker");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    for candidate in [
        "/usr/local/bin/fdebi-apt-worker",
        "/usr/lib/fdebi/fdebi-apt-worker",
    ] {
        if std::path::Path::new(candidate).is_file() {
            return Ok(PathBuf::from(candidate));
        }
    }
    Err("fdebi-apt-worker not found".into())
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: 调用方不再使用该 fd
        unsafe {
            libc::close(fd);
        }
    }
}

/// 读 PTY master,按行清洗后转发;EIO 视为对端关闭。
fn read_pty(mut master: File, tx: Sender<Msg>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let text = clean_line(&line[..line.len().saturating_sub(1)]);
                    if !text.is_empty() {
                        let _ = tx.send(Msg::Output(text));
                    }
                }
                // 无换行的提示符(debconf 问题)也应立即呈现
                if pending.len() > 4096 {
                    let text = clean_line(&pending);
                    pending.clear();
                    if !text.is_empty() {
                        let _ = tx.send(Msg::Output(text));
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let text = clean_line(&pending);
        if !text.is_empty() {
            let _ = tx.send(Msg::Output(text));
        }
    }
}

/// 去掉 ANSI 转义序列、回车与退格;保留可读文本。
fn clean_line(bytes: &[u8]) -> String {
    let mut out = String::new();
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: 参数直到 @..~
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: 直到 BEL 或 ST
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' => {}
            '\x08' => {
                out.pop();
            }
            _ => out.push(c),
        }
    }
    out.trim_end().to_string()
}

fn read_control(control: File, tx: Sender<Msg>) {
    let reader = BufReader::new(control);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(event) => {
                let _ = tx.send(Msg::Event(event));
            }
            Err(e) => {
                let _ = tx.send(Msg::Output(format!("invalid worker event: {e}: {line}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clean_line;

    #[test]
    fn strips_terminal_sequences() {
        assert_eq!(clean_line(b"\x1b[1mHello\x1b[0m\r"), "Hello");
        assert_eq!(clean_line(b"abc\x08d"), "abd");
        assert_eq!(clean_line(b"\x1b]0;title\x07ok"), "ok");
    }
}

/// 用假 worker(替代 fdebi-apt-worker)验证 PTY/控制通道/输入/取消。
#[cfg(test)]
mod io_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    use super::*;
    use fdebi_apt_proto::Request;

    /// 环境变量是进程级的:测试必须串行设置 FDEBI_APT_WORKER
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct FakeWorker {
        dir: PathBuf,
        script: PathBuf,
    }

    impl FakeWorker {
        fn new(tag: &str, body: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fdebi-worker-test-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("fake-worker");
            std::fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&script, PermissionsExt::from_mode(0o755)).unwrap();
            Self { dir, script }
        }

        fn activate(&self) {
            // SAFETY: 测试内单线程使用;测试结束前恢复
            unsafe { std::env::set_var("FDEBI_APT_WORKER", &self.script) };
        }
    }

    impl Drop for FakeWorker {
        fn drop(&mut self) {
            // SAFETY: 同上
            unsafe { std::env::remove_var("FDEBI_APT_WORKER") };
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn drives_pty_control_and_input() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake = FakeWorker::new(
            "io",
            "echo pty-hello\n\
             printf '%s\\n' '{\"event\":\"stage\",\"phase\":\"installing\",\"desc\":\"fake\"}' >&3\n\
             read line\n\
             echo \"got:$line\"\n\
             printf '%s\\n' '{\"event\":\"result\",\"ok\":true,\"detail\":\"\",\"items\":[]}' >&3\n",
        );
        fake.activate();

        let cancel = CancelHandle::new();
        let input = Arc::new(InputHandle::new());
        let worker = spawn(Mode::Analyze, &Request::default(), &cancel, &input).unwrap();

        let mut output = Vec::new();
        // 先等 PTY 输出与 stage 事件(此时假 worker 正阻塞在 read)
        loop {
            match worker.rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Msg::Output(line)) => output.push(line),
                Ok(Msg::Event(Event::Stage { .. })) => break,
                Ok(_) => {}
                Err(e) => panic!("no stage event: {e}"),
            }
        }
        assert!(input.send("answer"), "input handle not attached");

        let mut saw_answer = false;
        let mut saw_result = false;
        while let Ok(msg) = worker.rx.recv_timeout(Duration::from_secs(10)) {
            match msg {
                Msg::Output(line) => {
                    saw_answer |= line.contains("got:answer");
                }
                Msg::Event(Event::Result { ok, .. }) => {
                    saw_result = ok;
                    break;
                }
                _ => {}
            }
        }
        let status = worker.finish(&input).unwrap();
        assert!(status.success());
        assert!(
            output.iter().any(|l| l.contains("pty-hello")),
            "pty output missing: {output:?}"
        );
        assert!(saw_answer, "input not delivered to pty");
        assert!(saw_result, "control event not received");
    }

    #[test]
    fn cancel_terminates_process_group() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake = FakeWorker::new("cancel", "sleep 30\n");
        fake.activate();

        let cancel = CancelHandle::new();
        let input = Arc::new(InputHandle::new());
        let worker = spawn(Mode::Analyze, &Request::default(), &cancel, &input).unwrap();
        cancel.cancel();
        let status = worker.finish(&input).unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}

