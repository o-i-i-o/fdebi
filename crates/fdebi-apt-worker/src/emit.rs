//! 事件输出:每行一个 JSON `Event`,写入控制通道(fd 3)。

use std::io::Write;
use std::sync::{Arc, Mutex};

use fdebi_apt_proto::Event;

/// 可克隆的事件发射器;进度回调发生在同一线程,但批准用共享锁以便
/// 将来并行下载线程也能安全写入。
#[derive(Clone)]
pub struct Emitter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Emitter {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    pub fn emit(&self, event: &Event) {
        let mut w = self.inner.lock().unwrap();
        // 控制通道失败不 panic:daemon 可能已取消事务
        if serde_json::to_writer(&mut *w, event).is_ok() {
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }

    pub fn output(&self, line: impl Into<String>) {
        self.emit(&Event::Output { line: line.into() });
    }
}
