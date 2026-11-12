//! apt 安装标记(extended_states)补齐。
//!
//! 本工具经 dpkg 直接写库,不经过 apt:新装的依赖在 apt 看来仍是“手动安装”,
//! `apt autoremove` 永远不会回收。因此安装成功后用 `apt-mark` 补齐标记,
//! 对齐 apt 语义:
//! - 随包新装的依赖 → auto(可被 autoremove 回收)
//! - 用户显式安装的本地 deb → manual(即使此前是作为依赖自动装的)
//!
//! 标记失败不改变安装结论(包已装好,只是自动清理语义退化),
//! 调用方将错误降级为输出警告。

use std::process::{Command, Stdio};

use crate::error::FdebiError;
use crate::install::CancelHandle;
use crate::sysstate::base_name;
use crate::ui::InstallProgress;

/// 从求解器的 install 集筛出“随包依赖”:去掉本地包自身(显式安装)
pub fn dependency_names(install: &[String], local: &str) -> Vec<String> {
    let local_base = base_name(local);
    install
        .iter()
        .filter(|key| base_name(key) != local_base)
        .cloned()
        .collect()
}

/// 安装成功后补齐标记:依赖 → auto,本地包 → manual
pub fn apply(
    progress: &dyn InstallProgress,
    cancel: &CancelHandle,
    auto: &[String],
    manual: &str,
) -> Result<(), FdebiError> {
    if !auto.is_empty() {
        if cancel.is_cancelled() {
            return Err(FdebiError::Cancelled);
        }
        run(progress, "auto", auto)?;
    }
    if !manual.is_empty() {
        if cancel.is_cancelled() {
            return Err(FdebiError::Cancelled);
        }
        run(progress, "manual", &[manual.to_string()])?;
    }
    Ok(())
}

/// 执行一次 apt-mark,输出逐行转发(成功与失败信息都进终端视图)
fn run(progress: &dyn InstallProgress, mode: &str, names: &[String]) -> Result<(), FdebiError> {
    let out = Command::new("apt-mark")
        .arg(mode)
        .args(names)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        progress.output_line(line);
    }
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        progress.output_line(line);
    }
    if !out.status.success() {
        return Err(FdebiError::Subprocess(
            format!("apt-mark {mode}"),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_package_excluded_from_auto_marks() {
        let install = vec![
            "myapp".to_string(),
            "libfoo".to_string(),
            "libbar:amd64".to_string(),
        ];
        assert_eq!(
            dependency_names(&install, "myapp"),
            vec!["libfoo".to_string(), "libbar:amd64".to_string()]
        );
    }

    #[test]
    fn same_name_other_arch_stays_manual() {
        // 本地包自身的键可能带架构后缀;按基名排除
        let install = vec!["myapp:i386".to_string(), "libfoo".to_string()];
        assert_eq!(
            dependency_names(&install, "myapp"),
            vec!["libfoo".to_string()]
        );
    }

    #[test]
    fn no_dependencies_yields_empty() {
        assert!(dependency_names(&["myapp".to_string()], "myapp").is_empty());
    }
}
