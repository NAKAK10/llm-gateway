# llm-gateway

[English](README.md) | 日本語

すべてのエージェント CLI の前段に置く、ひとつのローカルエンドポイント。

`llm-gateway` はクライアントが必要とする 3 つのワイヤープロトコル —
Anthropic Messages(`/v1/messages`)、OpenAI Chat(`/v1/chat/completions`)、
OpenAI Responses(`/v1/responses`)— を話し、リクエストの `model`
フィールドだけを書き換えて、レスポンスは**バイト単位で無加工のまま**
ストリーミングで返します。モデル選択、フォールバック、コスト集計、
監査可能なルーティング記録は、すべてひとつの設定ファイルに集約されます。

```
llm-gateway launch claude    ─┐
llm-gateway launch codex      ┼→  llm-gateway serve :4000  →  anthropic / openai /
llm-gateway launch opencode  ─┘        route → fallback →      openrouter / ollama …
OpenClaw(手動セットアップ)  ─┘        record
```

クライアントは `launch` で起動し、環境変数 / CLI オーバーライドで
リダイレクトを注入します — **クライアントの設定ファイルは一切変更しません**。

## インストール

```sh
brew install NAKAK10/tap/llm-gateway
# またはソースから:
cargo install --git https://github.com/NAKAK10/llm-gateway
```

## リリース手順(メンテナー向け)

