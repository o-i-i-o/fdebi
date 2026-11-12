//! conffile 预检:安装前识别「本地已修改且新包携带不同版本」的配置文件。
//!
//! 守护进程无 tty,dpkg 的 conffile 交互提示不可透传;预检把决策提前到
//! GUI 对话框(保留我的版本 / 使用包的新版本),安装时映射为
//! `dpkg --force-confold/--force-confnew` 确定性执行(见 install::ConfPolicy)。
//!
//! 冲突判定与 dpkg 提示条件一致:
//! 1. 磁盘文件存在;
//! 2. 磁盘内容 ≠ 新包携带内容(内容一致时静默替换,无提示);
//! 3. 磁盘内容 ≠ status 记录的装包时 md5(未修改时 dpkg 静默升级)。

use crate::debfile::DebFile;
use crate::sysstate::SysState;
use md5::{Digest, Md5};

/// 一个需要用户决策的 conffile 冲突
#[derive(Debug, Clone)]
pub struct ConffileConflict {
    pub package: String,
    pub path: String,
}

/// 预检 deb 的 conffile 冲突(包未安装时不产生冲突)
pub fn conflicts(deb: &DebFile, sys: &SysState) -> Vec<ConffileConflict> {
    let pkg = deb.field_trimmed("Package").unwrap_or("").to_string();
    if pkg.is_empty() {
        return Vec::new();
    }
    let arch = deb.field_trimmed("Architecture").unwrap_or("all");
    let key = if arch == "all" || arch == sys.native_arch {
        pkg.clone()
    } else {
        format!("{pkg}:{arch}")
    };
    // 精确按 key 查找,避免多架构系统上裸名索引指向另一架构的已装包
    let Some(installed) = sys.installed.iter().find(|p| p.key == key) else {
        return Vec::new();
    };

    let Ok(shipped) = deb.conffiles() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in shipped {
        let member = path.trim_start_matches('/');
        let Ok(content) = deb.data_content(member) else {
            continue;
        };
        let disk = std::fs::read(&path).ok();
        let recorded = installed
            .conffiles
            .iter()
            .find(|(p, _)| p == &path)
            .map(|(_, h)| h.as_str());
        if is_conflict(disk.as_deref(), &content, recorded) {
            out.push(ConffileConflict {
                package: pkg.clone(),
                path,
            });
        }
    }
    out
}

/// 单文件冲突判定(纯函数):
/// 磁盘文件不存在、或与新包内容一致、或与装包时记录一致(用户未改)→ 无冲突
fn is_conflict(disk: Option<&[u8]>, shipped: &[u8], recorded_md5: Option<&str>) -> bool {
    let Some(disk) = disk else {
        return false;
    };
    let disk_md5 = md5_hex(disk);
    if disk_md5 == md5_hex(shipped) {
        return false;
    }
    match recorded_md5 {
        Some(h) => h != disk_md5,
        None => false,
    }
}

fn md5_hex(data: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_decision() {
        let shipped = b"new config\n";
        // 文件不存在:全新落地,无冲突
        assert!(!is_conflict(None, shipped, Some("aaa")));
        // 内容与新包一致:静默替换
        assert!(!is_conflict(Some(b"new config\n"), shipped, Some("aaa")));
        // 未修改(recorded md5 匹配):dpkg 静默升级
        let recorded = md5_hex(b"old config\n");
        assert!(!is_conflict(
            Some(b"old config\n"),
            shipped,
            Some(&recorded)
        ));
        // 本地修改过且与新包不同:冲突
        assert!(is_conflict(Some(b"my config\n"), shipped, Some(&recorded)));
        // 未登记的 conffile(新包新增):按无提示处理
        assert!(!is_conflict(Some(b"my config\n"), shipped, None));
    }

    #[test]
    fn md5_hex_known_vector() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    }
}
