//! 事务管理器:状态机 + 工作线程 + 信号发射。
//!
//! 状态机(对应 readme:分析中→等待授权→下载中→安装中→完成/失败):
//! analyzing → waiting-auth → downloading → installing → done | failed | cancelled
//! 安装/卸载各由一个独立的 apt worker 进程执行(PTY 交互 + fd 3 事件流),
//! daemon 只做授权、文件校验、事件转发与终态裁定。
//! 信号契约(org.fdebi):
//! - TransactionEvent(u32 id, String state)
//! - OutputLine(u32 id, String line)
//! - ProgressChanged(u32 id, f64 percent /*-1=pulse*/, String description)
//! - PackageStatus(u32 id, String path, String name, String state, String detail)
//!   state: installing | done | failed
//! - InputState(u32 id, bool enabled):交互式安装可应答开关
//! - Finished(u32 id, bool ok, String detail)

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use fdebi_apt_proto::{
    Change, ConfPolicy as WorkerConfPolicy, Event, Item, ItemResult, Mode, PackageState, Phase,
    Request, Stamp,
};
use fdebi_core::health;
use fdebi_core::i18n::{gettext, ngettext, tf};
use fdebi_core::install::{CancelHandle, ConfPolicy};
use fdebi_core::lock;
use fdebi_core::reverse;
use fdebi_core::sysstate::SysState;
use fdebi_core::ui::InstallProgress;
use fdebi_core::{DebFile, FdebiError};

use crate::polkit;
use crate::worker::{self, InputHandle, Msg};

pub const ACTIONS: &str = "org.fdebi";

pub const ANALYZING: &str = "analyzing";
pub const WAITING_AUTH: &str = "waiting-auth";
pub const DOWNLOADING: &str = "downloading";
pub const INSTALLING: &str = "installing";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";
pub const CANCELLED: &str = "cancelled";

/// 事务表上限(含运行中):超出后丢弃最早的事务记录
const MAX_TXNS: usize = 64;

fn is_terminal(state: &str) -> bool {
    matches!(state, DONE | FAILED | CANCELLED)
}

struct Txn {
    state: Mutex<(String, String)>,
    cancel: Arc<CancelHandle>,
    /// 交互式安装的应答入口(仅事务发起者写入)
    input: Arc<InputHandle>,
    /// 发起者总线唯一名:SendInput 据此校验归属
    caller: String,
}

struct Inner {
    next_id: AtomicU32,
    txns: Mutex<HashMap<u32, Arc<Txn>>>,
}

pub struct Manager {
    inner: Inner,
    /// 安装/卸载串行化:两个 worker 同时运行会争抢 apt/dpkg 全局锁
    install_gate: Mutex<()>,
    /// SysState 缓存:(status mtime, lists mtime, state);apt_opts 非空时绕过
    sys_cache: Mutex<Option<(SystemTime, SystemTime, Arc<SysState>)>>,
}

