//! dpkg 互斥探测:执行 dpkg 前确认没有其他包管理操作正在运行。
//!
//! 互斥本身仍由 dpkg 的全局锁保证;这里的探测只用于等待期间向用户说明
//! 卡住的原因。探测经 `ps -e -o comm=` 读取进程名(与 deepin-deb-installer
//! 的 isDpkgRunning 同一思路),不解析 dpkg/apt 的私有锁文件。

use std::process::Command;
use std::time::Duration;

use crate::error::FdebmError;
use crate::i18n::gettext;
use crate::install::CancelHandle;
use crate::ui::InstallProgress;

/// 轮询间隔
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 是否有 dpkg 主进程正在运行
pub fn dpkg_running() -> bool {
    let Ok(out) = Command::new("ps").args(["-e", "-o", "comm="]).output() else {
        // ps 不可用时按空闲处理:真正的互斥仍由 dpkg 锁保证
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| is_dpkg_process(line.trim()))
}

fn is_dpkg_process(comm: &str) -> bool {
    comm == "dpkg"
}

/// 等待其他 dpkg 事务结束;等待期间可取消。返回时 dpkg 空闲。
pub fn wait_until_idle(
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
) -> Result<(), FdebmError> {
    if !dpkg_running() {
        return Ok(());
    }
    progress.stage(&gettext(
        "Another package operation is in progress; waiting...",
    ));
    progress.output_line(&gettext("dpkg is running; waiting for it to finish..."));
    while dpkg_running() {
        if cancel.is_cancelled() {
            return Err(FdebmError::Cancelled);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_dpkg_process;

    #[test]
    fn only_dpkg_main_process_matches() {
        assert!(is_dpkg_process("dpkg"));
        assert!(!is_dpkg_process("dpkg-deb"));
        assert!(!is_dpkg_process("apt-get"));
        assert!(!is_dpkg_process(""));
    }
}
