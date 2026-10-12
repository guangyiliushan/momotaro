---
title: 設定
description: momotaro.toml——保管庫の場所、モデルエンドポイントの配線、ネットワークポリシー。秘密は環境に残します。
type: reference
sidebar_position: 2
---

# 設定 {/* #configuration */}

Momotaro のワークスペースは、`momotaro.toml` を含み、データベース（`.momotaro/`）を 1 つ持つディレクトリです。設定ファイルは意図的に短く保たれています。書くのは場所と環境変数の**名前**だけで、秘密は決して書きません。

```toml title="momotaro.toml"
[vault]
path = "notes"

[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env  = "MOMOTARO_API_KEY"

[policy]
allow_network = false
```

## `[vault]`——ノートの場所 {/* #vault */}

- `path`——インデックスするノートツリー。ワークスペースからの相対（絶対でも可だが、ワークスペースの**中**に解決される必要がある。`保管庫 == ワークスペースの根` は合法）。記録されるのはワークスペース相対のパスで、それゆえ保管庫は他所に住めません。

## `[model]`——プロバイダーエンドポイント {/* #model */}

- `model_id`——質問に使うモデル。
- `base_url_env`——API ルート（例：`MOMOTARO_BASE_URL`）を保持する環境変数の**名前**。値そのものはあなたの環境に残り、ファイルには入りません。
- `api_key_env`——キーを保持する環境変数の名前。キー不要のエンドポイントでは省略します。

エンドポイントが要るのは、実際にモデルと話す瞬間だけ——`ask` と `search --mode expand`。それまでは、ツールチェーンの残りは構造上オフラインです。

## `[policy]`——既定で何が許されるか {/* #policy */}

- `allow_network`——ベータは保守的：ネットワークアクセスは**オフ**で、あなたが許すまで動きません——コマンド単位（`--allow-network`）か、ここで一度だけ。オフの間、`ask` と `search --mode expand` は接続を張る前に拒否されます。

## ワークスペースを健全に保つ 2 つの習慣 {/* #habits */}

- **秘密はファイルに入れない。** ファイルにあるのは環境変数の名前だけ。値は秘密のあるべき場所に。
- **同期するのはノート、ワークスペースではない。** `.momotaro/` にはデータベースがあります。同期サービスはデータベースを壊します。同期するのは保管庫フォルダだけに。

## 次に読むもの {/* #next */}

- [コマンドラインリファレンス](cli.md)——すべてのコマンドとそのオプション。
- [クイックスタート](../guides/quickstart.md)——これらすべての実践。