impl Manager {
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            inner: Inner {
                next_id: AtomicU32::new(1),
                txns: Mutex::new(HashMap::new()),
            },
            install_gate: Mutex::new(()),
            sys_cache: Mutex::new(None),
        })
    }

    fn alloc(&self, cancel: Arc<CancelHandle>, caller: String) -> u32 {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let mut txns = self.inner.txns.lock().unwrap();
        // 常驻守护进程:优先回收已结束的事务,避免表随运行时长无限增长;
        // 仍在执行的事务不能回收(取消/应答/状态查询还要用它)
        if txns.len() >= MAX_TXNS
            && let Some((&oldest, _)) = txns
                .iter()
                .filter(|(_, t)| is_terminal(&t.state.lock().unwrap().0))
                .min_by_key(|(id, _)| **id)
        {
            txns.remove(&oldest);
        }
        txns.insert(
            id,
            Arc::new(Txn {
                state: Mutex::new((ANALYZING.to_string(), String::new())),
                cancel,
                input: Arc::new(InputHandle::new()),
                caller,
            }),
        );
        id
    }

    /// 事务状态:仅事务发起者可见(对他人表现为不存在)
    pub fn status(&self, id: u32, caller: &str) -> Option<(String, String)> {
        let txns = self.inner.txns.lock().unwrap();
        let txn = txns.get(&id)?;
        (txn.caller == caller).then(|| txn.state.lock().unwrap().clone())
    }

    /// 取消事务:仅事务发起者可取消
    pub fn cancel(&self, id: u32, caller: &str) -> bool {
        let txns = self.inner.txns.lock().unwrap();
        match txns.get(&id) {
            Some(txn) if txn.caller == caller => {
                txn.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// 交互应答:仅事务发起者可写,且当前确有交互式 dpkg 在等待输入
    pub fn send_input(&self, id: u32, caller: &str, line: &str) -> bool {
        let txns = self.inner.txns.lock().unwrap();
        match txns.get(&id) {
            Some(txn) if txn.caller == caller => txn.input.send(line),
            _ => false,
        }
    }

    /// 系统状态缓存:dpkg/status 与 lists 目录 mtime 不变时复用
    fn sys_state(&self, apt_opts: &[String]) -> Result<Arc<SysState>, FdebiError> {
        if apt_opts.is_empty() {
            let st_mtime = std::fs::metadata(fdebi_core::sysstate::DPKG_STATUS)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let li_mtime = std::fs::metadata(fdebi_core::sysstate::LISTS_DIR)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let mut cache = self.sys_cache.lock().unwrap();
            if let Some((st, li, state)) = cache.as_ref()
                && *st == st_mtime
                && *li == li_mtime
            {
                return Ok(state.clone());
            }
            let state = Arc::new(SysState::load(apt_opts)?);
            *cache = Some((st_mtime, li_mtime, state.clone()));
            Ok(state)
        } else {
            Ok(Arc::new(SysState::load(apt_opts)?))
        }
    }
}

// ---------------------------------------------------------------------------
// 信号发射
// ---------------------------------------------------------------------------

/// 统一信号发射:路径/接口固定为 /org/fdebi @ org.fdebi
fn emit<T>(conn: &zbus::Connection, signal: &str, body: &T)
where
    T: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    let path = zbus::zvariant::ObjectPath::try_from("/org/fdebi").unwrap();
    let iface = zbus::names::InterfaceName::try_from(ACTIONS).unwrap();
    let _ = async_io::block_on(conn.emit_signal(None::<&str>, &path, &iface, signal, body));
}

fn emit_event(conn: &zbus::Connection, id: u32, state: &str) {
    emit(conn, "TransactionEvent", &(id, state.to_string()));
}

fn emit_finished(conn: &zbus::Connection, id: u32, ok: bool, detail: &str) {
    emit(conn, "Finished", &(id, ok, detail.to_string()));
}

fn emit_progress(conn: &zbus::Connection, id: u32, percent: f64, desc: &str) {
    emit(conn, "ProgressChanged", &(id, percent, desc.to_string()));
}

fn emit_package(conn: &zbus::Connection, id: u32, path: &str, name: &str, state: &str, detail: &str) {
    emit(
        conn,
        "PackageStatus",
        &(id, path.to_string(), name.to_string(), state.to_string(), detail.to_string()),
    );
}

fn emit_input_state(conn: &zbus::Connection, id: u32, enabled: bool) {
    emit(conn, "InputState", &(id, enabled));
}

/// 更新事务状态并广播 TransactionEvent
fn set_state(conn: &zbus::Connection, txn: &Txn, id: u32, state: &str) {
    *txn.state.lock().unwrap() = (state.to_string(), String::new());
    emit_event(conn, id, state);
}

/// 终态落账 + Finished 信号
fn finalize(conn: &zbus::Connection, txn: &Txn, state: &str, detail: &str, id: u32, ok: bool) {
    *txn.state.lock().unwrap() = (state.to_string(), detail.to_string());
    emit_event(conn, id, state);
    emit_finished(conn, id, ok, detail);
}

// ---------------------------------------------------------------------------
// 事务启动(D-Bus 方法调用入口)
// ---------------------------------------------------------------------------

/// 一次安装事务的请求参数(收拢为结构体,避免 run_install 参数过长)
struct InstallRequest {
    /// 本地 deb 路径(可多个)
    paths: Vec<String>,
    apt_opts: Vec<String>,
    conf_policy: ConfPolicy,
    caller: String,
}

pub fn start_install(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    paths: Vec<String>,
    apt_opts: Vec<String>,
    conf_policy: String,
    caller: String,
) -> u32 {
    let cancel = Arc::new(CancelHandle::new());
    let id = manager.alloc(cancel.clone(), caller.clone());
    let m = manager.clone();
    let conn = conn.clone();
    let req = InstallRequest {
        paths,
        apt_opts,
        conf_policy: ConfPolicy::from_arg(&conf_policy),
        caller,
    };
    std::thread::spawn(move || {
        let txn = m.inner.txns.lock().unwrap().get(&id).cloned();
        let Some(txn) = txn else { return };
        run_install(&m, &conn, &txn, id, &req);
    });
    id
}

pub fn start_remove(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    name: String,
    caller: String,
) -> u32 {
    let cancel = Arc::new(CancelHandle::new());
    let id = manager.alloc(cancel.clone(), caller.clone());
    let m = manager.clone();
    let conn = conn.clone();
    std::thread::spawn(move || {
        let txn = m.inner.txns.lock().unwrap().get(&id).cloned();
        let Some(txn) = txn else { return };
        run_remove(&m, &conn, &txn, id, &name, &caller);
    });
    id
}

/// 打开一个待分析/安装的本地文件:拒绝 FIFO/设备等非普通文件,避免被诱导
/// 在打开时阻塞;其余打开错误转成用户可见文案。
fn open_item(path: &str) -> Result<DebFile, String> {
    if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(gettext("not a regular file"));
    }
    DebFile::open(path).map_err(|e| e.to_string())
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// deb 的 apt 键(键派生规则与 solver/conffile 一致)
fn deb_key(sys: &SysState, deb: &DebFile) -> String {
    let name = deb.field_trimmed("Package").unwrap_or("");
    let arch = deb.field_trimmed("Architecture").unwrap_or("all");
    if arch == "all" || arch == sys.native_arch {
        name.to_string()
    } else {
        format!("{name}:{arch}")
    }
}

/// deb 的已安装版本(未安装为空串)
fn installed_version_of(sys: &SysState, deb: &DebFile) -> String {
    sys.installed_version(&deb_key(sys, deb))
        .unwrap_or_default()
        .to_string()
}

/// deb 在软件源中的最高候选版本(源中没有该包为空串)
fn repo_version_of(sys: &SysState, deb: &DebFile) -> String {
    sys.candidate_version(&deb_key(sys, deb))
        .unwrap_or_default()
        .to_string()
}

fn conffile_conflicts_of(sys: &SysState, deb: &DebFile) -> Vec<(String, String)> {
    fdebi_core::conffile::conflicts(deb, sys)
        .into_iter()
        .map(|c| (c.package, c.path))
        .collect()
}

/// 变更计划中来自未信任仓库的同版本包(apt 需要认证确认)
fn unauthenticated_for(sys: &SysState, changes: &[Change]) -> Vec<String> {
    let mut out = Vec::new();
    for change in changes {
        let Some(indices) = sys.candidates.get(&change.name) else {
            continue;
        };
        let untrusted = indices.iter().any(|&i| {
            sys.all_candidates.get(i).is_some_and(|c| {
                c.pkg.version == change.new_version
                    && c.repo.as_ref().is_some_and(|repo| !repo.trusted)
            })
        });
        if untrusted {
            out.push(change.name.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// worker 的逐项分析结果 + 本地系统状态 → D-Bus 分析结果
fn result_from_worker(
    sys: &SysState,
    deb: &DebFile,
    item: &ItemResult,
    broken: &[String],
    apt_stale: bool,
) -> fdebi_core::AnalyzeResult {
    let installed_version = installed_version_of(sys, deb);
    let repo_version = repo_version_of(sys, deb);
    let conffile_conflicts = conffile_conflicts_of(sys, deb);
    if !item.installable {
        return (
            false,
            item.failure.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            installed_version,
            repo_version,
            broken.to_vec(),
            conffile_conflicts,
            apt_stale,
        );
    }
    let install: Vec<(String, String, String, String)> = item
        .install
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                c.kind.as_str().to_string(),
                c.old_version.clone(),
                c.new_version.clone(),
            )
        })
        .collect();
    let unauthenticated = unauthenticated_for(sys, &item.install);
    (
        true,
        String::new(),
        install,
        item.remove.clone(),
        unauthenticated,
        installed_version,
        repo_version,
        broken.to_vec(),
        conffile_conflicts,
        apt_stale,
    )
}

/// 系统异常包快照;查询失败不阻断分析(按无异常处理)
fn broken_snapshot() -> Vec<String> {
    health::broken_packages().unwrap_or_default()
}

fn worker_conf_policy(policy: ConfPolicy) -> WorkerConfPolicy {
    match policy {
        ConfPolicy::Default => WorkerConfPolicy::Default,
        ConfPolicy::Keep => WorkerConfPolicy::Keep,
        ConfPolicy::Replace => WorkerConfPolicy::Replace,
    }
}

/// 让 worker 完成一次只读分析;返回与请求顺序一致的结果。
fn worker_analyze(paths: &[String], apt_opts: &[String]) -> Result<Vec<ItemResult>, String> {
    let req = Request {
        items: paths
            .iter()
            .map(|path| Item {
                path: path.clone(),
                stamp: None,
            })
            .collect(),
        apt_opts: apt_opts.to_vec(),
        conf_policy: WorkerConfPolicy::Default,
        remove_name: None,
    };
    let cancel = CancelHandle::new();
    let input = Arc::new(InputHandle::new());
    let worker = worker::spawn(Mode::Analyze, &req, &cancel, &input)?;
    let mut result: Option<(bool, String, Vec<ItemResult>)> = None;
    for msg in worker.rx.iter() {
        if let Msg::Event(Event::Result { ok, detail, items }) = msg {
            result = Some((ok, detail, items));
            break;
        }
    }
    let status = worker
        .finish(&input)
        .map_err(|e| format!("apt worker: {e}"))?;
    match result {
        Some((true, _, items)) => Ok(items),
        Some((false, detail, _)) => Err(if detail.is_empty() {
            gettext("analysis failed")
        } else {
            detail
        }),
        None => Err(tf(
            "apt worker exited with {0}",
            &[&status.to_string()],
        )),
    }
}

/// 只读分析(D-Bus 方法内同步执行,重活放独立线程避免阻塞总线任务)
pub fn analyze(
    manager: &Arc<Manager>,
    path: &str,
    apt_opts: &[String],
) -> Result<fdebi_core::AnalyzeResult, String> {
    // 保留旧错误语义:文件无法打开或系统状态加载失败时返回 D-Bus 错误
    let deb = open_item(path)?;
    let sys = manager.sys_state(apt_opts).map_err(|e| e.to_string())?;
    let broken = broken_snapshot();
    let apt_stale = fdebi_core::sysstate::lists_stale(apt_opts);
    let items = worker_analyze(&[path.to_string()], apt_opts)?;
    let item = items
        .first()
        .ok_or_else(|| gettext("internal error: missing analysis result"))?;
    Ok(result_from_worker(&sys, &deb, item, &broken, apt_stale))
}

/// 卸载前反向依赖预检:返回 [(受影响包, 依赖组)];只读,无需授权
pub fn check_removal(manager: &Arc<Manager>, name: &str) -> Result<Vec<(String, String)>, String> {
    let sys = manager.sys_state(&[]).map_err(|e| e.to_string())?;
    Ok(reverse::reverse_dependents(&sys, &[name.to_string()])
        .into_iter()
        .map(|r| (r.package, r.requires))
        .collect())
}

/// 批量只读分析:一次加载系统状态,由 apt worker 给出每项的安装计划,
/// 再用本地 SysState 补充已安装/候选/认证/异常/conffile 信息。
///
/// 返回值与 `paths` 一一对应;单个文件打开失败只影响该项。
pub fn analyze_batch(
    manager: &Arc<Manager>,
    paths: &[String],
    apt_opts: &[String],
) -> Result<Vec<fdebi_core::AnalyzeItem>, String> {
    let sys = manager.sys_state(apt_opts).map_err(|e| e.to_string())?;
    let broken = broken_snapshot();
    let apt_stale = fdebi_core::sysstate::lists_stale(apt_opts);

    // 先打开全部文件:打开失败逐项报告,不阻塞其余包的批内分析
    let opened: Vec<Result<DebFile, String>> = paths.iter().map(|p| open_item(p)).collect();
    let valid: Vec<(usize, &String)> = paths
        .iter()
        .enumerate()
        .filter(|(i, _)| opened[*i].is_ok())
        .collect();
    let valid_paths: Vec<String> = valid.iter().map(|(_, p)| (*p).clone()).collect();

    let worker_result = if valid_paths.is_empty() {
        Ok(Vec::new())
    } else {
        worker_analyze(&valid_paths, apt_opts)
    };
    // worker 整体失败时,所有打开成功的项共用同一错误
    let mut items = match worker_result {
        Ok(items) => items.into_iter().map(Some).collect::<Vec<_>>(),
        Err(e) => valid_paths.iter().map(|_| Some(ItemResult::failed("", e.clone()))).collect(),
    }
    .into_iter();

    let mut out: Vec<fdebi_core::AnalyzeItem> = Vec::with_capacity(paths.len());
    for (path, opened) in paths.iter().zip(opened.iter()) {
        let item = match opened {
            Err(e) => (
                path.clone(),
                false,
                e.clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                String::new(),
                String::new(),
                broken.clone(),
                Vec::new(),
                apt_stale,
            ),
            Ok(deb) => match items.next().flatten() {
                Some(worker_item) => {
                    let (installable, failure, install, remove, unauthenticated, installed_version,
                        repo_version, item_broken, conflicts, item_apt_stale) =
                        result_from_worker(&sys, deb, &worker_item, &broken, apt_stale);
                    (
                        path.clone(),
                        installable,
                        failure,
                        install,
                        remove,
                        unauthenticated,
                        installed_version,
                        repo_version,
                        item_broken,
                        conflicts,
                        item_apt_stale,
                    )
                }
                None => (
                    path.clone(),
                    false,
                    gettext("internal error: missing analysis result"),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    installed_version_of(&sys, deb),
                    repo_version_of(&sys, deb),
                    broken.clone(),
                    conffile_conflicts_of(&sys, deb),
                    apt_stale,
                ),
            },
        };
        out.push(item);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 工作线程流程
// ---------------------------------------------------------------------------

/// 安装前解析成功的一个本地 deb
struct Prepared {
    path: String,
    /// 打开时捕获的指纹(dpkg 执行前复核)
    stamp: Option<Stamp>,
}

/// 取消时的进度摘要
fn installed_summary(installed: usize, total: usize) -> String {
    tf(
        &ngettext(
            "Installed {0} of {1} package",
            "Installed {0} of {1} packages",
            total as u32,
        ),
        &[&installed.to_string(), &total.to_string()],
    )
}

/// 运行 worker 并把事件转成 D-Bus 信号;返回终态 Result 与已完成包数。
fn pump_events(
    conn: &zbus::Connection,
    txn: &Txn,
    id: u32,
    rx: &Receiver<Msg>,
) -> (Option<(bool, String)>, usize) {
    let mut result = None;
    let mut done = 0usize;
    for msg in rx.iter() {
        match msg {
            Msg::Output(line) => emit(conn, "OutputLine", &(id, line)),
            Msg::Event(Event::Stage { phase, desc }) => {
                let state = match phase {
                    Phase::Analyzing => ANALYZING,
                    Phase::Downloading => DOWNLOADING,
                    Phase::Installing | Phase::Removing => INSTALLING,
                };
                set_state(conn, txn, id, state);
                emit_progress(conn, id, -1.0, &desc);
            }
            Msg::Event(Event::Progress { percent, desc }) => {
                emit_progress(conn, id, percent, &desc);
            }
            Msg::Event(Event::Package {
                path,
                name,
                state,
                detail,
            }) => {
                let state = match state {
                    PackageState::Installing => "installing",
                    PackageState::Done => "done",
                    PackageState::Failed => "failed",
                };
                if state == "done" {
                    done += 1;
                }
                emit_package(conn, id, &path, &name, state, &detail);
            }
            Msg::Event(Event::Output { line }) => emit(conn, "OutputLine", &(id, line)),
            Msg::Event(Event::Result { ok, detail, .. }) => result = Some((ok, detail)),
        }
    }
    (result, done)
}

fn run_install(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    txn: &Arc<Txn>,
    id: u32,
    req: &InstallRequest,
) {
    let InstallRequest {
        paths,
        apt_opts,
        conf_policy,
        caller,
    } = req;
    let total = paths.len();
    emit(conn, "TransactionEvent", &(id, ANALYZING.to_string()));

    // 1. Polkit 授权(整批一次)。必须先于打开文件:未授权调用者不得让
    //    root 守护进程读取任意路径(FIFO 会阻塞 open)或承担解析开销。
    *txn.state.lock().unwrap() = (WAITING_AUTH.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, WAITING_AUTH.to_string()));
    let auth = async_io::block_on(polkit::check_authorized(
        conn,
        caller,
        "org.fdebi.install-local-package",
    ));
    match auth {
        Ok(true) => {}
        Ok(false) => {
            finalize(conn, txn, FAILED, &gettext("Not authorized"), id, false);
            return;
        }
        Err(e) => {
            finalize(
                conn,
                txn,
                FAILED,
                &tf("polkit: {0}", &[&e.to_string()]),
                id,
                false,
            );
            return;
        }
    }

    // 授权后响应取消(用户在认证窗口等待期间已点关闭)
    if txn.cancel.is_cancelled() {
        finalize(
            conn,
            txn,
            CANCELLED,
            &installed_summary(0, total),
            id,
            false,
        );
        return;
    }

    // 2. 等外部包管理操作结束:否则方案可能基于正在被改写的状态
    let progress = TxnProgress {
        id,
        conn: conn.clone(),
    };
    if let Err(e) = lock::wait_until_idle(&progress, &txn.cancel) {
        if matches!(e, FdebiError::Cancelled) {
            finalize(
                conn,
                txn,
                CANCELLED,
                &installed_summary(0, total),
                id,
                false,
            );
        } else {
            finalize(conn, txn, FAILED, &e.to_string(), id, false);
        }
        return;
    }

    // 3. 打开全部本地 deb 并捕获指纹;失败项逐包上报,不阻塞其余包
    let mut iterator = paths.iter();
    let mut prepared: Vec<Prepared> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    while let Some(path) = iterator.next() {
        match open_item(path) {
            Ok(_) => {
                prepared.push(Prepared {
                    path: path.clone(),
                    stamp: Stamp::capture(Path::new(path)).ok(),
                });
            }
            Err(e) => {
                emit_package(conn, id, path, &file_name_of(path), "failed", &e);
                failures.push(format!("{}: {e}", file_name_of(path)));
            }
        }
    }

    if prepared.is_empty() {
        finalize(
            conn,
            txn,
            FAILED,
            &if failures.is_empty() {
                gettext("no installable local package")
            } else {
                failures.join("\n")
            },
            id,
            false,
        );
        return;
    }

    // 4. 串行化并启动 worker;PTY 承载 debconf/dpkg 交互
    let _gate = manager.install_gate.lock().unwrap();
    if txn.cancel.is_cancelled() {
        finalize(
            conn,
            txn,
            CANCELLED,
            &installed_summary(0, total),
            id,
            false,
        );
        return;
    }

    let worker_req = Request {
        items: prepared
            .iter()
            .map(|item| Item {
                path: item.path.clone(),
                stamp: item.stamp,
            })
            .collect(),
        apt_opts: apt_opts.clone(),
        conf_policy: worker_conf_policy(*conf_policy),
        remove_name: None,
    };
    // 安装前健康快照:装后只报告本次事务新增的异常包
    let broken_before = broken_snapshot();
    let worker = match worker::spawn(Mode::Install, &worker_req, &txn.cancel, &txn.input) {
        Ok(worker) => worker,
        Err(e) => {
            finalize(conn, txn, FAILED, &e, id, false);
            return;
        }
    };
    emit_input_state(conn, id, true);
    let (result, done) = pump_events(conn, txn, id, &worker.rx);
    emit_input_state(conn, id, false);
    let status = worker.finish(&txn.input);
    txn.cancel.unregister();

    // 5. 终态裁定
    if txn.cancel.is_cancelled() {
        finalize(
            conn,
            txn,
            CANCELLED,
            &installed_summary(done, total),
            id,
            false,
        );
        return;
    }
    match result {
        None => {
            let detail = match status {
                Ok(st) => tf("apt worker exited with {0}", &[&st.to_string()]),
                Err(e) => format!("apt worker: {e}"),
            };
            failures.push(detail.clone());
            finalize(conn, txn, FAILED, &failures.join("\n"), id, false);
        }
        Some((false, detail)) => {
            if !detail.is_empty() {
                failures.push(detail);
            }
            finalize(conn, txn, FAILED, &failures.join("\n"), id, false);
        }
        Some((true, _)) => {
            // 装后健康检查:dpkg 退出码为 0 也可能留下未配置的依赖方
            // (例如 Provides 丢失或 Postinst 依赖未满足)。只报告新增问题。
            let broken_after = broken_snapshot();
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
                finalize(conn, txn, DONE, "", id, true);
            } else {
                finalize(conn, txn, FAILED, &failures.join("\n"), id, false);
            }
        }
    }
}

fn run_remove(
    manager: &Arc<Manager>,
    conn: &zbus::Connection,
    txn: &Arc<Txn>,
    id: u32,
    name: &str,
    caller: &str,
) {
    *txn.state.lock().unwrap() = (WAITING_AUTH.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, WAITING_AUTH.to_string()));
    let auth = async_io::block_on(polkit::check_authorized(
        conn,
        caller,
        "org.fdebi.remove-package",
    ));
    match auth {
        Ok(true) => {}
        Ok(false) => {
            finalize(conn, txn, FAILED, &gettext("Not authorized"), id, false);
            return;
        }
        Err(e) => {
            finalize(
                conn,
                txn,
                FAILED,
                &tf("polkit: {0}", &[&e.to_string()]),
                id,
                false,
            );
            return;
        }
    }

    let _gate = manager.install_gate.lock().unwrap();
    if txn.cancel.is_cancelled() {
        finalize(conn, txn, CANCELLED, "", id, false);
        return;
    }
    set_state(conn, txn, id, INSTALLING);

    let worker_req = Request {
        items: Vec::new(),
        apt_opts: Vec::new(),
        conf_policy: WorkerConfPolicy::Default,
        remove_name: Some(name.to_string()),
    };
    let worker = match worker::spawn(Mode::Remove, &worker_req, &txn.cancel, &txn.input) {
        Ok(worker) => worker,
        Err(e) => {
            finalize(conn, txn, FAILED, &e, id, false);
            return;
        }
    };
    emit_input_state(conn, id, true);
    let (result, _) = pump_events(conn, txn, id, &worker.rx);
    emit_input_state(conn, id, false);
    let status = worker.finish(&txn.input);
    txn.cancel.unregister();

    if txn.cancel.is_cancelled() {
        finalize(conn, txn, CANCELLED, "", id, false);
        return;
    }
    match result {
        None => {
            let detail = match status {
                Ok(st) => tf("apt worker exited with {0}", &[&st.to_string()]),
                Err(e) => format!("apt worker: {e}"),
            };
            finalize(conn, txn, FAILED, &detail, id, false);
        }
        Some((false, detail)) => finalize(conn, txn, FAILED, &detail, id, false),
        Some((true, _)) => finalize(conn, txn, DONE, "", id, true),
    }
}

// ---------------------------------------------------------------------------
// 进度适配器:core 等待回调 → D-Bus 信号
// ---------------------------------------------------------------------------

struct TxnProgress {
    id: u32,
    conn: zbus::Connection,
}

impl InstallProgress for TxnProgress {
    fn output_line(&self, line: &str) {
        emit(&self.conn, "OutputLine", &(self.id, line.to_string()));
    }

    fn stage(&self, desc: &str) {
        emit_progress(&self.conn, self.id, -1.0, desc);
    }

    fn input_state(&self, enabled: bool) {
        emit_input_state(&self.conn, self.id, enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_initiator_can_cancel_or_query() {
        let m = Manager::new_shared();
        let id = m.alloc(Arc::new(CancelHandle::new()), ":1.42".to_string());
        assert_eq!(
            m.status(id, ":1.42"),
            Some((ANALYZING.to_string(), String::new()))
        );
        assert_eq!(m.status(id, ":1.43"), None);
        assert!(!m.cancel(id, ":1.43"));
        assert!(m.cancel(id, ":1.42"));
    }

    #[test]
    fn prune_only_recycles_finished_transactions() {
        let m = Manager::new_shared();
        let ids: Vec<u32> = (0..MAX_TXNS)
            .map(|_| m.alloc(Arc::new(CancelHandle::new()), ":1.1".to_string()))
            .collect();
        // 全部仍在执行:表满时新增事务也不得回收进行中的事务
        let _ = m.alloc(Arc::new(CancelHandle::new()), ":1.1".to_string());
        assert!(m.status(ids[0], ":1.1").is_some());
        // 结束最老事务后,再分配时才回收它(且只回收已结束的)
        *m.inner
            .txns
            .lock()
            .unwrap()
            .get(&ids[0])
            .unwrap()
            .state
            .lock()
            .unwrap() = (DONE.to_string(), String::new());
        let _ = m.alloc(Arc::new(CancelHandle::new()), ":1.1".to_string());
        assert!(m.status(ids[0], ":1.1").is_none());
        assert!(m.status(ids[1], ":1.1").is_some());
    }
}
