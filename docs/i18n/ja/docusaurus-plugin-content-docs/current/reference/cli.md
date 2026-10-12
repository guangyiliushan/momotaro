---
title: コマンドラインリファレンス
description: すべての Momotaro コマンドとその引数・終了時の振る舞い——0.x ツールの安定した表面。
type: reference
sidebar_position: 1
---

# コマンドラインリファレンス {/* #cli */}

すべてのコマンドは 1 つのバイナリと共通の 3 つのグローバルオプションを共有します。このページは「何が存在し、何を取り、どう終わるか」を列挙します——将来のバージョンが足すかもしれないフラグは意図的に載せません。

## コマンド {/* #commands */}

| コマンド | すること |
|---|---|
| `momotaro init` | 現在のディレクトリにローカルストアを初期化し、設定を検証します。 |
| `momotaro index [PATH]` | Markdown ノートをインデックスします（`PATH` は設定の保管庫を上書き）。決定論的・オフライン。 |
| `momotaro search [QUERY]…` | インデックスへの語彙検索。既定ではモデルトークンゼロ。 |
| `momotaro ask [QUERY]…` | ノートから引用付きで答えます。モデル呼び出しは多くとも 1 回。 |
| `momotaro trace <RUN_ID>` | 1 回のランを再生：検索、コンテキスト、引用、判定。 |
| `momotaro doctor` | 何も変えずにワークスペースの健全性を検査します。 |
| `momotaro stats` | ストアの計数を印字：状態、データベースパス、ソース数、チャンク数。 |
| `momotaro watch` | 保管庫の変化に合わせてストアを最新に保ちます（常駐）。 |
| `momotaro reconcile` | 1 回でストアを保管庫全体に合わせます。 |
| `momotaro rebuild-index` | 正典のストレージから検索インデックスを組み直します。 |

知っておく価値のあるコマンド固有オプション：

- `search`：`-n/--top <N>` はヒット数の上限（既定 10。`0` も合法で何も返しません）。`--mode lexical|expand` は純粋な BM25（既定）か、検索前の LLM 書き換え 1 回を選びます。
- `ask`：`--session <id>` は既存セッションの続き；`--client-request-id <id>` は再試行されるリクエストを再生安全にします。
- `doctor`：`--coverage` は健全性検査ではなく、直近のインデックス実行が各ファイルに何をしたかを報告します。

## グローバルオプション {/* #global-options */}

| オプション | 意味 |
|---|---|
| `--json` | stdout に機械可読の JSON を出力します。 |
| `--no-llm` | すべてのモデル呼び出しを禁止します。`ask` と `search --mode expand` は拒否されます。語彙 `search` は影響を受けません。 |
| `--allow-network` | 必要なコマンドにネットワークを許可します。ベータは既定でオフです。 |
| `-h`, `--help` / `-V`, `--version` | ヘルプとバージョン。 |

## 終了コード {/* #exit-codes */}

- **`0`** 成功、**`1`** 失敗——ただし 1 つだけ意図的な例外：`doctor` は*健全性*をゲートにするので、未初期化や invalid のワークスペースは、レポートが正常に印字されても `1` で終わります。（`doctor --coverage` は検査ではなくレポート：ログが読める限り `0` で終わります。）
- 使い方の誤り（未知のフラグやサブコマンド）は **`2`**——`--json` でも JSON ボディは得られません。解析が、プログラム自身のエラー出口より先に失敗するからです。
- `index` のファイル単位のエラーはバッチを失敗させません：`N errors`（N > 0）と書かれた要約でも `0` で終わります。

これらの規則の理由——そして規則が存在するのはどんな復旧路のためか——は[再構築と復旧](../guides/rebuild-and-recover.md)にあります。

## 例：`stats` {/* #example-stats */}

```text
status: ready
schema version: 9
database: …/.momotaro/momotaro.db
initialized: true
sources: 2
chunks: 2
```

`--json` は同じフィールドを運びます。まだ分からない計数は両方の出口で `unknown` と印字されます——偽の `0` は決して出しません。

## 次に読むもの {/* #next */}

- [設定](configuration.md)——`momotaro.toml` ファイル。
- [クイックスタート](../guides/quickstart.md)——コマンドの実際の流れ。
