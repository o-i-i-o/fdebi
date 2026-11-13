//! 系统状态层:dpkg 已安装库的纯 Rust 读取(只读数据面)。
//!
//! - 已安装状态:直接解析 `/var/lib/dpkg/status`(deb822,复用 control 解析器)
//! - 仓库候选/认证信息不在此解析:安装计划与仓库版本由 `fdebi-apt-worker`
//!   (libapt-pkg)直接给出,避免在 daemon 侧重复扫描 Packages 索引。
//!
//! 数据结构完全公开,可由测试直接构造。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::control::parse_control;
use crate::error::FdebiError;

/// dpkg status 的标准路径
pub const DPKG_STATUS: &str = "/var/lib/dpkg/status";
/// apt 列表目录
pub const LISTS_DIR: &str = "/var/lib/apt/lists";

/// 依赖表达式的最小单元;name 可能带 :arch 后缀
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub ver: String,
    pub oper: String,
}

/// 包元数据(已安装条目与仓库候选共用)
#[derive(Debug, Clone, Default)]
pub struct Pkg {
    /// binary:Package 键(原生/all 不带后缀,外部架构带 :arch)
    pub key: String,
    pub name: String,
    pub arch: String,
    pub version: String,
    pub depends: Vec<Vec<Dep>>,
    pub pre_depends: Vec<Vec<Dep>>,
    pub conflicts: Vec<Vec<Dep>>,
    pub breaks: Vec<Vec<Dep>>,
    pub replaces: Vec<Vec<Dep>>,
    /// Provides 列表:虚拟名 + 版本(dpkg ≥1.17.11 版本化提供;
    /// 版本化关系对虚拟名仅命中版本化提供)
    pub provides: Vec<(String, Option<String>)>,
    pub essential: bool,
    /// Protected: yes(dpkg ≥1.20;旧写法 "Important: yes" 等价,dpkg 兼容两者)。
    /// 与 Essential 同级的保护性约束:不得在事务中被自动卸载(Debian Policy 5.6.11,
    /// 对应 dpkg --force-remove-protected 的守门对象)
    pub protected: bool,
    /// Multi-Arch 字段原值:no/same/foreign/allowed(缺省为空串)。
    /// 依赖架构匹配必须据此判断:":any" 只接受 allowed,无限定依赖
    /// 仅接受同架构/all 或 foreign(Debian Multi-Arch 规范)
    pub multi_arch: String,
    /// 已安装版本登记的 conffiles:(绝对路径, 装包时的 md5);
    /// 仅 status 有此字段,仓库索引条目为空表
    pub conffiles: Vec<(String, String)>,
}

/// 系统状态快照:已安装库
#[derive(Debug, Default)]
pub struct SysState {
    pub native_arch: String,
    pub foreign_arches: Vec<String>,
    /// 已安装包(按 key 排序)
    pub installed: Vec<Pkg>,
    /// 包名 / 包名:架构 → installed 下标(仅已安装条目)
    pub index: BTreeMap<String, usize>,
}

impl SysState {
    /// 加载已安装状态(dpkg status + 本机/外部架构)。
    /// 仓库候选与安装计划由 `fdebi-apt-worker` 负责,本函数不触碰 apt 索引。
    pub fn load() -> Result<Self, FdebiError> {
        let native_arch = run_line(Command::new("dpkg").arg("--print-architecture"))?;
        let foreign_arches = run_line(Command::new("dpkg").arg("--print-foreign-architectures"))?
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        let installed = load_installed(&native_arch)?;
        let mut index = BTreeMap::new();
        for (i, pkg) in installed.iter().enumerate() {
            index.entry(pkg.key.clone()).or_insert(i);
            index
                .entry(format!("{}:{}", pkg.name, pkg.arch))
                .or_insert(i);
            index.entry(pkg.name.clone()).or_insert(i);
        }

        Ok(SysState {
            native_arch,
            foreign_arches,
            installed,
            index,
        })
    }

    /// 已安装包的当前版本。只按 key(或 name:arch)精确匹配,不退化为
    /// 裸包名扫描:多架构系统上 foo:i386 已装而 native foo 未装时,
    /// 裸名回退会把 i386 的版本误报成 native foo 的版本(降级门/GUI 均受影响)。
    pub fn installed_version(&self, key_or_name: &str) -> Option<&str> {
        self.installed
            .iter()
            .find(|p| p.key == key_or_name || format!("{}:{}", p.name, p.arch) == key_or_name)
            .map(|p| p.version.as_str())
    }

}

