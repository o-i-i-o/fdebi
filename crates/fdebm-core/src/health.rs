//! 系统包管理健康检查(`dpkg --audit` 的机器可读等价物)。
//!
//! gdebi 用 apt 的 `broken_count` 判断安装前后系统是否处于依赖不完整
//! 状态;fdebm 不依赖 libapt,改用 `dpkg-query` 的状态缩写:
//! - 第二列(H/F/U/W/t)表示半安装、半配置、已解包未配置、触发器未处理,
//!   这些状态都需要 `apt-get install -f` 或重装才能恢复;
//! - `rc`(仅剩配置文件)与 `un`(未安装)是正常状态,不报告。
//!
//! 分析阶段用作“系统已有未完成操作”的提示;安装阶段以事务开始前的
//! 快照做差集,只报告本次事务新增的问题。

use std::process::Command;

use crate::error::FdebmError;

/// 状态缩写是否表示包处于异常状态
fn abnormal_status(abbrev: &str) -> bool {
    matches!(
        abbrev.as_bytes().get(1),
        Some(b'H' | b'F' | b'U' | b'W' | b't')
    )
}

/// 当前处于异常状态的包(按包名排序,去重)
pub fn broken_packages() -> Result<Vec<String>, FdebmError> {
    let out = Command::new("dpkg-query")
        .args(["-W", "-f=${binary:Package}\t${db:Status-Abbrev}\n"])
        .env("LC_ALL", "C")
        .output()?;
    if !out.status.success() {
        return Err(FdebmError::Subprocess(
            "dpkg-query -W".into(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    let mut broken: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pkg, status) = line.split_once('\t')?;
            abnormal_status(status.trim()).then(|| pkg.trim().to_string())
        })
        .filter(|p| !p.is_empty())
        .collect();
    broken.sort();
    broken.dedup();
    Ok(broken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_abbrev_classification() {
        assert!(abnormal_status("iF"));
        assert!(abnormal_status("iU"));
        assert!(abnormal_status("iH"));
        assert!(abnormal_status("iW"));
        assert!(abnormal_status("it"));
        assert!(!abnormal_status("ii"));
        assert!(!abnormal_status("rc"));
        assert!(!abnormal_status("un"));
        assert!(!abnormal_status(""));
    }
}
