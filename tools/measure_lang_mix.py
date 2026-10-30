#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""混排占比 + frontmatter lang 覆盖率测量（D208 / D204）。

D208 的口径：N1 按 chunk、N2 按文档、N3 按答案 span。本脚本先跑 **N2（文档级）**
作为分布的第一次采样；chunk 级要等 ingest 落 `analysis_lang`（D210）之后才有真 chunk。

统计**排除代码块 / 公式 / 引用块**（D208 明令），否则数字被污染。

纯标准库。用法：
    python tools/measure_lang_mix.py [<corpus-dir> ...]
不给参数时扫默认语料（测试 fixture + 评测 fixture）。
"""
from __future__ import annotations

import os
import re
import sys
from collections import Counter

# UAX #24 脚本属性的**近似**分区判定（D204：假名是 ja 判据核心，不用汉字占比）。
# 注意：这是字符区间的近似，不是完整 UCD；正式实现应读 UCD 或用 unicode-script 类库。
SCRIPTS = (
    ("hira", 0x3040, 0x309F),
    ("kana", 0x30A0, 0x30FF),
    ("han", 0x3400, 0x4DBF),
    ("han", 0x4E00, 0x9FFF),
    ("han", 0xF900, 0xFAFF),
    ("han", 0x20000, 0x2FA1F),
    ("latin", 0x0041, 0x024F),
    ("latin", 0xFF21, 0xFF3A),
    ("latin", 0xFF41, 0xFF5A),
    ("hangul", 0xAC00, 0xD7AF),
    ("hangul", 0x1100, 0x11FF),
)

CODE_FENCE = re.compile(r"^\s*(```|~~~)")
INLINE_CODE = re.compile(r"`[^`]*`")
FORMULA = re.compile(r"\$\$.*?\$\$|\$[^$\n]+\$", re.S)
QUOTE_BLOCK = re.compile(r"^\s*>")


def strip_non_language(text: str) -> str:
    """D208：代码块 / 公式 / 引用块不算语言内容。"""
    out, in_fence = [], False
    for line in text.splitlines():
        if CODE_FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence or QUOTE_BLOCK.match(line):
            continue
        out.append(line)
    body = "\n".join(out)
    body = INLINE_CODE.sub(" ", body)
    body = FORMULA.sub(" ", body)
    return body


def script_counts(text: str) -> Counter:
    c: Counter = Counter()
    for ch in text:
        if not ch.isalpha() and not (0x3040 <= ord(ch) <= 0x30FF):
            continue
        cp = ord(ch)
        for name, lo, hi in SCRIPTS:
            if lo <= cp <= hi:
                c[name] += 1
                break
    return c


def classify(c: Counter) -> str:
    """D204：假名命中即 ja。Han 与中文统计不可分，故不含假名的纯 Han 判 zh。"""
    kana = c["hira"] + c["kana"]
    total = sum(c.values())
    if total == 0:
        return "empty"
    if kana > 0:
        return "ja" if kana / total >= 0.05 else "ja-mixed"
    if c["han"] / total >= 0.5:
        return "zh-or-hanonly"  # 纯汉字：ja/zh 不可分（D204 的 J1 盲点）
    return "other"


def is_mixed(c: Counter) -> bool:
    """N2 的泛混排判定：两个以上脚本各占 >= 10%。**不等于涉日混排。**"""
    total = sum(c.values())
    if total == 0:
        return False
    return sum(1 for v in c.values() if v / total >= 0.10) >= 2


def is_ja_mixed(c: Counter) -> tuple[bool, bool]:
    """**涉日混排**（D208 口径修正）：D204 的分派粒度只被它决定。

    泛混排（如中英）对 D204 **不构成压力** —— zh 与 en 在现有栈里都跑得动。
    真正逼出 run 级分派的是**含日文、且另一脚本占主导**的文档：
    文档级分派会把日文段落交给非日文分析器，而**召回归零是无声的**。

    返回 (含日文, 涉日混排)。涉日混排 = 有假名 **且** 另一脚本占比 >= 30%。
    """
    total = sum(c.values())
    has_ja = (c["hira"] + c["kana"]) > 0
    if not has_ja or total == 0:
        return has_ja, False
    other = total - (c["hira"] + c["kana"])
    return True, (other / total) >= 0.30


def main() -> int:
    roots = sys.argv[1:] or [
        "crates/momotaro-run/tests/fixtures/vault",
        "tools/evals/fixtures",
    ]
    files = []
    for r in roots:
        for dp, dn, fn in os.walk(r):
            dn[:] = [d for d in dn if d not in ("target", "node_modules")]
            for f in fn:
                if f.endswith((".md", ".txt")):
                    files.append(os.path.join(dp, f))

    print(f"corpus files: {len(files)}")
    print()
    print(f"{'file':52s} {'class':16s} {'kana%':>7s} {'han%':>7s} {'lat%':>7s} {'mixed':>6s} {'ja_mixed':>9s} {'lang':>7s}")
    ratios, mixed_n, declared = [], 0, 0
    ja_any_n, ja_mixed_n = 0, 0
    for path in sorted(files):
        raw = open(path, encoding="utf-8", errors="ignore").read()
        # frontmatter lang 覆盖率（D204 的 J2；当前应为 0）
        m = re.match(r"^---\n(.*?)\n---", raw, re.S)
        lang = ""
        if m:
            lm = re.search(r"^lang:\s*(\S+)", m.group(1), re.M)
            if lm:
                lang = lm.group(1)
                declared += 1
        body = strip_non_language(raw)
        c = script_counts(body)
        total = sum(c.values()) or 1
        kana_p = (c["hira"] + c["kana"]) / total
        ratios.append(kana_p)
        mixed = is_mixed(c)
        mixed_n += int(mixed)
        has_ja, ja_mixed = is_ja_mixed(c)
        ja_any_n += int(has_ja)
        ja_mixed_n += int(ja_mixed)
        print(
            f"{os.path.relpath(path):52s} {classify(c):16s} "
            f"{kana_p:7.3f} {c['han']/total:7.3f} {c['latin']/total:7.3f} "
            f"{str(mixed):>6s} {str(ja_mixed):>9s} {lang or '-':>7s}"
        )

    print()
    rs = sorted(ratios)
    if rs:
        def pct(p):
            i = min(len(rs) - 1, int(p * len(rs)))
            return rs[i]
        print(f"kana ratio  P50={pct(.5):.3f}  P90={pct(.9):.3f}  max={rs[-1]:.3f}")
    n = len(files) or 1
    print(f"mixed (泛, >=2 scripts at >=10%):      {mixed_n}/{len(files)} = {mixed_n/n*100:.1f}%"
          f"   <- 对 D204 **不构成压力**")
    print(f"含日文的文档 (D208 关心):               {ja_any_n}/{len(files)} = {ja_any_n/n*100:.1f}%")
    print(f"**涉日混排** (有假名且他脚本>=30%):      {ja_mixed_n}/{len(files)} = {ja_mixed_n/n*100:.1f}%"
          f"   <- **只有这个数字决定 D204 分派粒度**")
    print(f"frontmatter `lang` declared (D204 J2):  {declared}/{len(files)} = {declared/n*100:.1f}%")
    print()
    print("NOTE: 文档级（N2）采样。chunk 级（N1）需 ingest 落 analysis_lang（D210）后才有真 chunk。")
    print("NOTE: 脚本判定用字符区间近似，非完整 UCD；正式实现按 D204 走 UAX #24 属性。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
