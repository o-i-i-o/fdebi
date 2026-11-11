//! fdebm-core:deb 安装流程的终端测试前端(gdebi 风格 CLI)。
//!
//! 开发/测试专用:独立 bin target,复用本库的完整安装管线(解析 → 求解 →
//! 下载 → dpkg 批装 → apt 标记),与守护进程 run_install 同路径,但无
//! D-Bus/Polkit,便于在命令行直接验证 deb 安装流程。不参与 deb 打包
//! (scripts/build-deb.sh 只安装 fdebm-daemon / fdebm-gtk);本文件不属于
//! 库 target,不会链接进任何打包二进制。
//!
//! 用法:
//!   fdebm-core -n foo.deb              仅分析求解,打印安装方案
//!   fdebm-core foo.deb                 分析后确认,再执行安装
//!   fdebm-core -y --keep a.deb b.deb   脚本化批量安装(按依赖排序)

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use clap::Parser;

use fdebm_core::aptmark;
use fdebm_core::batch::{self, LocalMeta};
use fdebm_core::conffile;
use fdebm_core::download::{self, DownloadProgress};
use fdebm_core::health;
use fdebm_core::i18n::{gettext, ngettext, tf};
use fdebm_core::install::{self, CancelHandle, ConfPolicy, FileStamp, InstallBatch};
use fdebm_core::lock;
use fdebm_core::pty::InputHandle;
use fdebm_core::solver::{self, Resolution, Verdict};
use fdebm_core::sysstate::SysState;
use fdebm_core::ui::{AlertKind, InstallProgress, Ui};
use fdebm_core::{DebFile, FdebmError};

/// fdebm-core:deb 安装流程测试 CLI(开发工具,不参与打包)
#[derive(Parser)]
#[command(name = "fdebm-core", version)]
struct Args {
    /// 仅分析求解,打印安装方案,不下载不安装
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// 跳过安装确认,直接执行(脚本化测试)
    #[arg(short = 'y', long)]
    yes: bool,

    /// conffile 冲突保留本地修改(dpkg --force-confold)
    #[arg(long, conflicts_with = "replace")]
    keep: bool,

    /// conffile 冲突采用包内新版本(dpkg --force-confnew)
    #[arg(long)]
    replace: bool,

    /// 透传 apt 索引加载的选项(Debug::* 等),可重复
    #[arg(short = 'o')]
    apt_opt: Vec<String>,

    /// 本地 .deb 文件(可多个,批内按依赖关系排序逐包安装)
    #[arg(required = true, value_name = "DEB")]
    debs: Vec<String>,
}

fn main() {
    fdebm_core::i18n::init();
    let args = Args::parse();
    let conf_policy = if args.keep {
        ConfPolicy::Keep
    } else if args.replace {
        ConfPolicy::Replace
    } else {
        ConfPolicy::Default
    };
    let cancel = Arc::new(CancelHandle::new());
    spawn_signal_watcher(cancel.clone());
    std::process::exit(run(&args, conf_policy, &cancel));
}

/// SIGINT/SIGTERM → 取消句柄:信号处理函数只置原子标志(异步信号安全),
/// 由观察线程调用 cancel()(向运行中的 dpkg 进程组投递 SIGTERM)
static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

fn spawn_signal_watcher(cancel: Arc<CancelHandle>) {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    std::thread::spawn(move || {
        while !SIGNALLED.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        cancel.cancel();
    });
}

