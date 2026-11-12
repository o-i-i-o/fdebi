//! GUI 摘要生成:从已打开的包提取呈现字段,渲染细节由前端决定。

use std::io::Write;
use std::process::{Command, Stdio};

use crate::debfile::DebFile;
use crate::i18n::tf;

/// 呈现给用户的包摘要
#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub description: String,
    /// Depends 字段按逗号拆分
    pub depends: Vec<String>,
}

fn field_or(deb: &DebFile, name: &str) -> String {
    deb.field_trimmed(name).unwrap_or("").to_string()
}

/// 从已打开的包生成摘要(GUI 在 load 流程中复用同一 DebFile)
pub fn describe_deb(deb: &DebFile) -> PackageInfo {
    PackageInfo {
        package: field_or(deb, "Package"),
        version: field_or(deb, "Version"),
        architecture: field_or(deb, "Architecture"),
        description: field_or(deb, "Description"),
        depends: deb
            .field_trimmed("Depends")
            .map(|s| s.split(',').map(str::trim).map(String::from).collect())
            .unwrap_or_default(),
    }
}

/// Debian long description → (摘要, 正文)。
///
/// 按 Debian Policy 5.6.13 的格式约定处理(GUI 只负责加粗摘要):
/// - 首行是摘要,单独显示;
/// - 续行前导的一个空格是格式空格,去掉;更深的缩进属于 verbatim 块,保留;
/// - 单独的 "." 行表示段落分隔,转为空行;
/// - `* `/`- ` 项目符号统一为 "• "(避免 GTK 文本视图里星号看起来像
///   强调标记),条目内的相对缩进保留。
///
/// 与 gdebi 的实现不同:不把换行折叠成空格再按双空格切段 —— 那样会
/// 破坏段落边界、代码块与嵌套列表。
pub fn description_parts(raw: &str) -> (String, String) {
    let mut lines = raw.split('\n');
    let summary = lines.next().unwrap_or("").trim().to_string();
    let mut body: Vec<String> = Vec::new();
    for line in lines {
        let line = line.strip_prefix(' ').unwrap_or(line);
        let trimmed = line.trim_end();
        if trimmed.trim() == "." {
            body.push(String::new());
            continue;
        }
        let content = trimmed.trim_start();
        if let Some(rest) = content
            .strip_prefix("* ")
            .or_else(|| content.strip_prefix("- "))
        {
            body.push(format!("• {rest}"));
        } else {
            body.push(trimmed.to_string());
        }
    }
    while body.first().is_some_and(String::is_empty) {
        body.remove(0);
    }
    while body.last().is_some_and(String::is_empty) {
        body.pop();
    }
    (summary, body.join("\n"))
}

/// 探测二进制所需的魔数通常位于文件头部,64 KiB 足够且避免大成员全量写入
const MIME_PROBE_LEN: usize = 64 * 1024;

/// 成员内容 → 展示文本:文本原样显示,二进制显示文件类型。
/// 内嵌 NUL 的字节流经 glib 的 &str→C 字符串转换非法
/// (debug 构建直接 panic),且二进制内容塞入文本视图本就是乱码,
/// 判定后展示 MIME 类型更可读。
pub fn content_preview(bytes: &[u8]) -> String {
    // 含 NUL 即视为二进制(git 同款启发式);无 NUL 时 lossy 结果
    // 不会含 NUL,保证前端 set_text 路径安全
    if !bytes.contains(&0) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let size = bytes.len();
    match mime_type(bytes) {
        Some(mime) => tf("Binary file ({0}), {1} bytes", &[&mime, &size.to_string()]),
        None => tf("Binary file, {0} bytes", &[&size.to_string()]),
    }
}

/// 经 file(1) 探测 MIME 类型;file 不可用或探测失败返回 None
fn mime_type(bytes: &[u8]) -> Option<String> {
    let probe = &bytes[..bytes.len().min(MIME_PROBE_LEN)];
    let mut child = Command::new("file")
        .args(["--mime-type", "-b", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(probe)
        .ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mime = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!mime.is_empty()).then_some(mime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_first_line_is_summary() {
        let (summary, body) = description_parts("summary line\n long text\n");
        assert_eq!(summary, "summary line");
        assert_eq!(body, "long text");
    }

    #[test]
    fn description_dot_is_paragraph_break() {
        let (_, body) = description_parts("s\n first\n .\n second\n");
        assert_eq!(body, "first\n\nsecond");
    }

    #[test]
    fn description_bullets_are_normalized() {
        let (_, body) = description_parts("s\n  * one\n  - two\n");
        assert_eq!(body, "• one\n• two");
    }

    #[test]
    fn description_preserves_verbatim_indent() {
        let (_, body) = description_parts("s\n     indented code\n");
        assert_eq!(body, "    indented code");
    }

    #[test]
    fn description_without_body() {
        let (summary, body) = description_parts("only summary");
        assert_eq!(summary, "only summary");
        assert!(body.is_empty());
    }

    #[test]
    fn text_content_shown_verbatim() {
        assert_eq!(content_preview(b"hello world"), "hello world");
    }

    #[test]
    fn binary_content_shows_mime_type() {
        // ELF 头:file 应识别为可执行/目标文件
        let elf = b"\x7fELF\x02\x01\x01\x00\x00binary";
        let preview = content_preview(elf);
        assert!(preview.starts_with("Binary file ("), "{preview}");
        assert!(!preview.contains('\0'));
    }

    #[test]
    fn binary_content_never_contains_nul() {
        // file 可用时给 MIME 类型,不可用时回退纯字节数;两种分支
        // 均以 "Binary file" 开头且不含 NUL(前端 set_text 安全)
        let preview = content_preview(&[0x00, 0x01, 0x02]);
        assert!(preview.starts_with("Binary file"), "{preview}");
        assert!(!preview.contains('\0'));
    }
}
