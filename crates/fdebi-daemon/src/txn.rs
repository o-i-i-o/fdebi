//! 事务管理器:状态机 + 工作线程 + 信号发射。
//!
//! 状态机(对应 readme:分析中→等待授权→下载中→安装中→完成/失败):
//! analyzing → waiting-auth → downloading → installing → done | failed | cancelled
//! 批量安装:授权一次,逐包求解/下载/安装;单包失败不中断其余包,
//! 每包状态经 PackageStatus 上报。
//! 信号契约(org.fdebi):
//! - TransactionEvent(u32 id, String state)
//! - OutputLine(u32 id, String line)
//! - ProgressChanged(u32 id, f64 percent /*-1=pulse*/, String description)
//! - PackageStatus(u32 id, String path, String name, String state, String detail)
//!   state: installing | done | failed
//! - InputState(u32 id, bool enabled):交互式安装可应答开关
//! - Finished(u32 id, bool ok, String detail)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use fdebi_core::aptmark;
use fdebi_core::batch::{self, LocalMeta};
use fdebi_core::download::{self, DownloadProgress};
use fdebi_core::health;
use fdebi_core::i18n::{gettext, ngettext, tf};
use fdebi_core::install::{self, CancelHandle, ConfPolicy, FileStamp, InstallBatch};
use fdebi_core::lock;
use fdebi_core::pty::InputHandle;
use fdebi_core::solver::{self, Verdict};
use fdebi_core::sysstate::SysState;
use fdebi_core::ui::{AlertKind, InstallProgress, Ui};
use fdebi_core::{DebFile, FdebiError};

use crate::polkit;

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

