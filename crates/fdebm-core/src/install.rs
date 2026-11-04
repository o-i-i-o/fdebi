//! dpkg 执行层(纯 Rust 管道的最后一段:实际写入仍由 dpkg 完成)。
//!
//! 语义:
//! - 需先卸载的冲突包:`dpkg --auto-deconfigure -r <names>`(源自求解器)
//! - 依赖 deb + 本地 deb 单次批量:`dpkg --auto-deconfigure -i <debs>`,
//!   dpkg 自行处理解包/配置顺序与 Pre-Depends
//! - conffile 提示:GUI 预检阶段由用户决策([`ConfPolicy`]),安装时以
//!   `--force-confold/--force-confnew` 确定性执行
//! - debconf:包携带模板时([`InstallBatch::interactive`])在 PTY 中运行并
//!   使用 readline 前端,前端经 [`pty::InputHandle`] 应答应答;其余安装用
//!   管道 + noninteractive(无 tty,提示按默认值执行)
//! - 进度:子进程输出逐行转发 [`InstallProgress::output_line`]
//!   (stdout/stderr 双 reader 线程合并,避免管道缓冲死锁)
//! - 互斥:执行前经 [`lock`] 探测包管理前端是否占锁(dpkg/apt-get/apt 等)
//!   并等待其结束
//! - 复核:dpkg 执行前用 [`FileStamp`] 确认本地 deb 未被替换
//! - 标记:成功后经 [`aptmark`] 补齐 apt 标记(依赖 auto / 本地包 manual),
//!   对齐 apt 的 autoremove 语义;标记失败仅告警,不影响安装结论
//! - 失败:保留输出尾部并归类([`FailureKind`]),返回 [`InstallReport`]
//! - 取消:[`CancelHandle`] 标志位 + 对子进程组发 SIGTERM

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use crate::aptmark;
use crate::error::FdebmError;
use crate::i18n::{gettext, tf};
use crate::lock;
use crate::pty;
use crate::ui::{AlertKind, InstallProgress, Ui};

/// conffile 覆盖策略(GUI 预检对话框的用户决策;D-Bus 上以字符串传递)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfPolicy {
    /// 无冲突:dpkg 默认语义(未修改的 conffile 静默升级)
    #[default]
    Default,
    /// 保留本地已修改版本(新版本落为 <file>.dpkg-dist)
    Keep,
    /// 使用包携带的新版本(本地版本备份为 <file>.dpkg-old)
    Replace,
}

impl ConfPolicy {
    /// D-Bus 字符串(""/"keep"/"replace")→ 策略
    pub fn from_arg(s: &str) -> Self {
        match s {
            "keep" => Self::Keep,
            "replace" => Self::Replace,
            _ => Self::Default,
        }
    }

    fn dpkg_arg(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Keep => Some("--force-confold"),
            Self::Replace => Some("--force-confnew"),
        }
    }
}

/// 取消句柄:跨线程共享;cancel() 置位并向当前子进程组发 SIGTERM
#[derive(Default)]
pub struct CancelHandle {
    cancelled: AtomicBool,
    pgid: Mutex<Option<u32>>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 请求取消:置位并终止运行中的子进程组(幂等)
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pgid.lock().unwrap() {
            // process_group(0) 使子进程自成一个进程组,pgid == 子 pid;
            // 负 pid 表示向整组投递,维护脚本等子进程也一并收到 SIGTERM
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        }
    }

    pub(crate) fn register(&self, pid: u32) {
        *self.pgid.lock().unwrap() = Some(pid);
    }

    pub(crate) fn unregister(&self) {
        *self.pgid.lock().unwrap() = None;
    }
}

/// 本地 deb 的文件指纹:求解/授权/下载期间文件可能被替换或删除
/// (TOCTOU),dpkg 执行前据此确认仍是用户确认过的那一份。
/// 含 ctime:它与 mtime 不同,文件属主无法回填,可发现"改写内容后
/// 把 mtime/大小恢复原样"的绕过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl FileStamp {
    pub fn capture(path: &Path) -> Result<Self, FdebmError> {
        let m = std::fs::metadata(path)?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        })
    }

    /// 文件仍存在且指纹一致
    pub fn matches(&self, path: &Path) -> bool {
        Self::capture(path).is_ok_and(|s| s == *self)
    }
}

