//! D-Bus 契约层(org.fdebm):GUI 瘦客户端与守护进程的唯一接口。
//!
//! 方法:
//! - AnalyzeLocalFile(s path, as apt_opts) → (b installable, s failure,
//!   a(ssss) install[(包名,变更类型,旧版本,新版本)], as remove,
//!   as unauthenticated, s installed_version, s repo_version,
//!   as broken_packages, a(ss) conffile_conflicts, b apt_stale)
//! - AnalyzeLocalFiles(as paths, as apt_opts) → 逐文件结果数组
//!   (路径, 可安装, 错误信息, install[(包名,变更类型,旧版本,新版本)],
//!   remove[], unauthenticated[], 已安装版本, 软件源候选版本,
//!   异常状态包[], conffile 冲突, apt 索引过期;按输入顺序;
//!   批内前序可安装包作为后续包的已装上下文)
//! - CheckRemoval(s name) → a(ss)[(受影响包, 依赖组)](只读反向依赖预检)
//! - InstallLocalFiles(as paths, as apt_opts, s conf_policy) → u id
//! - RemovePackage(s name) → u id
//! - Cancel(u id) → b accepted(仅事务发起者)
//! - SendInput(u id, s line) → b accepted(交互式安装应答,仅事务发起者)
//! - GetTransactionStatus(u id) → (s state, s error)(仅事务发起者)
//!
//! 信号:TransactionEvent / OutputLine / ProgressChanged / PackageStatus /
//! InputState(交互应答开关)/ Finished。
//!
//! 分析为只读操作,免 Polkit 授权;安装为批量事务(逐包执行、失败不中断)。

use std::sync::Arc;

use zbus::message::Header;

use crate::txn::{self, Manager};

pub struct FdebmIface {
    manager: Arc<Manager>,
}

impl FdebmIface {
    pub fn new(manager: Arc<Manager>) -> Self {
        Self { manager }
    }

    fn caller(header: &Header<'_>) -> Option<String> {
        header
            .sender()
            .map(|s| s.as_str().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[zbus::interface(name = "org.fdebm")]
impl FdebmIface {
    async fn analyze_local_file(
        &self,
        path: String,
        apt_opts: Vec<String>,
    ) -> zbus::fdo::Result<fdebm_core::AnalyzeResult> {
        // 求解是重活(索引解析),放独立线程避免阻塞总线 executor
        let manager = self.manager.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            // send 是 async 方法,同步线程内必须用 send_blocking:
            // let _ = tx.send(..) 只创建 future 即丢弃,消息永远不会入队
            let _ = tx.send_blocking(txn::analyze(&manager, &path, &apt_opts));
        });
        rx.recv()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// 批量分析:一次加载系统状态并按批内依赖顺序求解,供 GUI 多选安装使用
    async fn analyze_local_files(
        &self,
        paths: Vec<String>,
        apt_opts: Vec<String>,
    ) -> zbus::fdo::Result<Vec<fdebm_core::AnalyzeItem>> {
        let manager = self.manager.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(txn::analyze_batch(&manager, &paths, &apt_opts));
        });
        rx.recv()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// 卸载前反向依赖预检(只读):返回 [(受影响包, 依赖组)]
    async fn check_removal(&self, name: String) -> zbus::fdo::Result<Vec<(String, String)>> {
        let manager = self.manager.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(txn::check_removal(&manager, &name));
        });
        rx.recv()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn install_local_files(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        paths: Vec<String>,
        apt_opts: Vec<String>,
        conf_policy: String,
    ) -> zbus::fdo::Result<u32> {
        if paths.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "no package files given".to_string(),
            ));
        }
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        Ok(txn::start_install(
            &self.manager,
            conn,
            paths,
            apt_opts,
            conf_policy,
            caller,
        ))
    }

    async fn remove_package(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        name: String,
    ) -> zbus::fdo::Result<u32> {
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        Ok(txn::start_remove(&self.manager, conn, name, caller))
    }

    async fn cancel(&self, #[zbus(header)] header: Header<'_>, id: u32) -> zbus::fdo::Result<bool> {
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        Ok(self.manager.cancel(id, &caller))
    }

    /// 交互式安装应答:写入 dpkg 的 stdin;非发起者或当前无交互安装时拒绝
    async fn send_input(
        &self,
        #[zbus(header)] header: Header<'_>,
        id: u32,
        line: String,
    ) -> zbus::fdo::Result<bool> {
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        Ok(self.manager.send_input(id, &caller, &line))
    }

    async fn get_transaction_status(
        &self,
        #[zbus(header)] header: Header<'_>,
        id: u32,
    ) -> zbus::fdo::Result<(String, String)> {
        let caller = Self::caller(&header)
            .ok_or_else(|| zbus::fdo::Error::Failed("unknown caller".to_string()))?;
        self.manager
            .status(id, &caller)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("unknown transaction {id}")))
    }
}
