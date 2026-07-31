# opencode — 手動セットアップ

[English](../../clients/opencode.md) | 日本語

サポートされている方法は `llm-gateway launch opencode` で、その場合この
ページの作業は一切不要です。このページはリダイレクトを手動で恒久化する
ためのものです。ゲートウェイが `~/.config/opencode/opencode.json` を
書き換えることはありません。

## opencode をゲートウェイに向ける

`~/.config/opencode/opencode.json` に追記:

```json
{
  "provider": {
    "gateway": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "LLM Gateway",
      "options": {
        "baseURL": "http://127.0.0.1:4000/v1",
        "apiKey": "{env:LLM_GATEWAY_KEY}",
        "headers": { "x-gw-client": "opencode" }
      },
      "models": {
        "default": {}
      }
    }
  }
}
```

あとは `-m gateway/default` で選択するか、`agents/*.md` ファイルで
エージェントごとに設定します(`model: gateway/default`)。クライアントは依然
として何らかの model id を必要としますが、ゲートウェイはその選択で route を
決めず、設定された各 route の `description` に対して内容分類します。

補足:

- **`models` 以下のすべてのキーは、ゲートウェイの `GET /v1/models` に
  一字一句そのまま載っていなければならない。** 少しでも食い違うと opencode
  はモデルを何も表示せず、エラーも出さない — このクライアント最大の
  時間泥棒。確認方法:
  ```sh
  curl -s http://127.0.0.1:4000/v1/models | jq -r '.data[].id'
  ```
- `{env:VAR}` は(`OPENCODE_CONFIG_CONTENT` の中と違って)*ファイル内*では
  機能するため、キーをここにリテラルで書く必要はない。
- `@ai-sdk/openai-compatible` は `/v1/chat/completions` を話す。Responses
  API を使いたい場合は `@ai-sdk/openai` を — ゲートウェイは両方を提供している。

## 動作確認

```sh
opencode run -m gateway/default "ping"
llm-gateway stats --by client     # opencode の行が出ること
```

逆のテスト: ゲートウェイを停止すると、同じコマンドが**失敗する**こと。

## 元に戻す

`provider.gateway` ブロックと `gateway/…` へのモデル参照を削除。