/// dpkg 失败归类:把输出尾部映射为可读原因,前端不必解析 dpkg 文本
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 成功,或输出中没有可识别的原因(仅凭退出码报告)
    Unknown,
    /// 全局锁被占用(dpkg 前后端锁)
    Locked,
    /// 磁盘空间不足
    DiskFull,
    /// 依赖问题(未满足的依赖)
    Dependency,
    /// 维护脚本(postinst/prerm 等)失败
    Script,
}

impl FailureKind {
    /// 从合并后的输出行归类;顺序即优先级
    pub fn classify(lines: &[String]) -> Self {
        let text = lines.join("\n").to_lowercase();
        let has = |needle: &str| text.contains(needle);
        if has("lock was locked by another process")
            || has("unable to acquire the dpkg frontend lock")
            || has("unable to acquire the dpkg backend lock")
            || has("is another process using it")
        {
            Self::Locked
        } else if has("no space left on device") {
            Self::DiskFull
        } else if has("dependency problems prevent configuration")
            || has("unmet dependencies")
            || has("dependency problems - leaving")
        {
            Self::Dependency
        } else if has("maintainer script")
            || has("post-installation script")
            || has("pre-installation script")
            || has("post-removal script")
            || has("pre-removal script")
        {
            Self::Script
        } else {
            Self::Unknown
        }
    }

    fn message(self) -> Option<String> {
        match self {
            Self::Locked => Some(gettext(
                "Another package operation is in progress. Please try again later.",
            )),
            Self::DiskFull => Some(gettext(
                "There is not enough disk space to complete the operation.",
            )),
            Self::Dependency => Some(gettext(
                "Dependency problems prevented the operation from completing.",
            )),
            Self::Script => Some(gettext(
                "A package maintainer script failed; see the log for details.",
            )),
            Self::Unknown => None,
        }
    }
}

/// dpkg 一步(卸载/安装)的执行结果
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub code: i32,
    pub failure: FailureKind,
}

impl InstallReport {
    fn from_outcome(out: DpkgOutcome) -> Self {
        Self {
            code: out.code,
            failure: FailureKind::classify(&out.tail),
        }
    }

    /// 面向用户的失败描述;成功时为空串
    pub fn detail(&self) -> String {
        if self.code == 0 {
            return String::new();
        }
        match self.failure.message() {
            Some(msg) => msg,
            None => tf("dpkg exited with status {0}", &[&self.code.to_string()]),
        }
    }
}

/// 一次安装事务的执行载荷
pub struct InstallBatch {
    /// 本地 deb(命令行目标)
    pub local: PathBuf,
    /// 打开 deb 时捕获的指纹;Some 时 dpkg 执行前复核
    pub local_stamp: Option<FileStamp>,
    /// 已下载的依赖 deb(download_all 的产物)
    pub dep_debs: Vec<PathBuf>,
    /// 需先卸载的冲突包(求解器给出的 remove 集)
    pub remove: Vec<String>,
    /// conffile 覆盖策略(GUI 预检决策)
    pub conf_policy: ConfPolicy,
    /// 随包新装的依赖键(changes.install 去掉本地包);成功后 apt-mark auto
    pub auto_mark: Vec<String>,
    /// 本地包名;成功后 apt-mark manual(对齐 apt 显式安装语义)
    pub local_package: String,
    /// 包(本地或其依赖)携带 debconf 模板:在 PTY 中运行并接受应答
    pub interactive: bool,
}

