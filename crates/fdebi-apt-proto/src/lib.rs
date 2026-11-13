//! daemon → worker 请求与 worker → daemon 事件。
//!
//! 传输方式:请求由 daemon 写入 0600 临时文件;事件经 fd 3 以 JSON Lines
//! 单向输出(每行一个 [`Event`])。PTY 只承载人类可读输出与 stdin,
//! 不混入协议数据。

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// worker 运行模式(命令行第一个参数)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Analyze,
    Install,
    Remove,
}

impl Mode {
    pub fn as_arg(self) -> &'static str {
        match self {
            Mode::Analyze => "analyze",
            Mode::Install => "install",
            Mode::Remove => "remove",
        }
    }

    pub fn from_arg(arg: &str) -> Option<Self> {
        match arg {
            "analyze" => Some(Mode::Analyze),
            "install" => Some(Mode::Install),
            "remove" => Some(Mode::Remove),
            _ => None,
        }
    }
}

/// conffile 冲突处理策略(与 D-Bus `conf_policy` 一致)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfPolicy {
    #[default]
    Default,
    Keep,
    Replace,
}

/// 本地 deb 指纹:daemon 捕获后由 worker 在执行前复核(TOCTOU)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}

impl Stamp {
    pub fn capture(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        let m = std::fs::metadata(path)?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        })
    }

    /// 文件仍存在且指纹一致
    pub fn matches(&self, path: &Path) -> bool {
        Self::capture(path).is_ok_and(|s| s == *self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub path: String,
    /// 分析请求可省略;安装/卸载必须提供
    #[serde(default)]
    pub stamp: Option<Stamp>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub apt_opts: Vec<String>,
    #[serde(default)]
    pub conf_policy: ConfPolicy,
    #[serde(default)]
    pub remove_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    New,
    Upgrade,
    Downgrade,
    Reinstall,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::New => "new",
            ChangeKind::Upgrade => "upgrade",
            ChangeKind::Downgrade => "downgrade",
            ChangeKind::Reinstall => "reinstall",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    /// `pkg.fullname(true)`:非本机架构带 `:arch`
    pub name: String,
    pub kind: ChangeKind,
    pub old_version: String,
    pub new_version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemResult {
    pub path: String,
    pub installable: bool,
    pub failure: String,
    pub install: Vec<Change>,
    pub remove: Vec<String>,
}

impl ItemResult {
    pub fn failed(path: impl Into<String>, failure: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            installable: false,
            failure: failure.into(),
            install: Vec::new(),
            remove: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Analyzing,
    Downloading,
    Installing,
    Removing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageState {
    Installing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// 阶段切换:daemon 据此更新事务状态
    Stage { phase: Phase, desc: String },
    /// percent < 0 表示不确定(pulse)
    Progress { percent: f64, desc: String },
    /// 逐包状态:path 用于前端行匹配;detail 仅 failed 时非空
    Package {
        path: String,
        name: String,
        state: PackageState,
        detail: String,
    },
    /// 人类可读输出(进入 OutputLine)
    Output { line: String },
    /// 终态;分析模式下 items 与请求顺序一致
    Result {
        ok: bool,
        detail: String,
        items: Vec<ItemResult>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json_roundtrip() {
        let events = vec![
            Event::Stage {
                phase: Phase::Installing,
                desc: "dpkg".into(),
            },
            Event::Progress {
                percent: 42.5,
                desc: "unpack".into(),
            },
            Event::Package {
                path: "/tmp/a.deb".into(),
                name: "a".into(),
                state: PackageState::Installing,
                detail: String::new(),
            },
            Event::Result {
                ok: true,
                detail: String::new(),
                items: vec![ItemResult {
                    path: "/tmp/a.deb".into(),
                    installable: true,
                    failure: String::new(),
                    install: vec![Change {
                        name: "a".into(),
                        kind: ChangeKind::New,
                        old_version: String::new(),
                        new_version: "1.0".into(),
                    }],
                    remove: Vec::new(),
                }],
            },
        ];
        for event in events {
            let line = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&line).unwrap();
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                line,
                "roundtrip mismatch"
            );
        }
    }

    #[test]
    fn request_json_roundtrip() {
        let req = Request {
            items: vec![Item {
                path: "/tmp/a.deb".into(),
                stamp: Some(Stamp {
                    dev: 1,
                    ino: 2,
                    size: 3,
                    mtime: 4,
                    mtime_nsec: 5,
                    ctime: 6,
                    ctime_nsec: 7,
                }),
            }],
            apt_opts: vec!["Acquire::http::Proxy=http://x".into()],
            conf_policy: ConfPolicy::Keep,
            remove_name: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.conf_policy, ConfPolicy::Keep);
    }

    #[test]
    fn stamp_detects_replacement_and_removal() {
        let dir = std::env::temp_dir().join(format!("fdebi-proto-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.deb");
        std::fs::write(&path, b"one").unwrap();
        let stamp = Stamp::capture(&path).unwrap();
        assert!(stamp.matches(&path));

        // 内容替换(大小变化)必须被发现
        std::fs::write(&path, b"two-longer").unwrap();
        assert!(!stamp.matches(&path));

        // 删除后也不再匹配
        std::fs::remove_file(&path).unwrap();
        assert!(!stamp.matches(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
