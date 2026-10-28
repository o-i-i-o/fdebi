#!/usr/bin/env python3
"""fdebm 的 po 文件校验器。

用法::

    python3 po/po_lint.py po/*.po        # 校验给定文件
    make -C po lint                      # 同上(推荐)

校验项(错误级,任一命中则退出码非 0):

- gettext 自身语法:调用 ``msgfmt --check``(若系统存在 msgfmt)
- 头部字段:Language 与文件名一致、Plural-Forms 与语言复数规则一致、
  charset 为 UTF-8、Project-Id-Version 指向 fdebm、无翻译器占位符、
  无 Launchpad 遗留字段
- 条目状态:不得残留 fuzzy 条目(运行期不生效)、不得残留 ``#~`` 废弃条目
- 占位符:``{0}``/``{1}`` 编号集合必须与 msgid 完全一致(复数条目逐条比对);
  译文不得出现 printf 风格 ``%s``/``%d``(fdebm 统一使用 Rust 风格占位符)
- 标记:HTML 标签集合必须与 msgid 一致(如 ``<b>``/``</b>``、``<span …>``)
- 空白:译文与 msgid 的首尾空白、结尾换行符必须一致

未翻译(空 msgstr)只作提示,不视为错误——各语种进度不同。

本模块同时向 ``po/`` 下的维护脚本提供 :data:`LANGS` 语言表以及
:func:`parse_po` / :func:`serialize_po` 这两个保序的 po 读写函数。
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

# 语言表:代码 -> (英文名, nplurals, plural 表达式)
# 复数表达式取自 GNU gettext 手册中各语言的标准定义。
LANGS = {
    "an": ("Aragonese", 2, "(n != 1)"),
    "ar": ("Arabic", 6,
           "(n==0 ? 0 : n==1 ? 1 : n==2 ? 2 : n%100>=3 && n%100<=10 ? 3"
           " : n%100>=11 ? 4 : 5)"),
    "ast": ("Asturian", 2, "(n != 1)"),
    "bg": ("Bulgarian", 2, "(n != 1)"),
    "bn": ("Bengali", 2, "(n != 1)"),
    "bs": ("Bosnian", 3,
           "(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4"
           " && (n%100<10 || n%100>=20) ? 1 : 2)"),
    "ca": ("Catalan", 2, "(n != 1)"),
    "ca@valencia": ("Valencian", 2, "(n != 1)"),
    "cs": ("Czech", 3, "(n==1) ? 0 : (n>=2 && n<=4) ? 1 : 2"),
    "da": ("Danish", 2, "(n != 1)"),
    "de": ("German", 2, "(n != 1)"),
    "el": ("Greek", 2, "(n != 1)"),
    "en_AU": ("English (Australia)", 2, "(n != 1)"),
    "en_CA": ("English (Canada)", 2, "(n != 1)"),
    "en_GB": ("English (United Kingdom)", 2, "(n != 1)"),
    "eo": ("Esperanto", 2, "(n != 1)"),
    "es": ("Spanish", 2, "(n != 1)"),
    "et": ("Estonian", 2, "(n != 1)"),
    "eu": ("Basque", 2, "(n != 1)"),
    "fa": ("Persian", 2, "(n > 1)"),
    "fi": ("Finnish", 2, "(n != 1)"),
    "fo": ("Faroese", 2, "(n != 1)"),
    "fr": ("French", 2, "(n > 1)"),
    "gl": ("Galician", 2, "(n != 1)"),
    "he": ("Hebrew", 2, "(n != 1)"),
    "hr": ("Croatian", 3,
           "(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4"
           " && (n%100<10 || n%100>=20) ? 1 : 2)"),
    "hu": ("Hungarian", 2, "(n != 1)"),
    "hy": ("Armenian", 2, "(n != 1)"),
    "id": ("Indonesian", 1, "0"),
    "it": ("Italian", 2, "(n != 1)"),
    "ja": ("Japanese", 1, "0"),
    "ko": ("Korean", 1, "0"),
    "ku": ("Kurdish", 2, "(n != 1)"),
    "lt": ("Lithuanian", 3,
           "(n%10==1 && n%100!=11 ? 0 : n%10>=2 && (n%100<10"
           " || n%100>=20) ? 1 : 2)"),
    "mr": ("Marathi", 2, "(n != 1)"),
    "ms": ("Malay", 1, "0"),
    "nb": ("Norwegian Bokmal", 2, "(n != 1)"),
    "nl": ("Dutch", 2, "(n != 1)"),
    "oc": ("Occitan", 2, "(n > 1)"),
    "pl": ("Polish", 3,
           "(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2)"),
    "pt": ("Portuguese", 2, "(n != 1)"),
    "pt_BR": ("Portuguese (Brazil)", 2, "(n > 1)"),
    "ro": ("Romanian", 3,
           "(n==1 ? 0 : (n==0 || (n%100 > 0 && n%100 < 20)) ? 1 : 2)"),
    "ru": ("Russian", 3,
           "(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4"
           " && (n%100<10 || n%100>=20) ? 1 : 2)"),
    "si": ("Sinhala", 2, "(n != 1)"),
    "sk": ("Slovak", 3, "(n==1) ? 0 : (n>=2 && n<=4) ? 1 : 2"),
    "sl": ("Slovenian", 4,
           "(n%100==1 ? 0 : n%100==2 ? 1 : n%100==3 || n%100==4 ? 2 : 3)"),
    "sr": ("Serbian", 3,
           "(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4"
           " && (n%100<10 || n%100>=20) ? 1 : 2)"),
    "sv": ("Swedish", 2, "(n != 1)"),
    "te": ("Telugu", 2, "(n != 1)"),
    "th": ("Thai", 1, "0"),
    "tl": ("Filipino", 2, "(n > 1)"),
    "tr": ("Turkish", 2, "(n != 1)"),
    "uk": ("Ukrainian", 3,
           "(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4"
           " && (n%100<10 || n%100>=20) ? 1 : 2)"),
    "ur": ("Urdu", 2, "(n != 1)"),
    "zh_CN": ("Chinese (Simplified)", 1, "0"),
    "zh_HK": ("Chinese (Hong Kong)", 1, "0"),
    "zh_TW": ("Chinese (Traditional)", 1, "0"),
}

_DECODE = {
    "n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\",
    "a": "\a", "b": "\b", "f": "\f", "v": "\v",
}


def _unescape(segment: str) -> str:
    """把 po 里的一段 ``"…"`` 解开成真实字符串。"""
    body = segment.strip()
    if body.startswith('"') and body.endswith('"') and len(body) >= 2:
        body = body[1:-1]
    out: list[str] = []
    i = 0
    while i < len(body):
        ch = body[i]
        if ch == "\\" and i + 1 < len(body):
            nxt = body[i + 1]
            if nxt in _DECODE:
                out.append(_DECODE[nxt])
                i += 2
                continue
            if nxt.isdigit():
                j = i + 1
                digits = ""
                while j < len(body) and len(digits) < 3 and body[j] in "01234567":
                    digits += body[j]
                    j += 1
                out.append(chr(int(digits, 8)))
                i = j
                continue
            out.append(nxt)
            i += 2
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def _escape(value: str) -> str:
    """把真实字符串转成 po 里的单段 ``"…"``(不含外层引号)。"""
    return (value.replace("\\", "\\\\").replace('"', '\\"')
            .replace("\n", "\\n").replace("\t", "\\t").replace("\r", "\\r"))


class Block:
    """一条 po 条目(含文件头)的保序表示。"""

    __slots__ = ("comments", "fields")

    def __init__(self, comments=None, fields=None):
        self.comments: list[str] = comments or []
        # [(name, value)],name 为 msgid / msgid_plural / msgstr / msgstr[N]
        self.fields: list[tuple[str, str]] = fields or []

    # -- 便捷访问 ------------------------------------------------------
    def get(self, name: str) -> str | None:
        for n, v in self.fields:
            if n == name:
                return v
        return None

    def plurals(self) -> list[tuple[int, str]]:
        out = []
        for name, value in self.fields:
            m = re.fullmatch(r"msgstr\[(\d+)\]", name)
            if m:
                out.append((int(m.group(1)), value))
        return out

    @property
    def msgid(self) -> str:
        return self.get("msgid") or ""

    @property
    def msgstr(self) -> str:
        """非复数条目取 msgstr,复数条目取 msgstr[0]。"""
        if self.plurals():
            forms = self.plurals()
            return forms[0][1] if forms else ""
        return self.get("msgstr") or ""

    @property
    def is_header(self) -> bool:
        return self.msgid == "" and self.get("msgid") is not None

    @property
    def is_obsolete(self) -> bool:
        return any(c.startswith("#~") for c in self.comments)

    @property
    def is_fuzzy(self) -> bool:
        return any(re.match(r"^#\s*,[^#]*\bfuzzy\b", c) for c in self.comments)

    def previous_msgid(self) -> str | None:
        """取出 ``#| msgid …`` 记录的旧 msgid(fuzzy 条目的对照文本)。"""
        for idx, line in enumerate(self.comments):
            m = re.match(r'^#\|\s*msgid\s+(".*)$', line)
            if not m:
                continue
            value = _unescape(m.group(1))
            for follow in self.comments[idx + 1:]:
                fm = re.match(r'^#\|\s+(".*)$', follow)
                if not fm:
                    break
                value += _unescape(fm.group(1))
            return value
        return None

    def flags(self) -> list[str]:
        for line in self.comments:
            m = re.match(r"^#,\s*(.*)$", line)
            if m:
                return [f.strip() for f in m.group(1).split(",")]
        return []

    def set_flags(self, flags: list[str]) -> None:
        flags = [f for f in flags if f]
        kept = []
        dropped = False
        for line in self.comments:
            if re.match(r"^#\s*,", line) and not dropped:
                dropped = True
                if flags:
                    kept.append("#, " + ", ".join(flags))
                continue
            kept.append(line)
        if flags and not dropped:
            kept.insert(0, "#, " + ", ".join(flags))
        self.comments = kept

    def drop_previous(self) -> None:
        self.comments = [c for c in self.comments if not c.startswith("#|")]

    def set_plural(self, forms: list[str]) -> None:
        """整体替换 msgstr[N] 列表。"""
        self.fields = [(n, v) for n, v in self.fields
                       if not re.fullmatch(r"msgstr\[\d+\]", n)]
        for i, value in enumerate(forms):
            self.fields.append((f"msgstr[{i}]", value))


