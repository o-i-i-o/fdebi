//! 系统状态层:dpkg 已安装库与 apt 仓库索引的纯 Rust 读取(纯 Rust 管道的数据面)。
//!
//! - 已安装状态:直接解析 `/var/lib/dpkg/status`(deb822,复用 control 解析器)
//! - 仓库索引:`apt-get indextargets` 仅做元数据枚举(文件路径/来源/信任标记,
//!   与 dpkg-query 同为"系统能力经子进程"),Packages 索引本体以纯 Rust 解压解析
//!   (lz4/xz/zstd/gz 四种压缩,全部 Rust 后端,无 C 依赖)
//! - 求解与安装不再调用 apt-get;下载层见 [`crate::download`]
//!
//! 数据结构对求解器([`crate::solver`])完全开放,可由测试直接构造。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::control::parse_control;
use crate::error::FdebmError;
use crate::version::cmp_version;

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
    /// 已安装版本登记的 conffiles:(绝对路径, 装包时的 md5);
    /// 仅 status 有此字段,仓库索引条目为空表
    pub conffiles: Vec<(String, String)>,
}

/// 仓库来源信息(来自 indextargets)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repo {
    pub origin: String,
    pub label: String,
    pub suite: String,
    pub codename: String,
    pub component: String,
    pub site: String,
    /// 仓库根 URI(如 http://archive.ubuntu.com/ubuntu)
    pub base_uri: String,
    pub trusted: bool,
}

/// 一个可安装候选:仓库条目(带下载坐标)或本地 deb
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub pkg: Pkg,
    /// 仓库来源;本地 deb 为 None
    pub repo: Option<Repo>,
    /// 仓库内相对路径(Filename 字段)
    pub repo_filename: String,
    pub sha256: String,
    pub size: u64,
    /// 本地 deb 的路径(仅本地候选)
    pub local_path: Option<PathBuf>,
}

impl Candidate {
    pub fn is_local(&self) -> bool {
        self.local_path.is_some()
    }
}

/// indextargets 条目
#[derive(Debug, Clone, Default)]
struct IndexTarget {
    uri: String,
    repo: Repo,
}

/// 系统状态快照:已安装库 + 全部仓库候选
#[derive(Debug, Default)]
pub struct SysState {
    pub native_arch: String,
    pub foreign_arches: Vec<String>,
    /// 已安装包(按 key 排序)
    pub installed: Vec<Pkg>,
    /// 包名 / 包名:架构 → installed 下标(仅已安装条目)
    pub index: BTreeMap<String, usize>,
    /// key → 候选下标(全局扁平表,按版本降序)
    pub candidates: BTreeMap<String, Vec<usize>>,
    /// 全部候选(仓库 + 本地 deb)
    pub all_candidates: Vec<Candidate>,
}

