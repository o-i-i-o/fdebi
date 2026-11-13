//! 分析模式:逐项给出 apt 视角下的安装计划。
//!
//! 批量语义:第 i 项的缓存包含前序成功解析的本地 deb 与当前 deb,前序包
//! 可作为依赖来源;但只标记当前项,因此前序包只有在被依赖时才会出现在计划里。
//! 解析失败(非普通文件/损坏)的项不进入后续缓存。

use fdebi_apt_proto::{
    Change, ChangeKind, Event, Item, ItemResult, Phase, Request,
};
use fdebi_core::DebFile;
use fdebi_core::i18n::{gettext, tf};
use oma_apt::util::{cmp_versions, show_broken_pkg};
use oma_apt::{Cache, Package, Version};

use crate::config;
use crate::emit::Emitter;

/// 仓库索引的 Index-Type 标记;已安装条目与本地 deb 分别是
/// "Debian dpkg status file" 与 "Debian deb file",据此区分"源里的版本"。
const REPO_INDEX: &str = "Debian Package Index";

pub fn run(req: &Request, out: &Emitter) -> Result<(), String> {
    config::apply(req, out);
    let native = config::native_arch()?;
    let total = req.items.len();
    let mut results = Vec::with_capacity(total);
    // 只有成功解析的项才进入后续分析的缓存前缀
    let mut good: Vec<Item> = Vec::new();

    for (i, item) in req.items.iter().enumerate() {
        let idx = (i + 1).to_string();
        let total_s = total.to_string();
        out.emit(&Event::Stage {
            phase: Phase::Analyzing,
            desc: tf("Analyzing {0} ({1}/{2})", &[&item.path, &idx, &total_s]),
        });
        let result = analyze_one(&good, item, &native);
        if result.installable {
            good.push(item.clone());
        }
        results.push(result);
    }

    out.emit(&Event::Result {
        ok: true,
        detail: String::new(),
        items: results,
    });
    Ok(())
}