def parse_po(text: str) -> list[Block]:
    """解析 po 文本为 Block 列表,保持原有顺序。"""
    blocks: list[Block] = []
    cur = Block()
    field_idx: int | None = None
    for raw in text.splitlines():
        if not raw.strip():
            if cur.fields:
                blocks.append(cur)
                cur = Block()
                field_idx = None
            continue
        if raw.startswith("#"):
            if cur.fields:
                blocks.append(cur)
                cur = Block()
                field_idx = None
            cur.comments.append(raw)
            continue
        if raw.startswith('"'):
            if field_idx is None:
                raise ValueError(f"无法归位的续行: {raw!r}")
            name, value = cur.fields[field_idx]
            cur.fields[field_idx] = (name, value + _unescape(raw))
            continue
        m = re.match(r"^(msgid_plural|msgid|msgstr(?:\[\d+\])?)\s+(.*)$", raw)
        if not m:
            raise ValueError(f"无法识别的行: {raw!r}")
        cur.fields.append((m.group(1), _unescape(m.group(2))))
        field_idx = len(cur.fields) - 1
    if cur.fields:
        blocks.append(cur)
    return blocks


def serialize_po(blocks: list[Block]) -> str:
    """把 Block 列表写回 po 文本(单行字符串,交给 msgcat 折行)。"""
    out: list[str] = []
    for block in blocks:
        chunk = list(block.comments)
        for name, value in block.fields:
            chunk.append(f'{name} "{_escape(value)}"')
        out.append("\n".join(chunk))
    return "\n\n".join(out) + "\n"