デフォルトブランチは `dev` です。リリースするには `dev` 上で
`Cargo.toml` の `version` を上げ、`dev` を `main` にマージします。
リリースワークフローが macOS バイナリ(arm64 + x86_64)をビルドし、
GitHub Releases に `v{version}` を公開、
[NAKAK10/homebrew-tap](https://github.com/NAKAK10/homebrew-tap)
の formula を自動更新します。バージョンを上げずにマージしても何も
リリースされない(タグが既に存在する)ため、ドキュメントのみのマージは安全です。

## クイックスタート

```sh
llm-gateway init            # 対話形式; ~/.config/llm-gateway/config.json を書き出す (chmod 600)
llm-gateway serve           # 127.0.0.1:4000 でゲートウェイを起動
llm-gateway launch claude   # Claude Code をゲートウェイ経由で起動
llm-gateway stats           # ルートごとの消費量を表示
```

## サポートしているクライアント

| クライアント | 方法 |
|---|---|
| Claude Code | `llm-gateway launch claude` |
| Codex CLI | `llm-gateway launch codex` |
| opencode | `llm-gateway launch opencode` |
| OpenClaw | 手動セットアップ — `docs/clients/openclaw.md` 参照 |

`launch` は起動時にリダイレクトを注入するだけで、クライアントの設定
ファイルには何も書き込みません。各クライアントの手動(恒久的)
セットアップは `docs/clients/` に、日本語版は `docs/ja/clients/` にあります。

## サポートしているプロバイダー

以下はすべて `llm-gateway init` がそのまま雛形を生成できます。プロバイダーは
`baseUrl` + `api` + `apiKey` の設定にすぎないので、このリストにない
エンドポイントでも OpenAI 互換 / Anthropic 互換であれば**何でも**使えます —
各プロバイダーのコピペ可能な設定は `docs/ja/providers.md` を参照してください。

| プロバイダー | `baseUrl` | `api` | キーの環境変数 |
|---|---|---|---|
| Anthropic | `https://api.anthropic.com` | `anthropic-messages` | `ANTHROPIC_API_KEY` |
| OpenAI | `https://api.openai.com/v1` | `openai-responses` | `OPENAI_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | `openai-chat`(`anthropic-messages` も可) | `OPENROUTER_API_KEY` |
| Google Gemini | `https://generativelanguage.googleapis.com/v1beta/openai` | `openai-chat` | `GEMINI_API_KEY` |
| xAI (Grok) | `https://api.x.ai/v1` | `openai-chat` | `XAI_API_KEY` |
| Mistral | `https://api.mistral.ai/v1` | `openai-chat` | `MISTRAL_API_KEY` |
| DeepSeek | `https://api.deepseek.com/v1` | `openai-chat` | `DEEPSEEK_API_KEY` |
| Groq | `https://api.groq.com/openai/v1` | `openai-chat` | `GROQ_API_KEY` |
| Together AI | `https://api.together.xyz/v1` | `openai-chat` | `TOGETHER_API_KEY` |
| Sakana AI (Fugu) | `https://api.sakana.ai/v1` | `openai-chat` | `SAKANA_API_KEY` |
| PLaMo (Preferred Networks) | `https://api.platform.preferredai.jp/v1` | `openai-chat` | `PLAMO_API_KEY` |
| Ollama Cloud | `https://ollama.com/v1` | `openai-chat` | `OLLAMA_API_KEY` |
| Ollama(ローカル) | `http://127.0.0.1:11434/v1` | `openai-chat` | *(不要)* |

## 設定リファレンス

すべての設定は `~/.config/llm-gateway/`(`LLM_GATEWAY_CONFIG_DIR` で変更可)
にあります。`config.json` は **JSON5** としてパースされるため、コメントや
末尾カンマが使えます。変更はホットリロードされ、壊れた編集をしても直前の
設定で稼働し続け、エラーがログに出ます。

```json5
{
  server: {
    host: "127.0.0.1",        // ループバック以外にバインドするには apiKey が必須
    port: 4000,
    apiKey: "${LLM_GATEWAY_KEY}",   // ループバックなら省略可
  },
  providers: {
    "<id>": {
      baseUrl: "https://openrouter.ai/api/v1",
      api: "openai-chat",     // openai-chat | openai-responses | anthropic-messages
      apiKey: "sk-…",         // リテラル | "${ENV_VAR}" | "keychain:<name>"
      headers: { "X-Title": "llm-gateway" },   // 任意の追加ヘッダー
      injectUsage: true,      // ストリーミング chat に stream_options.include_usage を付与
    },
  },
  routes: {
    "<name>": {               // クライアントが `model` に入れる名前; `:` と `/` は禁止
      title: "…",
      description: "テキスト、または ./llm/file.md",   // 将来のセマンティックルーティング用コーパス
      model: {
        default: "<provider>/<model>",        // 最初の `/` でのみ分割される
        fallbacks: ["<provider>/<model>"],    // default と同じプロトコルのみ; 最初のバイト受信前に試行
      },
    },
    "claude-*": {             // ワイルドカード: `*` はリクエストされたモデル名に展開
      model: { default: "anthropic/*" },
    },
  },
  launch: {
    claude:   { model: "claude-sonnet-4-6", extraArgs: [] },
    codex:    { model: "gpt-5.6", wireApi: "responses", extraArgs: [] },
    opencode: { model: "role-default", models: [], extraArgs: [] },
  },
  logging: {
    dir: "./logs",            // 設定ディレクトリからの相対パス
    usage: true,              // usage-YYYY-MM.jsonl — 1 リクエスト 1 行
    debug: false,             // trace-YYYY-MM-DD.jsonl — プロンプト本文が記録される!
  },
}
```

| フィールド | 補足 |
|---|---|
| `server.apiKey` | 起動時に一度だけ解決されるため、変更には再起動が必要。`host` がループバック以外の場合は必須 — この 1 つのキーがすべてのプロバイダー認証情報を守る。 |
| `providers.<id>.apiKey` | リクエスト試行のたびに解決されるため、環境変数 / Keychain のローテーションが即時反映される。 |
| `providers.<id>.api` | フォールバックはプロトコルをまたげない。`config check` が検証する。 |
| `routes.<name>` | 完全一致がワイルドカードに勝ち、ワイルドカード同士では最長プレフィックスが勝つ。 |
| `description` | `./` `../` `/` `~/` で始まる場合はファイルパスとして扱われる。 |
| `logging.debug` | `--debug` はユーザーテキストを 200 文字に切り詰め、`--debug-full` は全文を残す。プロンプトが平文でディスクに残るため、意図的に有効化すること。 |

## コマンド

```
llm-gateway serve [--debug] [--debug-full] [--port N]
llm-gateway init
llm-gateway launch <claude|codex|opencode> [--model R] [--isolate] [--print] [-- ARGS]
llm-gateway config check|show|gitignore
llm-gateway stats [--by route|client|provider|model|day] [--since D] [--until D]
llm-gateway trace [--tail] [--route R] [--client C]
llm-gateway providers
```

## フォールバックがやること(と、やらないこと)

フォールバックは接続失敗・ヘッダータイムアウト・408・429・5xx で発動します —
**最初のレスポンスバイトを受け取る前**に限られます。ストリーミングが
始まったレスポンスは確定であり、生成途中の失敗でプロバイダーを切り替える
ことはできません。フォールバックは同一プロトコル間のみ。Anthropic
プロトコルでベンダーをまたぐ冗長化をしたい場合は、OpenRouter の Anthropic
互換エンドポイントにフォールバックを向けてください。

## クライアントの手動セットアップ

サポートされている方法は `launch` ですが、すべてのクライアントは手動でも
設定できます — Claude Code、Codex CLI、opencode、OpenClaw については
`docs/clients/`(日本語版: `docs/ja/clients/`)を参照してください。
どちらの方法でも、ゲートウェイがクライアントの設定ファイルを書き換える
ことはありません。

## セキュリティに関する注意

- `config.json` には API キーがリテラルで入ることがあります: 作成時に
  `0600`、`config check` で検査、`config show` でマスク表示、
  `config gitignore` でテンプレート出力。
- `server.apiKey` なしでループバック以外にバインドしようとすると起動を拒否します。
- `--debug` はプロンプト本文を `logs/` に書き込みます。ディレクトリの扱いには注意してください。

## ライセンス

MIT OR Apache-2.0.
