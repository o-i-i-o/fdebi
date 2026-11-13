//! apt 全局配置:在创建 Cache 之前一次性施加。
//!
//! worker 是短生命周期进程,因此可以安全地修改进程级 apt 配置;
//! 不同事务之间不会互相污染。

use fdebi_apt_proto::{ConfPolicy, Request};
use oma_apt::config::Config;
use oma_apt::error::AptErrors;

use crate::emit::Emitter;

/// 应用请求中的 apt 选项与 fdebi 的固定策略。
pub fn apply(req: &Request, out: &Emitter) {
    let cfg = Config::new();

    for opt in &req.apt_opts {
        let Some((key, value)) = opt.split_once('=') else {
            out.output(format!("Ignored malformed apt option: {opt}"));
            continue;
        };
        if allowed_opt(key) {
            cfg.set(key, value);
        } else {
            out.output(format!("Ignored restricted apt option: {opt}"));
        }
    }

    // dpkg 直接继承 worker 的 PTY:debconf/readline 与 /dev/tty 可用
    cfg.set("Dpkg::Use-Pty", "false");
    // 前端是逐行文本视图,不需要 apt 的 fancy 进度条与颜色
    cfg.set("Dpkg::Progress-Fancy", "false");
    cfg.set("APT::Color", "false");
    // 与 fdebi 的既有约定一致:不装 Recommends/Suggests
    cfg.set("APT::Install-Recommends", "false");
    cfg.set("APT::Install-Suggests", "false");
    // do_install 阶段由 fdebi 自己决定交互,不让 apt 等待确认
    cfg.set("APT::Get::Assume-Yes", "true");
    // 本地 deb 允许降级覆盖,语义与 dpkg -i 一致
    cfg.set("APT::Get::allow-downgrades", "true");

    // 用户 apt_opts 不得改写可写缓存目录(未授权的 Analyze 也走 root)
    cfg.set("Dir::Cache", "/var/cache/apt/");
    cfg.set("Dir::Cache::Archives", "/var/cache/apt/archives/");
    cfg.set("Dir::Cache::pkgcache", "/var/cache/apt/pkgcache.bin");
    cfg.set("Dir::Cache::srcpkgcache", "/var/cache/apt/srcpkgcache.bin");

    match req.conf_policy {
        ConfPolicy::Keep => cfg.set_vector("Dpkg::Options", &vec!["--force-confold"]),
        ConfPolicy::Replace => cfg.set_vector("Dpkg::Options", &vec!["--force-confnew"]),
        ConfPolicy::Default => {}
    }
}

/// 允许调用方传入的 apt 选项范围。
///
/// 拒绝 `Dir`/`Dir::Cache*`:分析请求未经授权,不能让 root 把缓存写到
/// 调用方指定路径。只读的 `Dir::Etc::*` 等仍可用于自定义源。
pub fn allowed_opt(key: &str) -> bool {
    !key.is_empty() && key != "Dir" && !key.starts_with("Dir::Cache")
}

/// 本机架构(apt 架构列表的第一项)。
pub fn native_arch() -> Result<String, String> {
    Config::new()
        .get_architectures()
        .into_iter()
        .next()
        .ok_or_else(|| "cannot determine native architecture".to_string())
}

/// 将 apt 错误集合转为用户可见文本(发现的所有错误/警告)。
pub fn err_string(err: &AptErrors) -> String {
    err.to_string().trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::allowed_opt;

    #[test]
    fn rejects_write_capable_dirs() {
        assert!(!allowed_opt(""));
        assert!(!allowed_opt("Dir"));
        assert!(!allowed_opt("Dir::Cache"));
        assert!(!allowed_opt("Dir::Cache::Archives"));
        assert!(allowed_opt("Dir::Etc::sourcelist"));
        assert!(allowed_opt("Acquire::http::Proxy"));
    }
}
