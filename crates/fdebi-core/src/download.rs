//! 纯 Rust 下载层:ureq(rustls)获取依赖 deb + sha256 完整性校验。
//!
//! 信任边界(与 apt 的差异,文档化):不重验仓库 GPG 签名 —— 索引的
//! 真实性由 apt-get update 时验证,此处信任 indextargets 的 Trusted 标记;
//! 下载完整性由 Packages 索引中的 SHA256 保证。不可信来源的候选会被
//! 求解器标记为 unauthenticated,由 GUI 在安装前确认。

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::error::FdebiError;
use crate::i18n::gettext;
use crate::solver::DownloadSpec;

/// 依赖 deb 的本地缓存目录(守护进程以 root 运行,可写)
pub const CACHE_DIR: &str = "/var/cache/fdebi";

/// 下载进度回调
pub trait DownloadProgress {
    /// 单项开始(name,第几项,共几项)
    fn item_start(&self, name: &str, idx: usize, total: usize);
    /// 单项进度(已收字节;size 为索引声明值,未知为 0)
    fn item_progress(&self, name: &str, received: u64, size: u64);
    /// 单项完成(含缓存命中)
    fn item_done(&self, name: &str);
}

pub struct NoopProgress;
impl DownloadProgress for NoopProgress {
    fn item_start(&self, _: &str, _: usize, _: usize) {}
    fn item_progress(&self, _: &str, _: u64, _: u64) {}
    fn item_done(&self, _: &str) {}
}

/// 取消句柄(与安装层共用;下载无子进程,仅检查标志位)
pub use crate::install::CancelHandle;

/// 下载全部依赖 deb(已存在且校验通过的直接复用),返回本地文件路径列表
pub fn download_all(
    specs: &[DownloadSpec],
    progress: &dyn DownloadProgress,
    cancel: &CancelHandle,
) -> Result<Vec<PathBuf>, FdebiError> {
    std::fs::create_dir_all(CACHE_DIR)?;
    let mut paths = Vec::with_capacity(specs.len());
    let total = specs.len();
    for (idx, spec) in specs.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(FdebiError::Cancelled);
        }
        progress.item_start(&spec.name, idx + 1, total);
        if cached_ok(spec) {
            progress.item_done(&spec.name);
            paths.push(spec.dest.clone());
            continue;
        }
        fetch_one(spec, progress, cancel)?;
        paths.push(spec.dest.clone());
        progress.item_done(&spec.name);
    }
    Ok(paths)
}

/// 缓存命中判定:文件存在且 sha256 匹配
fn cached_ok(spec: &DownloadSpec) -> bool {
    if spec.sha256.is_empty() {
        return false;
    }
    File::open(&spec.dest)
        .map(|mut f| {
            let mut hasher = Sha256::new();
            std::io::copy(&mut f, &mut hasher).is_ok()
                && format!("{:x}", hasher.finalize()) == spec.sha256
        })
        .unwrap_or(false)
}

fn fetch_one(
    spec: &DownloadSpec,
    progress: &dyn DownloadProgress,
    cancel: &CancelHandle,
) -> Result<(), FdebiError> {
    let resp = ureq::get(&spec.url)
        .call()
        .map_err(|e| FdebiError::Download(spec.url.clone(), e.to_string()))?;
    if resp.status() != 200 {
        return Err(FdebiError::Download(
            spec.url.clone(),
            format!("HTTP {}", resp.status()),
        ));
    }
    let mut reader = resp.into_reader();
    let partial = spec.dest.with_extension("part");
    let mut file = File::create(&partial)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            drop(file);
            let _ = std::fs::remove_file(&partial);
            return Err(FdebiError::Cancelled);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| FdebiError::Download(spec.url.clone(), e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        received += n as u64;
        progress.item_progress(&spec.name, received, spec.size);
    }
    file.flush()?;
    drop(file);
    let got = format!("{:x}", hasher.finalize());
    if !spec.sha256.is_empty() && got != spec.sha256 {
        let _ = std::fs::remove_file(&partial);
        return Err(FdebiError::Download(
            spec.url.clone(),
            gettext("sha256 checksum mismatch"),
        ));
    }
    std::fs::rename(&partial, &spec.dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_progress_compiles() {
        let p = NoopProgress;
        p.item_start("x", 1, 1);
        p.item_progress("x", 1, 2);
        p.item_done("x");
    }
}