fn run(args: &Args, conf_policy: ConfPolicy, cancel: &CancelHandle) -> i32 {
    let items = open_items(&args.debs);
    let mut sys_cache: Option<(SystemTime, SystemTime, Arc<SysState>)> = None;

    // ---- 分析:快照系统状态,逐包求解并打印方案 ----
    let sys = match sys_cached(&mut sys_cache, &args.apt_opt) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fdebm-core: {e}");
            return 1;
        }
    };
    let mut analysis_failed = false;
    let mut unauthenticated: Vec<String> = Vec::new();
    // 系统已有未完成操作时先提示(gdebi 用 apt broken_count 拒绝继续;
    // 这里只警告,用户仍可尝试安装/修复)
    if let Ok(broken) = health::broken_packages()
        && !broken.is_empty()
    {
        eprintln!(
            "{}: {}",
            gettext("Warning: some packages are not fully installed"),
            broken.join(", ")
        );
        eprintln!(
            "    {}",
            gettext("Run 'sudo apt-get install -f' to repair.")
        );
    }
    for item in &items {
        match analyze_item(&sys, item) {
            Ok(res) => unauthenticated.extend(res.changes.unauthenticated.iter().cloned()),
            Err(_) => analysis_failed = true,
        }
    }
    unauthenticated.sort();
    unauthenticated.dedup();

    if args.dry_run {
        return i32::from(analysis_failed);
    }

    // ---- 确认(守护进程路径此处是 Polkit 授权 + GUI 确认框)----
    let ui = CliUi;
    if !unauthenticated.is_empty() {
        let _ = ui.alert(
            AlertKind::Warning,
            &gettext("The following packages cannot be authenticated:"),
            Some(&unauthenticated.join(", ")),
        );
    }
    if !args.yes {
        let prompt = if unauthenticated.is_empty() {
            gettext("Install these packages? [y/N]")
        } else {
            gettext("Install them anyway? [y/N]")
        };
        if !ui.confirm(&prompt, None).unwrap_or(false) {
            println!("{}", gettext("Cancelled."));
            return 0;
        }
    }

    install_phase(args, conf_policy, cancel, &items, &mut sys_cache)
}

/// mtime 缓存的系统状态加载(与守护进程 sys_state 同策略):
/// dpkg status 与 apt 列表目录的 mtime 未变时直接复用,免去重复解析
/// 全部仓库索引(真实索引规模下单次加载约 0.8s)
fn sys_cached(
    cache: &mut Option<(SystemTime, SystemTime, Arc<SysState>)>,
    apt_opts: &[String],
) -> Result<Arc<SysState>, FdebmError> {
    if apt_opts.is_empty() {
        let st_mtime = std::fs::metadata(fdebm_core::sysstate::DPKG_STATUS)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let li_mtime = std::fs::metadata(fdebm_core::sysstate::LISTS_DIR)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some((st, li, state)) = cache
            && *st == st_mtime
            && *li == li_mtime
        {
            return Ok(state.clone());
        }
        let state = Arc::new(SysState::load(apt_opts)?);
        *cache = Some((st_mtime, li_mtime, state.clone()));
        return Ok(state);
    }
    Ok(Arc::new(SysState::load(apt_opts)?))
}

/// 打印单个 deb 的分析方案;可安装时返回其求解结果
fn analyze_item(sys: &SysState, item: &Item) -> Result<Resolution, String> {
    println!("==> {}", item.path_str);
    if let Some(reason) = &item.error {
        eprintln!("    {reason}");
        return Err(reason.clone());
    }
    let deb = item.deb.as_ref().expect("打开失败项必有 error");
    let name = deb.field_trimmed("Package").unwrap_or("").to_string();
    let version = deb.field_trimmed("Version").unwrap_or("").to_string();
    let arch = deb
        .field_trimmed("Architecture")
        .unwrap_or("all")
        .to_string();
    let key = if arch == "all" || arch == sys.native_arch {
        name.clone()
    } else {
        format!("{name}:{arch}")
    };
    let installed = sys
        .installed_version(&key)
        .map(String::from)
        .unwrap_or_else(|| gettext("not installed"));
    println!("    {}: {name} {version} [{arch}]", gettext("Package"));
    println!("    {}: {installed}", gettext("Installed version"));
    if let Some(repo) = sys.candidate_version(&key) {
        println!("    {}: {repo}", gettext("Repository version"));
    }

    let conflicts = conffile::conflicts(deb, sys);
    match solver::resolve(sys, deb) {
        Ok(Verdict::Installable(res)) => {
            println!(
                "    {}: {}",
                gettext("Install"),
                res.changes.install.join(", ")
            );
            if !res.changes.remove.is_empty() {
                println!(
                    "    {}: {}",
                    gettext("Remove"),
                    res.changes.remove.join(", ")
                );
            }
            if !res.downloads.is_empty() {
                let total: u64 = res.downloads.iter().map(|d| d.size).sum();
                println!(
                    "    {}: {} ({})",
                    gettext("Download"),
                    res.downloads.len(),
                    human_size(total)
                );
                for d in &res.downloads {
                    println!("      {} {} [{}]", d.name, d.version, human_size(d.size));
                }
            }
            if !res.changes.unauthenticated.is_empty() {
                println!(
                    "    {}: {}",
                    gettext("Unauthenticated"),
                    res.changes.unauthenticated.join(", ")
                );
            }
            for c in &conflicts {
                println!(
                    "    {}: {} [{}]",
                    gettext("Conffile conflict"),
                    c.path,
                    c.package
                );
            }
            Ok(res)
        }
        Ok(Verdict::Uninstallable(failure)) => {
            eprintln!("    {failure}");
            Err(failure)
        }
        Err(e) => {
            eprintln!("    {e}");
            Err(e.to_string())
        }
    }
}