/// 批量逐包状态:path 用于前端行匹配;detail 仅 failed 时非空
fn emit_package(conn: &zbus::Connection, id: u32, item: &BatchItem, state: &str, detail: &str) {
    emit(
        conn,
        "PackageStatus",
        &(
            id,
            item.path_str.clone(),
            item.name.clone(),
            state.to_string(),
            detail.to_string(),
        ),
    );
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
    /// 本地 deb 路径(可多个;按批内依赖排序后逐包执行)
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

/// 求解结论 → D-Bus 分析结果(批量路径直接复用 resolve_batch 的结论)
fn result_from_verdict(
    sys: &SysState,
    deb: &DebFile,
    verdict: Verdict,
    broken: &[String],
    apt_stale: bool,
) -> fdebi_core::AnalyzeResult {
    let installed_version = installed_version_of(sys, deb);
    let repo_version = repo_version_of(sys, deb);
    let conffile_conflicts = conffile_conflicts_of(sys, deb);
    let broken = broken.to_vec();
    match verdict {
        Verdict::Installable(res) => (
            true,
            String::new(),
            res.changes
                .install_details
                .into_iter()
                .map(|c| {
                    (
                        c.name,
                        c.kind.as_str().to_string(),
                        c.old_version,
                        c.new_version,
                    )
                })
                .collect(),
            res.changes.remove,
            res.changes.unauthenticated,
            installed_version,
            repo_version,
            broken,
            conffile_conflicts,
            apt_stale,
        ),
        Verdict::Uninstallable(failure) => (
            false,
            failure,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            installed_version,
            repo_version,
            broken,
            conffile_conflicts,
            apt_stale,
        ),
    }
}

/// 系统异常包快照;查询失败不阻断分析(按无异常处理)
fn broken_snapshot() -> Vec<String> {
    health::broken_packages().unwrap_or_default()
}

/// 单文件分析的结论组装
fn analyze_deb(
    sys: &SysState,
    deb: &DebFile,
    apt_stale: bool,
) -> Result<fdebi_core::AnalyzeResult, FdebiError> {
    let verdict = solver::resolve(sys, deb)?;
    Ok(result_from_verdict(
        sys,
        deb,
        verdict,
        &broken_snapshot(),
        apt_stale,
    ))
}

/// 只读分析(D-Bus 方法内同步执行,重活放独立线程避免阻塞总线任务)
pub fn analyze(
    manager: &Arc<Manager>,
    path: &str,
    apt_opts: &[String],
) -> Result<fdebi_core::AnalyzeResult, String> {
    // 只读免授权:先校验并打开文件,再加载系统状态
    let deb = open_item(path)?;
    let sys = manager.sys_state(apt_opts).map_err(|e| e.to_string())?;
    let apt_stale = fdebi_core::sysstate::lists_stale(apt_opts);
    analyze_deb(&sys, &deb, apt_stale).map_err(|e| e.to_string())
}

/// 卸载前反向依赖预检:返回 [(受影响包, 依赖组)];只读,无需授权
pub fn check_removal(manager: &Arc<Manager>, name: &str) -> Result<Vec<(String, String)>, String> {
    let sys = manager.sys_state(&[]).map_err(|e| e.to_string())?;
    Ok(solver::reverse_dependents(&sys, &[name.to_string()])
        .into_iter()
        .map(|r| (r.package, r.requires))
        .collect())
}

/// 批量只读分析:一次加载系统状态,按批内依赖顺序求解,并把前面已判定
/// 可安装的本地包作为后续包的已安装上下文(A 依赖同批 B 时 A 不再误判)。
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

    // 先打开全部文件:打开失败逐项报告,不阻塞其余包的批内求解
    let opened: Vec<Result<DebFile, String>> = paths.iter().map(|p| open_item(p)).collect();
    let debs: Vec<&DebFile> = opened.iter().filter_map(|r| r.as_ref().ok()).collect();
    let mut verdicts = solver::resolve_batch(&sys, &debs).into_iter();

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
            Ok(deb) => {
                // 打开成功的项与 resolve_batch 输入顺序一致
                let (
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
                ) = match verdicts.next() {
                    Some(Ok(v)) => result_from_verdict(&sys, deb, v, &broken, apt_stale),
                    Some(Err(e)) => (
                        false,
                        e.to_string(),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        installed_version_of(&sys, deb),
                        repo_version_of(&sys, deb),
                        broken.clone(),
                        conffile_conflicts_of(&sys, deb),
                        apt_stale,
                    ),
                    None => (
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
                };
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
        };
        out.push(item);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 工作线程流程
// ---------------------------------------------------------------------------

/// 批量中的一个本地 deb(打开失败项也保留,用于逐包上报)
struct BatchItem {
    /// 原样路径字符串(GUI 按它匹配状态行)
    path_str: String,
    path: PathBuf,
    /// 包名(取不到 Package 字段时退回文件名)
    name: String,
    /// 打开时捕获的指纹(dpkg 执行前复核)
    stamp: Option<FileStamp>,
    deb: Option<DebFile>,
    open_error: Option<String>,
}

impl BatchItem {
    fn new(path: &str) -> Self {
        let path_buf = PathBuf::from(path);
        let name = path_buf
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        Self {
            path_str: path.to_string(),
            stamp: FileStamp::capture(&path_buf).ok(),
            path: path_buf,
            name,
            deb: None,
            open_error: None,
        }
    }
}

/// control.tar 携带 debconf templates → 安装时需要交互应答
fn deb_has_templates(deb: &DebFile) -> bool {
    deb.control_content("templates")
        .is_ok_and(|t| !t.is_empty())
}

/// 批量取消时的进度摘要
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

    // 1. Polkit 授权(GUI 瘦客户端,认证由守护进程发起;整批一次)。
    //    必须先于打开文件:未授权调用者不得让 root 守护进程读取任意路径
    //    (FIFO 会阻塞 open)或承担解析开销。
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

    // 2. 打开全部本地 deb:打开失败项逐包上报,不阻塞其余包
    let mut items: Vec<BatchItem> = paths.iter().map(|p| BatchItem::new(p)).collect();
    for item in &mut items {
        // 目录/FIFO/设备等非普通文件:open 会阻塞或无意义,直接拒绝。
        // stat 失败(不存在等)交给 DebFile::open 产生带路径的错误信息。
        if std::fs::metadata(&item.path).is_ok_and(|m| !m.is_file()) {
            item.open_error = Some(gettext("not a regular file"));
            continue;
        }
        match DebFile::open(&item.path) {
            Ok(deb) => {
                if let Some(name) = deb.field_trimmed("Package") {
                    item.name = name.to_string();
                }
                item.deb = Some(deb);
            }
            Err(e) => item.open_error = Some(e.to_string()),
        }
    }

    *txn.state.lock().unwrap() = (INSTALLING.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, INSTALLING.to_string()));

    // 3. 逐包执行:批内依赖先装(求解下一包时才能看到已装满足者);
    //    单包失败只记入失败列表,继续后续包
    let metas: Vec<LocalMeta> = items
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

    let mut installed = 0usize;
    let mut failures: Vec<String> = Vec::new();
    // 安装前健康快照:装后只报告本次事务新增的异常包(避免把用户
    // 本来就存在的未配置包算到 fdebi 头上)
    let broken_before = broken_snapshot();

    for (step, &idx) in order.iter().enumerate() {
        if txn.cancel.is_cancelled() {
            finalize(
                conn,
                txn,
                CANCELLED,
                &installed_summary(installed, total),
                id,
                false,
            );
            return;
        }
        let item = &items[idx];

        if let Some(reason) = &item.open_error {
            emit_package(conn, id, item, "failed", reason);
            failures.push(format!("{}: {reason}", item.name));
            continue;
        }
        let Some(deb) = item.deb.as_ref() else {
            continue;
        };

        emit_package(conn, id, item, "installing", "");
        emit_progress(
            conn,
            id,
            -1.0,
            &tf(
                "Installing {0} ({1}/{2})",
                &[&item.name, &(step + 1).to_string(), &total.to_string()],
            ),
        );

        // 先等外部包管理操作结束再取快照:否则方案可能基于正在被改写的状态
        let lock_progress = TxnProgress {
            id,
            conn: conn.clone(),
        };
        if let Err(e) = lock::wait_until_idle(&lock_progress, &txn.cancel) {
            if matches!(e, FdebiError::Cancelled) {
                finalize(
                    conn,
                    txn,
                    CANCELLED,
                    &installed_summary(installed, total),
                    id,
                    false,
                );
            } else {
                finalize(conn, txn, FAILED, &e.to_string(), id, false);
            }
            return;
        }

        // 每包重读系统状态:上一包可能改变了已装集合
        let sys = match manager.sys_state(apt_opts) {
            Ok(s) => s,
            Err(e) => {
                finalize(conn, txn, FAILED, &e.to_string(), id, false);
                return;
            }
        };
        let resolution = match solver::resolve(&sys, deb) {
            Ok(Verdict::Installable(r)) => r,
            Ok(Verdict::Uninstallable(failure)) => {
                emit_package(conn, id, item, "failed", &failure);
                failures.push(format!("{}: {failure}", item.name));
                continue;
            }
            Err(e) => {
                let detail = e.to_string();
                emit_package(conn, id, item, "failed", &detail);
                failures.push(format!("{}: {detail}", item.name));
                continue;
            }
        };

        if !resolution.downloads.is_empty() {
            *txn.state.lock().unwrap() = (DOWNLOADING.to_string(), String::new());
            emit(conn, "TransactionEvent", &(id, DOWNLOADING.to_string()));
            let dp = TxnProgress {
                id,
                conn: conn.clone(),
            };
            if let Err(e) = download::download_all(&resolution.downloads, &dp, &txn.cancel) {
                if matches!(e, FdebiError::Cancelled) {
                    finalize(
                        conn,
                        txn,
                        CANCELLED,
                        &installed_summary(installed, total),
                        id,
                        false,
                    );
                    return;
                }
                let detail = e.to_string();
                emit_package(conn, id, item, "failed", &detail);
                failures.push(format!("{}: {detail}", item.name));
                continue;
            }
            *txn.state.lock().unwrap() = (INSTALLING.to_string(), String::new());
            emit(conn, "TransactionEvent", &(id, INSTALLING.to_string()));
        }

        // 本地包的 apt 键:外部架构带 :arch(与 extended_states 的键一致)
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
            conf_policy: *conf_policy,
            // 依赖标记对齐 apt:只把本次新装的依赖标为 auto(升级已装包
            // 不改动其手动标记),本地包标为 manual
            auto_mark: aptmark::dependency_names(&resolution.changes.install, &local_key)
                .into_iter()
                .filter(|key| sys.installed_version(key).is_none())
                .collect(),
            local_package: local_key,
            interactive,
        };
        let ui = TxnUi {
            id,
            conn: conn.clone(),
        };
        let progress = TxnProgress {
            id,
            conn: conn.clone(),
        };
        match install::install(&ui, &progress, &txn.cancel, &txn.input, &batch) {
            Ok(report) if report.code == 0 => {
                installed += 1;
                emit_package(conn, id, item, "done", "");
            }
            Ok(report) => {
                let detail = report.detail();
                emit_package(conn, id, item, "failed", &detail);
                failures.push(format!("{}: {detail}", item.name));
            }
            Err(FdebiError::Cancelled) => {
                finalize(
                    conn,
                    txn,
                    CANCELLED,
                    &installed_summary(installed, total),
                    id,
                    false,
                );
                return;
            }
            Err(e) => {
                let detail = e.to_string();
                emit_package(conn, id, item, "failed", &detail);
                failures.push(format!("{}: {detail}", item.name));
            }
        }
    }

    // 4. 装后健康检查:dpkg 退出码为 0 也可能留下未配置的依赖方
    //    (例如 Provides 丢失或 Postinst 依赖未满足)。只报告新增问题。
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

    // 5. 汇总:失败包逐条列在 Finished detail(前端原样呈现,失败项在前)
    if failures.is_empty() {
        finalize(conn, txn, DONE, "", id, true);
    } else {
        finalize(conn, txn, FAILED, &failures.join("\n"), id, false);
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
    let _ = manager;
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
    *txn.state.lock().unwrap() = (INSTALLING.to_string(), String::new());
    emit(conn, "TransactionEvent", &(id, INSTALLING.to_string()));
    let ui = TxnUi {
        id,
        conn: conn.clone(),
    };
    let progress = TxnProgress {
        id,
        conn: conn.clone(),
    };
    match install::remove(&ui, &progress, &txn.cancel, name) {
        Ok(report) if report.code == 0 => finalize(conn, txn, DONE, "", id, true),
        Ok(report) => finalize(conn, txn, FAILED, &report.detail(), id, false),
        Err(e) => {
            let (state, ok) = match e {
                FdebiError::Cancelled => (CANCELLED, false),
                _ => (FAILED, false),
            };
            finalize(conn, txn, state, &e.to_string(), id, ok);
        }
    }
}

