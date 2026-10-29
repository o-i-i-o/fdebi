//! .deb 文件打开:ar 容器 → control.tar(gz/xz/zstd/无压缩)→ control 内容。
//! control 以原始字节保留(dpkg 语义,允许非 UTF-8 字节,如 gdebi-test6);
//! 字段解析在 lossy 转换后的文本上进行,仅用于展示。
//!
//! data.tar 的成员列表与内容提取经 `dpkg-deb --fsys-tarfile` 子进程:
//! 与 apt-get/dpkg 同为"系统能力经子进程"模式,一次覆盖全部压缩格式
//! (gzip/xz/zstd),避免逐格式引入解压依赖。

use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::control::parse_control;
use crate::error::FdebmError;

/// 已打开的 .deb:control 内容与解析结果
pub struct DebFile {
    path: PathBuf,
    control_tar: Vec<u8>,
    control_bytes: Vec<u8>,
    paragraphs: Vec<crate::control::ControlParagraph>,
}

impl DebFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FdebmError> {
        let path = path.as_ref();
        let file = File::open(path)
            .map_err(|e| FdebmError::OpenFailed(path.to_path_buf(), e.to_string()))?;
        let mut archive = ar::Archive::new(file);

        let mut control_tar: Option<Vec<u8>> = None;
        while let Some(entry) = archive.next_entry() {
            let mut entry = entry?;
            let name = entry.header().identifier().to_vec();
            if !name.starts_with(b"control.tar") {
                // data.tar 可能达 GB 级:定位到成员末尾,Drop 时便不再复制读取
                entry.seek(SeekFrom::End(0))?;
                continue;
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            control_tar = Some(data);
        }

        let control_tar = control_tar.ok_or_else(|| FdebmError::NotADeb(path.to_path_buf()))?;
        let control_bytes = extract_control_bytes(decompress_control_tar(&control_tar)?)?;
        let paragraphs = parse_control(&String::from_utf8_lossy(&control_bytes));
        Ok(Self {
            path: path.to_path_buf(),
            control_tar,
            control_bytes,
            paragraphs,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// control 文件原始字节(与 `dpkg-deb -f` 输出对齐)
    pub fn raw_control(&self) -> &[u8] {
        &self.control_bytes
    }

    /// 主段落字段值(lossy,展示用)
    pub fn field(&self, name: &str) -> Option<&str> {
        self.paragraphs.first()?.get(name)
    }

    /// 修剪后的字段值(展示用)
    pub fn field_trimmed(&self, name: &str) -> Option<&str> {
        self.field(name).map(str::trim)
    }

    /// control.tar 成员名列表(排序)
    pub fn control_filelist(&self) -> Result<Vec<String>, FdebmError> {
        let mut out = Vec::new();
        let plain = decompress_control_tar(&self.control_tar)?;
        for entry in tar::Archive::new(Cursor::new(plain)).entries()? {
            let entry = entry?;
            out.push(normalize_member(&entry.path()?.to_string_lossy()));
        }
        out.sort();
        Ok(out)
    }

    /// data.tar 成员名列表(按 tar 流顺序)
    pub fn data_filelist(&self) -> Result<Vec<String>, FdebmError> {
        let child = Command::new("dpkg-deb")
            .arg("--fsys-tarfile")
            .arg(&self.path)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut out = Vec::new();
        for entry in tar::Archive::new(child.stdout.expect("piped stdout")).entries()? {
            let entry = entry?;
            out.push(normalize_member(&entry.path()?.to_string_lossy()));
        }
        Ok(out)
    }

    /// control.tar 内单文件内容(name 为去前缀后的展示名)
    pub fn control_content(&self, name: &str) -> Result<Vec<u8>, FdebmError> {
        let plain = decompress_control_tar(&self.control_tar)?;
        for entry in tar::Archive::new(Cursor::new(plain)).entries()? {
            let mut entry = entry?;
            if normalize_member(&entry.path()?.to_string_lossy()) == name {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                return Ok(bytes);
            }
        }
        Ok(Vec::new())
    }

    /// data.tar 内单文件内容(name 为去前缀后的展示名)
    pub fn data_content(&self, name: &str) -> Result<Vec<u8>, FdebmError> {
        let child = Command::new("dpkg-deb")
            .arg("--fsys-tarfile")
            .arg(&self.path)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        for entry in tar::Archive::new(child.stdout.expect("piped stdout")).entries()? {
            let mut entry = entry?;
            if normalize_member(&entry.path()?.to_string_lossy()) == name {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                return Ok(bytes);
            }
        }
        Ok(Vec::new())
    }

    /// 包声明的 conffiles 列表(DEBIAN/conffiles 成员,每行一个绝对路径;
    /// 成员不存在时返回空表)
    pub fn conffiles(&self) -> Result<Vec<String>, FdebmError> {
        let raw = self.control_content("conffiles")?;
        Ok(String::from_utf8_lossy(&raw)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect())
    }
}

/// tar 成员名规范化:去 "./" 前缀;根目录项 "./" 保留原样
/// (与 python-apt 的 TarMember.name 语义一致)
fn normalize_member(name: &str) -> String {
    match name.strip_prefix("./") {
        Some("") => "./".to_string(),
        Some(n) => n.to_string(),
        None => name.to_string(),
    }
}

/// control.tar 原始字节 → 解压后的 tar 字节(魔数判定:gz/xz/zstd,
/// 其余按无压缩处理;均纯 Rust 后端,与索引解压同一套依赖)
fn decompress_control_tar(data: &[u8]) -> Result<Vec<u8>, FdebmError> {
    const GZ: [u8; 2] = [0x1f, 0x8b];
    const XZ: [u8; 6] = [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00];
    const ZSTD: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
    if data.starts_with(&GZ) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut out)
            .map_err(|e| FdebmError::ControlDecompress(e.to_string()))?;
        Ok(out)
    } else if data.starts_with(&XZ) {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut Cursor::new(data), &mut out)
            .map_err(|e| FdebmError::ControlDecompress(e.to_string()))?;
        Ok(out)
    } else if data.starts_with(&ZSTD) {
        let mut out = Vec::new();
        ruzstd::StreamingDecoder::new(data)
            .map_err(|e| FdebmError::ControlDecompress(e.to_string()))?
            .read_to_end(&mut out)
            .map_err(|e| FdebmError::ControlDecompress(e.to_string()))?;
        Ok(out)
    } else {
        Ok(data.to_vec())
    }
}

