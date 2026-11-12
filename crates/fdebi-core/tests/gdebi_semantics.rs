//! Debian/gdebi 业务流程测试。
//!
//! tests/ 下的 gdebi-test*.deb 是原 gdebi 测试套件的示例包,本文件用它们
//! 驱动与守护进程 AnalyzeLocalFile 相同的完整管线:DebFile::open(真实 ar/
//! tar/压缩字节)→ resolve(架构/降级门 + SAT 求解),在手工构造的
//! 系统状态上断言 结论/变更集。
//!
//! 构造的系统均围绕 Conflicts/Depends 关系展开 —— 具体语义以 dpkg(1)
//! 与 Debian Policy 第 7 章为准,注释里逐条对照。

use std::path::PathBuf;

use fdebi_core::solver::{Verdict, resolve};
use fdebi_core::sysstate::{Pkg, SysState};
use fdebi_core::{DebFile, FdebiError};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests")
}

/// 打开固件 deb(全部 Architecture: all,在 amd64 假设架构上可解)
fn fixture(name: &str) -> DebFile {
    let path = repo_root().join(name);
    DebFile::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
}

fn pkg(key: &str, ver: &str) -> Pkg {
    let name = key.split(':').next().unwrap_or(key).to_string();
    Pkg {
        key: key.into(),
        name,
        arch: "amd64".into(),
        version: ver.into(),
        ..Default::default()
    }
}

/// 空白系统 + 指定已装包(与 solver 单测同款索引规则)
fn sys_with(pkgs: Vec<Pkg>) -> SysState {
    let mut st = SysState {
        native_arch: "amd64".into(),
        ..Default::default()
    };
    for p in pkgs {
        let i = st.installed.len();
        st.index.entry(p.key.clone()).or_insert(i);
        st.index
            .entry(format!("{}:{}", p.name, p.arch))
            .or_insert(i);
        st.index.entry(p.name.clone()).or_insert(i);
        st.installed.push(p);
    }
    st
}

fn installed(v: Verdict) -> fdebi_core::solver::Resolution {
    match v {
        Verdict::Installable(r) => r,
        Verdict::Uninstallable(m) => panic!("expected installable, got: {m}"),
    }
}

fn uninstallable(v: Verdict) -> String {
    match v {
        Verdict::Uninstallable(m) => m,
        Verdict::Installable(_) => panic!("expected uninstallable"),
    }
}

/// test2:依赖包不存在 → 不可安装(报告可读原因,而非解析崩溃)
#[test]
fn test2_missing_dependency_reports_uninstallable() {
    let msg = uninstallable(resolve(&sys_with(vec![]), &fixture("gdebi-test2.deb")).unwrap());
    assert!(!msg.is_empty());
}

/// test3:or 组 `pkg-that-does-not-exists | apt` 在已装 apt 上满足 ——
/// or 组由 SAT 取舍,不因首分支不可满足而整体失败(gdebi 经典用例)
#[test]
fn test3_or_group_falls_back_to_installed_provider() {
    let st = sys_with(vec![pkg("apt", "0.9")]);
    let r = installed(resolve(&st, &fixture("gdebi-test3.deb")).unwrap());
    assert!(r.changes.install.iter().any(|k| k.contains("gdebi-test3")));
    assert!(r.changes.remove.is_empty());
    assert!(r.downloads.is_empty());
    // 已满足依赖时不得把无关仓库候选拉进变更集
    assert_eq!(r.changes.install.len(), 1);
}

/// test1:与已装 apt 冲突 → 求解给出"先卸载 apt"的方案
/// (冲突方非 Essential;若为 Essential 直接判不可安装)
#[test]
fn test1_conflict_on_installed_requires_removal() {
    let st = sys_with(vec![pkg("apt", "0.9")]);
    let r = installed(resolve(&st, &fixture("gdebi-test1.deb")).unwrap());
    assert!(r.changes.remove.iter().any(|k| k.contains("apt")));
    assert!(r.downloads.is_empty());
}