fn analyze_one(prefix: &[Item], item: &Item, native: &str) -> ItemResult {
    // 先拒绝 FIFO/设备等非普通文件:open 会阻塞,不能被请求方诱导
    if std::fs::metadata(&item.path).is_ok_and(|m| !m.is_file()) {
        return ItemResult::failed(&item.path, gettext("not a regular file"));
    }
    let deb = match DebFile::open(&item.path) {
        Ok(deb) => deb,
        Err(e) => return ItemResult::failed(&item.path, e.to_string()),
    };

    let Some(name) = deb.field_trimmed("Package") else {
        return ItemResult::failed(&item.path, gettext("missing Package field"));
    };
    let Some(version) = deb.field_trimmed("Version") else {
        return ItemResult::failed(&item.path, gettext("missing Version field"));
    };
    let arch = deb.field_trimmed("Architecture").unwrap_or("all");
    let key = if arch == "all" || arch == native {
        name.to_string()
    } else {
        format!("{name}:{arch}")
    };

    // 只保留可用的前缀路径,并加入已确认可打开的当前项。
    let mut paths = Vec::with_capacity(prefix.len() + 1);
    for p in prefix {
        if let Ok(path) = std::fs::canonicalize(&p.path) {
            paths.push(path.to_string_lossy().into_owned());
        }
    }
    match std::fs::canonicalize(&item.path) {
        Ok(path) => paths.push(path.to_string_lossy().into_owned()),
        Err(e) => return ItemResult::failed(&item.path, e.to_string()),
    }

    let cache = match Cache::new(&paths) {
        Ok(cache) => cache,
        Err(e) => return ItemResult::failed(&item.path, config::err_string(&e)),
    };

    let Some(pkg) = cache.get(&key) else {
        return ItemResult::failed(
            &item.path,
            format!("package {key} not found in local deb index"),
        );
    };
    // 源里的最高候选版本(不含本地 deb 与已装条目),供 GUI 提示“源中有新版”
    let repo = repo_version(&pkg);
    let Some(ver) = pkg.get_version(version) else {
        return failed_with(
            &item.path,
            format!("version {version} of {key} not found in local deb index"),
            &repo,
        );
    };

    let already_same = pkg
        .installed()
        .is_some_and(|installed| installed.version() == version);
    ver.set_candidate();
    if already_same {
        pkg.mark_reinstall(true);
    } else if !pkg.mark_install(true, true) {
        return failed_with(
            &item.path,
            format!("cannot mark {key} {version} for installation"),
            &repo,
        );
    }
    pkg.protect();

    if let Err(e) = cache.resolve(false) {
        return failed_with(&item.path, config::err_string(&e), &repo);
    }

    if cache.depcache().broken_count() > 0 {
        let broken = cache
            .get_changes(true)
            .filter_map(|pkg| show_broken_pkg(&cache, &pkg, true))
            .collect::<Vec<_>>()
            .join("; ");
        return failed_with(
            &item.path,
            if broken.is_empty() {
                gettext("dependency resolution failed")
            } else {
                broken
            },
            &repo,
        );
    }

    let mut install = Vec::new();
    let mut remove = Vec::new();
    let mut unauthenticated = Vec::new();
    for pkg in cache.get_changes(true) {
        let full = pkg.fullname(true);
        if pkg.marked_delete() {
            remove.push(full);
            continue;
        }
        let kind = if pkg.marked_new_install() {
            ChangeKind::New
        } else if pkg.marked_upgrade() {
            ChangeKind::Upgrade
        } else if pkg.marked_downgrade() {
            ChangeKind::Downgrade
        } else if pkg.marked_reinstall() {
            ChangeKind::Reinstall
        } else {
            continue;
        };
        let old_version = pkg
            .installed()
            .map(|v| v.version().to_string())
            .unwrap_or_default();
        let new_version = pkg
            .candidate()
            .map(|v| v.version().to_string())
            .unwrap_or_default();
        // 计划版本的任一仓库来源未通过签名验证 → 需要用户确认
        let untrusted = pkg
            .get_version(&new_version)
            .is_some_and(|v| untrusted_repo_version(&v));
        if untrusted {
            unauthenticated.push(full.clone());
        }
        install.push(Change {
            name: full,
            kind,
            old_version,
            new_version,
        });
    }

    ItemResult {
        path: item.path.clone(),
        installable: true,
        failure: String::new(),
        install,
        remove,
        repo_version: repo,
        unauthenticated,
    }
}

/// 失败但已能确定包名时仍带上源内版本,便于 GUI 一致展示
fn failed_with(path: &str, failure: impl Into<String>, repo: &str) -> ItemResult {
    ItemResult {
        repo_version: repo.to_string(),
        ..ItemResult::failed(path, failure)
    }
}

/// 源里的最高候选版本;版本列表顺序不作假设,逐个比较取最大。
fn repo_version(pkg: &Package<'_>) -> String {
    let mut best: Option<String> = None;
    for ver in pkg.versions() {
        if !ver.version_files().any(|f| is_repo_index(&f.package_file())) {
            continue;
        }
        let v = ver.version();
        let higher = best
            .as_deref()
            .is_none_or(|b| cmp_versions(v, b).is_ok_and(|o| o.is_gt()));
        if higher {
            best = Some(v.to_string());
        }
    }
    best.unwrap_or_default()
}

/// 该版本是否有任一仓库索引提供(区别于本地 deb / 已装条目)
fn untrusted_repo_version(ver: &Version<'_>) -> bool {
    ver.version_files().any(|f| {
        let file = f.package_file();
        is_repo_index(&file) && !file.index_file().is_trusted()
    })
}

fn is_repo_index(file: &oma_apt::PackageFile<'_>) -> bool {
    file.index_type().is_some_and(|t| t == REPO_INDEX)
}
