---
title: 用語集
description: 1 概念に 1 語、定義は一度だけ——すべてのページが使う語彙。
type: reference
sidebar_position: 6
---

# 用語集 {/* #glossary */}

1 概念に 1 語、定義は一度だけ。用語がページに初めて現れるとき、ここへリンクします。

{/* generated from terminology.yaml — do not edit by hand */}

## ワークスペース {/* #workspace */}

`momotaro.toml` を含むディレクトリ。1 つのデータベースを持ち、論文の原本はそのそばに置かれます。

_避ける表記_: vault, project, profile, app-data store.

## 保管庫 {/* #vault */}

ワークスペースが指す、唯一のノートツリー。

_避ける表記_: library, knowledge base, corpus.

## ソース {/* #source */}

ライブラリが引用できるノート・論文・ウェブ文書の 1 つ。その同一性はソースキーであり、行 ID や生きたファイルパスではありません。

_避ける表記_: file, document.

## ソースキー {/* #source-key */}

ソースのスキーム付き同一性（`note:` / `arxiv:`）。NFC 正規化され、大文字小文字は決して畳み込まれません。

_避ける表記_: canonical file path, database id, URL as key.

## ソースリビジョン {/* #source-revision */}

ソースの不変のバイトスナップショット。更新は上書きではなく、新しいリビジョンの追加です。

_避ける表記_: version, file, document.

## チャンク {/* #chunk */}

1 つのソースリビジョンの決定的な射影。検索が当て、引用が指す単位です。

_避ける表記_: passage, embedding, canonical fact.

## オリジンクラス {/* #origin-class */}

バイトの書き手が誰か: `owner` | `paper` | `web` | `agent` | `system`。

_避ける表記_: trust score, provenance.

## プロジェクト {/* #project */}

名前の付いたソースのまとまり（将来は注釈やピン留めしたスパークも含む）。プロジェクトを削除してもソースは削除されません。

_避ける表記_: workspace, folder, tenant.

## セッション {/* #session */}

作成時に 0 個または 1 個のプロジェクトに結び付く会話のひも。その結び付きが既定の検索範囲を決めます。

_避ける表記_: chat, thread, project.

## 注釈 {/* #annotation */}

ソースリビジョンとロケータに留められた人手のメモ。オリジンクラスは常に `owner` です。

_避ける表記_: comment, highlight, memory.

## スパーク {/* #spark */}

エージェントが提案する引用付きカード（`proposed` | `pinned` | `dismissed`）。引用を持つピン留め済みスパークだけが検索ソースになります。

_避ける表記_: memory, idea, wiki page.

## プロファイル {/* #profile */}

名前の付いた実行既定値の束（モデルスロット・ポリシー・ツールセット）。メモリでもノートでもありません。

_避ける表記_: persona, memory, user profile.

## ラン {/* #run */}

受理された、監査可能な作業の 1 単位。終端状態は同じ記録に書かれた終端イベントが決めます。

_避ける表記_: job, request, agent loop.

## アスク {/* #ask */}

コード側で検索とコンテキスト構築を行い、そのあとプロバイダー呼び出しを多くとも 1 回だけ行うラン。

_避ける表記_: agent, tool loop, chat completion.

## 同一ライブラリ検索 {/* #same-library-retrieval */}

手書きノートと論文を 1 つの語彙索引で扱い、クエリはオリジンクラスが混ざった結果をオリジン加重付きで返します。

_避ける表記_: hybrid search, knowledge graph.

## コンテキスト範囲 {/* #context-scope */}

P1（結び付いたプロジェクトのソース、未結び付けなら保管庫全体）と P2（それ以外）の区別。モデルのコンテキストに入るものを決め、検索が記録するものには影響しません。

_避ける表記_: search filter, permission, tenant.

## 引用 {/* #citation */}

主張をバイトに固定する五つ組: ソースキー・リビジョン・ロケータ・引用文・ハッシュ。

_避ける表記_: reference, footnote, link.

## 検証 {/* #verification */}

引用を記録されたバイトと照合すること。失敗は第一級の状態として示され、決して握りつぶされません。

_避ける表記_: fact check, spell check.

## トレース {/* #trace */}

ランの記録された再構成（クエリ・証拠・プロンプト・モデルの回答・判定）。事後に再生できます。

_避ける表記_: log, history.

## フィード {/* #feed */}

ワークスペースが巡回する RSS / Atom / JSON Feed のソース。OPML で一括取り込みでき、フィード内の論文は確認ゲートを通ってからライブラリに入ります。

_避ける表記_: reader, newsletter.
