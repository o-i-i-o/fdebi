//! D-Bus 契约层(org.fdebi):GUI 瘦客户端与守护进程的唯一接口。
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

pub struct FdebiIface {
    manager: Arc<Manager>,
}

impl FdebiIface {
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

/// apt_opts 白名单:只放行行为(apt::)/网络(Acquire::)/调试(Debug::)三类键。
/// AnalyzeLocalFile 免 Polkit 授权,任意本地用户可调用,守护进程以 root
/// 运行 libapt;不限键名的 Key=Value 可重定向 root 的读写路径
/// (Dir::Etc::sourcelist 等)或下载执行点(Dir::Bin::methods 等),
/// 等于把 root 能力开放给普通用户,必须在此收紧。
/// libapt 配置键大小写不敏感,故统一小写比较;
/// apt-cli 的 AptCli::Hooks::* 顶层键不匹配 "apt::" 前缀,同样被拒。
fn validate_apt_opts(opts: &[String]) -> zbus::fdo::Result<()> {
    const ALLOWED: [&str; 3] = ["apt::", "acquire::", "debug::"];
    for opt in opts {
        let key = opt.split('=').next().unwrap_or(opt);
        let ok = ALLOWED
            .iter()
            .any(|p| key.to_ascii_lowercase().starts_with(p));
        if !ok {
            return Err(zbus::fdo::Error::AccessDenied(format!(
                "apt option '{key}' is not allowed"
            )));
        }
    }
    Ok(())
}

#[zbus::interface(name = "org.fdebi")]
impl FdebiIface {
    async fn analyze_local_file(
        &self,
        path: String,
        apt_opts: Vec<String>,
    ) -> zbus::fdo::Result<fdebi_core::AnalyzeResult> {
        validate_apt_opts(&apt_opts)?;
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
    ) -> zbus::fdo::Result<Vec<fdebi_core::AnalyzeItem>> {
        validate_apt_opts(&apt_opts)?;
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
        validate_apt_opts(&apt_opts)?;
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

#[cfg(test)]
mod tests {
    use super::validate_apt_opts;

    fn allowed(opt: &str) -> bool {
        validate_apt_opts(&[opt.to_string()]).is_ok()
    }

    #[test]
    fn whitelist_allows_behavior_network_debug_keys() {
        assert!(allowed("APT::Get::AllowUnauthenticated=true"));
        assert!(allowed("apt::Sandbox::User=_apt")); // 键大小写不敏感
        assert!(allowed("Acquire::http::Proxy=http://proxy:3128"));
        assert!(allowed("Debug::pkgProblemResolver=1"));
    }

    #[test]
    fn whitelist_rejects_path_and_exec_redirection() {
        // Dir:: 可重定向 root 读写路径;Bin::methods 是下载执行点
        assert!(!allowed("Dir::Etc::sourcelist=/tmp/evil.list"));
        assert!(!allowed("dir::bin::methods=/tmp/evil")); // 小写亦拒
        assert!(!allowed("Dpkg::Options::=--force-confnew"));
        assert!(!allowed("AptCli::Hooks::Install=/tmp/evil.sh"));
        // 格式不合法的裸键(缺 Key=Value)一并拒绝
        assert!(!allowed("Dir::State::status"));
    }

    #[test]
    fn whitelist_accepts_empty_list() {
        assert!(validate_apt_opts(&[]).is_ok());
    }
}
