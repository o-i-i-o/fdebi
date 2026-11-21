//! Safe Rust wrapper around the libapt-pkg C ABI (`native/apt_shim.cc`).
//!
//! One [`Apt`] owns an apt cache built from the system configuration plus the
//! local `.deb` files registered as *volatile* sources (apt's `apt install
//! ./foo.deb` mechanism).  libapt-pkg is not thread safe, so the shim keeps a
//! process-wide mutex for the lifetime of the session and [`Apt`] is
//! deliberately `!Send`/`!Sync`: create it, use it, and drop it on one thread.
//!
//! Dependency solving, version comparison, repository candidate selection,
//! conffile metadata, broken-package detection, reverse dependencies, archive
//! download and `apt-mark` state writing all come from libapt-pkg.  The rest of
//! fdebi only sees plain Rust records.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::debfile::DebFile;
use crate::error::FdebiError;
use crate::i18n::{gettext, tf};
use crate::solver::{ChangeKind, Changes, DownloadSpec, InstallChange, Resolution, Verdict};
use crate::version::cmp_version;

/// apt archives downloaded by fdebi ([`Apt::fetch`] creates it on demand;
/// daemon runs as root).
pub const CACHE_DIR: &str = "/var/cache/fdebi";
const CACHE_DIR_APT: &str = "/var/cache/fdebi/";
const LISTS_DIR: &str = "/var/lib/apt/lists";

#[repr(C)]
struct FdebiApt {
    _private: [u8; 0],
}

type FetchCb = Option<unsafe extern "C" fn(user: *mut c_void, current: u64, total: u64) -> c_int>;

