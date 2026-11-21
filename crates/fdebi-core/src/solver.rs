//! 求解结果的数据类型(依赖求解本体在 [`crate::apt`],由 libapt-pkg 完成)。
//!
//! 这些类型是 daemon/GTK/CLI 共用的结果契约:文本记录由 shim 转换而来,
//! 前端只消费结构化字段,不感知 libapt-pkg。

use crate::i18n::gettext;

/// 下载坐标(仓库候选被选中时生成;本地 deb 直接走 dpkg -i)
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    /// apt 包键(FullName(true),外部架构带 :arch)
    pub name: String,
    pub version: String,
    /// 归档大小(字节;来源为 Packages 元数据)
    pub size: u64,
    /// 归档是否来自受信任的软件源
    pub trusted: bool,
}

/// 待安装包的变更类型(相对当前已装版本)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    New,
    Upgrade,
    Downgrade,
    Reinstall,
}

impl ChangeKind {
    /// D-Bus/AnalyzeResult 线型标签
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Upgrade => "upgrade",
            Self::Downgrade => "downgrade",
            Self::Reinstall => "reinstall",
        }
    }

    /// 线型标签 → 枚举(未知标签按新装处理,保证旧客户端/新守护进程兼容)
    pub fn from_arg(s: &str) -> Self {
        match s {
            "upgrade" => Self::Upgrade,
            "downgrade" => Self::Downgrade,
            "reinstall" => Self::Reinstall,
            _ => Self::New,
        }
    }
}

/// 单个安装项的分类明细(GUI/CLI 呈现"新装/升级/降级/重装")
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallChange {
    pub name: String,
    pub kind: ChangeKind,
    /// 已装版本(新装为空串)
    pub old_version: String,
    /// 候选版本
    pub new_version: String,
}

/// 求解结果中的变更集(名称列表 + 分类明细,GUI 呈现用)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub install: Vec<String>,
    /// 与 `install` 同序的分类明细
    pub install_details: Vec<InstallChange>,
    pub remove: Vec<String>,
    pub unauthenticated: Vec<String>,
}

/// 完整求解结果
#[derive(Debug, Clone)]
pub struct Resolution {
    pub changes: Changes,
    pub downloads: Vec<DownloadSpec>,
}

/// 依赖检查结论
#[derive(Debug, Clone)]
pub enum Verdict {
    Installable(Resolution),
    Uninstallable(String),
}

impl Verdict {
    /// 失败原因;可安装为 None
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Installable(_) => None,
            Self::Uninstallable(reason) => Some(reason),
        }
    }

    /// 统一的"求解失败"文案(shim 返回失败且无细分原因时使用)
    pub fn unsolvable() -> Self {
        Self::Uninstallable(gettext(
            "Failed to satisfy all dependencies (conflicting requests or missing packages)",
        ))
    }
}
