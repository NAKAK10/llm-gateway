# llm-gateway

[English](README.md) | 日本語

すべてのエージェント CLI の前段に置く、ひとつのローカルエンドポイント。

`llm-gateway` はクライアントが必要とする 3 つのワイヤープロトコル —
Anthropic Messages(`/v1/messages`)、OpenAI Chat(`/v1/chat/completions`)、
OpenAI Responses(`/v1/responses`)— を話し、リクエストの `model`
フィールドだけを書き換えて、レスポンスは**バイト単位で無加工のまま**
ストリーミングで返します。モデル選択、フォールバック、コスト集計、
監査可能なルーティング記録は、すべてひとつの設定ファイルに集約されます。

唯一の意図的な例外が[クロスプロトコルルーティング](#クロスプロトコルルーティング)です。
クライアントとプロバイダーのプロトコルが異なる場合に限り、リクエストと
レスポンスを実際に組み立て直します — そうしなければその組み合わせはそもそも
動かないからです。同一プロトコル同士の通信(依然として大多数を占めます)
には手を加えません。

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

## agent ごとのモデル

サブエージェントが個別に指定しているモデルは、**agent ファイルを一切変更せず**
そのまま生きたうえで、全リクエストがゲートウェイを経由します:

| クライアント | agent のモデル指定元 | ゲートウェイへの経路 |
|---|---|---|
| Claude Code | サブエージェントの `model:` frontmatter | env リダイレクトがプロセス全体に効く。ID は `claude-*` で解決 |
| Codex CLI | `~/.codex/agents/*.toml` の `model =` | プロバイダー指定はグローバルなので全 agent が経由。モデル名は `gpt-*` が転送 |
| opencode | `agents/*.md` の `model: openai/…` | `launch` が `launch.opencode.overrideProviders`(既定 `openai`, `anthropic`)の組み込みプロバイダーもゲートウェイに向ける。opencode はモデル参照ごとにプロバイダーを選ぶため、これが無いと固定 agent が黙ってゲートウェイを素通りする |

既定のルーティングは**モデル名ベース**です(完全一致 → 最長 wildcard)。
リクエスト内容から route を選ぶセマンティック分類は、`semantic` を持つ
route でのみ動きます — 下記の
[セマンティックルーティング](#セマンティックルーティング)を参照してください。

## セマンティックルーティング

コンテンツベースルーティングは、`model` 名ではなくリクエストの**内容**から
route を選びます。動くのは `semantic` ブロックを持つ route のときだけで、
それ以外は従来どおり「完全一致 → 最長 wildcard」です。

**`semantic` cargo feature 付きのビルドが必要です** — Homebrew 版には
入っていますが、`--features semantic` なしの `cargo install` には入りません。
埋め込みモデルが ~500MB あるため、ビルド時オプトインにしています。feature
無しのバイナリは起動時に警告を出し、該当 route をそのまま自身の `model` に
転送するので、設定はどちらでも有効です。モデルは `semantic` 付き route が
ある状態で最初に `serve` したときにダウンロードされ、そのような route が
存在するときだけメモリに読み込まれます。

`routes[].semantic` はどの route にも追加できる任意フィールドです:

| フィールド | 型 | 既定 | 意味 |
|---|---|---|---|
| `candidates` | `string[]` | `[]` | 選択対象になる route 名。空なら「`description` を持つ他の全 route」。 |
| `threshold` | `number` | `0.45` | リクエストと候補群との top-1 cosine 類似度がこれを下回った場合、auto route 自身の `model` が代わりに使われる。 |

設計上のポイント:

- **auto route 自身の `model` は、どの候補も閾値に届かなかったときの
  行き先**です。だから `semantic` を持つ route にも、他の route と同様に
  `model` が必要です。
- **明示的な route 名は絶対に上書きされません。** 分類が走るのは
  `semantic` を持つ route 自身が名前で要求されたときだけです。これは
  既存の「明示的な route 名は常に勝ち、常に予測可能」という設計方針の
  継続です(`src/route.rs`、`docs/roadmap.md` の Phase 2 参照)。
- **候補には `description` が必須です** — これが分類コーパスになります
  (長い説明は今と同じく `llm/*.md` に置けます)。
- **リクエストが届き得ない候補は、実行時に除外されます** —
  `/v1/chat/completions` へのリクエストが `anthropic-messages` の候補に
  解決されることは決してありません。その方向への変換が存在しないからです。
  一方 Claude Code からのリクエストは `openai-chat` の候補を選べます —
  その方向は変換されるからです
  ([クロスプロトコルルーティング](#クロスプロトコルルーティング)参照)。
- **`semantic` を持つ route 名にワイルドカード(`*`)は使えません。**

```json5
routes: {
  "auto": {
    semantic: {
      candidates: ["role-light", "role-deep", "role-code"],
      threshold: 0.45,
    },
    // どの候補も閾値に届かなかったときの行き先
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["openrouter/qwen/qwen3-8b"],
    },
  },

  "role-light": {
    description: "短い定型作業。要約、整形、コミットメッセージ生成、命名",
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["groq/llama-3.3-70b-versatile"],
    },
  },

  "role-deep": {
    description: "./llm/role-deep.md",
    model: {
      default: "openrouter/anthropic/claude-opus-5",
      fallbacks: ["openrouter/google/gemini-3-pro"],
    },
  },

  "role-code": {
    description: "コード生成、リファクタリング、テスト作成、バグ修正",
    model: {
      default: "openrouter/qwen/qwen3-coder",
      fallbacks: ["deepseek/deepseek-coder"],
    },
  },
}
```

## クロスプロトコルルーティング

Claude Code は Anthropic Messages しか話しませんが、安価あるいはローカルな
プロバイダーのほとんどは OpenAI Chat しか話しません。そこで一方向だけを
変換します:

| クライアントが話す | プロバイダーが話す | 結果 |
|---|---|---|
| `anthropic-messages` | `openai-chat` | 変換される — Claude Code から Ollama、Groq、DeepSeek、Gemini、Mistral、Together、Sakana AI、PLaMo に到達できる |
| 両側が同じ | — | 従来どおりバイト単位の無加工転送 |
| それ以外の組み合わせ | — | 従来どおり説明付きの `400` |

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
},
routes: {
  // Claude Code から: llm-gateway launch claude --model role-cheap
  "role-cheap": { model: { default: "ollama-local/qwen3:8b" } },
}
```

変換されたルートで失われるもの:

- **プロンプトキャッシュ、`thinking` ブロック、citation、Anthropic の
  サーバーサイドツール**(`web_search`、`bash`、`text_editor`)は破棄されます
  — 変換先のプロトコルにはそれらの置き場がありません。
  `cache_creation_input_tokens` は常に 0 になります。
- **`/v1/messages/count_tokens` はローカルの推定値で応答します** —
  `openai-chat` にはトークンカウント用のエンドポイントがないためです。
  何も返さなければ Claude Code のコンテキストサイズ計算が壊れるので、
  推定値であることをトレースログの `result: "estimated_locally"` で示します。
- **レスポンスは組み立て直される**ため、プロバイダーが送ってきたものと
  バイト単位で同一ではありません。`llm-gateway trace` はこれらの
  リクエストに `xlat=anthropic-messages->openai-chat` を付けます —
  出力がおかしいと感じたら、まずこのフィールドを確認してください。
- 使用量集計には**影響しません**: トークン数は変換前のアップストリームの
  バイト列から読み取ります。

何が引き継がれ、何が引き継がれないかの完全な一覧は
`docs/ja/gotchas.md` を参照してください。

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
| GitHub Copilot | `https://api.githubcopilot.com` | `openai-chat` | *(GitHub トークン、例: `command:gh auth token`)* |
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

この表の `openai-chat` プロバイダーはすべて Claude Code からも到達できます
— [クロスプロトコルルーティング](#クロスプロトコルルーティング)参照。

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
    // `model` はそのクライアントの「メイン(既定)モデル」だけを決める route 名。
    // role route(`role-strategy` 等)でも wildcard が拾う ID でもよい。
    // agent ごとのモデルはそのまま生きる — 下の「agent ごとのモデル」参照。
    claude:   { model: "claude-sonnet-4-6", extraArgs: [] },
    codex:    { model: "gpt-5.6", wireApi: "responses", extraArgs: [] },
    opencode: { model: "role-default", models: [],
                overrideProviders: ["openai", "anthropic"], extraArgs: [] },
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
