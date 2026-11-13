//! 安装模式:一个 apt 事务完成全部本地 deb 的求解、下载与安装。
//!
//! 与旧逐包 dpkg 流程不同,依赖由 libapt-pkg 统一求解;本地 deb 先复制到
//! `Dir::Cache::Archives`,再由 apt 的 acquire/dpkg 流程安装。PTY 由 daemon
//! 提供,`Dpkg::Use-Pty=false` 让 dpkg 直接使用该终端。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fdebi_apt_proto::{Event, Item, PackageState, Phase, Request};
use fdebi_core::DebFile;
use fdebi_core::i18n::{gettext, tf};
use oma_apt::Cache;
use oma_apt::config::Config;
use oma_apt::error::AptErrors;
use oma_apt::progress::{
    AcquireProgress, DynAcquireProgress, DynInstallProgress, InstallProgress,
};
use oma_apt::raw::{AcqTextStatus, ItemDesc, PkgAcquire};
use oma_apt::util::{apt_lock, apt_lock_inner, apt_unlock, apt_unlock_inner};

use crate::config;
use crate::emit::Emitter;

#[derive(Clone)]
struct Valid {
    path: String,
    name: String,
    version: String,
    key: String,
}

pub fn run(req: &Request, out: &Emitter) -> Result<(), String> {
    config::apply(req, out);
    let native = config::native_arch()?;

    // 1. 复核指纹与文件类型;失败项立即上报并从计划剔除
    let mut valid = Vec::new();
    for item in &req.items {
        match prepare(item, &native) {
            Ok(v) => valid.push(v),
            Err((name, detail)) => out.emit(&Event::Package {
                path: item.path.clone(),
                name,
                state: PackageState::Failed,
                detail,
            }),
        }
    }
    if valid.is_empty() {
        out.emit(&Event::Result {
            ok: false,
            detail: gettext("no installable local package"),
            items: Vec::new(),
        });
        return Ok(());
    }

    // 2. 建立缓存并标记全部可达项
    let paths: Vec<String> = valid.iter().map(|v| v.path.clone()).collect();
    let cache = match Cache::new(&paths) {
        Ok(cache) => cache,
        Err(e) => {
            let detail = config::err_string(&e);
            fail_all(out, &valid, &detail);
            out.emit(&Event::Result {
                ok: false,
                detail,
                items: Vec::new(),
            });
            return Ok(());
        }
    };

    let mut marked: Vec<Valid> = Vec::new();
    for v in &valid {
        let Some(pkg) = cache.get(&v.key) else {
            out.emit(&Event::Package {
                path: v.path.clone(),
                name: v.name.clone(),
                state: PackageState::Failed,
                detail: format!("package {} not found in local deb index", v.key),
            });
            continue;
        };
        let Some(ver) = pkg.get_version(&v.version) else {
            out.emit(&Event::Package {
                path: v.path.clone(),
                name: v.name.clone(),
                state: PackageState::Failed,
                detail: format!("version {} not found for {}", v.version, v.key),
            });
            continue;
        };
        let already_same = pkg
            .installed()
            .is_some_and(|installed| installed.version() == v.version);
        ver.set_candidate();
        if already_same {
            pkg.mark_reinstall(true);
        } else if !pkg.mark_install(true, true) {
            out.emit(&Event::Package {
                path: v.path.clone(),
                name: v.name.clone(),
                state: PackageState::Failed,
                detail: format!("cannot mark {} {} for installation", v.key, v.version),
            });
            continue;
        }
        pkg.protect();
        marked.push(v.clone());
    }

    if marked.is_empty() {
        out.emit(&Event::Result {
            ok: false,
            detail: gettext("no installable local package"),
            items: Vec::new(),
        });
        return Ok(());
    }

    if let Err(e) = cache.resolve(false) {
        let detail = config::err_string(&e);
        fail_all(out, &marked, &detail);
        out.emit(&Event::Result {
            ok: false,
            detail,
            items: Vec::new(),
        });
        return Ok(());
    }

    if cache.depcache().broken_count() > 0 {
        use oma_apt::util::show_broken_pkg;
        let detail = cache
            .get_changes(true)
            .filter_map(|pkg| show_broken_pkg(&cache, &pkg, true))
            .collect::<Vec<_>>()
            .join("; ");
        let detail = if detail.is_empty() {
            gettext("dependency resolution failed")
        } else {
            detail
        };
        fail_all(out, &marked, &detail);
        out.emit(&Event::Result {
            ok: false,
            detail,
            items: Vec::new(),
        });
        return Ok(());
    }

    // 3. 本地 deb 复制到 apt 缓存目录(apt 的 acquire 会把它们当作本地文件)
    if let Err(e) = stage_local_debs(&marked) {
        fail_all(out, &marked, &e);
        out.emit(&Event::Result {
            ok: false,
            detail: e,
            items: Vec::new(),
        });
        return Ok(());
    }

    // 4. 全局锁 → 下载/获取 → 安装
    let mut lock = AptLock::acquire().map_err(|e| {
        fail_all(out, &marked, &e);
        e
    })?;

    out.emit(&Event::Stage {
        phase: Phase::Downloading,
        desc: gettext("Downloading packages"),
    });
    let mut acquire = AcquireProgress::new(WorkerAcquire { out: out.clone() });
    if let Err(e) = cache.get_archives(&mut acquire) {
        let detail = config::err_string(&AptErrors::from(e));
        drop(lock);
        fail_all(out, &marked, &detail);
        out.emit(&Event::Result {
            ok: false,
            detail,
            items: Vec::new(),
        });
        return Ok(());
    }
    // dpkg 需要自己拿锁
    lock.release_inner();

    out.emit(&Event::Stage {
        phase: Phase::Installing,
        desc: gettext("Installing packages"),
    });

    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let announced = Arc::new(Mutex::new(HashSet::<String>::new()));
    let progress = WorkerInstall {
        out: out.clone(),
        targets: marked
            .iter()
            .map(|v| (v.name.clone(), v.path.clone()))
            .collect(),
        announced: announced.clone(),
        errors: errors.clone(),
    };
    let mut progress = InstallProgress::new(progress);

    if let Err(e) = cache.do_install(&mut progress) {
        // 与 oma-pm 一致:do_install 失败后重新锁上 inner,再统一解锁
        apt_lock_inner().ok();
        drop(lock);
        let mut detail = config::err_string(&e);
        let extra = errors.lock().unwrap();
        if !extra.is_empty() {
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str(&extra.join("\n"));
        }
        fail_all(out, &marked, &detail);
        out.emit(&Event::Result {
            ok: false,
            detail,
            items: Vec::new(),
        });
        return Ok(());
    }
    drop(lock);

    // 5. 逐项完成
    for v in &marked {
        out.emit(&Event::Package {
            path: v.path.clone(),
            name: v.name.clone(),
            state: PackageState::Done,
            detail: String::new(),
        });
    }
    let detail = errors.lock().unwrap().join("\n");
    out.emit(&Event::Result {
        ok: true,
        detail,
        items: Vec::new(),
    });
    Ok(())
}

