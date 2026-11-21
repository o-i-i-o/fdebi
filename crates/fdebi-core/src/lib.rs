//! fdebi-core:本地 .deb 解析、libapt-pkg 依赖求解与安装编排核心(独立项目 fdebi 的核心库)。
//!
//! 定位:轻量的可视化 .deb 安装器(双击 deb 文件或 GUI 内选择文件安装)。
//! 分层:GUI 瘦客户端 → D-Bus 守护进程([`fdebi-daemon`]) → [`apt`] 求解 →
//! [`ui`] trait 回调 → dpkg 执行层。
//!
//! 依赖解析/索引/下载全部委托 libapt-pkg(经 C ABI shim,见 native/apt_shim.cc;
//! 非 Linux 平台编译等签名错误桩以通过 cargo check):
//! - [`debfile`]:.deb 打开与 control 提取(原始字节保留,lossy 字段访问)
//! - [`apt`]:libapt-pkg 会话(缓存构建、依赖求解、仓库下载、标记写入)
//! - [`solver`]:求解结果的数据类型(Verdict/Resolution/Changes,无求解逻辑)
//! - [`batch`]:批量本地 deb 的安装顺序(批内依赖拓扑排序)
//! - [`conffile`]:安装前 conffile 冲突预检
//! - [`lock`]:包管理前端占锁探测与等待(ps comm 列,含 apt 系交互前端)
//! - [`pty`]:debconf 交互式安装的伪终端执行(行式问答 + 应答注入)
//! - [`install`]:dpkg 执行层(批量 -i + 取消句柄),进度经 [`ui::InstallProgress`]
//!   转发
//!
//! [`fdebi-daemon`]: https://github.com/ 载荷分离说明见 crates/fdebi-daemon

mod control;

pub mod apt;
pub mod batch;
pub mod conffile;
pub mod debfile;
pub mod error;
pub mod flow;
pub mod i18n;
pub mod install;
pub mod lock;
pub mod pty;
pub mod solver;
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
/// [`solver::ChangeKind`];未知标签按 new 处理)。
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
