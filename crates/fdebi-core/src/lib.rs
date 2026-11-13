//! fdebi-core:本地 .deb 解析与系统状态的共享库(fdebi 的公共底座)。
//!
//! 定位:轻量的可视化 .deb 安装器(双击 deb 文件或 GUI 内选择文件安装)。
//! 分层:GUI 瘦客户端 → D-Bus 守护进程(`fdebi-daemon`)→ `fdebi-apt-worker`
//! (libapt-pkg);本库提供两侧共用的纯 Rust 逻辑,不做依赖求解与安装编排。
//!
//! 模块:
//! - [`debfile`]:.deb 打开与 control 提取(原始字节保留,lossy 字段访问)
//! - [`version`]:dpkg 版本号比较
//! - [`sysstate`]:dpkg 状态 + apt 仓库索引的纯 Rust 读取
//! - [`reverse`]:卸载前的只读反向依赖检查
//! - [`conffile`]:安装前 conffile 冲突预检
//! - [`health`]:系统包管理健康检查(dpkg-query 异常状态,装前提示/装后复查)
//! - [`lock`]:包管理前端占锁探测与等待(ps comm 列,含 apt 系交互前端)
//! - [`install`]:事务共用原语(conffile 策略、取消句柄)
//! - [`types`]:分析结果的展示模型(变更类型/变更集)
//! - [`ui`]:前端回调 trait(`Ui`/`InstallProgress`)与告警类型
//! - [`flow`]:deb 内容的摘要与预览(供 GUI 展示)
//! - [`i18n`]:gettext 绑定

mod control;

pub mod conffile;
pub mod debfile;
pub mod error;
pub mod flow;
pub mod health;
pub mod i18n;
pub mod install;
pub mod lock;
pub mod reverse;
pub mod sysstate;
pub mod types;
pub mod ui;
pub mod version;

pub use debfile::DebFile;
pub use error::FdebiError;

/// D-Bus `AnalyzeLocalFile` 返回契约(daemon/gtk 共用,类型别名仅消解
/// clippy::type_complexity,线型不变):
/// (可安装, 错误信息, install[(包名, 变更类型, 旧版本, 新版本)], remove[],
///  unauthenticated[], 已安装版本, 软件源候选版本, 异常状态包[],
///  conffile 冲突 [(包名, 路径)], apt 索引过期)
///
/// 变更类型:`new` / `upgrade` / `downgrade` / `reinstall`(见
/// [`types::ChangeKind`];未知标签按 new 处理)。
pub type AnalyzeResult = (
    bool,
    String,
    Vec<(String, String, String, String)>,
    Vec<String>,
    Vec<String>,
    String,
    String,
    Vec<String>,
    Vec<(String, String)>,
    bool,
);

/// D-Bus `AnalyzeLocalFiles` 的单文件结果(按输入顺序返回):
/// (路径, 可安装, 错误信息, install[(包名, 变更类型, 旧版本, 新版本)],
///  remove[], unauthenticated[], 已安装版本, 软件源候选版本,
///  异常状态包[], conffile 冲突 [(包名, 路径)], apt 索引过期)
pub type AnalyzeItem = (
    String,
    bool,
    String,
    Vec<(String, String, String, String)>,
    Vec<String>,
    Vec<String>,
    String,
    String,
    Vec<String>,
    Vec<(String, String)>,
    bool,
);
pub use version::cmp_version;
