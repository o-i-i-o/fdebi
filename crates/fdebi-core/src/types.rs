//! 分析结果中与前端共用的展示类型(D-Bus 契约的配套模型)。
//!
//! 这些类型不参与依赖求解,只描述"哪些包会变、怎么变",供 GTK 等前端
//! 呈现变更列表。

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

/// 单个安装项的分类明细(GUI 呈现"新装/升级/降级/重装")
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
