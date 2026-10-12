---
title: クイックスタート
description: 空のディレクトリから検索できるワークスペースへ——初期化、インデックス、検索、そして最初の引用付き回答。
type: task
sidebar_position: 1
---

# クイックスタート {/* #quickstart */}

このページは、何もない状態から検索できるワークスペースと最初の引用付き回答までを歩きます。ベータ版はプロジェクトのソースからビルドするコマンドラインツールで、ビルド済みインストーラーは最初の正式リリースとともに届きます。

## 1. コマンドラインをビルドする {/* #build */}

Rust ツールチェーンが必要です。プロジェクトのチェックアウトで：

```bash
git clone https://github.com/guangyiliushan/momotaro
cd momotaro
cargo build --release -p momotaro-cli
```

バイナリは `target/release/` に出来ます——`momotaro-cli`（Windows では `momotaro-cli.exe`）。以降のページではこれを `momotaro` と書きます。

## 2. ワークスペースをノートに向ける {/* #workspace */}

[ワークスペース](../glossary.md#workspace)は `momotaro.toml` を含み、データベースを 1 つだけ持つディレクトリです。その中にノートフォルダを作ります：

```bash
mkdir study && cd study
mkdir notes
```

ノートフォルダを指す設定を書きます：

```toml title="momotaro.toml"
[vault]
path = "notes"
```

そして初期化：

```bash
momotaro init
```

```text
workspace initialized
```

ワークスペースを守る 2 つの約束：保管庫はワークスペースの**中**に住むこと、同期するときは**ノートフォルダだけ**を同期すること——`.momotaro/` ディレクトリは決して同期しないでください。データベースは同期サービスを越えられません。

すでにノートツリーがある場合——Obsidian のフォルダや研究用ディレクトリ——新しく作らず `[vault] path` をそこに向けてください。ワークベンチは素の Markdown を読み、front matter はそのまま保ち、あなたのファイルを書き換えません。

## 3. ノートをインデックスする {/* #index */}

`notes/` に Markdown を 2、3 枚入れてから：

```bash
momotaro index
```

```text
indexed 2 of 2 files (2 new revisions, 0 skipped), 2 chunks, 0 errors (0 key collisions) in 90 ms
```

インデックスは決定論的です——アカウントもキーもネットワークも不要——そしてやったことを正直に報告します。再実行が処理するのは変わったものだけ：バイトも場所も変わっていないファイルは新しいリビジョンを生まないので、大きなライブラリの 2 周目は安く済みます。ワークスペースの確認はいつでも 1 コマンドです：

```bash
momotaro doctor
```

```text
workspace is ready (SQLite 3.53.2)
```

## 4. 検索する {/* #search */}

```bash
momotaro search "spectral theorem PCA"
```

```text
1. "note:pca.md"#0  "PCA and the spectral theorem"  score=11.700
    Principal component analysis diagonalizes the covariance matrix. The spectral theorem is why the eig...
2. "note:fourier.md"#0  "Fourier transform"  score=0.369
    The Fourier transform decomposes a signal into frequencies. In the discrete setting, the sampling th...
```

語彙検索はモデルのトークンを一切使わず、オフラインで動きます。プロバイダーに触れるのは `ask` と展開検索だけ——インデックス、`doctor`、トレース、再構築はすべてプロバイダーなしで動きます。スクリプトが結果を読むときは `--json` を。

## 5. 引用付きで質問する {/* #ask */}

質問はモデル呼び出しを 1 回使うので、プロバイダーが必要です。設定には環境変数の**名前**だけを書き、値はあなたの環境に残します：

```toml title="momotaro.toml"
[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env  = "MOMOTARO_API_KEY"
```

```bash
export MOMOTARO_BASE_URL="https://api.example.com/v1"
export MOMOTARO_API_KEY="sk-…"
momotaro ask "スペクトル定理と PCA はどう関係しますか？" --allow-network
```

答えは引用、[ラン](../glossary.md#run) id、そしてそのまま貼れる次のコマンドを伴って返ります。すべての引用はあなたが見る前に、記録されたバイトと照合済みです——検証が失敗したらどうなるかは[検索と引用](../concepts/retrieval-and-citations.md)にあります。

## 次に読むもの {/* #next */}

- [引用付きで質問する](ask-with-citations.md)——セッション、再試行、ネットワークポリシー。
- [ストアを最新に保つ](keep-the-store-current.md)——保管庫を見張り、照合する。
- [実行を調べる](trace-a-run.md)——モデルが何を見たかを再生する。
- [再構築と復旧](rebuild-and-recover.md)——インデックスを置き換える必要があるとき。
