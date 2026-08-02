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

## `overrideProviders`: opencode ビルトインプロバイダのリダイレクト

`llm-gateway launch opencode`(および `config.json` の
`launch.opencode.overrideProviders`)は、opencode の**ビルトイン**プロバイダ
id もリダイレクトできる。これにより、エージェントファイルや
`opencode.json` が `model: gateway/…` の代わりに `model: openai/gpt-…` の
ように pin していても、gateway 経由になる。リダイレクトはそのプロバイダの
`options.baseURL` を差し替えるだけで、npm パッケージ(＝ネイティブのワイヤー
プロトコル)はそのまま残る。これが安全かどうかは、パッケージが実際にどこへ
POST するかで決まる — ゲートウェイが受けるのは `/v1/messages`、
`/v1/chat/completions`、`/v1/responses` の3パスだけだからだ。

| provider id | npm パッケージ | 最終パス | redirect 可否 |
|---|---|---|---|
| `openai` | `@ai-sdk/openai` | `/v1/responses` | 可 |
| `anthropic` | `@ai-sdk/anthropic` | `/v1/messages` | 可 |
| `openrouter` | `@openrouter/ai-sdk-provider` | `/v1/chat/completions` | 可(下記の方言注意点あり) |
| `groq` | `@ai-sdk/groq` | `/v1/chat/completions` | 可(下記の方言注意点あり) |
| `mistral` | `@ai-sdk/mistral` | `/v1/chat/completions` | 可(下記の方言注意点あり) |
| `deepseek` | `@ai-sdk/openai-compatible` | `/v1/chat/completions` | 可(素の OpenAI 形式) |
| `togetherai` | `@ai-sdk/togetherai` | `/v1/chat/completions` | 可(素の OpenAI 形式) |
| `xai` | `@ai-sdk/xai` | `/v1/responses` または `/v1/chat/completions` | 可(どちらでも一致) |
| `google` | `@ai-sdk/google` | `/v1/models/{id}:generateContent` | **不可** — このゲートウェイのどのルートとも一致せず、redirect すると動いていたリクエストが 404 になる |
| `github-copilot` | opencode 内製 SDK + 認証プラグイン | 動的 | **不可** — 投げ先が固定されておらず安全に redirect できない |
| `ollama` | — | — | **対象外** — models.dev に id 自体が存在せず pin のしようがない |

既定の `overrideProviders` は上表の「可」8つ。`google` /
`github-copilot` / `ollama` は意図的に含まれていない — 自分で
`overrideProviders` に足しても動くようにはならない。理由は上表の通り。

**方言についての注意:** `openrouter` / `groq` / `mistral` および `openai`
自体も、素の OpenAI 互換 JSON を超えたフィールド(ルーティングヒント、
プロバイダ独自のサンプリングパラメータなど)を送りうる。redirect した後、
そうしたフィールドはゲートウェイのルートが解決した先の上流にそのまま
転送されるため、上流がそれを解釈できなければ 400 になることがある。
それでも、この redirect が塞ごうとしている無言のバイパスよりはましだ —
400 はメッセージだが、バイパスはメッセージにならない。ただし「redirect
されている」ことは「そのプロバイダのどのモデルでも動く」ことを保証しない。

**`x-gw-auto-route: 0` の場合の注意点:** auto-route を無効にすると、
ゲートウェイはクライアントが送ってきたモデル id をそのままルート名として
完全一致検索する(`src/route.rs` の `find_route` を参照 — prefix マッチや
あいまい一致はしない)。redirect されたビルトインプロバイダは自身の
ネイティブなモデル名(例: `gpt-5`。ゲートウェイのルート名ではない)を送る
ため、たまたま同名のルートが無い限り 404 になる。これは新しく足した
プロバイダだけでなく、既定の `openai`/`anthropic` にも既に当てはまる制約。
既定の auto-route 有効時は、クライアントが送ったモデル id によらず全リクエ
ストを分類するため、この制約を回避できる。

`llm-gateway launch opencode` は起動時に、`overrideProviders` の対象外の
プロバイダを pin しているエージェントファイル・`opencode.json` を検出して
警告する。

## `--isolate`

`llm-gateway launch opencode --isolate` は `--pure` を追加します。これが
無効化するのは opencode の*外部プラグイン*だけです。設定ファイル
(`opencode.json`、エージェントの frontmatter、注入される
`OPENCODE_CONFIG_CONTENT`)はすべて普段どおり読み込まれます。Claude Code の
`--isolate` とは違い、こちらは設定の読み込みを一切止めません。

Claude Code・Codex との比較は README の「コマンド」節
([README.ja.md](../../../README.ja.md#コマンド))を参照してください。

## 動作確認

```sh
opencode run -m gateway/default "ping"
llm-gateway stats --by client     # opencode の行が出ること
```

逆のテスト: ゲートウェイを停止すると、同じコマンドが**失敗する**こと。

## 元に戻す

`provider.gateway` ブロックと `gateway/…` へのモデル参照を削除。
