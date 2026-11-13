//! 错误类型。用户可见文案为英文 msgid,经 gettext 翻译(见 i18n 模块);
//! C locale / 无翻译时回落英文原文。

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::i18n::{gettext, tf};

#[derive(Debug)]
pub enum FdebiError {
    OpenFailed(PathBuf, String),
    NotADeb(PathBuf),
    MissingControl,
    /// control.tar 解压失败:原因
    ControlDecompress(String),
    MissingField(&'static str),
    Io(io::Error),
    /// 子进程(dpkg 系统能力调用)执行失败:命令 + stderr 摘要
    Subprocess(String, String),
    /// 求解/下载期间文件被替换或删除(TOCTOU):路径
    FileChanged(PathBuf),
    /// 用户请求取消
    Cancelled,
}

impl From<io::Error> for FdebiError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl fmt::Display for FdebiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::OpenFailed(path, reason) => tf(
                "cannot open {0}: {1}",
                &[&path.display().to_string(), reason],
            ),
            Self::NotADeb(path) => tf(
                "{0} is not a Debian package",
                &[&path.display().to_string()],
            ),
            Self::MissingControl => gettext("control file not found in package"),
            Self::ControlDecompress(reason) => {
                tf("cannot decompress control archive: {0}", &[reason])
            }
            Self::MissingField(name) => tf("missing control field: {0}", &[name]),
            // 透明转发:OS strerror 已随系统 locale 本地化,不再包裹
            Self::Io(e) => return write!(f, "{e}"),
            Self::Subprocess(cmd, stderr) => {
                tf("subprocess {0} failed: {1}", &[cmd, stderr.trim()])
            }
            Self::FileChanged(path) => tf(
                "package file {0} changed or disappeared before installation",
                &[&path.display().to_string()],
            ),
            Self::Cancelled => gettext("operation cancelled"),
        };
        f.write_str(&msg)
    }
}

impl Error for FdebiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
