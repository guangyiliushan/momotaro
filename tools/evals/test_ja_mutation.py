#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""D211 硬约束 3 的变异测试：**每语言段的门必须能失败**。

「门全绿」不等于「门有用」—— 一个永远不会红的门是装饰。本文件故意**改坏**，
断言门确实变红，从而证明它不是装饰。三条变异：

  M1  把 ja 段某条的 `sufficient` 改成错的 key → 门必须报 unexpected miss
  M2  把 ja 的 known_miss 白名单撑过上限 → 门必须报 ceiling
  M3  手改 committed 的 golden_set.json 一个字节 → `--check` 必须报不一致

M1/M2 打的是 `gate_failures` 的纯函数（无需 CLI）；
M3 打的是**注解层**（防止有人绕过生成器改数据）。

用法：python tools/evals/test_ja_mutation.py
"""
from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
EVAL_SEARCH = os.path.join(HERE, "eval_search.py")
GOLDEN = os.path.join(HERE, "golden_set.json")


def load_eval_search():
    spec = importlib.util.spec_from_file_location("eval_search", EVAL_SEARCH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class Args:
    """gate_failures 读的参数面；默认给一个「门开着」的配置。"""

    def __init__(self, **kw):
        self.floor_mrr = kw.get("floor_mrr", 0.0)
        self.floor_inj_mrr = kw.get("floor_inj_mrr", 0.0)
        self.floor_ja_mrr = kw.get("floor_ja_mrr", 0.0)
        self.fail_on_unexpected_miss = kw.get("fail_on_unexpected_miss", True)
        self.max_known_miss_fused = kw.get("max_known_miss_fused", 0)
        self.max_known_miss_displaced = kw.get("max_known_miss_displaced", 10)
        self.max_hn_seats = kw.get("max_hn_seats", 100)
        self.max_poison_seats = kw.get("max_poison_seats", 100)
        self.max_inj_poison_seats = kw.get("max_inj_poison_seats", 100)
        self.max_ja_known_miss = kw.get("max_ja_known_miss", 2)


def clean_report() -> dict:
    return {
        "mrr": 1.0,
        "unexpected_misses": 0,
        "known_misses": {"fused": 0, "displaced": 0},
        "hn_seats": {"semi_relevant": 0, "no_info": 0},
        "poison_seats": 0,
        "per_query": [],
    }


def main() -> int:
    mod = load_eval_search()
    fails: list[str] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        print(f"  {'PASS' if ok else 'FAIL'}  {name}{(' — ' + detail) if detail else ''}")
        if not ok:
            fails.append(name)

    print("M1  改坏 ja 的 sufficient → 门必须红")
    main_r, inj_r = clean_report(), clean_report()
    bad_ja = clean_report()
    bad_ja["unexpected_misses"] = 1
    bad_ja["mrr"] = 0.0
    # 地板要 >0 才有意义：0.0 < 0.0 不成立，那测的不是门。
    out = mod.gate_failures(main_r, inj_r, Args(floor_ja_mrr=0.5), bad_ja)
    check("ja unexpected miss 触发门", any("ja unexpected miss" in x for x in out), str(out))
    check("ja mrr 地板触发门", any("ja mrr" in x for x in out))

    print("M2  撑爆 ja 的 known_miss 白名单 → 门必须红")
    over_ja = clean_report()
    over_ja["known_misses"] = {"fused": 0, "displaced": 5}
    out = mod.gate_failures(main_r, inj_r, Args(max_ja_known_miss=2), over_ja)
    check("白名单上限触发门", any("ja known-miss" in x for x in out), str(out))

    print("M3  手改 committed golden_set.json → --check 必须红")
    raw = open(GOLDEN, encoding="utf-8").read()
    mutated = raw.replace("傅里叶变换", "傅里叶变X", 1)
    if mutated == raw:
        check("变异未命中目标串", False, "golden_set.json 里找不到变异目标串")
    else:
        with tempfile.TemporaryDirectory() as td:
            bad = os.path.join(td, "golden_set.json")
            with io.open(bad, "w", encoding="utf-8", newline="\n") as f:
                f.write(mutated)
            proc = subprocess.run(
                [sys.executable, os.path.join(HERE, "gen_annotations.py"), "--check"],
                cwd=ROOT, capture_output=True, text=True,
            )
            # --check 比对的是 committed 文件本身；把变异写回原位做一次真比对
            bak = raw
            with io.open(GOLDEN, "w", encoding="utf-8", newline="\n") as f:
                f.write(mutated)
            proc = subprocess.run(
                [sys.executable, os.path.join(HERE, "gen_annotations.py"), "--check"],
                cwd=ROOT, capture_output=True, text=True,
            )
            with io.open(GOLDEN, "w", encoding="utf-8", newline="\n") as f:
                f.write(bak)
            check("--check 检出手改", proc.returncode != 0,
                  (proc.stdout + proc.stderr).strip().splitlines()[-1] if (proc.stdout + proc.stderr).strip() else "")

    print("M4  干净报告 → 门必须绿（证明门不是永远红）")
    out = mod.gate_failures(main_r, inj_r, Args(), clean_report())
    check("干净状态通过", not out, str(out))

    print()
    if fails:
        print(f"{len(fails)} 条变异测试失败：{fails}")
        return 1
    print("全部通过：门**能失败**，不是装饰。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