// ---------------------------------------------------------------------------
// 进度/UI 适配器:core 回调 → D-Bus 信号
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
        emit(&self.conn, "InputState", &(self.id, enabled));
    }
}

impl DownloadProgress for TxnProgress {
    fn item_start(&self, name: &str, idx: usize, total: usize) {
        emit_progress(
            &self.conn,
            self.id,
            -1.0,
            &tf("Fetching file {0} of {1}", &[name, &total.to_string()]),
        );
        let _ = idx;
    }

    fn item_progress(&self, name: &str, received: u64, size: u64) {
        let percent = if size > 0 {
            received as f64 * 100.0 / size as f64
        } else {
            -1.0
        };
        emit_progress(&self.conn, self.id, percent, name);
    }

    fn item_done(&self, _name: &str) {}
}

/// 守护进程无交互界面:告警转为输出行;确认类回调一律拒绝
struct TxnUi {
    id: u32,
    conn: zbus::Connection,
}

impl Ui for TxnUi {
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), FdebiError> {
        let kind = match kind {
            AlertKind::Error => "error",
            AlertKind::Warning => "warning",
            AlertKind::Info => "info",
        };
        emit(
            &self.conn,
            "OutputLine",
            &(
                self.id,
                match body {
                    Some(b) => format!("{kind}: {header}: {b}"),
                    None => format!("{kind}: {header}"),
                },
            ),
        );
        Ok(())
    }

    fn confirm(&self, _header: &str, _body: Option<&str>) -> Result<bool, FdebiError> {
        Ok(false)
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