/// 安装阶段:与守护进程 run_install 同管线(等锁 → 逐包重解 → 下载 →
/// dpkg 批装 → apt 标记);批量一次确认,单包失败不中断其余包。
fn install_phase(
    args: &Args,
    conf_policy: ConfPolicy,
    cancel: &CancelHandle,
    items: &[Item],
    cache: &mut Option<(SystemTime, SystemTime, Arc<SysState>)>,
) -> i32 {
    let input = Arc::new(InputHandle::new());
    let ui = CliUi;
    let progress = CliProgress {
        input: input.clone(),
        reader_started: AtomicBool::new(false),
    };

    let metas: Vec<LocalMeta<'_>> = items
        .iter()
        .map(|it| {
            let field = |name: &str| {
                it.deb
                    .as_ref()
                    .and_then(|d| d.field_trimmed(name))
                    .unwrap_or("")
            };
            LocalMeta {
                name: field("Package"),
                depends: field("Depends"),
                pre_depends: field("Pre-Depends"),
                provides: field("Provides"),
            }
        })
        .collect();
    let order = batch::order_by_dependencies(&metas);

    let total = items.len();
    let mut installed = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let broken_before = health::broken_packages().unwrap_or_default();

    for (step, &idx) in order.iter().enumerate() {
        if cancel.is_cancelled() {
            eprintln!(
                "==> {}: {}",
                gettext("Cancelled"),
                progress_note(total, installed)
            );
            return 1;
        }
        let item = &items[idx];

        if let Some(reason) = &item.error {
            eprintln!("==> {}: {reason}", item.path_str);
            failures.push(format!("{}: {reason}", item.name));
            continue;
        }
        let Some(deb) = item.deb.as_ref() else {
            continue;
        };

        println!(
            "\n==> {}",
            tf(
                "Installing {0} ({1}/{2})",
                &[&item.name, &(step + 1).to_string(), &total.to_string()],
            )
        );

        // 先等外部包管理操作结束再取快照:否则方案可能基于正在被改写的状态
        if let Err(e) = lock::wait_until_idle(&progress, cancel) {
            return flow_aborted(e, total, installed);
        }

        // 每包重读系统状态(mtime 缓存):上一包可能改变了已装集合
        let sys = match sys_cached(cache, &args.apt_opt) {
            Ok(s) => s,
            Err(e) => return flow_aborted(e, total, installed),
        };
        let resolution = match solver::resolve(&sys, deb) {
            Ok(Verdict::Installable(r)) => r,
            Ok(Verdict::Uninstallable(failure)) => {
                eprintln!("    {failure}");
                failures.push(format!("{}: {failure}", item.name));
                continue;
            }
            Err(e) => {
                eprintln!("    {e}");
                failures.push(format!("{}: {e}", item.name));
                continue;
            }
        };

        if !resolution.downloads.is_empty()
            && let Err(e) = download::download_all(&resolution.downloads, &progress, cancel)
        {
            if matches!(e, FdebmError::Cancelled) {
                eprintln!(
                    "==> {}: {}",
                    gettext("Cancelled"),
                    progress_note(total, installed)
                );
                return 1;
            }
            eprintln!("    {e}");
            failures.push(format!("{}: {e}", item.name));
            continue;
        }

        // 本地包的 apt 键:外部架构带 :arch(与守护进程同规则)
        let local_key = {
            let arch = deb.field_trimmed("Architecture").unwrap_or("");
            if arch.is_empty() || arch == "all" || arch == sys.native_arch {
                item.name.clone()
            } else {
                format!("{}:{}", item.name, arch)
            }
        };
        // debconf 模板检测:本地包或任一依赖携带 templates → 交互式(PTY)
        let interactive = deb_has_templates(deb)
            || resolution
                .downloads
                .iter()
                .any(|d| DebFile::open(&d.dest).is_ok_and(|dep| deb_has_templates(&dep)));
        let batch = InstallBatch {
            local: item.path.clone(),
            local_stamp: item.stamp,
            dep_debs: resolution
                .downloads
                .iter()
                .map(|d| d.dest.clone())
                .collect(),
            remove: resolution.changes.remove.clone(),
            conf_policy,
            // 依赖标记对齐 apt:新装依赖标 auto,本地包标 manual
            auto_mark: aptmark::dependency_names(&resolution.changes.install, &local_key)
                .into_iter()
                .filter(|key| sys.installed_version(key).is_none())
                .collect(),
            local_package: local_key,
            interactive,
        };
        match install::install(&ui, &progress, cancel, &input, &batch) {
            Ok(report) if report.code == 0 => {
                installed += 1;
                println!("==> {} {}", gettext("done"), item.path_str);
            }
            Ok(report) => {
                let detail = report.detail();
                eprintln!("    {detail}");
                failures.push(format!("{}: {detail}", item.name));
            }
            Err(e) => return flow_aborted(e, total, installed),
        }
    }

    // 装后健康检查:只报告本次事务新增的异常包(与守护进程同语义)
    let broken_after = health::broken_packages().unwrap_or_default();
    let new_broken: Vec<&str> = broken_after
        .iter()
        .filter(|p| !broken_before.contains(p))
        .map(String::as_str)
        .collect();
    if !new_broken.is_empty() {
        failures.push(tf(
            "The installation left the system in an inconsistent state: {0}. Run 'sudo apt-get install -f' to repair.",
            &[&new_broken.join(", ")],
        ));
    }

    if failures.is_empty() {
        println!("==> {}", gettext("All packages installed."));
        return 0;
    }
    eprintln!(
        "==> {} ({installed}/{total}):",
        gettext("The following packages could not be installed")
    );
    for f in &failures {
        eprintln!("    {f}");
    }
    1
}

