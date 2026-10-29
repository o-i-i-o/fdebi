//! 前端交互能力 trait 层:核心流程只通过这些 trait 与 UI 通信,
//! GUI 前端的差异全部收敛在各前端的实现里。
//!
//! 约定:核心流程通过 [`Ui`] 呈现错误后仍返回 Err,前端收到 Err
//! 只决定退出码,不再重复输出。

use crate::error::FdebmError;

/// 告警类别
#[derive(Debug, Clone, Copy)]
pub enum AlertKind {
    Error,
    Warning,
    Info,
}

/// 前端交互能力(模态语义)
pub trait Ui {
    /// 告警/错误呈现
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), FdebmError>;

    /// 是/否确认;非交互流程直接返回预设值
    fn confirm(&self, header: &str, body: Option<&str>) -> Result<bool, FdebmError>;
}

/// dpkg 安装进度:子进程输出逐行转发,前端写入进度对话框的终端输出区
pub trait InstallProgress {
    fn output_line(&self, line: &str);

    /// 阶段提示(等待锁、开始安装等),显示在进度对话框的描述处
    fn stage(&self, _desc: &str) {}

    /// 交互输入开关:交互式安装(dpkg 在 PTY 中运行)开始/结束时切换,
    /// 前端据此启用应答输入框
    fn input_state(&self, _enabled: bool) {}
}