/// 执行安装,返回 dpkg 执行结果。unauthenticated 的确认由前端负责;
/// 交互式安装的应答经 `input` 注入(仅 [`InstallBatch::interactive`] 时生效)。
pub fn install(
    ui: &dyn Ui,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
    input: &pty::InputHandle,
    batch: &InstallBatch,
) -> Result<InstallReport, FdebmError> {
    if cancel.is_cancelled() {
        return Err(FdebmError::Cancelled);
    }
    lock::wait_until_idle(progress, cancel)?;
    verify_inputs(batch)?;

    if !batch.remove.is_empty() {
        progress.stage(&gettext("Removing conflicting packages..."));
        let mut cmd = Command::new("dpkg");
        cmd.arg("--auto-deconfigure").arg("-r");
        for name in &batch.remove {
            cmd.arg(name);
        }
        let report = InstallReport::from_outcome(run_dpkg(cmd, progress, cancel)?);
        if report.code != 0 {
            let _ = ui.alert(
                AlertKind::Error,
                &gettext("Error during install"),
                Some(&report.detail()),
            );
            return Ok(report);
        }
        if cancel.is_cancelled() {
            return Err(FdebmError::Cancelled);
        }
        // 卸载与安装之间可能被其他包管理操作插入
        lock::wait_until_idle(progress, cancel)?;
        // 卸载耗时期间本地 deb 也可能被替换,装机前再复核一次
        verify_inputs(batch)?;
    }

    progress.stage(&gettext("Installing packages..."));
    let mut cmd = Command::new("dpkg");
    cmd.arg("--auto-deconfigure").arg("-i");
    // conffile 策略是 dpkg 选项,必须排在归档文件参数之前 —— 曾追加在
    // 文件列表之后,dpkg 将其误解析为归档路径(dpkg 退出状态 2)
    if let Some(arg) = batch.conf_policy.dpkg_arg() {
        cmd.arg(arg);
    }
    cmd.arg(std::path::absolute(&batch.local)?);
    for deb in &batch.dep_debs {
        cmd.arg(deb);
    }
    // 交互式:readline 前端把 debconf 问题转成行式问答(PTY 提供终端语义);
    // 其余情况(noninteractive)问题按默认策略静默处理
    let frontend = if batch.interactive {
        "readline"
    } else {
        "noninteractive"
    };
    cmd.env("DEBIAN_FRONTEND", frontend);
    let outcome = if batch.interactive {
        let (code, tail) = pty::run(cmd, progress, cancel, input)?;
        DpkgOutcome { code, tail }
    } else {
        run_dpkg(cmd, progress, cancel)?
    };
    let report = InstallReport::from_outcome(outcome);
    if report.code != 0 {
        let _ = ui.alert(
            AlertKind::Error,
            &gettext("Error during install"),
            Some(&report.detail()),
        );
    } else {
        mark_installed(progress, cancel, batch);
    }
    Ok(report)
}

/// 成功后补齐 apt 标记;失败降级为输出警告(包已装好,只是 autoremove 语义退化)
fn mark_installed(progress: &dyn InstallProgress, cancel: &CancelHandle, batch: &InstallBatch) {
    if batch.auto_mark.is_empty() && batch.local_package.is_empty() {
        return;
    }
    // dpkg 成功后用户已取消:跳过收尾(标记可在下次安装时补齐)
    if cancel.is_cancelled() {
        return;
    }
    if !batch.auto_mark.is_empty() {
        progress.stage(&gettext(
            "Marking dependencies as automatically installed...",
        ));
    }
    if let Err(e) = aptmark::apply(progress, cancel, &batch.auto_mark, &batch.local_package)
        && !matches!(e, FdebmError::Cancelled)
    {
        progress.output_line(&tf(
            "Warning: could not update apt installation marks: {0}",
            &[&e.to_string()],
        ));
    }
}

/// 执行卸载,返回 dpkg 执行结果(dpkg 语义:反向依赖会阻止卸载并报错)
pub fn remove(
    ui: &dyn Ui,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
    name: &str,
) -> Result<InstallReport, FdebmError> {
    if cancel.is_cancelled() {
        return Err(FdebmError::Cancelled);
    }
    lock::wait_until_idle(progress, cancel)?;
    progress.stage(&gettext("Removing packages..."));
    let mut cmd = Command::new("dpkg");
    cmd.arg("-r").arg(name);
    let report = InstallReport::from_outcome(run_dpkg(cmd, progress, cancel)?);
    if report.code != 0 {
        let _ = ui.alert(
            AlertKind::Error,
            &gettext("Error during install"),
            Some(&report.detail()),
        );
    }
    Ok(report)
}