/// apt 索引是否可能落后于软件源配置(对应 deepin `shouldUpdateCache`,
/// 但 fdebi 直接读 lists,只做提示不阻塞):
/// - lists 目录没有 Packages 索引 → 需要 `apt update`;
/// - sources.list / sources.list.d 的 mtime 晚于最近一次更新 → 配置改过但未更新。
///
/// 不能拿索引文件的 mtime 当“最近更新时刻”:apt 会把服务器返回的
/// `Last-Modified` 写回本地索引文件(`pkgAcqIndex` 的既有行为),该 mtime
/// 反映的是仓库发布时间而非本机下载时间,与本地 sources 的 mtime 比较会在
/// 配置改动后永远判为过期。改为用 lists 目录自身的 mtime:apt 每次更新都会
/// 在目录内重命名/删除索引文件,目录 mtime 即最近一次成功更新的时刻。
///
/// 两侧都取不到时返回 false(容器/自定义布局下不误报)。
pub fn lists_stale(apt_opts: &[String]) -> bool {
    let lists_dir = apt_opts
        .iter()
        .find_map(|o| o.strip_prefix("Dir::State::lists="))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(LISTS_DIR));
    let has_index = newest_mtime(&lists_dir, |name| {
        name.contains("Packages") && !name.contains("Translation")
    })
    .is_some();
    if !has_index {
        return true;
    }
    let last_update = std::fs::metadata(&lists_dir)
        .and_then(|m| m.modified())
        .ok();

    let sources_list = apt_opts
        .iter()
        .find_map(|o| o.strip_prefix("Dir::Etc::sourcelist="))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/apt/sources.list"));
    let sources_parts = apt_opts
        .iter()
        .find_map(|o| o.strip_prefix("Dir::Etc::sourceparts="))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/apt/sources.list.d"));
    let sources = std::fs::metadata(&sources_list)
        .and_then(|m| m.modified())
        .ok()
        .into_iter()
        .chain(newest_mtime(&sources_parts, |name| {
            name.ends_with(".list") || name.ends_with(".sources")
        }))
        .max();

    stale_from(sources, last_update)
}

/// 纯判定:配置比最近一次更新新 → 过期;缺配置或更新时刻 → 不误报
fn stale_from(
    sources: Option<std::time::SystemTime>,
    last_update: Option<std::time::SystemTime>,
) -> bool {
    match (sources, last_update) {
        (Some(s), Some(l)) => s > l,
        _ => false,
    }
}

/// 目录下满足条件的最新 mtime(目录不存在/读取失败返回 None)
fn newest_mtime(dir: &Path, accept: impl Fn(&str) -> bool) -> Option<std::time::SystemTime> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(&accept))
        .filter_map(|e| e.metadata().and_then(|m| m.modified()).ok())
        .max()
}

/// 从 control 段落字段构建 Pkg(键派生需 native_arch)
pub fn pkg_from_fields<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    native_arch: &str,
) -> Option<Pkg> {
    let name = get("Package")?.trim().to_string();
    let arch = get("Architecture").unwrap_or("").trim().to_string();
    let version = get("Version").unwrap_or("").trim().to_string();
    let key = if arch.is_empty() || arch == "all" || arch == native_arch {
        name.clone()
    } else {
        format!("{}:{}", name, arch)
    };
    Some(Pkg {
        key,
        name,
        arch,
        version,
        depends: parse_depends(get("Depends").unwrap_or("")),
        pre_depends: parse_depends(get("Pre-Depends").unwrap_or("")),
        conflicts: parse_depends(get("Conflicts").unwrap_or("")),
        breaks: parse_depends(get("Breaks").unwrap_or("")),
        replaces: parse_depends(get("Replaces").unwrap_or("")),
        provides: provided_names(get("Provides").unwrap_or("")),
        essential: get("Essential").map(str::trim) == Some("yes"),
        protected: get("Protected").map(str::trim) == Some("yes")
            || get("Important").map(str::trim) == Some("yes"),
        multi_arch: get("Multi-Arch").map(str::trim).unwrap_or("").to_string(),
        conffiles: parse_conffiles(get("Conffiles").unwrap_or("")),
    })
}

/// status 的 Conffiles 字段 → (路径, md5) 列表
/// (行形如 " /etc/foo <md5>",可带 obsolete 标记;损坏行跳过)
fn parse_conffiles(field: &str) -> Vec<(String, String)> {
    field
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let path = it.next()?;
            let md5 = it.next()?;
            Some((path.to_string(), md5.to_string()))
        })
        .collect()
}