unsafe extern "C" {
    fn fdebi_apt_open() -> *mut FdebiApt;
    fn fdebi_apt_close(ctx: *mut FdebiApt);
    fn fdebi_apt_set_option(ctx: *mut FdebiApt, key: *const c_char, value: *const c_char) -> c_int;
    fn fdebi_apt_add_volatile(ctx: *mut FdebiApt, path: *const c_char) -> c_int;
    fn fdebi_apt_build(ctx: *mut FdebiApt) -> c_int;
    fn fdebi_apt_native_arch(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_foreign_arches(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_installed_version(ctx: *mut FdebiApt, key: *const c_char) -> *const c_char;
    fn fdebi_apt_repo_version(ctx: *mut FdebiApt, key: *const c_char) -> *const c_char;
    fn fdebi_apt_conffiles(ctx: *mut FdebiApt, key: *const c_char) -> *const c_char;
    fn fdebi_apt_broken(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_solve(
        ctx: *mut FdebiApt,
        name: *const c_char,
        version: *const c_char,
        arch: *const c_char,
        path: *const c_char,
        priors: *const *const c_char,
        n_priors: usize,
    ) -> c_int;
    fn fdebi_apt_result_install(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_result_remove(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_result_unauth(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_result_downloads(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_last_error(ctx: *mut FdebiApt) -> *const c_char;
    fn fdebi_apt_fetch(
        ctx: *mut FdebiApt,
        name: *const c_char,
        version: *const c_char,
        cb: FetchCb,
        user: *mut c_void,
    ) -> *mut c_char;
    fn fdebi_apt_cancelled(ctx: *mut FdebiApt) -> c_int;
    fn fdebi_apt_reverse_deps(ctx: *mut FdebiApt, removed: *const c_char) -> *const c_char;
    fn fdebi_apt_mark(
        ctx: *mut FdebiApt,
        auto_names: *const *const c_char,
        n_auto: usize,
        manual: *const c_char,
    ) -> c_int;
    fn fdebi_apt_free_string(s: *mut c_char);
}

/// A batch member which has already been accepted by an earlier solve.
#[derive(Debug, Clone)]
pub struct Prior {
    pub key: String,
    pub version: String,
    pub arch: String,
    pub path: String,
}

/// One apt session.  See the module docs for the threading contract.
pub struct Apt {
    raw: *mut FdebiApt,
    native_arch: String,
    foreign_arches: Vec<String>,
    built: bool,
    /// libapt-pkg global state is not thread safe; keeping the handle on its
    /// creating thread also keeps the C++ mutex owner there.
    _not_send: PhantomData<Rc<()>>,
}

impl Apt {
    /// Open apt with system configuration plus caller supplied `Key=Value`
    /// options.  No lists/status parsing happens until [`Apt::build`].
    pub fn open(apt_opts: &[String]) -> Result<Self, FdebiError> {
        let raw = unsafe { fdebi_apt_open() };
        if raw.is_null() {
            return Err(FdebiError::Apt(gettext(
                "libapt-pkg could not be initialized",
            )));
        }
        let mut apt = Self {
            raw,
            native_arch: String::new(),
            foreign_arches: Vec::new(),
            built: false,
            _not_send: PhantomData,
        };

        // fdebi owns its archive cache; disabling apt's binary caches avoids
        // stale files in /var/cache/apt and makes each session self-contained.
        apt.set_option("Dir::Cache::archives", CACHE_DIR_APT)?;
        apt.set_option("Dir::Cache::pkgcache", "")?;
        apt.set_option("Dir::Cache::srcpkgcache", "")?;
        for opt in apt_opts {
            let Some((key, value)) = opt.split_once('=') else {
                return Err(FdebiError::Apt(tf(
                    "invalid apt option '{0}' (expected Key=Value)",
                    &[opt],
                )));
            };
            if key.is_empty() {
                return Err(FdebiError::Apt(tf(
                    "invalid apt option '{0}' (empty key)",
                    &[opt],
                )));
            }
            apt.set_option(key, value)?;
        }

        let native = apt.query(fdebi_apt_native_arch);
        if native.is_empty() {
            return Err(FdebiError::Apt(gettext(
                "libapt-pkg did not report a native architecture",
            )));
        }
        apt.native_arch = native;
        apt.foreign_arches = apt
            .query(fdebi_apt_foreign_arches)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();
        Ok(apt)
    }

    /// Open a session and register every local deb as a volatile apt source.
    pub fn with_debs(apt_opts: &[String], paths: &[PathBuf]) -> Result<Self, FdebiError> {
        let mut apt = Self::open(apt_opts)?;
        for path in paths {
            apt.add_volatile(path)?;
        }
        apt.build()?;
        Ok(apt)
    }

    fn set_option(&mut self, key: &str, value: &str) -> Result<(), FdebiError> {
        let key =
            CString::new(key).map_err(|_| FdebiError::Apt(gettext("invalid apt option name")))?;
        let value = CString::new(value)
            .map_err(|_| FdebiError::Apt(gettext("invalid apt option value")))?;
        let rc = unsafe { fdebi_apt_set_option(self.raw, key.as_ptr(), value.as_ptr()) };
        if rc != 0 {
            return Err(FdebiError::Apt(gettext("could not configure libapt-pkg")));
        }
        Ok(())
    }

    pub fn add_volatile(&mut self, path: &Path) -> Result<(), FdebiError> {
        if self.built {
            return Err(FdebiError::Apt(gettext(
                "cannot add a package after the apt cache was built",
            )));
        }
        let path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| FdebiError::Apt(gettext("invalid package path")))?;
        let rc = unsafe { fdebi_apt_add_volatile(self.raw, path.as_ptr()) };
        if rc != 0 {
            return Err(FdebiError::Apt(gettext(
                "could not register the local package",
            )));
        }
        Ok(())
    }

    pub fn build(&mut self) -> Result<(), FdebiError> {
        if self.built {
            return Ok(());
        }
        let rc = unsafe { fdebi_apt_build(self.raw) };
        if rc != 0 {
            return Err(self.error("could not build the apt cache"));
        }
        self.built = true;
        Ok(())
    }

    pub fn native_arch(&self) -> &str {
        &self.native_arch
    }

    #[allow(dead_code)]
    pub fn foreign_arches(&self) -> &[String] {
        &self.foreign_arches
    }

    /// apt key for a local deb (`name`, or `name:arch` for foreign arch).
    pub fn key_for(&self, deb: &DebFile) -> String {
        let name = deb.field_trimmed("Package").unwrap_or("");
        let arch = deb.field_trimmed("Architecture").unwrap_or("all");
        if arch == "all" || arch == self.native_arch {
            name.to_string()
        } else {
            format!("{name}:{arch}")
        }
    }

    fn ensure_built(&self) -> Result<(), FdebiError> {
        if self.built {
            Ok(())
        } else {
            Err(FdebiError::Apt(gettext("apt cache is not built")))
        }
    }

    fn error(&self, fallback: &str) -> FdebiError {
        let message = self.query(fdebi_apt_last_error);
        FdebiError::Apt(if message.is_empty() {
            fallback.to_string()
        } else {
            message
        })
    }

    fn query(&self, f: unsafe extern "C" fn(*mut FdebiApt) -> *const c_char) -> String {
        let ptr = unsafe { f(self.raw) };
        if ptr.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    fn query_key(
        &self,
        f: unsafe extern "C" fn(*mut FdebiApt, *const c_char) -> *const c_char,
        key: &str,
    ) -> Result<String, FdebiError> {
        self.ensure_built()?;
        let key = CString::new(key).map_err(|_| FdebiError::Apt(gettext("invalid package key")))?;
        let ptr = unsafe { f(self.raw, key.as_ptr()) };
        if ptr.is_null() {
            return Ok(String::new());
        }
        Ok(unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned())
    }

    /// Installed version of `key`, empty when not installed.
    pub fn installed_version(&self, key: &str) -> Result<String, FdebiError> {
        self.query_key(fdebi_apt_installed_version, key)
    }

    /// Highest version available from a repository, empty when absent.
    pub fn repo_version(&self, key: &str) -> Result<String, FdebiError> {
        self.query_key(fdebi_apt_repo_version, key)
    }

    /// `(path, md5)` pairs recorded in dpkg's status entry for the installed
    /// version of `key`.
    pub fn conffiles(&self, key: &str) -> Result<Vec<(String, String)>, FdebiError> {
        let records = self.query_key(fdebi_apt_conffiles, key)?;
        Ok(records
            .lines()
            .filter_map(|line| {
                let (path, md5) = line.split_once('\t')?;
                let (path, md5) = (path.trim(), md5.trim());
                (!path.is_empty() && !md5.is_empty()).then(|| (path.to_string(), md5.to_string()))
            })
            .collect())
    }

    /// Packages in a half-installed/half-configured state, plus installed
    /// packages with unsatisfied dependencies.
    pub fn broken(&mut self) -> Result<Vec<String>, FdebiError> {
        self.ensure_built()?;
        Ok(self
            .query(fdebi_apt_broken)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect())
    }

    /// Ask libapt-pkg to solve installing `deb`, with earlier accepted batch
    /// members treated as already installed.
    pub fn solve(&mut self, deb: &DebFile, priors: &[Prior]) -> Result<Verdict, FdebiError> {
        self.ensure_built()?;
        let name = deb.field_trimmed("Package").unwrap_or("").to_string();
        let version = deb.field_trimmed("Version").unwrap_or("").to_string();
        let arch = deb
            .field_trimmed("Architecture")
            .unwrap_or("all")
            .to_string();
        if name.is_empty() || version.is_empty() {
            return Ok(Verdict::Uninstallable(gettext(
                "Invalid package: missing Package or Version field",
            )));
        }

        // dpkg/gdebi semantics: never silently downgrade.  This also covers
        // earlier batch members with the same key (apt only knows the system
        // state, not the not-yet-installed batch context).
        let key = if arch == "all" || arch == self.native_arch {
            name.clone()
        } else {
            format!("{name}:{arch}")
        };
        let installed = self.installed_version(&key)?;
        let newest = priors
            .iter()
            .filter(|p| p.key == key)
            .map(|p| p.version.as_str())
            .chain(std::iter::once(installed.as_str()))
            .filter(|v| !v.is_empty())
            .max_by(|a, b| cmp_version(a, b));
        if let Some(newest) = newest
            && cmp_version(newest, &version) == std::cmp::Ordering::Greater
        {
            return Ok(Verdict::Uninstallable(gettext(
                "A later version is already installed",
            )));
        }

        let c_name = cstring(&name)?;
        let c_version = cstring(&version)?;
        let c_arch = cstring(&arch)?;
        let c_path = cstring(&deb.path().to_string_lossy())?;
        let prior_lines: Vec<CString> = priors
            .iter()
            .map(|p| cstring(&format!("{}\t{}\t{}\t{}", p.key, p.version, p.arch, p.path)))
            .collect::<Result<_, _>>()?;
        let prior_ptrs: Vec<*const c_char> = prior_lines.iter().map(|line| line.as_ptr()).collect();

        let rc = unsafe {
            fdebi_apt_solve(
                self.raw,
                c_name.as_ptr(),
                c_version.as_ptr(),
                c_arch.as_ptr(),
                c_path.as_ptr(),
                prior_ptrs.as_ptr(),
                prior_ptrs.len(),
            )
        };

        match rc {
            0 => Ok(Verdict::Installable(self.resolution())),
            1 => Ok(Verdict::Uninstallable(gettext(
                "Failed to satisfy all dependencies (conflicting requests or missing packages)",
            ))),
            2 => Ok(Verdict::Uninstallable(gettext(
                "Invalid package: missing Package or Version field",
            ))),
            3 => Ok(Verdict::Uninstallable(tf(
                "Wrong architecture '{0}'",
                &[&arch],
            ))),
            4 => Ok(Verdict::Uninstallable(gettext(
                "A later version is already installed",
            ))),
            _ => Err(self.error("dependency solving failed")),
        }
    }

    fn resolution(&self) -> Resolution {
        let install_details: Vec<InstallChange> = text(fdebi_apt_result_install, self.raw)
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(4, '\t');
                let name = fields.next()?;
                let kind = fields.next()?;
                let old_version = fields.next()?;
                let new_version = fields.next()?;
                (!name.is_empty()).then(|| InstallChange {
                    name: name.to_string(),
                    kind: ChangeKind::from_arg(kind),
                    old_version: old_version.to_string(),
                    new_version: new_version.to_string(),
                })
            })
            .collect();
        let remove = lines(fdebi_apt_result_remove, self.raw);
        let unauthenticated = lines(fdebi_apt_result_unauth, self.raw);
        let downloads = text(fdebi_apt_result_downloads, self.raw)
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(4, '\t');
                let name = fields.next()?;
                let version = fields.next()?;
                let size = fields.next()?.parse().unwrap_or(0);
                let trusted = fields.next()? == "1";
                (!name.is_empty()).then(|| DownloadSpec {
                    name: name.to_string(),
                    version: version.to_string(),
                    size,
                    trusted,
                })
            })
            .collect();
        Resolution {
            changes: Changes {
                install: install_details
                    .iter()
                    .map(|change| change.name.clone())
                    .collect(),
                install_details,
                remove,
                unauthenticated,
            },
            downloads,
        }
    }

    /// Fetch one repository archive, reporting byte progress through `cb`.
    /// Returns the path of the downloaded file.
    pub fn fetch<F>(&mut self, name: &str, version: &str, mut cb: F) -> Result<PathBuf, FdebiError>
    where
        F: FnMut(u64, u64) -> bool,
    {
        self.ensure_built()?;
        std::fs::create_dir_all(CACHE_DIR)?;
        struct Callback<'a> {
            callback: &'a mut dyn FnMut(u64, u64) -> bool,
        }
        unsafe extern "C" fn trampoline(user: *mut c_void, current: u64, total: u64) -> c_int {
            let callback = unsafe { &mut *(user as *mut Callback<'_>) };
            if (callback.callback)(current, total) {
                1
            } else {
                0
            }
        }

        let name = cstring(name)?;
        let version = cstring(version)?;
        let mut holder = Callback { callback: &mut cb };
        let ptr = unsafe {
            fdebi_apt_fetch(
                self.raw,
                name.as_ptr(),
                version.as_ptr(),
                Some(trampoline),
                &mut holder as *mut Callback<'_> as *mut c_void,
            )
        };
        if ptr.is_null() {
            if unsafe { fdebi_apt_cancelled(self.raw) } != 0 {
                return Err(FdebiError::Cancelled);
            }
            return Err(self.error("cannot download the package archive"));
        }
        let path = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { fdebi_apt_free_string(ptr) };
        Ok(PathBuf::from(path))
    }

    /// Reverse-dependency warnings: `(dependent package, dependency group)`.
    pub fn reverse_deps(&mut self, removed: &str) -> Result<Vec<(String, String)>, FdebiError> {
        self.ensure_built()?;
        let removed = cstring(removed)?;
        let ptr = unsafe { fdebi_apt_reverse_deps(self.raw, removed.as_ptr()) };
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        Ok(unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .lines()
            .filter_map(|line| {
                let (package, requires) = line.split_once('\t')?;
                (!package.is_empty() && !requires.is_empty())
                    .then(|| (package.to_string(), requires.to_string()))
            })
            .collect())
    }

    /// Mark dependencies auto and the explicitly installed local package
    /// manual, then write apt's extended state file.
    pub fn mark(&mut self, auto: &[String], manual: &str) -> Result<(), FdebiError> {
        self.ensure_built()?;
        let auto_lines: Vec<CString> = auto
            .iter()
            .map(|name| cstring(name))
            .collect::<Result<_, _>>()?;
        let auto_ptrs: Vec<*const c_char> = auto_lines.iter().map(|line| line.as_ptr()).collect();
        let manual = cstring(manual)?;
        let rc = unsafe {
            fdebi_apt_mark(
                self.raw,
                auto_ptrs.as_ptr(),
                auto_ptrs.len(),
                manual.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(self.error("could not update apt installation marks"));
        }
        Ok(())
    }
}

impl Drop for Apt {
    fn drop(&mut self) {
        unsafe { fdebi_apt_close(self.raw) };
    }
}

fn cstring(value: &str) -> Result<CString, FdebiError> {
    CString::new(value).map_err(|_| FdebiError::Apt(gettext("invalid string for libapt-pkg")))
}

fn text(f: unsafe extern "C" fn(*mut FdebiApt) -> *const c_char, raw: *mut FdebiApt) -> String {
    let ptr = unsafe { f(raw) };
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

fn lines(
    f: unsafe extern "C" fn(*mut FdebiApt) -> *const c_char,
    raw: *mut FdebiApt,
) -> Vec<String> {
    text(f, raw)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

/// apt lists are older than the configured sources (or absent): a prompt to
/// run `apt update`, never a hard failure.
pub fn lists_stale(apt_opts: &[String]) -> bool {
    use std::path::Path;

    fn newest_mtime(dir: &Path, accept: impl Fn(&str) -> bool) -> Option<std::time::SystemTime> {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_str().is_some_and(&accept))
            .filter_map(|entry| entry.metadata().and_then(|m| m.modified()).ok())
            .max()
    }

    let lists_dir = apt_opts
        .iter()
        .find_map(|opt| opt.strip_prefix("Dir::State::lists="))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(LISTS_DIR));
    let lists = newest_mtime(&lists_dir, |name| {
        name.contains("Packages") && !name.contains("Translation")
    });

    let sources_list = apt_opts
        .iter()
        .find_map(|opt| opt.strip_prefix("Dir::Etc::sourcelist="))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/apt/sources.list"));
    let sources_parts = apt_opts
        .iter()
        .find_map(|opt| opt.strip_prefix("Dir::Etc::sourceparts="))
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

    match (sources, lists) {
        (_, None) => true,
        (Some(sources), Some(lists)) => sources > lists,
        (None, Some(_)) => false,
    }
}
