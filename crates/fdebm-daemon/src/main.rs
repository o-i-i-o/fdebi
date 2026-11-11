//! fdebm-daemon:特权 D-Bus 守护进程(纯 Rust 管道的执行体)。
//!
//! 架构(crates/readme.md):GUI 永不 root;本守护进程是唯一"实干者",
//! 以 root 注册系统总线 org.fdebm,经 Polkit 授权后执行:
//! - AnalyzeLocalFile:解析 + 纯 Rust SAT 求解(只读,免授权)
//! - AnalyzeLocalFiles:批量分析(只读;批内前序可安装包作为后续包的已装上下文)
//! - InstallLocalFiles / RemovePackage:事务(下载 → dpkg),发进度信号
//! - SendInput:交互式安装(debconf 模板)的应答,写入 dpkg 的 stdin
//!
//! 部署:data/dbus、data/polkit 三个文件(见各文件头注释)。
//! 开发期:sudo ./target/debug/fdebm-daemon 前台运行。

mod iface;
mod polkit;
mod txn;

use zbus::connection::Builder;

const BUS_NAME: &str = "org.fdebm";
const OBJECT_PATH: &str = "/org/fdebm";

fn main() {
    let conn = async_io::block_on(async {
        let manager = txn::Manager::new_shared();
        Builder::system()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, iface::FdebmIface::new(manager))?
            .build()
            .await
    });
    // 连接句柄须保活(析构即断开);内部执行器线程随 Connection 常驻
    let _conn = match conn {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fdebm-daemon: cannot start on system bus: {e}");
            eprintln!("hint: install data/dbus/*.conf + *.service and data/polkit/*.policy,");
            eprintln!("      or run `sudo fdebm-daemon` in the foreground for development.");
            std::process::exit(1);
        }
    };
    eprintln!("fdebm-daemon: serving {BUS_NAME} at {OBJECT_PATH}");
    loop {
        std::thread::park();
    }
}
