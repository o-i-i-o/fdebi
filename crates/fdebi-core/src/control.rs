//! 最小 deb822 解析:保留字段顺序与折行正文原貌,不解释字段语义。
//! 正确性由差分测试保证:与 `dpkg-deb -f` 输出逐字段对齐(见 diff-tests/)。

/// 一个 control 段落:字段按出现顺序保存,value 为 ':' 之后的原文
/// (首行通常带一个前导空格,折行正文保留原始缩进)。
#[derive(Debug, Clone, Default)]
pub struct ControlParagraph {
    fields: Vec<(String, String)>,
}

impl ControlParagraph {
    /// 取字段原文(未修剪)
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn push(&mut self, name: &str, value: String) {
        self.fields.push((name.to_string(), value));
    }

    fn append_continuation(&mut self, line: &str) {
        if let Some((_, v)) = self.fields.last_mut() {
            v.push('\n');
            v.push_str(line);
        }
    }
}

/// 字段名规则:首字符字母/数字,其余为字母/数字/'-'
fn is_field_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 解析 control 文本为段落列表(空行分段,'#' 注释行忽略)
pub fn parse_control(text: &str) -> Vec<ControlParagraph> {
    let mut out: Vec<ControlParagraph> = Vec::new();
    let mut cur = ControlParagraph::default();
    let mut in_paragraph = false;

    for line in text.lines() {
        if line.trim().is_empty() {
            if in_paragraph {
                out.push(std::mem::take(&mut cur));
                in_paragraph = false;
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim_end();
            if is_field_name(name) {
                cur.push(name, line[colon + 1..].to_string());
                in_paragraph = true;
                continue;
            }
        }
        // 续行:以空格/制表符开头,归入最近一个字段
        if line.starts_with(' ') || line.starts_with('\t') {
            cur.append_continuation(line);
        }
    }
    if in_paragraph {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_description() {
        let text = "Package: foo\nDescription: short\n long text\n .\n more\n";
        let paras = parse_control(text);
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].get("Package"), Some(" foo"));
        assert_eq!(
            paras[0].get("Description"),
            Some(" short\n long text\n .\n more")
        );
    }

    #[test]
    fn multiple_paragraphs_and_comments() {
        let text = "# comment\nPackage: foo\n\nPackage: bar\nVersion: 1\n";
        let paras = parse_control(text);
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[1].get("Version"), Some(" 1"));
    }
}
