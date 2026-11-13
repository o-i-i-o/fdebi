//! 卸载模式:`apt remove` 语义,但保留旧 dpkg 的安全边界——
//! 若解析器要连带删除反向依赖,则中止而不执行。

use std::sync::{Arc, Mutex};

use fdebi_apt_proto::{Event, PackageState, Phase, Request};
use fdebi_core::i18n::gettext;
use oma_apt::Cache;
use oma_apt::progress::{DynInstallProgress, InstallProgress};
use oma_apt::util::{apt_lock, apt_lock_inner, apt_unlock, apt_unlock_inner};

use crate::config;
use crate::emit::Emitter;

pub fn run(req: &Request, out: &Emitter) -> Result<(), String> {
    config::apply(req, out);
    let name = req
        .remove_name
        .clone()
        .ok_or_else(|| "missing package name".to_string())?;

    let cache = Cache::new::<&str>(&[]).map_err(|e| config::err_string(&e))?;
    let Some(pkg) = cache.get(&name) else {
        return fail(out, &name, format!("package {name} not found"));
    };
    if !pkg.is_installed() {
        return fail(out, &name, format!("package {name} is not installed"));
    }
    let key = pkg.fullname(true);
    if !pkg.mark_delete(false) {
        return fail(out, &name, format!("cannot mark {name} for removal"));
    }
    pkg.protect();

    if let Err(e) = cache.resolve(false) {
        return fail(out, &name, config::err_string(&e));
    }

    // 反向依赖检查:apt 可能为了满足依赖而连带删除其它包;
    // 旧 dpkg -r 会直接拒绝,这里保持一致。
    let extras: Vec<String> = cache
        .get_changes(true)
        .filter(|p| p.marked_delete() && p.fullname(true) != key)
        .map(|p| p.fullname(true))
        .collect();
    if !extras.is_empty() {
        return fail(
            out,
            &name,
            format!(
                "{}: {}",
                gettext("removal would also remove reverse dependencies"),
                extras.join(", ")
            ),
        );
    }

    out.emit(&Event::Stage {
        phase: Phase::Removing,
        desc: format!("Removing {key}"),
    });
    out.emit(&Event::Package {
        path: String::new(),
        name: key.clone(),
        state: PackageState::Installing,
        detail: String::new(),
    });

    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let progress = WorkerRemove {
        out: out.clone(),
        errors: errors.clone(),
    };
    let mut progress = InstallProgress::new(progress);

    let lock = match AptLock::acquire() {
        Ok(lock) => lock,
        Err(e) => return fail(out, &name, e),
    };
    // 卸载不需要 acquire;直接让 dpkg 接管锁
    let mut lock = lock;
    lock.release_inner();

    if let Err(e) = cache.do_install(&mut progress) {
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
        return fail(out, &key, detail);
    }
    drop(lock);

    let detail = errors.lock().unwrap().join("\n");
    out.emit(&Event::Package {
        path: String::new(),
        name: key,
        state: PackageState::Done,
        detail: String::new(),
    });
    out.emit(&Event::Result {
        ok: true,
        detail,
        items: Vec::new(),
    });
    Ok(())
}

fn fail(out: &Emitter, name: &str, detail: String) -> Result<(), String> {
    out.emit(&Event::Package {
        path: String::new(),
        name: name.to_string(),
        state: PackageState::Failed,
        detail: detail.clone(),
    });
    out.emit(&Event::Result {
        ok: false,
        detail,
        items: Vec::new(),
    });
    Ok(())
}

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

struct WorkerRemove {
    out: Emitter,
    errors: Arc<Mutex<Vec<String>>>,
}

impl DynInstallProgress for WorkerRemove {
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
    }

    fn error(&mut self, pkgname: String, _steps_done: u64, _total_steps: u64, error: String) {
        self.out.output(format!("E: {pkgname}: {error}"));
        self.errors.lock().unwrap().push(format!("{pkgname}: {error}"));
    }
}