/// test4:`Conflicts: apt (<= 0.1)`,已装 apt 0.9 未被版本门槛命中 ——
/// 存量态不新生违规,keep-all 快路径成立、无需卸载任何包
#[test]
fn test4_versioned_conflict_misses_installed() {
    let st = sys_with(vec![pkg("apt", "0.9")]);
    let r = installed(resolve(&st, &fixture("gdebi-test4.deb")).unwrap());
    assert!(r.changes.remove.is_empty());
    assert!(r.downloads.is_empty());
}

/// test5:`Conflicts: apt (>= 0.1)`,已装 apt 0.9 被命中 → 必须先卸载
#[test]
fn test5_versioned_conflict_hits_installed() {
    let st = sys_with(vec![pkg("apt", "0.9")]);
    let r = installed(resolve(&st, &fixture("gdebi-test5.deb")).unwrap());
    assert!(r.changes.remove.iter().any(|k| k.contains("apt")));
}

/// test6:无依赖字段(gdebi 管线成员名特殊样本)在任何系统上都可安装
#[test]
fn test6_no_dependencies_installs_clean() {
    let r = installed(resolve(&sys_with(vec![]), &fixture("gdebi-test6.deb")).unwrap());
    assert_eq!(r.changes.install.len(), 1);
    assert!(r.changes.remove.is_empty());
    assert!(r.downloads.is_empty());
}

/// test7:`Conflicts: foobarbaz` + `Provides: foobarbaz` —— 自冲突经虚拟
/// 提供命中自身,不得误判为不可安装(dpkg 允许冲突+自提供)
#[test]
fn test7_self_conflict_via_own_provides_is_installable() {
    let r = installed(resolve(&sys_with(vec![]), &fixture("gdebi-test7.deb")).unwrap());
    assert!(r.changes.remove.is_empty());
}

/// test8:经典 MTA 交替模式 —— Conflicts+Provides+Replaces
/// mail-transport-agent,与已装 MTA(exim4 经虚拟 Provides 命中冲突项):
/// dpkg 在解包阶段会自动移除 Conflicts+Replaces 的已装包(apt 求解同样
/// 登记 remove),solve 器显式登记该移除 —— GUI 呈现"需要移除 1 个包",
/// 守护进程先 `dpkg --auto-deconfigure -r exim4` 再安装,行为确定可预期。
#[test]
fn test8_conflicts_replaces_mta_with_installed_provider() {
    let st = sys_with(vec![Pkg {
        provides: vec![("mail-transport-agent".into(), None)],
        ..pkg("exim4", "4.98")
    }]);
    let r = installed(resolve(&st, &fixture("gdebi-test8.deb")).unwrap());
    assert!(r.changes.remove.iter().any(|k| k.contains("exim4")));
    assert!(r.changes.install.iter().any(|k| k.contains("gdebi-test8")));
    assert!(r.downloads.is_empty());
}

/// test9:nvidia-glx 双方(Conflicts+Provides)在系统上无他者占据该虚拟名
/// 时干净安装(自冲突经自身 Provides 命中,不得误判)
#[test]
fn test9_no_installed_provider_installs_clean() {
    let r = installed(resolve(&sys_with(vec![]), &fixture("gdebi-test9.deb")).unwrap());
    assert!(r.changes.remove.is_empty());
    assert!(r.downloads.is_empty());
}

/// Recommends 按 Debian Policy 7.4 为非强制:求解器从不把推荐包拉进
/// 安装/下载集(固件 Depends 缺省,仅 Recommends: 3dchess)
#[test]
fn recommends_test_installable_without_recommended_packages() {
    let r = installed(
        resolve(
            &sys_with(vec![]),
            &fixture("recommends-test-package_1.0_all.deb"),
        )
        .unwrap(),
    );
    assert_eq!(r.changes.install.len(), 1);
    assert!(r.downloads.is_empty());
}

/// 错误路径冒烟:not-a-deb 场景保持英文 msgid 基线(与 diff_oracle 同款约束)
#[test]
fn not_a_deb_error_message_kept_as_english_msgid() {
    let err = FdebiError::NotADeb(PathBuf::from("/tmp/x"));
    assert_eq!(err.to_string(), "/tmp/x is not a Debian package");
}