def header_fields(block: Block) -> dict[str, str]:
    """将文件头的 msgstr 解析成字段字典。"""
    fields: dict[str, str] = {}
    for line in (block.get("msgstr") or "").splitlines():
        if not line.strip():
            continue
        key, _, value = line.partition(":")
        fields[key.strip()] = value.strip()
    return fields


# -- 校验 ---------------------------------------------------------------

PRINTF = re.compile(r"%(?:\d+\$)?[-+ 0#']*\d*(?:\.\d+)?[sdifuxX]")
BRACE = re.compile(r"\{(\d+)\}")


def _tags(text: str) -> list[str]:
    return sorted(re.sub(r"\s+", " ", t) for t in re.findall(r"<[^>]+>", text))


def _edge(text: str) -> tuple[str, str, bool]:
    return (text[: len(text) - len(text.lstrip())],
            text[len(text.rstrip()):],
            text.endswith("\n"))


def check_file(path: Path) -> tuple[list[str], list[str], dict[str, int]]:
    """返回 (错误列表, 警告列表, 统计)。"""
    errors: list[str] = []
    warnings: list[str] = []
    stats = {"entries": 0, "translated": 0, "fuzzy": 0, "empty": 0}
    lang = path.stem
    blocks = parse_po(path.read_text(encoding="utf-8"))
    if not blocks or not blocks[0].is_header:
        return ["缺少文件头条目"], warnings, stats

    header = header_fields(blocks[0])
    if blocks[0].is_fuzzy:
        errors.append("文件头带 fuzzy 标记,元数据会被部分工具忽略")
    info = LANGS.get(lang)
    if info is None:
        errors.append(f"未知语言代码 {lang!r}(需登记到 po_lint.py 的 LANGS)")
    else:
        name, nplurals, expr = info
        want = f"nplurals={nplurals}; plural={expr};"
        got = (header.get("Plural-Forms") or "").replace("  ", " ").strip()
        if not got:
            errors.append("头部缺少 Plural-Forms")
        elif re.sub(r"\s+", " ", got) != re.sub(r"\s+", " ", want):
            errors.append(f"Plural-Forms 不符: 期望 {want!r}, 实际 {got!r}")
        if header.get("Language", "") != lang:
            errors.append(f"头部 Language 应为 {lang!r}, 实际 {header.get('Language', '')!r}")

    if not header.get("Project-Id-Version", "").startswith("fdebm"):
        errors.append(f"Project-Id-Version 未指向 fdebm: {header.get('Project-Id-Version')!r}")
    if "UTF-8" not in header.get("Content-Type", ""):
        errors.append("Content-Type 必须声明 charset=UTF-8")
    if header.get("MIME-Version") != "1.0":
        errors.append("MIME-Version 必须为 1.0")
    if header.get("Content-Transfer-Encoding") != "8bit":
        errors.append("Content-Transfer-Encoding 必须为 8bit")
    if not header.get("Report-Msgid-Bugs-To"):
        errors.append("头部缺少 Report-Msgid-Bugs-To")
    if "FULL NAME" in header.get("Last-Translator", "FULL NAME"):
        errors.append("Last-Translator 仍是 xgettext 占位符")
    if "LL@li.org" in header.get("Language-Team", ""):
        errors.append("Language-Team 仍是 xgettext 占位符")
    for stale in ("X-Launchpad-Export-Date", "X-Generator", "X-Poedit-Language"):
        if stale in header:
            errors.append(f"头部残留过时字段 {stale}")

    for block in blocks[1:]:
        where = block.msgid.splitlines()[0][:48] if block.msgid else "<空 msgid>"
        if block.is_obsolete:
            errors.append(f"[{where}] 残留废弃条目(#~),请清除")
            continue
        stats["entries"] += 1
        if block.is_fuzzy:
            stats["fuzzy"] += 1
            errors.append(f"[{where}] 仍是 fuzzy 状态(运行期不生效)")
        forms = block.plurals()
        if block.get("msgid_plural") is not None:
            if info:
                want = info[1]
                if len(forms) != want:
                    errors.append(f"[{where}] 复数形式数为 {len(forms)},应为 {want}")
            texts = [v for _, v in forms]
        else:
            texts = [block.get("msgstr") or ""]

        translated = [t for t in texts if t.strip()]
        if not translated:
            stats["empty"] += 1
            warnings.append(f"[{where}] 未翻译")
            continue
        stats["translated"] += 1

        want_ids = set(BRACE.findall(block.msgid))
        want_tags = _tags(block.msgid)
        want_edge = _edge(block.msgid)
        for idx, text in enumerate(texts):
            if not text.strip():
                continue
            suffix = "" if len(texts) == 1 else f" msgstr[{idx}]"
            if PRINTF.search(text):
                errors.append(f"[{where}]{suffix} 出现 printf 占位符 "
                              f"{PRINTF.search(text).group(0)!r},应为 {{N}}")
            got_ids = set(BRACE.findall(text))
            if got_ids != want_ids:
                errors.append(f"[{where}]{suffix} 占位符不一致: "
                              f"msgid={sorted(want_ids)} 译文={sorted(got_ids)}")
            if _tags(text) != want_tags:
                errors.append(f"[{where}]{suffix} HTML 标签不一致: "
                              f"msgid={want_tags} 译文={_tags(text)}")
            if _edge(text) != want_edge:
                errors.append(f"[{where}]{suffix} 首尾空白/换行与 msgid 不一致")
    return errors, warnings, stats


