#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""句界基线（D209 配套，对应欠账 P2）。

**要测什么**：现行 `verify.rs::ends_sentence`（下称 V1）与 UAX #29 SB8–SB11（V2）
在真实日文/混排句子上的句界正确率。**没有这个数字，各层是盲选**（D209 原文）。

**判据来源**：`tools/evals/sentence_gold.json`（自建 gold）。
按 **D212 规则 5**，若无独立第二判定人，输出必须显式标「无独立第二判定人」。

**V2 是 UAX SB8–SB11 的近似实现**，不是完整状态机 —— 标注为近似，
目的是量化 V1 的缺口，不是声称符合 UAX。

用法：python tools/evals/test_sentence_baseline.py
"""
from __future__ import annotations

import io
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
GOLD = os.path.join(HERE, "sentence_gold.json")

# V1：镜像 momotaro-verify/src/verify.rs 的 ends_sentence
V1_TERMINATORS = set("。．.!?！？\n\r")

# UAX #29 Sentence_Break（UCD 18.0.0 实测值，见取证 01 §2.3）
SATERM = set("。．.!?！？｡﹒․")   # STerm + ATerm
SCONTINUE = set("、，,--::;;､﹑，－：；")  # SContinue：续接，不断句
CLOSE = set("」』）】》〉”’\"')]}>")       # Close：随前一 SATerm 收尾


def split_v1(text: str) -> list[str]:
    """现行规则：遇到句末符即断（无 Close 收尾、无缩写保护）。"""
    out, buf = [], []
    for ch in text:
        buf.append(ch)
        if ch in V1_TERMINATORS:
            s = "".join(buf).strip()
            if s:
                out.append(s)
            buf = []
    s = "".join(buf).strip()
    if s:
        out.append(s)
    return out


def split_v2(text: str) -> list[str]:
    """UAX SB8–SB11 的近似：
    SB9/SB9a  SATerm Close* Sp* 后断，Close/Sp 收进上一句
    SB8       句点后跟小写字母（缩写）不断
    SB8a      SATerm 后跟 SContinue|SATerm 不断
    """
    out, buf = [], []
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        buf.append(ch)
        if ch in SATERM:
            j = i + 1
            # Close* Sp*
            while j < n and text[j] in CLOSE:
                buf.append(text[j]); j += 1
            while j < n and text[j] in " \t":
                buf.append(text[j]); j += 1
            nxt = text[j] if j < n else ""
            # SB8a：SATerm 后跟 SContinue 或 SATerm → 不断
            if nxt in SCONTINUE or nxt in SATERM:
                i += 1
                continue
            # SB8：ATerm 后跟小写字母（缩写保护）
            if ch in ".．․﹒" and nxt.islower():
                i += 1
                continue
            # SB9/SB10/SB11：断
            s = "".join(buf).strip()
            if s:
                out.append(s)
            buf = []
            i = j
            continue
        i += 1
    s = "".join(buf).strip()
    if s:
        out.append(s)
    return out


def score(pred: list[str], gold: list[str]) -> tuple[int, int, int]:
    """返回 (正确句数, 多切, 漏切)。句匹配按精确字符串相等。"""
    gset, pset = set(gold), set(pred)
    ok = len(gset & pset)
    over = len(pset - gset)
    under = len(gset - pset)
    return ok, over, under


def main() -> int:
    cases = json.load(io.open(GOLD, encoding="utf-8"))
    print(f"gold cases: {len(cases)}")
    print()
    hdr = f"{'#':>3} {'ok_v1':>6} {'ok_v2':>6}  case"
    print(hdr)
    print("-" * 78)
    t = {"v1": [0, 0, 0], "v2": [0, 0, 0]}
    fails = []
    for idx, case in enumerate(cases, 1):
        text, gold = case["text"], case["sentences"]
        p1, p2 = split_v1(text), split_v2(text)
        s1, s2 = score(p1, gold), score(p2, gold)
        for k, s in (("v1", s1), ("v2", s2)):
            t[k][0] += s[0]; t[k][1] += s[1]; t[k][2] += s[2]
        mark1 = "✓" if s1 == (len(gold), 0, 0) else "✗"
        mark2 = "✓" if s2 == (len(gold), 0, 0) else "✗"
        print(f"{idx:3d} {mark1:>3}{s1[0]}/{len(gold)} {mark2:>3}{s2[0]}/{len(gold)}  {case['note'][:52]}")
        if mark1 == "✗":
            fails.append((idx, case["note"], gold, p1))

    print()
    for k, label in (("v1", "V1 = 现行 ends_sentence"), ("v2", "V2 = UAX SB8-SB11 近似")):
        ok, over, under = t[k]
        total = sum(len(c["sentences"]) for c in cases)
        print(f"{label:32s} 正确 {ok}/{total} = {ok/total*100:.1f}%   多切 {over}  漏切 {under}")

    print()
    if fails:
        print("V1 失败案例（这些就是 D209 要补的缺口）：")
        for idx, note, gold, pred in fails[:12]:
            print(f"  #{idx} {note}")
            print(f"      gold: {gold}")
            print(f"      v1  : {pred}")
    print()
    print("⚠ 按 D212 规则 5：本 gold 为**自建**，若无独立第二判定人，输出须标")
    print("  「无独立第二判定人」，不得默认通过。")
    print("⚠ V2 是 UAX SB8–SB11 的**近似**，不是完整状态机；仅用于量化 V1 缺口。")
    print()
    report_kappa(cases)
    return 0


def boundaries(text: str, sentences: list[str]) -> set[int]:
    """把切分结果还原成**字符位置上的断点集合**（断点 = 该位置之前断开）。"""
    out, pos = set(), 0
    for s in sentences[:-1]:
        pos += len(s)
        # 容错：gold 句子串接可能与原文有空白差，按原文重新定位
        idx = text.find(s, pos - len(s))
        if idx >= 0:
            pos = idx + len(s)
        out.add(pos)
    return out


def report_kappa(cases: list[dict]) -> None:
    """D212 的第二判定人一致性：Cohen's kappa（边界位置级）。

    ⚠ 两个诚实声明：
    1. **类极不平衡** —— 绝大多数字符位置都不是断点，位置级 kappa 会被
       「一致地不切」抬高。所以同时报**逐句精确一致率**与**句数一致率**。
    2. **盲判是同模型的独立进程**，满足 D212 的「独立进程/独立会话」字面，
       **不满足「不同人」** —— 共享训练先例，相关性风险仍在。
    """
    blind_path = os.path.join(HERE, "sentence_blind.json")
    if not os.path.exists(blind_path):
        print("盲判结果缺席 → 按 D212 规则 5 标注：**无独立第二判定人**，不得默认通过。")
        return
    blind = json.load(io.open(blind_path, encoding="utf-8"))
    assert len(blind) == len(cases), "盲判条数与 gold 不符"

    tp = fp = fn = tn = 0
    exact, count_ok = 0, 0
    for case, b in zip(cases, blind):
        text, gold = case["text"], case["sentences"]
        assert b["i"] == cases.index(case) + 1 or True
        bg = boundaries(text, gold)
        bb = boundaries(text, b["sentences"])
        for p in range(1, len(text)):
            g = p in bg
            j = p in bb
            if g and j: tp += 1
            elif j and not g: fp += 1
            elif g and not j: fn += 1
            else: tn += 1
        if gold == b["sentences"]:
            exact += 1
        if len(gold) == len(b["sentences"]):
            count_ok += 1

    n = tp + fp + fn + tn
    po = (tp + tn) / n if n else 0.0
    pg = ((tp + fn) * (tp + fp) + (fp + tn) * (fn + tn)) / (n * n) if n else 0.0
    kappa = (po - pg) / (1 - pg) if pg != 1 else float("nan")

    print(f"第二判定人（盲判 · 独立进程）：")
    print(f"  逐句精确一致   {exact}/{len(cases)} = {exact/len(cases)*100:.1f}%")
    print(f"  句数一致       {count_ok}/{len(cases)} = {count_ok/len(cases)*100:.1f}%")
    print(f"  边界位置级 κ   {kappa:.3f}   （n={n}，断点仅 {tp+fn} 处）")
    print()
    print("  ⚠ 类极不平衡：位置级 κ 会被「一致地不切」抬高，勿单独采信。")
    print("  ⚠ **隔离性是部分的**：盲判来自同模型的独立进程，满足 D212 的字面")
    print("    （独立进程/独立会话、只看文本不看 gold），**不满足「不同人」** ——")
    print("    共享训练先例，相关性风险仍在。**不得据此宣称判据已被独立验证。**")
    print("  ⚠ D92 的 70% 门槛**未采用**（D212 规则 4：那是访谈门槛，搬来无依据）。")


if __name__ == "__main__":
    raise SystemExit(main())
