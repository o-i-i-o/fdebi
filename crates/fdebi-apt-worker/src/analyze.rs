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
use oma_apt::Cache;
use oma_apt::util::show_broken_pkg;

use crate::config;
use crate::emit::Emitter;

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
    let Some(ver) = pkg.get_version(version) else {
        return ItemResult::failed(
            &item.path,
            format!("version {version} of {key} not found in local deb index"),
        );
    };

    let already_same = pkg
        .installed()
        .is_some_and(|installed| installed.version() == version);
    ver.set_candidate();
    if already_same {
        pkg.mark_reinstall(true);
    } else if !pkg.mark_install(true, true) {
        return ItemResult::failed(
            &item.path,
            format!("cannot mark {key} {version} for installation"),
        );
    }
    pkg.protect();

    if let Err(e) = cache.resolve(false) {
        return ItemResult::failed(&item.path, config::err_string(&e));
    }

    if cache.depcache().broken_count() > 0 {
        let broken = cache
            .get_changes(true)
            .filter_map(|pkg| show_broken_pkg(&cache, &pkg, true))
            .collect::<Vec<_>>()
            .join("; ");
        return ItemResult::failed(
            &item.path,
            if broken.is_empty() {
                gettext("dependency resolution failed")
            } else {
                broken
            },
        );
    }

    let mut install = Vec::new();
    let mut remove = Vec::new();
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
    }
}
