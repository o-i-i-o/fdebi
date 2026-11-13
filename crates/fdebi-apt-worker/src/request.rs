//! 请求文件读取与校验。
//!
//! 请求由 root daemon 写入,worker 必须以同样的严格条件重新检查,
//! 避免其他用户替换文件内容或借软链接读取任意路径。

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use fdebi_apt_proto::Request;

pub fn read(path: &Path) -> Result<Request, String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot stat request file {}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err("request file is not a regular file".into());
    }
    if meta.mode() & 0o077 != 0 {
        return Err("request file permissions are too broad".into());
    }
    if meta.nlink() != 1 {
        return Err("request file must not be hard-linked".into());
    }
    if meta.uid() != 0 {
        return Err("request file is not owned by root".into());
    }

    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read request file {}: {e}", path.display()))?;
    // 读取后立即删除,避免事务期间残留可被替换的请求
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&data).map_err(|e| format!("invalid request JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};

    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fdebi-req-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_symlink() {
        let dir = TempDir::new("symlink");
        let real = dir.0.join("real.json");
        std::fs::write(&real, "{}").unwrap();
        let link = dir.0.join("link.json");
        symlink(&real, &link).unwrap();
        assert!(read(&link).unwrap_err().contains("not a regular file"));
    }

    #[test]
    fn rejects_broad_permissions() {
        let dir = TempDir::new("mode");
        let file = dir.0.join("req.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, PermissionsExt::from_mode(0o644)).unwrap();
        assert!(read(&file).unwrap_err().contains("permissions are too broad"));
    }

    #[test]
    fn rejects_hard_link() {
        let dir = TempDir::new("link");
        let file = dir.0.join("req.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, PermissionsExt::from_mode(0o600)).unwrap();
        std::fs::hard_link(&file, dir.0.join("other.json")).unwrap();
        assert!(read(&file).unwrap_err().contains("hard-linked"));
    }

    #[test]
    fn rejects_non_root_owner() {
        // 测试进程通常非 root:0600 普通文件仍应因属主不符被拒绝;
        // 若测试以 root 运行则跳过(无法构造非 root 属主)。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = TempDir::new("owner");
        let file = dir.0.join("req.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, PermissionsExt::from_mode(0o600)).unwrap();
        assert!(read(&file).unwrap_err().contains("not owned by root"));
    }
}
