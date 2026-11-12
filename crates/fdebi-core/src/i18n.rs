//! gettext 国际化封装(Linux 标准 gettext 流程,域为 `fdebi`;
//!
//! - 运行期:二进制启动早期调用 [`init`](完成 setlocale / bindtextdomain /
//!   textdomain);目录优先级 环境变量 `FDEBI_LOCALEDIR` > 编译期 `LOCALEDIR`
//!   (打包安装,如 `LOCALEDIR=/usr/local/share/locale cargo build`)>
//!   `/usr/share/locale`
//! - 提取:po/Makefile 的 `pot` 目标经 xgettext 扫描源码生成 fdebi.pot
//! - 编译:`mo` 目标经 msgfmt 产出 locale/<lang>/LC_MESSAGES/fdebi.mo
//!
//! gettext 链接系统 libintl(gettext-system 特性),与 glibc 协作:
//! 未安装翻译或 C locale 下 gettext 直接回落英文 msgid。
//!
//! 带参数的文案统一使用 `{0}`/`{1}` 编号占位符(语序由译文决定),
//! 经 [`tf`] 翻译后代入;xgettext 以 `--keyword=tf:1` 提取。

use std::ffi::OsString;

use gettextrs::{LocaleCategory, bindtextdomain, setlocale, textdomain};
pub use gettextrs::{gettext, ngettext};

/// 消息目录域名(独立命名,避免与其他 deb 安装器的 .mo 装包路径冲突)
pub const DOMAIN: &str = "fdebi";

/// 未显式指定时的系统安装域
const DEFAULT_LOCALEDIR: &str = "/usr/share/locale";

/// 初始化 locale 与消息目录绑定;在进程首次取串前调用一次
pub fn init() {
    // 空串 = 按环境变量(LANG / LC_*)选择语言;GTK 自身也依赖此调用
    setlocale(LocaleCategory::LcAll, "");
    let dir: OsString = std::env::var_os("FDEBI_LOCALEDIR")
        .unwrap_or_else(|| option_env!("LOCALEDIR").unwrap_or(DEFAULT_LOCALEDIR).into());
    if let Err(e) = bindtextdomain(DOMAIN, dir.to_string_lossy().into_owned()) {
        eprintln!("bindtextdomain({DOMAIN}) failed: {e}");
        return;
    }
    if let Err(e) = textdomain(DOMAIN) {
        eprintln!("textdomain({DOMAIN}) failed: {e}");
    }
}

/// 翻译带 `{0}`/`{1}` 编号占位符的模板并代入参数。
/// 未命中翻译时模板即英文原文。
pub fn tf(template: &str, args: &[&str]) -> String {
    let mut s = gettext(template);
    for (i, arg) in args.iter().enumerate() {
        s = s.replace(&format!("{{{i}}}"), arg);
    }
    s
}