/// 校验并解析一个本地 deb。
fn prepare(item: &Item, native: &str) -> Result<Valid, (String, String)> {
    let path = Path::new(&item.path);
    let fallback_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| item.path.clone());

    let meta = std::fs::metadata(path)
        .map_err(|e| (fallback_name.clone(), format!("cannot stat {}: {e}", item.path)))?;
    if !meta.is_file() {
        return Err((fallback_name, gettext("not a regular file")));
    }
    let Some(stamp) = item.stamp else {
        return Err((fallback_name, gettext("missing file stamp")));
    };
    if !stamp.matches(path) {
        return Err((fallback_name, gettext("file changed since analysis")));
    }

    let deb = DebFile::open(path)
        .map_err(|e| (fallback_name.clone(), e.to_string()))?;
    let name = deb
        .field_trimmed("Package")
        .ok_or_else(|| (fallback_name.clone(), gettext("missing Package field")))?
        .to_string();
    let version = deb
        .field_trimmed("Version")
        .ok_or_else(|| (name.clone(), gettext("missing Version field")))?
        .to_string();
    let arch = deb.field_trimmed("Architecture").unwrap_or("all");
    let key = if arch == "all" || arch == native {
        name.clone()
    } else {
        format!("{name}:{arch}")
    };

    Ok(Valid {
        path: item.path.clone(),
        name,
        version,
        key,
    })
}

