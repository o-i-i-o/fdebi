//! Build the libapt-pkg C++ shim on Linux; compile the error stub elsewhere.
//!
//! fdebi only runs on Debian-derived systems.  Building on Windows (or any
//! non-Linux host) still compiles the C ABI stub so `cargo check` can validate
//! the Rust side without libapt-pkg installed.
//!
//! Linux 上另支持 FDEBI_APT_STUB=1 强制编译桩:flatpak 沙箱等无 libapt-pkg
//! 的环境构建 GUI 客户端用(求解/安装全部走 D-Bus 守护进程,客户端进程内
//! 不会触达 apt shim)。

use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=native/apt_shim.h");
    println!("cargo:rerun-if-changed=native/apt_stub.c");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FDEBI_APT_STUB");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let force_stub = env::var("FDEBI_APT_STUB").is_ok_and(|v| v == "1");
    if target_os != "linux" || force_stub {
        cc::Build::new()
            .file("native/apt_stub.c")
            .include("native")
            .warnings(false)
            .compile("fdebi_apt_shim");
        return;
    }

    println!("cargo:rerun-if-changed=native/apt_shim.cc");
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .pic(true)
        .file("native/apt_shim.cc")
        .include("native")
        .warnings(false);

    // Debian installs apt-pkg headers in /usr/include, but honour pkg-config
    // when available so non-standard sysroots also work.
    if let Ok(output) = Command::new("pkg-config")
        .args(["--cflags", "apt-pkg"])
        .output()
        && output.status.success()
    {
        for flag in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            if let Some(path) = flag.strip_prefix("-I") {
                build.include(path);
            } else if flag.starts_with("-D") {
                build.flag(flag);
            }
        }
    }
    build.compile("fdebi_apt_shim");

    println!("cargo:rustc-link-lib=dylib=apt-pkg");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