/// 批量中止/取消时的进度摘要(与守护进程同 msgid)
fn progress_note(total: usize, installed: usize) -> String {
    tf(
        &ngettext(
            "Installed {0} of {1} package",
            "Installed {0} of {1} packages",
            total as u32,
        ),
        &[&installed.to_string(), &total.to_string()],
    )
}

/// 整批中止(等锁/系统状态加载失败或取消):打印摘要,返回退出码 1
fn flow_aborted(e: FdebmError, total: usize, installed: usize) -> i32 {
    match e {
        FdebmError::Cancelled => {
            eprintln!(
                "==> {}: {}",
                gettext("Cancelled"),
                progress_note(total, installed)
            );
        }
        e => eprintln!("fdebm-core: {e}"),
    }
    1
}

/// 批量中的一个本地 deb(与守护进程 BatchItem 同构;打开失败项也保留)
struct Item {
    /// 原样路径字符串
    path_str: String,
    path: PathBuf,
    /// 包名(取不到 Package 字段时退回文件名)
    name: String,
    /// 打开时捕获的指纹(dpkg 执行前复核)
    stamp: Option<FileStamp>,
    deb: Option<DebFile>,
    error: Option<String>,
}

fn open_items(paths: &[String]) -> Vec<Item> {
    paths
        .iter()
        .map(|p| {
            let path = PathBuf::from(p);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone());
            let mut item = Item {
                path_str: p.clone(),
                stamp: FileStamp::capture(&path).ok(),
                path,
                name,
                deb: None,
                error: None,
            };
            // 目录/FIFO/设备等非普通文件:open 会阻塞或无意义,直接拒绝
            if std::fs::metadata(&item.path).is_ok_and(|m| !m.is_file()) {
                item.error = Some(gettext("not a regular file"));
                return item;
            }
            match DebFile::open(&item.path) {
                Ok(deb) => {
                    if let Some(name) = deb.field_trimmed("Package") {
                        item.name = name.to_string();
                    }
                    item.deb = Some(deb);
                }
                Err(e) => item.error = Some(e.to_string()),
            }
            item
        })
        .collect()
}

