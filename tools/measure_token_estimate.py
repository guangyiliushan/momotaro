#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""token 估算系数实测（D204/D207 配套，对应欠账 P4）。

**要测什么**：`estimate_tokens` 的「CJK 一字算一 token」对日文是否系统性低估。
D27 的 1.4–1.5 系数是**按中文校准**的，日文档未测（D203 第四态：未测 ≠ 无问题）。
低估 → 上下文预算超发 → 撞 provider 上限（INC-09「撞并发墙」的隐藏来源）。

**怎么测**：把同一段文本分别喂给
  (a) 项目自己的 `estimate_tokens`（复刻 contracts/lib.rs 的纯函数）
  (b) 一个真实 BPE tokenizer（若环境里有）
输出**每字符 token 数**与 (a)/(b) 比值。**没有 (b) 就只报 (a)，并显式标 `unmeasured`。**

**D203 纪律**：不得用估计值冒充实测值。本脚本的输出在没有 tokenizer backend 时
是 `unmeasured`，不是「误差可接受」。

用法：
    python tools/measure_token_estimate.py
"""
from __future__ import annotations

# ── (a) 项目估算器的复刻（momotaro-contracts::estimate_tokens）─────────────
CJK_RANGES = (
    (0x3000, 0x303F), (0x3040, 0x30FF), (0x3130, 0x318F),
    (0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xAC00, 0xD7AF),
    (0xF900, 0xFAFF), (0xFF00, 0xFFEF), (0x20000, 0x2FA1F),
)


def is_cjk(ch: str) -> bool:
    cp = ord(ch)
    return any(lo <= cp <= hi for lo, hi in CJK_RANGES)


def estimate_tokens(text: str) -> int:
    """一字一 token（CJK）/ 四字一 token（其他），与 Rust 侧一致。"""
    cjk = other = 0
    for c in text:
        if is_cjk(c):
            cjk += 1
        else:
            other += 1
    return cjk + -(-other // 4)  # div_ceil


# ── (b) 真实 BPE backend（可选）─────────────────────────────────────────
def load_backend():
    """返回 (名字, 计数函数) 或 None。**不安装任何包**，只探测已有的。"""
    try:
        import tiktoken  # type: ignore
        enc = tiktoken.get_encoding("cl100k_base")
        return ("tiktoken:cl100k_base", lambda t: len(enc.encode(t)))
    except Exception:
        pass
    try:
        from tokenizers import Tokenizer  # type: ignore
        return ("tokenizers:<unloaded>", None)
    except Exception:
        pass
    return None


SAMPLES = {
    "zh（中文正文）":
        "矩阵分解为特征值与特征向量，谱定理告诉我们实对称矩阵可以正交对角化，"
        "这在主成分分析里是降维的理论基础。",
    "ja-hira（平假名）":
        "けんさくえんじんのしくみをりかいするためには、ほんやくにたよらずに"
        "げんぶんをよむことがだいじです。",
    "ja-mixed（日文正文）":
        "行列を固有値と固有ベクトルに分解すると、主成分分析の理論的基盤が得られます。"
        "実対称行列は直交対角化できることがスペクトル定理から従います。",
    "ja-kana（片假名外来语）":
        "コンピュータビジョンとディープラーニングを組み合わせたアプローチ。",
    "en（英文正文）":
        "Spectral theorem states that every real symmetric matrix can be "
        "diagonalized by an orthogonal matrix, which underpins PCA.",
}


def main() -> int:
    backend = load_backend()
    name, count = backend if backend else ("(none)", None)

    print(f"tokenizer backend: {name}")
    print()
    hdr = f"{'sample':24s} {'chars':>7s} {'est':>7s} {'real':>7s} {'est/char':>9s} {'real/char':>10s} {'ratio':>7s}"
    print(hdr)
    print("-" * len(hdr))

    measured = False
    for label, text in SAMPLES.items():
        n = len(text)
        est = estimate_tokens(text)
        if count is not None:
            real = count(text)
            measured = True
            ratio = est / real if real else float("inf")
            print(f"{label:24s} {n:7d} {est:7d} {real:7d} "
                  f"{est/n:9.3f} {real/n:10.3f} {ratio:7.3f}")
        else:
            print(f"{label:24s} {n:7d} {est:7d} {'—':>7s} "
                  f"{est/n:9.3f} {'—':>10s} {'—':>7s}")

    print()
    if measured:
        print("状态：**measured** —— ratio > 1 表示估算器高估，< 1 表示低估（会超发预算）。")
    else:
        print("状态：**unmeasured（D203 第四态）**")
        print("  环境里没有可用的 BPE tokenizer backend（tiktoken / tokenizers 均未安装）。")
        print("  上表的 est/char 只是**估算器自身的行为**，不是误差 —— **不得当作「误差可接受」的依据**。")
        print()
        print("  补测方式（择一）：")
        print("    python -m pip install tiktoken      # 再跑本脚本")
        print("    cargo add --dev tiktoken-rs         # Rust 侧")
        print("  或用 provider 的真实用量：`llm_turns` 表记了每次调用的实际 token 数，")
        print("  与同一次 build_context 的 estimate_tokens 输出对拍。")
    print()
    print("NOTE: 本脚本复刻 contracts::estimate_tokens 的算法（CJK 一字一 token，其余四字一 token）。")
    print("NOTE: Rust 侧 doctest 断言 estimate_tokens('あいう') == 3 —— 假名被当 CJK。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