def msgfmt_check(path: Path) -> list[str]:
    """用 msgfmt --check 做一遍 gettext 官方语法校验。"""
    try:
        proc = subprocess.run(
            ["msgfmt", "--check", "-o", "/dev/null", str(path)],
            capture_output=True, text=True, check=False)
    except FileNotFoundError:
        return ["未找到 msgfmt(请安装 gettext),已跳过语法校验"]
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout).strip().replace("\n", " ")
        return [f"msgfmt --check 失败: {detail}"]
    return []


def main(argv: list[str]) -> int:
    verbose = any(a in ("-v", "--verbose") for a in argv)
    paths = [Path(a) for a in argv if not a.startswith("-")]
    if not paths:
        paths = sorted(Path(".").glob("*.po"))
    if not paths:
        print("没有找到 .po 文件", file=sys.stderr)
        return 2
    total_err = 0
    rows = []
    for path in paths:
        errors, warnings, stats = check_file(path)
        errors += msgfmt_check(path)
        total_err += len(errors)
        rows.append((path.name, stats, len(errors), len(warnings)))
        for err in errors:
            print(f"✗ {path.name}: {err}")
        if verbose:
            for warn in warnings:
                print(f"· {path.name}: {warn}")

    print()
    print(f"{'文件':<16}{'条目':>5}{'已译':>6}{'未译':>6}{'错误':>6}")
    for name, stats, nerr, _ in rows:
        print(f"{name:<16}{stats['entries']:>5}{stats['translated']:>6}"
              f"{stats['empty']:>6}{nerr:>6}")
    print(f"\n共 {len(rows)} 个文件,{total_err} 个错误")
    return 1 if total_err else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