/// 解析 /var/lib/dpkg/status → 已安装包列表
pub fn load_installed(native_arch: &str) -> Result<Vec<Pkg>, FdebiError> {
    let text = std::fs::read_to_string(DPKG_STATUS)?;
    let mut out = Vec::new();
    for para in parse_control(&text) {
        // Status: install ok installed(排除 config-files/purge 残留)
        if !status_installed(para.get("Status").unwrap_or("")) {
            continue;
        }
        if let Some(pkg) = pkg_from_fields(|n| para.get(n), native_arch) {
            out.push(pkg);
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// dpkg Status 字段是否表示"已安装"(install|hold ok installed)
fn status_installed(status: &str) -> bool {
    let mut words = status.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next()),
        (
            Some("install") | Some("hold"),
            Some("ok"),
            Some("installed")
        )
    )
}

/// 拆分关系子句中的 "(op version)"。
///
/// dpkg 接受操作符与版本之间没有空白的写法(`foo(>=2.0)`、`foo (>=2.0)`),
/// dpkg-deb -f 输出也会规范化为 `foo (>= 2.0)`。因此不能假设空白分隔:
/// 先取括号内文本,再按前导比较运算符切分。
fn split_relation(inner: &str) -> (String, String) {
    let inner = inner.trim().trim_end_matches(')').trim();
    let op_len = inner
        .bytes()
        .take_while(|b| matches!(b, b'<' | b'>' | b'='))
        .count();
    let (oper, ver) = inner.split_at(op_len);
    (oper.trim().to_string(), ver.trim().to_string())
}

/// control 依赖字段 → or 组列表
pub fn parse_depends(s: &str) -> Vec<Vec<Dep>> {
    s.split(',')
        .filter(|g| !g.trim().is_empty())
        .map(|group| {
            group
                .split('|')
                .map(|d| {
                    let d = d.trim();
                    match d.find('(') {
                        Some(i) => {
                            let name = d[..i].trim().to_string();
                            let (oper, ver) = split_relation(&d[i + 1..]);
                            Dep { name, ver, oper }
                        }
                        None => Dep {
                            name: d.to_string(),
                            ver: String::new(),
                            oper: String::new(),
                        },
                    }
                })
                .collect()
        })
        .collect()
}

/// Provides 字段 → (虚拟名, 版本化提供版本) 列表
/// (仅 "(= ver)" 形式算版本化提供,与 dpkg 语义一致)
pub fn provided_names(field: &str) -> Vec<(String, Option<String>)> {
    field
        .split(',')
        .filter_map(|e| {
            let e = e.trim();
            if e.is_empty() {
                return None;
            }
            let (name, ver) = match e.find('(') {
                Some(i) => {
                    let (op, v) = split_relation(&e[i + 1..]);
                    let v = (op == "=" && !v.is_empty()).then_some(v);
                    (e[..i].trim(), v)
                }
                None => (e, None),
            };
            let name = name.split(':').next().unwrap_or(name).trim();
            (!name.is_empty()).then(|| (name.to_string(), ver))
        })
        .collect()
}

/// 包名去掉 :arch 后缀
pub fn base_name(name: &str) -> &str {
    name.split(':').next().unwrap_or(name)
}

fn run_captured(cmd: &mut Command) -> Result<(String, bool), FdebiError> {
    let out = cmd.env("LC_ALL", "C").output()?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    ))
}