/// dpkg 执行前复核输入文件:本地 deb 指纹未变、依赖 deb 仍存在
fn verify_inputs(batch: &InstallBatch) -> Result<(), FdebmError> {
    if let Some(stamp) = batch.local_stamp
        && !stamp.matches(&batch.local)
    {
        return Err(FdebmError::FileChanged(batch.local.clone()));
    }
    for deb in &batch.dep_debs {
        if !deb.exists() {
            return Err(FdebmError::FileChanged(deb.clone()));
        }
    }
    Ok(())
}

/// dpkg 一步的输出结果
struct DpkgOutcome {
    code: i32,
    /// 输出尾部:归类失败原因用
    tail: Vec<String>,
}

/// 启动(独立进程组)、注册取消句柄、转发输出、等待退出码
fn run_dpkg(
    mut cmd: Command,
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
) -> Result<DpkgOutcome, FdebmError> {
    use std::os::unix::process::CommandExt as _;
    // spawn 前设独立进程组:cancel() 可对整组 SIGTERM(含维护脚本子进程)
    cmd.process_group(0);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;
    // process_group(0) 后 pgid == 子 pid
    cancel.register(child.id());
    let tail = forward_output(&mut child, progress);
    // wait 失败也要注销,否则取消句柄会继续向可能已被复用的 pid 投递信号
    let status = child.wait();
    cancel.unregister();
    let status = status?;
    if cancel.is_cancelled() {
        return Err(FdebmError::Cancelled);
    }
    Ok(DpkgOutcome {
        code: status.code().unwrap_or(-1),
        tail,
    })
}

/// 输出尾部保留行数(失败归类只需末尾的诊断信息)
pub(crate) const TAIL_LINES: usize = 200;

/// 合并子进程 stdout/stderr 逐行转发到 progress,返回输出尾部
fn forward_output(child: &mut Child, progress: &dyn InstallProgress) -> Vec<String> {
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        }));
    }
    // 两个 reader 线程都结束后 channel 关闭,循环退出
    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    for line in rx {
        progress.output_line(&line);
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    for t in readers {
        let _ = t.join();
    }
    tail.into()
}

#[cfg(test)]
mod tests {
    use super::{FailureKind, FileStamp};

    #[test]
    fn stamp_detects_replacement_and_removal() {
        let path =
            std::env::temp_dir().join(format!("fdebm-stamp-{}-replace.deb", std::process::id()));
        std::fs::write(&path, b"first").unwrap();
        let stamp = FileStamp::capture(&path).unwrap();
        assert!(stamp.matches(&path));
        std::fs::write(&path, b"second!").unwrap();
        assert!(!stamp.matches(&path));
        std::fs::remove_file(&path).unwrap();
        assert!(!stamp.matches(&path));
    }

    #[test]
    fn classify_dpkg_failures() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            FailureKind::classify(&lines(&[
                "dpkg: error: dpkg frontend lock was locked by another process with pid 1234",
            ])),
            FailureKind::Locked
        );
        assert_eq!(
            FailureKind::classify(&lines(&[
                "dpkg: error processing archive foo.deb (--install):",
                "cannot copy extracted data: No space left on device",
            ])),
            FailureKind::DiskFull
        );
        assert_eq!(
            FailureKind::classify(&lines(&[
                "dpkg: dependency problems prevent configuration of foo:",
            ])),
            FailureKind::Dependency
        );
        assert_eq!(
            FailureKind::classify(&lines(&[
                "dpkg: error processing package foo (--install):",
                " installed post-installation script subprocess returned error exit status 1",
            ])),
            FailureKind::Script
        );
        assert_eq!(
            FailureKind::classify(&lines(&["Errors were encountered while processing:"])),
            FailureKind::Unknown
        );
    }
}
