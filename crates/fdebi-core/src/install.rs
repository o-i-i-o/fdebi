//! 事务共用的小型原语:conffile 策略与取消句柄。
//!
//! 实际的 apt/dpkg 编排放置在独立的 `fdebi-apt-worker` 进程中,daemon 只
//! 持有这里的类型来解析 D-Bus 参数并终止 worker 进程组。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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

    /// 登记运行中的 worker 进程组(daemon 的 PTY 驱动使用)
    pub fn register(&self, pid: u32) {
        *self.pgid.lock().unwrap() = Some(pid);
    }

    /// 取消登记(子进程已回收)
    pub fn unregister(&self) {
        *self.pgid.lock().unwrap() = None;
    }
}
