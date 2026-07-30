# プロバイダー

[English](../providers.md) | 日本語

プロバイダーとは `config.json` の `baseUrl` + `api` + `apiKey` にすぎず、
ゲートウェイ内にプロバイダーごとのコードは存在しません。したがって以下は
互換性マトリクスではなく、コピペで使える設定集です — 3 つのワイヤー
プロトコルのいずれかを話すエンドポイントであれば、このページに載って
いなくても**何でも**使えます。

`llm-gateway init` はこのページのすべてのプロバイダーの雛形を生成できます。
`llm-gateway providers` は設定済みの各プロバイダーを実際に叩き、キーが
解決できたか、エンドポイントが応答したかを表示します。

## 早見表

| プロバイダー | `baseUrl` | `api` | キーの環境変数 |
|---|---|---|---|
| Anthropic | `https://api.anthropic.com` | `anthropic-messages` | `ANTHROPIC_API_KEY` |
| OpenAI | `https://api.openai.com/v1` | `openai-responses` | `OPENAI_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | `openai-chat` / `anthropic-messages` | `OPENROUTER_API_KEY` |
| GitHub Copilot | `https://api.githubcopilot.com` | `openai-chat` | *(GitHub トークン — 下記参照)* |
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

`api` の値がそのプロバイダーにどのエンドポイントから到達できるかを決めます
— ただし 1 つだけ交差があります: `anthropic-messages` のクライアント
(Claude Code)は、ここに載っている任意の `openai-chat` プロバイダーに
到達できます。その方向は変換されるからです。そのようなルートで何を諦める
ことになるかは
[クロスプロトコルルーティング](../../README.ja.md#クロスプロトコルルーティング)
を参照してください。

`baseUrl` のルール(`docs/config-reference.md` 参照): 末尾スラッシュなし。
`anthropic-messages` のプロバイダーはホストのルートを指定(ゲートウェイが
`/v1/messages` を付加)、OpenAI 系のプロバイダーは `/v1` 相当のプレフィックス
まで含める(ゲートウェイが `/chat/completions` または `/responses` を付加)。

## プロバイダーごとの補足

### Anthropic

```json5
anthropic: {
  baseUrl: "https://api.anthropic.com",
  api: "anthropic-messages",
  apiKey: "${ANTHROPIC_API_KEY}",
},
```

ワイルドカードルート `claude-*`(`model: { default: "anthropic/*" }`)は
Claude Code が要求した id をそのまま転送するため、Anthropic の新しい
モデル id が出ても設定変更は不要です。

### OpenAI

```json5
openai: {
  baseUrl: "https://api.openai.com/v1",
  api: "openai-responses",
  apiKey: "${OPENAI_API_KEY}",
},
```

Codex はデフォルトで Responses API を話します。他のルートの
chat-completions フォールバック先としても使いたい場合は、
`api: "openai-chat"` で別 id をもう 1 つ登録してください
(フォールバックはプロトコルをまたげません)。

### OpenRouter

```json5
openrouter: {
  baseUrl: "https://openrouter.ai/api/v1",
  api: "openai-chat",
  apiKey: "${OPENROUTER_API_KEY}",
  headers: { "X-Title": "llm-gateway" },   // 任意のアトリビューション
},
// 同じアップストリームを Anthropic プロトコルで — `claude-*` が ApiKind を
// またがずにフォールバックできるようにする:
"openrouter-anthropic": {
  baseUrl: "https://openrouter.ai/api/v1",
  api: "anthropic-messages",
  apiKey: "${OPENROUTER_API_KEY}",
},
```

モデル id は `/` を含みます(`anthropic/claude-sonnet-4.6`)。ルートの
ターゲットは*最初の* `/` でのみ分割されるため、
`openrouter/anthropic/claude-sonnet-4.6` は正しくパースされます。

### GitHub Copilot

あなたの Copilot サブスクリプションに、ゲートウェイがフロントに立つ
どのクライアントからでも到達できます — Claude Code も
[クロスプロトコルルーティング](../../README.ja.md#クロスプロトコルルーティング)
経由で含みます。

```json5
"github-copilot": {
  baseUrl: "https://api.githubcopilot.com",
  api: "openai-chat",
  // `gh` はこのトークンを自身のスケジュールで更新するので、コピーせず
  // リクエストのたびに読み直す。複数アカウントがあるなら `--user <login>` を追加。
  apiKey: "command:gh auth token",
  headers: { "X-GitHub-Api-Version": "2026-06-01" },
},
```

`llm-gateway init` は `gh` が `PATH` にあればこのプロバイダーを提案し、
`command:` の参照を含めて上記そのままの雛形を書き出します。

認証情報は普通の GitHub トークンです — Copilot 専用の API キーも
トークン交換のステップもありません。どれも動きます:

- `gh auth login` してから `command:gh auth token`(推奨: 期限切れになりません)。
- `GITHUB_COPILOT_TOKEN` に置いた個人アクセストークンを
  `"${GITHUB_COPILOT_TOKEN}"` として参照。
- エディタ連携が既に保存しているトークンを Keychain に貼り付けたもの
  (`keychain:github-copilot`)。

`X-GitHub-Api-Version` は必須ではなく、素の `Authorization: Bearer` だけでも
動きます — ただしピン留めしておけば、GitHub 側のデフォルトが将来変わっても
セッション途中でレスポンスの形が変わることはありません。

**実際に使えるモデルは一覧より狭いです。**
`GET https://api.githubcopilot.com/models` は、プランで触れないモデルも含めて
Copilot が知っている全モデルを返します。触れないモデルは
`400 model_not_supported` を返します。手がかりになるのは `policy.state`
フィールドで、プレミアムモデルは概ね GitHub Copilot の設定で先に有効化が
必要です。確認するには:

```sh
curl -s https://api.githubcopilot.com/models \
  -H "Authorization: Bearer $(gh auth token)" \
  -H "X-GitHub-Api-Version: 2026-06-01" \
  | jq -r '.data[] | select(.capabilities.type=="chat") | "\(.id)\t\(.policy.state // "-")"'
```

もう 2 点:

- リクエストは他の Copilot 利用と同様にあなたの Copilot クォータに
  課金されます。ゲートウェイは Copilot の `x-initiator` / `Openai-Intent`
  分類ヘッダーを送りません — 正しい値は個々のリクエストに依存するため、
  ゲートウェイ全体で一定の値を送ると半分は誤りになるからです。
- Copilot は Claude モデル向けに `/v1/messages` も公開しており、これなら
  変換なしで済むはずですが、ゲートウェイはまだ使えません: そのエンドポイントは
  `Authorization: Bearer` を要求する一方、`anthropic-messages` プロバイダーは
  `x-api-key` で認証するためです。フォローアップとして追跡中です。

### Google Gemini

```json5
gemini: {
  baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
  api: "openai-chat",
  apiKey: "${GEMINI_API_KEY}",
},
```

Gemini API(`gemini-2.5-pro` などのモデル)の OpenAI 互換エンドポイントです。

### xAI (Grok)

```json5
xai: {
  baseUrl: "https://api.x.ai/v1",
  api: "openai-chat",
  apiKey: "${XAI_API_KEY}",
},
```

### Mistral

```json5
mistral: {
  baseUrl: "https://api.mistral.ai/v1",
  api: "openai-chat",
  apiKey: "${MISTRAL_API_KEY}",
},
```

### DeepSeek

```json5
deepseek: {
  baseUrl: "https://api.deepseek.com/v1",
  api: "openai-chat",
  apiKey: "${DEEPSEEK_API_KEY}",
},
```

### Groq

```json5
groq: {
  baseUrl: "https://api.groq.com/openai/v1",
  api: "openai-chat",
  apiKey: "${GROQ_API_KEY}",
},
```

プレフィックスが標準と異なる `/openai/v1` である点に注意。

### Together AI

```json5
together: {
  baseUrl: "https://api.together.xyz/v1",
  api: "openai-chat",
  apiKey: "${TOGETHER_API_KEY}",
},
```

### Sakana AI (Fugu)

```json5
sakana: {
  baseUrl: "https://api.sakana.ai/v1",
  api: "openai-chat",
  apiKey: "${SAKANA_API_KEY}",
},
```

Sakana AI の Fugu は OpenAI 互換 API の背後にあるオーケストレーション
モデルで、モデルは `fugu` と `fugu-ultra` です。キーは
[console.sakana.ai](https://console.sakana.ai) で取得できます。コンソールの
ダッシュボードにアカウントごとのベース URL が表示されるため、上記と異なる
場合はそちらを優先してください。Fugu は Anthropic 互換の Messages
エンドポイントも公開しているので、`claude-*` ルートのフォールバック先に
したい場合は `api: "anthropic-messages"` で別のプロバイダー id を
登録してください。

```json5
routes: {
  "role-orchestrator": {
    model: { default: "sakana/fugu-ultra", fallbacks: ["sakana/fugu"] },
  },
},
```

### PLaMo (Preferred Networks)

```json5
plamo: {
  baseUrl: "https://api.platform.preferredai.jp/v1",
  api: "openai-chat",
  apiKey: "${PLAMO_API_KEY}",
},
```

日本語に強い国産 LLM で、OpenAI 互換 API を提供しています
([docs.plamo.preferredai.jp](https://docs.plamo.preferredai.jp/))。

### Ollama

```json5
"ollama-cloud": {
  baseUrl: "https://ollama.com/v1",
  api: "openai-chat",
  apiKey: "${OLLAMA_API_KEY}",
},
"ollama-local": {
  baseUrl: "http://127.0.0.1:11434/v1",
  api: "openai-chat",
  apiKey: "local",   // プレースホルダー — ローカルサーバーは無視する
},
```

モデルの*値*には `:` を含められます(`ollama-cloud/glm-5.2:cloud`)—
禁止されているのはルート*名*だけです。

## それ以外のエンドポイント

OpenAI 互換のエンドポイント(vLLM、LM Studio、llama.cpp server、LiteLLM、
各クラウドベンダーの互換レイヤーなど)は `providers` エントリを 1 つ
追加するだけで使えます:

```json5
"my-vllm": {
  baseUrl: "http://127.0.0.1:8000/v1",
  api: "openai-chat",
  apiKey: "local",
},
```

あとはルートを `my-vllm/<model>` に向けて、`llm-gateway providers` で
疎通を確認してください。