/// control.tar 携带 debconf templates → 安装时需要交互应答(与守护进程同判)
fn deb_has_templates(deb: &DebFile) -> bool {
    deb.control_content("templates")
        .is_ok_and(|t| !t.is_empty())
}

/// 字节数 → 可读大小
fn human_size(size: u64) -> String {
    const KIB: f64 = 1024.0;
    let s = size as f64;
    if s >= KIB * KIB {
        format!("{:.1} MiB", s / (KIB * KIB))
    } else if s >= KIB {
        format!("{:.1} KiB", s / KIB)
    } else {
        format!("{size} B")
    }
}

/// 终端 UI:告警到 stderr;确认经 stdin 读取 y/N。
/// 只在安装开始前使用,与交互应答读取线程(见 CliProgress::input_state)
/// 不会同时读 stdin。
struct CliUi;

impl Ui for CliUi {
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), FdebmError> {
        let tag = match kind {
            AlertKind::Error => "error",
            AlertKind::Warning => "warning",
            AlertKind::Info => "info",
        };
        match body {
            Some(body) => eprintln!("{tag}: {header} {body}"),
            None => eprintln!("{tag}: {header}"),
        }
        Ok(())
    }

    fn confirm(&self, header: &str, _body: Option<&str>) -> Result<bool, FdebmError> {
        print!("{header} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let ok = std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0)
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        Ok(ok)
    }
}

/// 终端进度:dpkg 输出逐行打印;debconf 交互式安装开始时启动 stdin
/// 读取线程,把键入的行注入 PTY(等效 GUI 的应答输入框)
struct CliProgress {
    input: Arc<InputHandle>,
    reader_started: AtomicBool,
}

impl InstallProgress for CliProgress {
    fn output_line(&self, line: &str) {
        println!("{line}");
    }

    fn stage(&self, desc: &str) {
        println!("==> {desc}");
    }

    fn input_state(&self, enabled: bool) {
        if !enabled || self.reader_started.swap(true, Ordering::SeqCst) {
            return;
        }
        println!(
            "{}",
            gettext("Interactive install: type answers here and press Enter.")
        );
        let input = self.input.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(line) => {
                        input.send(&line);
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

impl DownloadProgress for CliProgress {
    fn item_start(&self, name: &str, idx: usize, total: usize) {
        println!(
            "==> {} [{name}]",
            tf(
                "Fetching file {0} of {1}",
                &[&idx.to_string(), &total.to_string()]
            )
        );
    }

    fn item_progress(&self, name: &str, received: u64, size: u64) {
        // size 未知(0)或溢出时退化为字节数显示
        match received.checked_mul(100).and_then(|v| v.checked_div(size)) {
            Some(percent) => print!("\r    {name}: {percent}%"),
            None => print!("\r    {name}: {received}"),
        }
        let _ = std::io::stdout().flush();
    }

    fn item_done(&self, name: &str) {
        println!("\r    {name}: {}", gettext("done"));
    }
}