/// 解压后的 control.tar → control 文件字节
fn extract_control_bytes(plain_tar: Vec<u8>) -> Result<Vec<u8>, FdebmError> {
    let mut tar = tar::Archive::new(Cursor::new(plain_tar));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().and_then(|s| s.to_str()) == Some("control") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    Err(FdebmError::MissingControl)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库 tests/ 目录下的 gdebi-test4.deb(data.tar 含 "./..deb" 与
    /// "./.deb" 特殊文件名,是成员名规范化的关键样本)
    fn test4() -> DebFile {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/gdebi-test4.deb");
        DebFile::open(&path).expect("open gdebi-test4.deb")
    }

    #[test]
    fn data_filelist_matches_python_apt_semantics() {
        // 真值取自 python-apt: data 成员名去 "./" 前缀,根目录项保留 "./",
        // 顺序为 tar 流序
        let deb = test4();
        assert_eq!(deb.data_filelist().unwrap(), vec!["./", "..deb", ".deb"]);
    }

    #[test]
    fn control_filelist_is_sorted() {
        // 控制成员列表经排序;根目录项为 "./"
        let deb = test4();
        assert_eq!(deb.control_filelist().unwrap(), vec!["./", "control"]);
    }

    /// 包声明 conffiles 的解析(fixture 自带 conffiles 成员)
    #[test]
    fn conffiles_list_from_control() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conf-test-package_1.1.deb");
        let deb = DebFile::open(&path).expect("open conf-test-package_1.1.deb");
        assert_eq!(deb.conffiles().unwrap(), vec!["/etc/configuration-file"]);
        // 无 conffiles 成员的包返回空表
        let plain = test4();
        assert!(plain.conffiles().unwrap().is_empty());
    }

    #[test]
    fn data_content_extracts_bytes() {
        // 长度基准来自 dpkg-deb -c(test4 固定内容);内嵌 NUL 是
        // flow::content_preview 二进制判定与 MIME 展示的关键样本
        let deb = test4();
        let bin = deb.data_content("..deb").unwrap();
        assert_eq!(bin.len(), 904);
        assert!(bin.contains(&0));
        assert_eq!(deb.data_content(".deb").unwrap().len(), 1396);
    }

    #[test]
    fn content_of_missing_member_is_empty() {
        let deb = test4();
        assert!(deb.data_content("no-such-file").unwrap().is_empty());
        assert!(deb.control_content("no-such-file").unwrap().is_empty());
    }

    #[test]
    fn control_content_returns_control_file() {
        let deb = test4();
        let bytes = deb.control_content("control").unwrap();
        assert!(!bytes.is_empty());
        assert!(String::from_utf8_lossy(&bytes).contains("Package: gdebi-test4"));
    }

    /// 现代 dpkg 默认压缩(xz):control.tar.xz 打开/字段/成员列表
    #[test]
    fn opens_xz_compressed_control_tar() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/control-xz_1.0_all.deb");
        let deb = DebFile::open(&path).expect("open control.tar.xz deb");
        assert_eq!(deb.field_trimmed("Package"), Some("control-xz"));
        assert_eq!(deb.control_filelist().unwrap(), vec!["./", "control"]);
    }

    /// zstd 压缩(新版 dpkg 可选):control.tar.zst 打开/字段/成员列表
    #[test]
    fn opens_zstd_compressed_control_tar() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/control-zst_1.0_all.deb");
        let deb = DebFile::open(&path).expect("open control.tar.zst deb");
        assert_eq!(deb.field_trimmed("Package"), Some("control-zst"));
        assert_eq!(deb.control_filelist().unwrap(), vec!["./", "control"]);
    }

    /// dpkg-deb 压缩矩阵(-Zgzip/-Zxz/-Zzstd/-Znone,与 Ubuntu/Debian
    /// 各版本默认压缩兼容):open/字段/成员列表/ar 成员名/原始 control
    /// 字节全部对齐 dpkg-deb(1) 规范行为
    #[test]
    fn opens_all_dpkg_compression_types() {
        let cases = [
            ("ctrl-gz", "control.tar.gz"),
            ("ctrl-xz", "control.tar.xz"),
            ("ctrl-zstd", "control.tar.zst"),
            ("ctrl-none", "control.tar"),
        ];
        for (pkg, member) in cases {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../tests/{pkg}_1.0_all.deb"));
            // ar 成员名符合规范(control.tar.<ext> / control.tar)
            let mut names = Vec::new();
            let mut archive = ar::Archive::new(File::open(&path).unwrap());
            while let Some(entry) = archive.next_entry() {
                names.push(
                    String::from_utf8_lossy(entry.unwrap().header().identifier()).into_owned(),
                );
            }
            assert!(names.contains(&member.to_string()), "{pkg}: {names:?}");
            assert!(names.contains(&"debian-binary".to_string()), "{pkg}");

            let deb = DebFile::open(&path).unwrap_or_else(|e| panic!("open {pkg}: {e}"));
            assert_eq!(deb.field_trimmed("Package"), Some(pkg), "{pkg}");
            assert_eq!(
                deb.control_filelist().unwrap(),
                vec!["./", "control"],
                "{pkg}"
            );
            // control 原始字节与 dpkg-deb -f 逐字节一致(压缩不得改动内容)
            let out = Command::new("dpkg-deb")
                .arg("-f")
                .arg(&path)
                .output()
                .unwrap();
            assert!(out.status.success(), "dpkg-deb -f {pkg}");
            assert_eq!(
                deb.raw_control(),
                out.stdout.as_slice(),
                "{pkg} control bytes"
            );
        }
    }

    /// data.tar.zst(deb 全 zstd 场景):文件列表与内容提取
    /// 经 dpkg-deb --fsys-tarfile 链路
    #[test]
    fn data_tar_zstd_filelist_and_content() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/data-zst_1.0_all.deb");
        let deb = DebFile::open(&path).expect("open data.tar.zst deb");
        assert_eq!(deb.field_trimmed("Package"), Some("data-zst"));
        let files = deb.data_filelist().unwrap();
        assert!(
            files.contains(&"usr/share/doc/data-zst/README".to_string()),
            "{files:?}"
        );
        let readme = deb.data_content("usr/share/doc/data-zst/README").unwrap();
        assert_eq!(String::from_utf8_lossy(&readme), "hello-data\n");
    }

    /// 截断的压缩流不得 panic,必须返回 ControlDecompress
    /// (样本取自规范 fixture 的真实压缩流,截去后半)
    #[test]
    fn truncated_compressed_streams_report_decompress_error() {
        let truncate_member = |deb_path: &str, member: &str| -> Vec<u8> {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(deb_path);
            let mut archive = ar::Archive::new(File::open(&path).unwrap());
            while let Some(entry) = archive.next_entry() {
                let mut entry = entry.unwrap();
                if entry.header().identifier() == member.as_bytes() {
                    let mut data = Vec::new();
                    entry.read_to_end(&mut data).unwrap();
                    data.truncate(data.len() / 2);
                    return data;
                }
            }
            panic!("member {member} not found");
        };
        let gz = truncate_member("../../tests/ctrl-gz_1.0_all.deb", "control.tar.gz");
        let zst = truncate_member("../../tests/ctrl-zstd_1.0_all.deb", "control.tar.zst");
        for (what, data) in [("gz", gz), ("zstd", zst)] {
            match decompress_control_tar(&data) {
                Err(FdebmError::ControlDecompress(_)) => {}
                other => panic!("truncated {what}: expected ControlDecompress, got {other:?}"),
            }
        }
    }
}