/// 把本地 deb 复制到 apt 的 archives 目录,供 acquire/do_install 使用。
fn stage_local_debs(items: &[Valid]) -> Result<(), String> {
    let archive_dir = Config::new().dir("Dir::Cache::Archives", "/var/cache/apt/archives/");
    let archive_dir = PathBuf::from(archive_dir);
    std::fs::create_dir_all(&archive_dir).map_err(|e| {
        format!(
            "cannot create archive directory {}: {e}",
            archive_dir.display()
        )
    })?;
    let archive_dir = std::fs::canonicalize(&archive_dir).unwrap_or(archive_dir);

    for item in items {
        let src = std::fs::canonicalize(&item.path)
            .map_err(|e| format!("cannot canonicalize {}: {e}", item.path))?;
        let file_name = src
            .file_name()
            .ok_or_else(|| format!("invalid file name: {}", item.path))?;
        let dest = archive_dir.join(file_name);
        if dest == src {
            continue;
        }
        if std::fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink()) {
            std::fs::remove_file(&dest)
                .map_err(|e| format!("cannot remove stale symlink {}: {e}", dest.display()))?;
        }
        std::fs::copy(&src, &dest).map_err(|e| {
            format!(
                "cannot copy {} to {}: {e}",
                src.display(),
                dest.display()
            )
        })?;
    }
    Ok(())
}

fn fail_all(out: &Emitter, items: &[Valid], detail: &str) {
    for item in items {
        out.emit(&Event::Package {
            path: item.path.clone(),
            name: item.name.clone(),
            state: PackageState::Failed,
            detail: detail.to_string(),
        });
    }
}

/// apt/dpkg 全局锁的 RAII 守卫。
///
/// `apt_lock()` 同时取得前端与 dpkg 锁;`get_archives()` 之后必须
/// `apt_unlock_inner()` 让 dpkg 自己加锁,最后 `apt_unlock()`。
struct AptLock {
    outer: bool,
    inner: bool,
}

impl AptLock {
    fn acquire() -> Result<Self, String> {
        apt_lock().map_err(|e| config::err_string(&e))?;
        Ok(Self {
            outer: true,
            inner: true,
        })
    }

    fn release_inner(&mut self) {
        if self.inner {
            apt_unlock_inner();
            self.inner = false;
        }
    }
}

impl Drop for AptLock {
    fn drop(&mut self) {
        if self.inner {
            apt_unlock_inner();
        }
        if self.outer {
            apt_unlock();
        }
    }
}

struct WorkerAcquire {
    out: Emitter,
}

impl DynAcquireProgress for WorkerAcquire {
    fn pulse_interval(&self) -> usize {
        500_000
    }

    fn hit(&mut self, item: &ItemDesc) {
        self.out.output(format!("Hit: {}", item.description()));
    }

    fn fetch(&mut self, item: &ItemDesc) {
        self.out.output(format!("Get: {}", item.description()));
        self.out.emit(&Event::Progress {
            percent: -1.0,
            desc: tf("Fetching {0}", &[&item.description()]),
        });
    }

    fn fail(&mut self, item: &ItemDesc) {
        self.out.output(format!("Err: {}", item.description()));
    }

    fn pulse(&mut self, status: &AcqTextStatus, _owner: &PkgAcquire) {
        self.out.emit(&Event::Progress {
            percent: status.percent(),
            desc: gettext("Downloading"),
        });
    }

    fn done(&mut self, _item: &ItemDesc) {}

    fn start(&mut self) {
        self.out.emit(&Event::Stage {
            phase: Phase::Downloading,
            desc: gettext("Downloading packages"),
        });
    }

    fn stop(&mut self, status: &AcqTextStatus) {
        self.out.output(format!(
            "Fetched {} bytes in {}s",
            status.fetched_bytes(),
            status.elapsed_time()
        ));
    }
}

struct WorkerInstall {
    out: Emitter,
    /// (包名, 路径):用于把 dpkg 状态映射回前端行
    targets: Vec<(String, String)>,
    announced: Arc<Mutex<HashSet<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl DynInstallProgress for WorkerInstall {
    fn status_changed(
        &mut self,
        pkgname: String,
        steps_done: u64,
        total_steps: u64,
        action: String,
    ) {
        let percent = if total_steps == 0 {
            -1.0
        } else {
            steps_done as f64 * 100.0 / total_steps as f64
        };
        self.out.emit(&Event::Progress {
            percent,
            desc: format!("{action} {pkgname}"),
        });

        let base = pkgname.split(':').next().unwrap_or(&pkgname);
        if let Some((_, path)) = self.targets.iter().find(|(name, _)| name == base) {
            let mut announced = self.announced.lock().unwrap();
            if announced.insert(path.clone()) {
                self.out.emit(&Event::Package {
                    path: path.clone(),
                    name: base.to_string(),
                    state: PackageState::Installing,
                    detail: String::new(),
                });
            }
        }
    }

    fn error(&mut self, pkgname: String, _steps_done: u64, _total_steps: u64, error: String) {
        self.out.output(format!("E: {pkgname}: {error}"));
        self.errors.lock().unwrap().push(format!("{pkgname}: {error}"));
    }
}