impl SysState {
    /// 加载系统状态(dpkg 状态 + indextargets 枚举 + 列表解析)。
    /// 本地 deb 不入缓存,由求解器作为独立单元注入(见 solver::resolve)。
    pub fn load(apt_opts: &[String]) -> Result<Self, FdebmError> {
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

        let mut state = SysState {
            native_arch,
            foreign_arches,
            installed,
            index,
            ..Default::default()
        };

        // 仓库候选:indextargets 枚举 → 列表文件解析
        let targets = indextargets(apt_opts)?;
        let lists_dir = apt_opts
            .iter()
            .find_map(|o| o.strip_prefix("Dir::State::lists="))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(LISTS_DIR));
        let mut candidates = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&lists_dir) {
            for entry in entries {
                let path = match entry {
                    Ok(e) => e.path(),
                    Err(_) => continue,
                };
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // 只关心二进制包索引(arch:all 条目包含在各 binary-* 索引内)
                if !name.contains("Packages") || name.contains("Translation") {
                    continue;
                }
                let repo = target_for_lists_file(&targets, name).unwrap_or_default();
                // 单个列表文件损坏/不可读时跳过(与 apt 容错行为一致),
                // 不让整个 SysState::load 失败
                let _ = parse_packages_file(&path, &state.native_arch, &repo, &mut candidates);
            }
        }
        state.add_candidates(candidates);
        Ok(state)
    }

    /// 批量加入候选并建立索引(同 key 同 version 去重,信任来源优先)
    pub fn add_candidates(&mut self, mut cands: Vec<Candidate>) {
        cands.sort_by(|a, b| {
            a.pkg
                .key
                .cmp(&b.pkg.key)
                .then_with(|| cmp_version(&b.pkg.version, &a.pkg.version))
        });
        for c in cands {
            let key = c.pkg.key.clone();
            let ver = c.pkg.version.clone();
            let trusted = c.repo.as_ref().is_none_or(|r| r.trusted);
            let list = self.candidates.entry(key).or_default();
            if let Some(&i) = list
                .iter()
                .find(|&&i| self.all_candidates[i].pkg.version == ver)
            {
                // 同版本重复:仅当新条目可信而旧条目不可信时替换
                let existing_trusted = self.all_candidates[i]
                    .repo
                    .as_ref()
                    .is_none_or(|r| r.trusted);
                if trusted && !existing_trusted && !self.all_candidates[i].is_local() {
                    self.all_candidates[i] = c;
                }
                continue;
            }
            list.push(self.all_candidates.len());
            self.all_candidates.push(c);
        }
    }

    /// 已安装包的当前版本(GUI 的同版本判定用)
    pub fn installed_version(&self, key_or_name: &str) -> Option<&str> {
        let &i = self.index.get(key_or_name)?;
        Some(self.installed[i].version.as_str())
    }
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
pub fn load_installed(native_arch: &str) -> Result<Vec<Pkg>, FdebmError> {
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

/// `apt-get indextargets` 枚举(仅元数据;子进程输出按 C locale 解析)
fn indextargets(apt_opts: &[String]) -> Result<Vec<IndexTarget>, FdebmError> {
    let mut cmd = Command::new("apt-get");
    cmd.arg("indextargets");
    for o in apt_opts {
        cmd.arg("-o").arg(o);
    }
    let out = run_captured(&mut cmd)?;
    if !out.1 {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    let mut cur = IndexTarget::default();
    let mut any = false;
    for line in out.0.lines() {
        if line.trim().is_empty() {
            if any {
                targets.push(std::mem::take(&mut cur));
                any = false;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(": ") {
            any = true;
            match k {
                "URI" => cur.uri = v.to_string(),
                "Repo-URI" => cur.repo.base_uri = v.trim_end_matches('/').to_string(),
                "Origin" => cur.repo.origin = v.to_string(),
                "Label" => cur.repo.label = v.to_string(),
                "Suite" => cur.repo.suite = v.to_string(),
                "Codename" => cur.repo.codename = v.to_string(),
                "Component" => cur.repo.component = v.to_string(),
                "Site" => cur.repo.site = v.to_string(),
                "Trusted" => cur.repo.trusted = v.trim() == "yes",
                _ => {}
            }
        }
    }
    if any {
        targets.push(cur);
    }
    Ok(targets)
}

/// 列表文件名 → 来源匹配:apt 以 "站点_路径(/→_)" 命名 lists 文件,
/// 用目标 URI 做同款变换后按后缀匹配(端口 ':' 的 %3a 编码两种形态都尝试)
fn target_for_lists_file(targets: &[IndexTarget], lists_name: &str) -> Option<Repo> {
    for t in targets {
        let rest = t.uri.split("://").nth(1).unwrap_or(&t.uri);
        let key = rest.replace('/', "_");
        if lists_name.ends_with(&key)
            || lists_name.ends_with(&key.replace(':', "%3a"))
            || lists_name.ends_with(&key.replace("%3a", ":"))
        {
            return Some(t.repo.clone());
        }
    }
    None
}

/// 解析单个 Packages 索引文件(按扩展名解压)
fn parse_packages_file(
    path: &Path,
    native_arch: &str,
    repo: &Repo,
    out: &mut Vec<Candidate>,
) -> Result<(), FdebmError> {
    let text = decompress_to_string(path)?;
    for para in parse_control(&text) {
        let Some(pkg) = pkg_from_fields(|n| para.get(n), native_arch) else {
            continue;
        };
        out.push(Candidate {
            pkg,
            repo: Some(repo.clone()),
            repo_filename: para.get("Filename").unwrap_or("").trim().to_string(),
            sha256: para.get("SHA256").unwrap_or("").trim().to_string(),
            size: para
                .get("Size")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0),
            local_path: None,
        });
    }
    Ok(())
}

/// 按扩展名/魔数解压索引文件为文本(lz4/xz/zstd/gz/无压缩)
fn decompress_to_string(path: &Path) -> Result<String, FdebmError> {
    let raw = std::fs::read(path)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let decomp_err = |e: String| FdebmError::ListDecompress(path.display().to_string(), e);
    let bytes: Vec<u8> = if name.ends_with(".lz4") {
        let mut out = Vec::new();
        lz4_flex::frame::FrameDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .map_err(|e| decomp_err(e.to_string()))?;
        out
    } else if name.ends_with(".xz") {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut &raw[..], &mut out).map_err(|e| decomp_err(e.to_string()))?;
        out
    } else if name.ends_with(".zst") {
        let mut out = Vec::new();
        ruzstd::StreamingDecoder::new(&raw[..])
            .map_err(|e| decomp_err(e.to_string()))?
            .read_to_end(&mut out)
            .map_err(|e| decomp_err(e.to_string()))?;
        out
    } else if name.ends_with(".gz") || raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .map_err(|e| decomp_err(e.to_string()))?;
        out
    } else {
        raw
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
                            let inner = d[i + 1..].trim_end_matches(')').trim();
                            let mut it = inner.splitn(2, char::is_whitespace);
                            let oper = it.next().unwrap_or("").to_string();
                            let ver = it.next().unwrap_or("").trim().to_string();
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
                    let inner = e[i..].trim_start_matches('(').trim_end_matches(')').trim();
                    let mut it = inner.splitn(2, char::is_whitespace);
                    let op = it.next().unwrap_or("").trim();
                    let v = it.next().unwrap_or("").trim();
                    let v = (op == "=" && !v.is_empty()).then(|| v.to_string());
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

fn run_captured(cmd: &mut Command) -> Result<(String, bool), FdebmError> {
    let out = cmd.env("LC_ALL", "C").output()?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    ))
}

fn run_line(cmd: &mut Command) -> Result<String, FdebmError> {
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

    #[test]
    fn index_target_matching() {
        let targets = vec![IndexTarget {
            uri: "http://archive.ubuntu.com/ubuntu/dists/noble/main/binary-amd64/Packages.lz4"
                .into(),
            repo: Repo {
                trusted: true,
                ..Default::default()
            },
        }];
        let repo = target_for_lists_file(
            &targets,
            "archive.ubuntu.com_ubuntu_dists_noble_main_binary-amd64_Packages.lz4",
        )
        .unwrap();
        assert!(repo.trusted);
        assert!(
            target_for_lists_file(
                &targets,
                "evil.example_org_dists_sid_main_binary-amd64_Packages.xz"
            )
            .is_none()
        );
    }

    #[test]
    fn packages_stanza_to_candidate() {
        // 未压缩 Packages 索引段落 → Candidate(Filename/SHA256/Size 提取)
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "fdebm-test_{}_dists_noble_main_binary-amd64_Packages",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "Package: jq\nArchitecture: all\nVersion: 1.7-1\n\
             Filename: pool/main/j/jq/jq_1.7-1_all.deb\n\
             SHA256: abc123\nSize: 65536\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let r = parse_packages_file(
            &path,
            "amd64",
            &Repo {
                trusted: true,
                base_uri: "http://archive.ubuntu.com/ubuntu".into(),
                ..Default::default()
            },
            &mut out,
        );
        let _ = std::fs::remove_file(&path);
        r.unwrap();
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c.pkg.key, "jq"); // all → 裸名
        assert_eq!(c.repo_filename, "pool/main/j/jq/jq_1.7-1_all.deb");
        assert_eq!(c.sha256, "abc123");
        assert_eq!(c.size, 65536);
        assert!(c.repo.as_ref().unwrap().trusted);
    }

    #[test]
    fn add_candidates_dedup_prefers_trusted() {
        let mut st = SysState::default();
        let mk = |trusted: bool, ver: &str| Candidate {
            pkg: Pkg {
                key: "jq".into(),
                name: "jq".into(),
                arch: "all".into(),
                version: ver.into(),
                ..Default::default()
            },
            repo: Some(Repo {
                trusted,
                ..Default::default()
            }),
            ..Default::default()
        };
        st.add_candidates(vec![mk(false, "1.0"), mk(true, "1.0"), mk(true, "0.9")]);
        let list = &st.candidates["jq"];
        assert_eq!(list.len(), 2);
        // 1.0 应保留信任来源
        let first = &st.all_candidates[list[0]];
        assert_eq!(first.pkg.version, "1.0");
        assert!(first.repo.as_ref().unwrap().trusted);
    }
}