fn run_line(cmd: &mut Command) -> Result<String, FdebiError> {
    Ok(run_captured(cmd)?.0.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg_from(text: &str) -> Option<Pkg> {
        let paras = parse_control(text);
        pkg_from_fields(|n| paras[0].get(n), "amd64")
    }

    #[test]
    fn status_paragraph_to_pkg() {
        let p = pkg_from(
            "Package: sl\nArchitecture: all\nVersion: 5.02-1\n\
             Status: install ok installed\nDepends: libc6 (>= 2.34)\n\
             Conflicts: sl-alternative\nProvides: train\nEssential: no\n",
        )
        .unwrap();
        assert_eq!(p.key, "sl"); // all → 裸名
        assert_eq!(p.version, "5.02-1");
        assert_eq!(p.depends[0][0].name, "libc6");
        assert_eq!(p.conflicts[0][0].name, "sl-alternative");
        assert_eq!(p.provides, vec![("train".to_string(), None)]);
        assert!(!p.essential);
    }

    #[test]
    fn status_partial_not_installed() {
        // Status 过滤谓词:config-files/purge 残留不算已安装
        assert!(!status_installed("install ok config-files"));
        assert!(!status_installed("deinstall ok config-files"));
        assert!(!status_installed("purge ok not-installed"));
        assert!(!status_installed(""));
        assert!(status_installed("install ok installed"));
        assert!(status_installed("hold ok installed"));
    }

    #[test]
    fn foreign_arch_key_gets_qualifier() {
        let p = pkg_from("Package: libc6-i386\nArchitecture: i386\nVersion: 1\n").unwrap();
        assert_eq!(p.key, "libc6-i386:i386");
    }

    /// Protected: yes 与旧写法 Important: yes 均记为保护(dpkg 兼容两者),
    /// 其他取值(如 no)与缺省不算
    #[test]
    fn protected_field_parsing() {
        let p = pkg_from("Package: dpkg\nVersion: 1\nProtected: yes\n").unwrap();
        assert!(p.protected && !p.essential);
        let p = pkg_from("Package: x\nVersion: 1\nImportant: yes\n").unwrap();
        assert!(p.protected);
        let p = pkg_from("Package: x\nVersion: 1\nProtected: no\n").unwrap();
        assert!(!p.protected);
    }

    /// Multi-Arch 字段保留原值,供依赖架构匹配判断
    #[test]
    fn multi_arch_field_parsed() {
        let p =
            pkg_from("Package: libx\nVersion: 1\nArchitecture: i386\nMulti-Arch: same\n").unwrap();
        assert_eq!(p.multi_arch, "same");
        let p = pkg_from("Package: liby\nVersion: 1\n").unwrap();
        assert_eq!(p.multi_arch, "");
    }

    /// dpkg 接受操作符与版本间无空白的写法并规范化输出,解析器不能
    /// 依赖空白分隔(曾把 "foo(>=2.0)" 解析成空约束,静默放宽依赖)
    #[test]
    fn relation_without_space_parsed() {
        let groups = parse_depends("foo(>=2.0), bar ( << 3 ), baz");
        assert_eq!(groups[0][0].name, "foo");
        assert_eq!(groups[0][0].oper, ">=");
        assert_eq!(groups[0][0].ver, "2.0");
        assert_eq!(groups[1][0].oper, "<<");
        assert_eq!(groups[1][0].ver, "3");
        assert_eq!(groups[2][0].name, "baz");
        assert!(groups[2][0].oper.is_empty());
    }

    /// Provides 的版本化提供同样支持无空白写法
    #[test]
    fn versioned_provides_without_space_parsed() {
        let p = pkg_from("Package: w3m\nVersion: 1\nProvides: www-browser(=2.0)\n").unwrap();
        assert_eq!(
            p.provides,
            vec![("www-browser".to_string(), Some("2.0".to_string()))]
        );
    }

    /// installed_version 只精确匹配 key/name:arch,不能把异架构已装包
    /// 的版本误报为 native 包的版本
    #[test]
    fn installed_version_requires_exact_arch() {
        let foo_i386 = pkg_from("Package: foo\nVersion: 1.0\nArchitecture: i386\n").unwrap();
        let st = SysState {
            installed: vec![foo_i386],
            native_arch: "amd64".into(),
            ..Default::default()
        };
        assert_eq!(st.installed_version("foo:i386"), Some("1.0"));
        assert_eq!(st.installed_version("foo"), None);
    }

    /// apt 索引新鲜度:配置比最近一次更新新才算过期;缺配置/缺更新时刻不误报
    #[test]
    fn apt_lists_staleness_classification() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = |s| UNIX_EPOCH + Duration::from_secs(s);
        assert!(stale_from(Some(t(20)), Some(t(10))));
        assert!(!stale_from(Some(t(10)), Some(t(20))));
        assert!(!stale_from(Some(t(10)), Some(t(10))));
        assert!(!stale_from(None, Some(t(10))));
        assert!(!stale_from(Some(t(10)), None));
        assert!(!stale_from(None, None));
    }

    #[test]
    fn status_conffiles_parsed() {
        let p = pkg_from(
            "Package: adduser\nVersion: 1\nStatus: install ok installed\n\
             Conffiles:\n /etc/adduser.conf 8f3bbb\n /etc/deluser.conf aaa111 obsolete\n",
        )
        .unwrap();
        assert_eq!(
            p.conffiles,
            vec![
                ("/etc/adduser.conf".to_string(), "8f3bbb".to_string()),
                ("/etc/deluser.conf".to_string(), "aaa111".to_string())
            ]
        );
    }

}
