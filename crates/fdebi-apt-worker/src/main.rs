//! fdebi apt/dpkg 事务 worker。
//!
//! 用法:`fdebi-apt-worker {analyze|install|remove} <request.json>`
//!
//! - 请求文件由 daemon 以 0600 写入 root 专属临时文件,读取后立即删除。
//! - 事件经 fd 3 单向输出,每行一个 JSON(`fdebi_apt_proto::Event`)。
//! - install/remove 的 stdio 由 daemon 接到 PTY 从端,dpkg 与维护脚本
//!   因此拥有真实终端,debconf readline 可正常问答。

mod analyze;
mod config;
mod emit;
mod install;
mod remove;
mod request;

use std::io::Write;
use std::os::fd::FromRawFd;
use std::process::ExitCode;

use fdebi_apt_proto::{Event, Mode};

use crate::emit::Emitter;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(mode) = args.next().and_then(|m| Mode::from_arg(&m)) else {
        eprintln!("usage: fdebi-apt-worker {{analyze|install|remove}} <request.json>");
        return ExitCode::from(2);
    };
    let Some(request_path) = args.next() else {
        eprintln!("usage: fdebi-apt-worker {{analyze|install|remove}} <request.json>");
        return ExitCode::from(2);
    };

    if unsafe { libc::geteuid() } != 0 {
        eprintln!("fdebi-apt-worker must run as root");
        return ExitCode::from(77);
    }

    let out = Emitter::new(control_writer());

    let req = match request::read(std::path::Path::new(&request_path)) {
        Ok(req) => req,
        Err(e) => {
            out.emit(&Event::Result {
                ok: false,
                detail: e,
                items: Vec::new(),
            });
            return ExitCode::from(1);
        }
    };

    fdebi_core::i18n::init();
    if mode != Mode::Analyze {
        // SAFETY: worker 此刻仍是单线程,尚未创建任何线程。
        unsafe {
            std::env::set_var("DEBIAN_FRONTEND", "readline");
        }
    }

    let result = match mode {
        Mode::Analyze => analyze::run(&req, &out),
        Mode::Install => install::run(&req, &out),
        Mode::Remove => remove::run(&req, &out),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            out.emit(&Event::Result {
                ok: false,
                detail: e,
                items: Vec::new(),
            });
            ExitCode::from(1)
        }
    }
}

/// 控制通道固定为 fd 3;手工运行时若未提供则退回 stderr,便于调试。
fn control_writer() -> Box<dyn Write + Send> {
    if unsafe { libc::fcntl(3, libc::F_GETFD) } >= 0 {
        // SAFETY: fd 3 存在且由 daemon 通过 dup2 指定;worker 独占所有权。
        Box::new(unsafe { std::fs::File::from_raw_fd(3) })
    } else {
        Box::new(std::io::stderr())
    }
}
