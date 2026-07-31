# Codex CLI — 手動セットアップ

[English](../../clients/codex.md) | 日本語

サポートされている方法は `llm-gateway launch codex` で、その場合この
ページの作業は一切不要です。このページはリダイレクトを手動で恒久化する
ためのものです。ゲートウェイが `~/.codex/config.toml` を書き換えることは
ありません。

## Codex をゲートウェイに向ける

`~/.codex/config.toml` に追記(**ユーザーレベル** — プロジェクト直下の
`.codex/config.toml` ではプロバイダーをリダイレクトできず、この用途では
無視されます):

```toml
[model_providers.gateway]
name = "LLM Gateway"
base_url = "http://127.0.0.1:4000/v1"
env_key = "LLM_GATEWAY_KEY"     # 環境変数の名前; 値は ~/.codex/.env に置く
wire_api = "responses"           # このバージョンでエラーになるなら "chat" を試す

[model_providers.gateway.http_headers]
"x-gw-client" = "codex"
```

そして `~/.codex/.env` に:

```
LLM_GATEWAY_KEY=<server.apiKey の値>
```

あとは実行ごとにオプトインするか:

```sh
codex -c 'model_provider="gateway"' -c 'disable_response_storage=true'
```

`config.toml` の先頭に追記して恒久化します:

```toml
model_provider = "gateway"
disable_response_storage = true
```

`agents/*.toml` の既存の `model = "gpt-…"` 行はそのままで構いません。
Codex は依然として model 文字列を要求しますが、`llm-gateway` はそれで route を
決めません。受信したすべてのリクエストを route の `description` に対して
内容分類し、閾値を超えなかったものは予約済みの `default` route が受けます。

補足:

- `env_key` に入れるのは**変数名**であってキーそのものではない。GUI から
  起動した Codex はシェルの export を見ないため、値は `~/.codex/.env` に
  置く必要がある。
- `disable_response_storage = true` はフォールバックが OpenRouter に向いた
  瞬間に効いてくる: OpenRouter の `/v1/responses` はステートレスで、
  `previous_response_id` が非 null だと 400 を返す — なければすべての会話が
  2 ターン目で死ぬ。
- `wire_api` は現行バージョンでは `responses` を受け付ける。`chat` がまだ
  存在するかはソースによって食い違う。ゲートウェイは両エンドポイントを
  提供しているので、まず `responses` を試し、Codex が拒否した場合のみ
  `chat` に切り替えること。

## 動作確認

```sh
codex exec -c 'model_provider="gateway"' "say ok"
llm-gateway stats --by client     # codex の行が出ること
```

逆のテスト: ゲートウェイを停止すると、同じコマンドが**失敗する**こと。

## 元に戻す

`model_provider = "gateway"` の行を削除(または `-c` を渡すのをやめる)。
プロバイダーブロックは残しておいて問題ありません — 参照されない限り不活性です。
