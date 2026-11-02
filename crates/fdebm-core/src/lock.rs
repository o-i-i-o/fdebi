//! dpkg 前端锁探测:执行特权包管理操作前,等待其他包管理前端占锁结束。
//!
//! 互斥本身由 dpkg 维护(前端锁 /var/lib/dpkg/lock-frontend);这里的探测
//! 只用于等待期间向用户说明卡住的原因,避免一上来就撞锁报错。探测沿用
//! `ps -e -o comm=` 读取进程名的通行做法,并把持有前端锁的交互式前端一并
//! 计入:apt-get/apt 升级等用户后台操作是标准的互斥场景,等待而非立即
//! 失败更符合 Debian 业务流。
//!
//! 白名单刻意**不含** `apt-mark`(本工具自身在安装收尾阶段调用,纳入
//! 即死锁)与 `unattended-upgrades`(非交互前端,撞锁会立即被
//! FailureKind::Locked 归类上报,长时间静默等待反而更难理解)。

use std::process::Command;
use std::time::Duration;

use crate::error::FdebmError;
use crate::i18n::gettext;
use crate::install::CancelHandle;
use crate::ui::InstallProgress;

/// 轮询间隔
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 是否有包管理前端正在运行
pub fn frontend_busy() -> bool {
    let Ok(out) = Command::new("ps").args(["-e", "-o", "comm="]).output() else {
        // ps 不可用时按空闲处理:真正的互斥仍由 dpkg 锁保证
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| is_lock_holder(line.trim()))
}

/// 进程名 → 是否持有 dpkg 前端锁
/// (ps comm 按可执行名匹配;dpkg 系子进程 dpkg-deb/dpkg-query 等不持有锁)
fn is_lock_holder(comm: &str) -> bool {
    matches!(comm, "dpkg" | "apt" | "apt-get" | "aptitude" | "synaptic")
}

/// 等待其他包管理事务结束;等待期间可取消。返回时 dpkg 前端空闲。
pub fn wait_until_idle(
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
) -> Result<(), FdebmError> {
    if !frontend_busy() {
        return Ok(());
    }
    progress.stage(&gettext(
        "Another package operation is in progress; waiting...",
    ));
    progress.output_line(&gettext("dpkg is running; waiting for it to finish..."));
    while frontend_busy() {
        if cancel.is_cancelled() {
            return Err(FdebmError::Cancelled);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_lock_holder;

    #[test]
    fn interactive_frontend_holders_match() {
        assert!(is_lock_holder("dpkg"));
        assert!(is_lock_holder("apt"));
        assert!(is_lock_holder("apt-get"));
        assert!(is_lock_holder("aptitude"));
        assert!(is_lock_holder("synaptic"));
        // 不持有前端锁 / 会导致死锁的名字不匹配
        assert!(!is_lock_holder("dpkg-deb"));
        assert!(!is_lock_holder("dpkg-query"));
        assert!(!is_lock_holder("apt-mark"));
        assert!(!is_lock_holder("unattended-upgr"));
        assert!(!is_lock_holder(""));
    }
}
