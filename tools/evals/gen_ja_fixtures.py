#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成日文评测 fixture（D211 / D213）。

**归一化等价那条必须精确控制字节** —— 用码位写入，不用编辑器输入，
否则预组合/分离两种形态会被编辑器自动折叠，测试就测不到东西了。

四个 D213 边缘场景各至少一条 fixture：
  E1 归一化等价  ja-normalized.md   （正文存**分离**形态 か+゙）
  E2 活用形      ja-conjugation.md  （存活用形 書きます）
  E3 混排        ja-mixed.md        （日英对照）
  E4 纯汉字日文  ja-kanji-only.md   （零假名，D204 J1 的已知盲点）
另加两条：ja-particle.md（助词密集，为 D207 押后的助词层留标本）、
         ja-wave.md（〜/～ 两种码位，测 D207 的显式映射）。
"""
from __future__ import annotations

import io
import os

HERE = os.path.dirname(os.path.abspath(__file__))
FIX = os.path.join(HERE, "fixtures")

# 码位常量：写转义不写字面量 —— 任何中间环节都会折叠预组合/分离形态
DECOMPOSED_GA = "\u304b\u3099"   # HIRAGANA KA + COMBINING VOICED SOUND MARK
PRECOMPOSED_GA = "\u304c"     # HIRAGANA GA（预组合）
WAVE = "〜"              # U+301C WAVE DASH（无分解映射）
FULL_TILDE = "～"           # U+FF5E FULLWIDTH TILDE

FILES = {
    # E1：**必须零 bigram 重叠**，否则 bigram 会把归一化差异洗掉（实测教训：
    # 查询「が行分解」与正文「か+゙行分解」的 bigram 重叠 2/3，无 NFC 也命中，
    # 测的不是 NFC 是召回。所以正文只放目标词 + 不含该 bigram 的填充。
    #   查询 `が行`(U+304C U+884C) 的 bigram = {が行}
    #   正文 `か+゙行` 的 bigrams = {が, ゙行}  → 零重叠
    # 无 NFC 则必漏；有 NFC 则正文归一成 が行，命中。
    "ja-normalized.md": f"""---
title: normalization-note
---

このノートは「{DECOMPOSED_GA}行」という語だけを扱います。
他の語は含みません。目的は形の差を観測することです。
""",

    # E2：活用形。查基本形 書く 要命中 書きます。
    "ja-conjugation.md": """---
title: 活用形のノート
---

研究ノートを書きます。毎日少しずつ書いています。昨日は書けなかったが、
今日こそ書きたい。書くことが習慣になると良いです。

論文を書く際には根拠を必ず添えてください。書き方は後で直せます。
""",

    # E3：混排。日英对照，D204 的弱点场景。
    "ja-mixed.md": """---
title: 日英混在ノート
---

主成分分析は Principal Component Analysis と呼ばれます。
次元削減 dimensionality reduction の代表的手法です。

The spectral theorem tells us that a real symmetric matrix can be
diagonalized orthogonally. 実対称行列は直交対角化できます。
固有値 decomposition を使います。
""",

    # E4：纯汉字日文（零假名）。D204 J1 的已知盲点。
    "ja-kanji-only.md": """---
title: 漢字表記のみのノート
---

行列分解理論。固有値問題。直交対角化。実対称行列。
線形代数基礎。次元削減手法。主成分分析理論。数値計算法。
""",

    # D207 押后的助词层标本：助词密集。
    "ja-particle.md": """---
title: 助詞の多いノート
---

このノートは日本語の助詞の使われ方を記録します。
これは本の例です。その本は私のものです。机の上にあります。
何をしていますか。どこに行きますか。誰が書きましたか。
""",

    # D207 的 〜/～ 映射标本：两种码位都要出现。
    "ja-wave.md": f"""---
title: 波ダッシュのノート
---

このノートは波ダッシュの表記ゆれを記録します。
価格は{WAVE}5000円です。範囲は 0{WAVE}100 です。
別表記として 0{FULL_TILDE}100 も使われます。二者は等価ではありません。
""",
}


def main() -> int:
    os.makedirs(FIX, exist_ok=True)
    for name, body in FILES.items():
        path = os.path.join(FIX, name)
        with io.open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(body)
        print("wrote", os.path.relpath(path))
    # 归一化等价的字节自检：两条形态必须**不同字节**
    a = PRECOMPOSED_GA.encode("utf-8")
    b = DECOMPOSED_GA.encode("utf-8")
    print()
    print(f"precomposed が  bytes: {a.hex()}")
    print(f"decomposed か+゙ bytes: {b.hex()}")
    print(f"byte-identical? {a == b}   <- 必须是 False，否则 E1 测不到东西")
    return 0 if a != b else 1


if __name__ == "__main__":
    raise SystemExit(main())
