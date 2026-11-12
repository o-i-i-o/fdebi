//! dpkg 版本号比较,与 `dpkg --compare-versions` 语义逐字对齐。
//! 实现为 dpkg verrevcmp 的直译(参考 dpkg 源码 lib/dpkg/parsehelp.c)。
//! 正确性由差分测试保证:oracle 为 dpkg 本体(见 diff-tests/)。

use std::cmp::Ordering;

/// dpkg 字符序:'~' 最小,字母次之,其余符号最大;缺失视为 0
fn order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_digit() => 0, // 非数字段中不会出现
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

/// 交替比较非数字段(按字符序)与数字段(去前导零后逐位比较)
fn verrevcmp(mut a: &[u8], mut b: &[u8]) -> Ordering {
    while !a.is_empty() || !b.is_empty() {
        let mut first_diff = 0i32;

        while (!a.is_empty() && !a[0].is_ascii_digit()) || (!b.is_empty() && !b[0].is_ascii_digit())
        {
            let d = order(a.first().copied()) - order(b.first().copied());
            if d != 0 {
                return d.cmp(&0);
            }
            a = &a[1..];
            b = &b[1..];
        }

        while a.first() == Some(&b'0') {
            a = &a[1..];
        }
        while b.first() == Some(&b'0') {
            b = &b[1..];
        }
        while a.first().is_some_and(|c| c.is_ascii_digit())
            && b.first().is_some_and(|c| c.is_ascii_digit())
        {
            if first_diff == 0 {
                first_diff = a[0] as i32 - b[0] as i32;
            }
            a = &a[1..];
            b = &b[1..];
        }
        if a.first().is_some_and(|c| c.is_ascii_digit()) {
            return Ordering::Greater;
        }
        if b.first().is_some_and(|c| c.is_ascii_digit()) {
            return Ordering::Less;
        }
        if first_diff != 0 {
            return first_diff.cmp(&0);
        }
    }
    Ordering::Equal
}

/// 拆 epoch:首个 ':' 前若全为数字则为 epoch,否则整体无 epoch
fn split_epoch(v: &str) -> (u64, &str) {
    if let Some(pos) = v.find(':') {
        let head = &v[..pos];
        if !head.is_empty() && head.bytes().all(|c| c.is_ascii_digit()) {
            return (head.parse().unwrap_or(0), &v[pos + 1..]);
        }
    }
    (0, v)
}

/// 拆 debian revision:最后一个 '-' 之后,缺省视为 "0"
fn split_revision(v: &str) -> (&str, &str) {
    match v.rfind('-') {
        Some(p) => (&v[..p], &v[p + 1..]),
        None => (v, "0"),
    }
}

/// 比较两个 Debian 版本号(对应 dpkg --compare-versions)
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    let (epoch_a, rest_a) = split_epoch(a);
    let (epoch_b, rest_b) = split_epoch(b);
    let (up_a, rev_a) = split_revision(rest_a);
    let (up_b, rev_b) = split_revision(rest_b);
    epoch_a
        .cmp(&epoch_b)
        .then_with(|| verrevcmp(up_a.as_bytes(), up_b.as_bytes()))
        .then_with(|| verrevcmp(rev_a.as_bytes(), rev_b.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    #[test]
    fn basics() {
        assert_eq!(cmp_version("1.0", "1.0"), Equal);
        assert_eq!(cmp_version("1.0", "2.0"), Less);
        assert_eq!(cmp_version("1.0~rc1", "1.0"), Less);
        assert_eq!(cmp_version("1.0", "1.0~rc1"), Greater);
        assert_eq!(cmp_version("1:0.9", "0.99"), Greater);
        assert_eq!(cmp_version("1.0-1", "1.0"), Greater);
        assert_eq!(cmp_version("1.0a", "1.0"), Greater);
        assert_eq!(cmp_version("01.0", "1.0"), Equal);
        assert_eq!(cmp_version("1.0", "1.0+b1"), Less);
    }
}
