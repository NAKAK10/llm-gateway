# llm-gateway

[English](README.md) | 日本語

すべてのエージェント CLI の前段に置く、ひとつのローカルエンドポイント。

`llm-gateway` はクライアントが必要とする 3 つのワイヤープロトコル —
Anthropic Messages(`/v1/messages`)、OpenAI Chat(`/v1/chat/completions`)、
OpenAI Responses(`/v1/responses`)— を話し、受信したすべてのリクエストを
route の `description` に対して分類し、上流に送る `model` フィールドだけを
書き換えて、レスポンスは**バイト単位で無加工のまま**ストリーミングで返します。
コンテンツベースのルーティング、フォールバック、コスト集計、監査可能な
ルーティング記録は、すべてひとつの設定ファイルに集約されます。

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

`launch` はセッションごとに一度、リクエストを内容で自動分類するか
（"yes"、既定かつ従来の挙動）、それともエージェントが実際に送ってきた
モデル名でルーティングするか（"no"）を尋ねます。`--auto` / `--no-auto`
で非対話的に答えられます。どちらも指定しない場合は端末で確認します
（標準入力が端末でない場合はプロンプトを省略し "yes" になります）。

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
llm-gateway init            # 対話形式; 埋め込みモデルをダウンロードしてから ~/.config/llm-gateway/config.json を書き出す (chmod 600)
llm-gateway serve           # 127.0.0.1:4000 でゲートウェイを起動
llm-gateway launch claude   # Claude Code をゲートウェイ経由で起動
llm-gateway stats           # ルートごとの消費量を表示
llm-gateway update          # 最新リリースへ更新
```

`init` はどの role を設定するか尋ねる前に、もう一つ質問をします:
「主にどの言語で指示を書きますか?」— English、日本語、中文、한국어、Español の
5 択です。`default` を含め、生成されるすべての route の `description` は
選んだ言語で書かれます。理由は[コンテンツ分類ルーティング](#コンテンツ分類ルーティング)
を参照してください。

**破壊的な設定変更:** 旧スキーマからの移行処理はありません。
`~/.config/llm-gateway/config.json`(または `~/.config/llm-gateway/` 全体)を
削除して、`llm-gateway init` をやり直してください。

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

## agent ごとのモデル文字列

サブエージェントが個別に指定しているモデルは、**agent ファイルを一切変更せず**
そのまま生きたうえで、全リクエストがゲートウェイを経由します。変わったのは、
その文字列が route を選ばなくなったことです。クライアントが成立するための
見かけ上の値として残るだけです。

| クライアント | クライアント側でモデル文字列を持つ場所 | 今の意味 |
|---|---|---|
| Claude Code | サブエージェントの `model:` frontmatter、または Claude 自身の `/model` UI | 文字列は送信・記録されるが、route 選択には使われない |
| Codex CLI | `~/.codex/agents/*.toml` の `model =` | Codex はモデル名を必要とするが、ゲートウェイは内容分類で route を選ぶ |
| opencode | `agents/*.md` の `model: openai/…` | `launch` は組み込みプロバイダーも引き続き上書きし、固定 agent がゲートウェイを素通りしないようにする |

## コンテンツ分類ルーティング

分類は常時オンです。受信したすべてのリクエストについて、ゲートウェイは
**最新の user テキスト**を埋め込み、ワイルドカードではない各 route の
`description` と static な `model2vec-rs` 埋め込みで比較し、固定 cosine
閾値 **0.45** を超えた最上位候補を選びます。

最新の user テキストが閾値に届かない場合 — あるいは最後の user message が
`tool_result` だけでテキストを持たない場合(エージェント的なターンでは
これが普通の状態です) — ゲートウェイは**過去の user テキストを遡って**
(最大 8 件)、閾値を超える直近のものを採用します。「続けて」のようなターンや
tool_result のターンをまたいでも会話は route を維持しますが、ゲートウェイが
会話ごとの状態を持つわけではありません: 毎リクエストに同梱されてくる履歴
そのものが状態です。最新のテキストが常に最初に試されるので、本当の話題転換は
これまでどおり即座に勝ちます。遡っても何も閾値を超えなければ、または
分類自体が走らなければ、予約済みの `default` route が使われます。この
すべてに先立って、各候補テキストからは `<system-reminder>...</system-reminder>`
ブロックが除去されます — これはハーネスが注入する定型文であり(Claude Code は
すべてのセッションの最初の user message にこれを注入します)、user 自身の
言葉ではありません。除去した結果が空白だけになった message は、テキストなしの
`tool_result` ターンと同様にスキップされます。影響するのは分類の入力だけで、
プロバイダーへ送る payload は変わりません。

重要な帰結:

- **クライアントが送る `model` では route は決まりません。** 残るのは
  クライアント自身の UI と、trace ログの `requested_model` だけです。
- **通常ビルドでは常に分類が入ります。** `semantic` は既定の cargo
  feature なので、Homebrew と素の `cargo install` は同じ挙動です。
- **`cargo install --no-default-features` が opt-out です。** その小さい
  ビルドは分類を完全に省き、常に `default` に流します。
- **`llm-gateway init` は常に埋め込みモデル**(およそ 500 MB)をダウンロードして
  から `config.json` を書きます。
- **ワイルドカードではない route には必ず実のある `description` が要ります。**
  その文章がそのまま分類コーパスなので、ボイラープレートしか書かなければ
  ボイラープレートなルーティングになります。
- **`description` は指示に使う言語で書いてください。** 埋め込みモデル
  (`potion-multilingual-128M`)は言語をまたいだ意味の整列が弱く、日本語の指示と
  英語の `description` の cosine 類似度は実測で 0.19〜0.26 — 閾値 0.45 に届き
  ません — なのに対し、同一言語同士なら 0.55〜0.79 あります。`llm-gateway
  init` は主にどの言語で指示を書くか尋ね、`default` を含むすべての route の
  `description` をその言語で生成します。
- **ワイルドカード route 名は、今や手書き設定向けの上級者用 escape hatch です。**
  `init` は生成せず、`GET /v1/models` にも載らず、分類器も採点しません。

```json5
routes: {
  "default": {
    description: "他の route に明確に当てはまらないリクエストの受け皿。",
    model: {
      default: "anthropic/*",
    },
  },

  "role-anthropic": {
    description: "慎重な段階的思考と完全なツール対応を必要とする、複雑な推論・コーディング・マルチステップな agent 的タスク。",
    model: {
      default: "anthropic/*",
      fallbacks: ["openrouter-anthropic/anthropic/*"],
    },
  },

  "role-cheap": {
    description: "要約、整形、コミットメッセージのような、低コストでレイテンシ重視の短い作業。",
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["groq/llama-3.3-70b-versatile"],
    },
  },

  "role-code": {
    description: "コード生成、リファクタリング、テスト作成、バグ修正。",
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
変換します — そしてこの判定は **route 単位ではなくターゲット単位**で行われます:
1 つの route の `default` と各 `fallbacks` はそれぞれ別のプロトコルを話して
よく、各試行はクライアントが送ってきたものに基づいて個別に変換またはパス
スルーされます。

| クライアントが話す | ターゲットが話す | 結果 |
|---|---|---|
| `anthropic-messages` | `openai-chat` | 変換される — Claude Code から Ollama、Groq、DeepSeek、Gemini、Mistral、Together、Sakana AI、PLaMo に到達できる |
| 両側が同じ | — | 従来どおりバイト単位の無加工転送 |
| それ以外の組み合わせ | — | そのターゲットはリクエスト送信前にスキップされる。route 内の全ターゲットがこの理由で到達不能な場合に限り `400` |

到達可能性がターゲットごとに判定されるため、1 つの route の中で default と
fallback に異なるプロトコルを自由に混在させられます — 例えば、無料の
`openai-chat` モデルを default にして、サブスクリプションの
`anthropic-messages` プロバイダーを fallback にする、といった構成です:

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
  "anthropic-subscription": { api: "anthropic-messages", transport: "claude-cli" },
},
routes: {
  // 分類がこの route に当てはまると判断したときに使われる。
  "role-cheap": {
    description: "短い定型作業: 要約、整形、コミットメッセージ生成",
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["anthropic-subscription/claude-sonnet-4-6"],
    },
  },
}
```

逆方向 — `openai-chat` のクライアントが `anthropic-messages` のターゲットに
到達しようとする場合 — の変換はまだ実装されていないため、そのターゲットは
`fallbacks` のどこに置かれていても、そのようなクライアントに対しては常に
スキップされます(`docs/roadmap.md` 参照)。

変換された*試行*(route の `default` とは限らず、実際にリクエストを処理する
ターゲット)で失われるもの:

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
  フォールバックが発火した場合、`resolved.translation` は route の
  `default` ではなく、実際に応答したターゲットの変換を反映します。
- 使用量集計には**影響しません**: トークン数は変換前のアップストリームの
  バイト列から読み取ります。

何が引き継がれ、何が引き継がれないかの完全な一覧は
`docs/ja/gotchas.md` を参照してください。

## サブスクリプションベースのプロバイダー

サブスクリプションは API キーではありません。Claude Pro/Max プランが認証
するのは *Claude Code* 自身、ChatGPT プランが認証するのは *Codex* 自身で
あり、ゲートウェイがどんな認証情報を持っていても、それらのプランを代理して
どちらのプロバイダーにも話すことはできません。だからこれらの場合に限り、
ゲートウェイは認証情報を持たず、代わりにログイン済みの公式クライアントを
そのまま実行します。`llm-gateway init` はどちらが欲しいかを尋ねます:

```
Anthropic: how do you pay for it?
  API key                    per-token billing; full API features
  Subscription (via `claude`)  no key; generation only — your tools are not passed through
```

| transport | 実行するもの | 見え方 | 検証状況 |
|---|---|---|---|
| `claude-cli` | `claude -p` | `anthropic-messages` | 済み — ストリーミング、ツール拒否、end to end |
| `codex-cli` | `codex exec` | `openai-chat` | 配線とエラー経路のみ検証済み; 詳細は後述 |

サブスクリプションを選んでも API キーのプロバイダーが消えることはありません
— プランは生成向き、キーはツールが要る何にでも向くので、設定は両方を持ち、
route ごとに使い分けられます。

```json5
providers: {
  // baseUrl も apiKey も無し: CLI 自身が認証する。
  "anthropic-subscription": { api: "anthropic-messages", transport: "claude-cli" },
  "openai-subscription": { api: "openai-chat", transport: "codex-cli" },
},
routes: {
  "default": {
    description: "他の route に明確に当てはまらないリクエストの受け皿。",
    model: { default: "anthropic/*" },
  },
  "role-sub": {
    description: "プロバイダー CLI 経由でローカルの Claude サブスクリプションに流すべき依頼。生成専用で、呼び出し元のツールは渡されない。",
    model: { default: "anthropic-subscription/sonnet" },
  },
  // model 文字列中の `default` は「CLI が設定されている通りのもの」を意味する
  // — ChatGPT プランがどのモデルを許すかは、ここからは分からない。
  "role-codex": {
    description: "Codex CLI 経由でローカルの ChatGPT サブスクリプションに流すべき依頼。",
    model: { default: "openai-subscription/default" },
  },
}
```

そこから先は他と変わらないただのプロバイダーです: route、フォールバック、
`trace`、`stats` すべて動きます。`transport: "claude-cli"` / `"codex-cli"`
だけが唯一異例な行で、対応するバイナリがインストールされていれば
`llm-gateway providers` は到達可能と報告します。

制限は CLI 由来であり、実在するものです:

- **あなたのツールはここに届きません。** `claude -p` が持つのは Claude
  Code 自身のツールであり、リクエストに含まれるツールではありません。
  ゲートウェイはそれらすべてを拒否するので、プロバイダー呼び出しが
  ファイルに触れることはできません。これは agent ループではなく、
  生成を一段上流で行っているだけです。
- **1 回のプロンプトになります。** `messages` 配列はラベル付きの
  書き起こしにフラット化されます。
- **呼び出しごとに ~5 秒のプロセス起動**があり、`temperature` /
  `top_p` / `stop_sequences` / `max_tokens` は破棄されます — CLI に
  対応するものがないためです。
- **`codex-cli` はトークン単位でストリーミングできません。** Codex の
  イベントはアイテム単位なので、回答は完成した状態で届きます —
  ゲートウェイはそれでも整形されたストリームを発行しますが、一度に
  まとめて届くだけです。`claude-cli` は正しくストリーミングします。
- リクエストは **API の残高ではなく、あなたのサブスクリプション**の
  制限を消費します。

生き残るもの: 本物のストリーミング(CLI が発行する Anthropic の
ストリームイベントがそのまま転送される)、キャッシュカウントを含む
`usage`、`stop_reason`。ゲートウェイの他の部分は何も変わりません —
変わるのは transport だけです。

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

日常的なスキーマは **4 つのトップレベルキー** だけです:
`server`、`providers`、`routes`、`logging`。

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
      apiKey: "sk-…",         // リテラル | "${ENV_VAR}" | "keychain:<name>" | "command:<cmd>"
      headers: { "X-Title": "llm-gateway" },
      injectUsage: true,
    },
  },
  routes: {
    "default": {
      description: "他の route に明確に当てはまらないリクエストの受け皿。",
      model: {
        default: "<provider>/<model>",
      },
    },
    "role-openai": {
      description: "OpenAI 系モデルによる汎用アシスタント作業、コーディング、ツール利用。",
      model: {
        default: "openai/*",                  // 最初の `/` でのみ分割される
        fallbacks: ["openrouter/openai/*"],   // プロトコルをまたいでもよい; 最初のバイト受信前に試行
      },
    },
  },
  logging: {
    dir: "./logs",
    usage: true,
    debug: false,             // trace-YYYY-MM-DD.jsonl — プロンプト本文が記録される!
    logging: false,           // コンソール診断ログ(embedding準備・fallback試行など)
  },
}
```

任意の上級者向けキーとして `launch` もあります。`init` はもう書き出さず、
大半の設定では不要ですが、クライアントごとの launcher 調整を手書きしたい
場合だけ追加します。

```json5
launch: {
  claude:   { extraArgs: [] },
  codex:    { wireApi: "responses", extraArgs: [] },
  opencode: { models: [], overrideProviders: ["openai", "anthropic"], extraArgs: [] },
}
```

| フィールド | 補足 |
|---|---|
| `server.apiKey` | 起動時に一度だけ解決されるため、変更には再起動が必要。`host` がループバック以外の場合は必須 — この 1 つのキーがすべてのプロバイダー認証情報を守る。 |
| `providers.<id>.apiKey` | リクエスト試行のたびに解決されるため、環境変数 / Keychain / `command:` のローテーションが即時反映される。 |
| `providers.<id>.api` | route の `default` と `fallbacks` はそれぞれ異なる `api` でもよい。クライアントのプロトコルから到達できるかは `config check` ではなく、リクエスト時に試行ごとに判定される。 |
| `routes.default` | 必須。どの route も分類閾値を超えないときの予約済み catch-all であり、同時に通常候補としても採点される。 |
| `routes.<name>.description` | ワイルドカードではない route では必須。インライン文字列、または `./` / `../` / `/` / `~/` のパス。これ自体が分類コーパス。 |
| `routes.<name>.model.default` | `"<provider>/<model>"`。最初の `/` でのみ分割される。 |
| `routes.<name>.model.fallbacks` | default と異なる `api` でもよい。最初のレスポンスバイト前に順番に試され、クライアントのプロトコルから到達できないターゲットはスキップされる([クロスプロトコルルーティング](#クロスプロトコルルーティング)参照)。 |
| `launch` | オプションの上級者用 escape hatch のみ: Claude/Codex/opencode の extra args、Codex の `wireApi`、opencode の `models` / `overrideProviders`。 |
| `logging.debug` | `--debug` はユーザーテキストを 200 文字に切り詰め、`--debug-full` は全文を残す。プロンプトが平文でディスクに残るため、意図的に有効化すること。 |
| `logging.logging` | デフォルトは無効。`true` にすると `serve` のコンソール診断ログ(どの route / provider が選ばれたか、embeddingモデルの準備、フォールバック各試行の結果)が stderr に出力される。明示的な `RUST_LOG` はこれより優先される。 |

## コマンド

```
llm-gateway serve [--debug] [--debug-full] [--port N]
llm-gateway init
llm-gateway launch <claude|codex|opencode> [--isolate] [--auto|--no-auto] [--print] [-- ARGS]
llm-gateway config check|show|gitignore
llm-gateway stats [--by route|client|provider|model|day] [--since D] [--until D]
llm-gateway trace [--tail] [--route R] [--client C]
llm-gateway providers
llm-gateway update [--check]
```

`update` は GitHub に最新リリースを問い合わせ、このビルドが古ければ
インストール方法に応じたアップグレードを実行します — Homebrew なら
`brew upgrade`、`cargo install` なら `cargo install --force`。自分の
バイナリを上書きすることはありません: それをするとパッケージマネージャーが
古いバージョンがまだあると思い込むためです。手で配置したバイナリの場合は
リリースのリンクを表示します。`--check` は何も変更せず報告だけします。

`serve` は他の何より先にポートへバインドします。そのポートがすでに
使われている場合 — ほとんどの場合、起動したままの前回の `llm-gateway serve`
です — `lsof` でそのプロセスを特定し、何もする前に確認を挟みます:

```
▲  port 4000 is already in use by another process (pid 12345)
◆  kill it and start this one instead?
│  ● Yes / ○ No
```

`No` と答えれば相手のプロセスには触れず起動もせずに終了し、`Yes` と答えれば
終了させてからバインドします。ターミナルにつながっていない非対話実行では、
推測せず `No` と答えたのと同じ結果になります。

## フォールバックがやること(と、やらないこと)

フォールバックは接続失敗・ヘッダータイムアウト・408・429・5xx で発動します —
**最初のレスポンスバイトを受け取る前**に限られます。ストリーミングが
始まったレスポンスは確定であり、生成途中の失敗でプロバイダーを切り替える
ことはできません。フォールバックは default と異なるプロトコルを話しても
構いません([クロスプロトコルルーティング](#クロスプロトコルルーティング)
参照)— クライアントのプロトコルから到達できないターゲットは、試行される
のではなくスキップされます。変換を一切挟まずに Anthropic プロトコルで
ベンダーをまたぐ冗長化をしたい場合は、OpenRouter の Anthropic 互換
エンドポイントにフォールバックを向けてください。

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
