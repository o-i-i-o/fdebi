//! 批量分析语义测试:同批本地 deb 之间的依赖可见性。
//!
//! 用 `dpkg-deb --build` 现场构造最小包(测试套件已依赖 dpkg-deb),
//! 驱动 [`fdebi_core::solver::resolve_batch`]:
//! - A 依赖同批 B 时两者可安装,单独分析 A 不可安装;
//! - 批内 B 覆盖已装旧版本时,A 看到的是 B 的新版本,且旧版本不算卸载;
//! - 批内互相冲突的包不会被同时判为可安装。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use fdebi_core::DebFile;
use fdebi_core::solver::{Verdict, resolve, resolve_batch};
use fdebi_core::sysstate::{Pkg, SysState};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("fdebi-batch-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn control(name: &str, version: &str, relations: &str) -> String {
    format!(
        "Package: {name}\nVersion: {version}\nArchitecture: all\n\
         Maintainer: Test <test@example.com>\nDescription: batch test\n{relations}"
    )
}

fn build_deb(dir: &Path, file: &str, control: &str) -> PathBuf {
    let root = dir.join(file.trim_end_matches(".deb"));
    let control_dir = root.join("DEBIAN");
    std::fs::create_dir_all(&control_dir).unwrap();
    std::fs::write(control_dir.join("control"), control).unwrap();

    let out = dir.join(file);
    let status = Command::new("dpkg-deb")
        .arg("--build")
        .arg("--root-owner-group")
        .arg(&root)
        .arg(&out)
        .status()
        .expect("spawn dpkg-deb");
    assert!(status.success(), "dpkg-deb --build failed for {file}");
    out
}

fn empty_system() -> SysState {
    SysState {
        native_arch: "amd64".into(),
        ..Default::default()
    }
}

fn installed(name: &str, version: &str) -> Pkg {
    Pkg {
        key: name.into(),
        name: name.into(),
        arch: "all".into(),
        version: version.into(),
        ..Default::default()
    }
}

fn installable(
    result: &Result<Verdict, fdebi_core::FdebiError>,
) -> &fdebi_core::solver::Resolution {
    match result {
        Ok(Verdict::Installable(r)) => r,
        Ok(Verdict::Uninstallable(msg)) => panic!("expected installable, got: {msg}"),
        Err(e) => panic!("expected installable, got error: {e}"),
    }
}

fn uninstallable(result: &Result<Verdict, fdebi_core::FdebiError>) -> &str {
    match result {
        Ok(Verdict::Uninstallable(msg)) => msg,
        Ok(Verdict::Installable(_)) => panic!("expected uninstallable"),
        Err(e) => panic!("expected uninstallable, got error: {e}"),
    }
}

/// A 依赖同批 B:批量分析时 B 先求解并作为 A 的已安装上下文。
#[test]
fn batch_dependency_is_visible_to_later_package() {
    let tmp = TempDir::new();
    let a_path = build_deb(
        tmp.path(),
        "batch-a.deb",
        &control("batch-a", "1.0", "Depends: batch-b (>= 1.0)\n"),
    );
    let b_path = build_deb(tmp.path(), "batch-b.deb", &control("batch-b", "1.0", ""));
    let a = DebFile::open(&a_path).unwrap();
    let b = DebFile::open(&b_path).unwrap();
    let sys = empty_system();

    // 单独分析 A:依赖不在系统里,也不在仓库中
    assert_uninstallable_marker(&resolve(&sys, &a).unwrap());

    // 输入顺序 A、B;批量求解按依赖排序先处理 B
    let results = resolve_batch(&sys, &[&a, &b]);
    assert_eq!(results.len(), 2);

    let a_res = installable(&results[0]);
    assert!(a_res.changes.install.contains(&"batch-a".to_string()));
    // 同批 B 由 B 自己的分析负责安装,不重复计入 A 的变更集
    assert!(!a_res.changes.install.contains(&"batch-b".to_string()));
    assert!(a_res.downloads.is_empty());
    assert!(a_res.changes.remove.is_empty());

    let b_res = installable(&results[1]);
    assert_eq!(b_res.changes.install, vec!["batch-b".to_string()]);
}

/// B 覆盖已装旧版本:A 依赖 B 的新版本,批量分析看到的是批内 B。
#[test]
fn batch_package_overrides_installed_old_version() {
    let tmp = TempDir::new();
    let a_path = build_deb(
        tmp.path(),
        "batch-a.deb",
        &control("batch-a", "1.0", "Depends: batch-b (>= 1.0)\n"),
    );
    let b_path = build_deb(tmp.path(), "batch-b.deb", &control("batch-b", "1.0", ""));
    let a = DebFile::open(&a_path).unwrap();
    let b = DebFile::open(&b_path).unwrap();
    let sys = SysState {
        installed: vec![installed("batch-b", "0.9")],
        ..empty_system()
    };

    // 已装 0.9 不满足 A 的 >= 1.0,单独分析失败
    assert_uninstallable_marker(&resolve(&sys, &a).unwrap());

    let results = resolve_batch(&sys, &[&a, &b]);
    let a_res = installable(&results[0]);
    assert!(a_res.changes.install.contains(&"batch-a".to_string()));
    // 批内 B 会替换旧版本,不应把旧版本报成待卸载
    assert!(a_res.changes.remove.is_empty());

    let b_res = installable(&results[1]);
    assert_eq!(b_res.changes.install, vec!["batch-b".to_string()]);
    assert!(b_res.changes.remove.is_empty());
}

/// 批内互相 Conflicts 的包不能同时可安装:后求解项报不可安装。
#[test]
fn batch_conflict_between_members_is_reported() {
    let tmp = TempDir::new();
    let a_path = build_deb(
        tmp.path(),
        "batch-a.deb",
        &control("batch-a", "1.0", "Conflicts: batch-b\n"),
    );
    let b_path = build_deb(tmp.path(), "batch-b.deb", &control("batch-b", "1.0", ""));
    let a = DebFile::open(&a_path).unwrap();
    let b = DebFile::open(&b_path).unwrap();
    let sys = empty_system();

    let results = resolve_batch(&sys, &[&a, &b]);
    assert!(matches!(results[0], Ok(Verdict::Installable(_))));
    uninstallable(&results[1]);
}

/// 升级丢失 Provides:依赖该虚拟包的已装包不允许被静默卸载
#[test]
fn dropped_provides_blocks_install_via_real_deb() {
    use fdebi_core::sysstate::Dep;

    let tmp = TempDir::new();
    // 新版本不再提供 editor-core
    let path = build_deb(tmp.path(), "editor.deb", &control("editor", "2.0", ""));
    let deb = DebFile::open(&path).unwrap();

    let mut old = installed("editor", "1.0");
    old.provides = vec![("editor-core".into(), None)];
    let mut plugin = installed("plugin", "1.0");
    plugin.depends = vec![vec![Dep {
        name: "editor-core".into(),
        ver: String::new(),
        oper: String::new(),
    }]];
    let sys = SysState {
        installed: vec![old, plugin],
        ..empty_system()
    };

    let verdict = resolve(&sys, &deb);
    let msg = uninstallable(&verdict);
    assert!(msg.contains("plugin"), "{msg}");
    assert!(msg.contains("editor-core"), "{msg}");
}

fn assert_uninstallable_marker(verdict: &Verdict) {
    assert!(
        matches!(verdict, Verdict::Uninstallable(_)),
        "expected uninstallable, got {verdict:?}"
    );
}
