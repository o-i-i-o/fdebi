//! fdebm-core:本地 .deb 解析、纯 Rust 依赖求解与安装编排核心(独立项目 fdebm 的核心库)。
//!
//! 定位:轻量的可视化 .deb 安装器(双击 deb 文件或 GUI 内选择文件安装)。
//! 分层:GUI 瘦客户端 → D-Bus 守护进程([`fdebm-daemon`]) → [`flow`] 摘要 →
//! [`ui`] trait 回调 → 解析/求解等纯逻辑。
//!
//! 纯 Rust 管道(无 libapt-pkg FFI;系统能力仅经 dpkg/dpkg-deb/indextargets
//! 子进程,索引解析与 HTTP 下载全 Rust 后端,见 crates/readme.md 路线二):
//! - [`debfile`]:.deb 打开与 control 提取(原始字节保留,lossy 字段访问)
//! - [`version`]:dpkg 版本号比较
//! - [`sysstate`]:dpkg 状态 + apt 仓库索引的纯 Rust 读取
//! - [`solver`]:varisat CDCL 编码的依赖求解
//! - [`batch`]:批量本地 deb 的安装顺序(批内依赖拓扑排序)
//! - [`conffile`]:安装前 conffile 冲突预检
//! - [`health`]:系统包管理健康检查(dpkg-query 异常状态,装前提示/装后复查)
//! - [`download`]:ureq(rustls)下载 + sha256 校验
//! - [`lock`]:包管理前端占锁探测与等待(ps comm 列,含 apt 系交互前端)
//! - [`aptmark`]:安装后 apt 标记补齐(依赖 auto / 本地包 manual)
//! - [`pty`]:debconf 交互式安装的伪终端执行(行式问答 + 应答注入)
//! - [`install`]:dpkg 执行层(批量 -i + 取消句柄),进度经 [`ui::InstallProgress`]
//!   转发
//!
//! [`fdebm-daemon`]: https://github.com/ 载荷分离说明见 crates/fdebm-daemon

mod control;

pub mod aptmark;
pub mod batch;
pub mod conffile;
pub mod debfile;
pub mod download;
pub mod error;
pub mod flow;
pub mod health;
pub mod i18n;
pub mod install;
pub mod lock;
pub mod pty;
pub mod solver;
pub mod sysstate;
pub mod ui;
pub mod version;

pub use debfile::DebFile;
pub use error::FdebmError;

/// D-Bus `AnalyzeLocalFile` 返回契约(daemon/gtk 共用,类型别名仅消解
/// clippy::type_complexity,线型不变):
/// (可安装, 错误信息, install[], remove[], unauthenticated[], 已安装版本,
///  软件源候选版本, 异常状态包[], conffile 冲突 [(包名, 路径)])
pub type AnalyzeResult = (
    bool,
    String,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    String,
    String,
    Vec<String>,
    Vec<(String, String)>,
);

/// D-Bus `AnalyzeLocalFiles` 的单文件结果(按输入顺序返回):
/// (路径, 可安装, 错误信息, install[], remove[], unauthenticated[],
///  已安装版本, 软件源候选版本, 异常状态包[], conffile 冲突 [(包名, 路径)])
pub type AnalyzeItem = (
    String,
    bool,
    String,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    String,
    String,
    Vec<String>,
    Vec<(String, String)>,
);
pub use version::cmp_version;
