# config.json リファレンス

[English](../config-reference.md) | 日本語

場所: `~/.config/llm-gateway/config.json`(ディレクトリは
`LLM_GATEWAY_CONFIG_DIR` で変更可)。JSON5 としてパースされるため、
コメント・末尾カンマ・クォートなしのキーが使えます。未知のフィールドは
**拒否**されるので、タイポは黙って無視されず、はっきり失敗します。

ホットリロード: `server.*` 以外のすべてのフィールドは保存時に反映されます。
パースや検証に失敗した場合は直前の設定が生き続け、理由がログに出ます。

**破壊的なスキーマ変更:** 旧フォーマットからの移行処理はありません。
`~/.config/llm-gateway/config.json`(または `~/.config/llm-gateway/` 全体)を
削除して `llm-gateway init` をやり直してください。

通常用途のトップレベル形状は **4 キー** だけです:
`server`、`providers`、`routes`、`logging`。

`launch` も残っていますが、launcher 特有の癖に対応するための任意の手編集
escape hatch にすぎず、`init` はもう書き出しません。

## server

| フィールド | デフォルト | 補足 |
|---|---|---|
| `host` | `"127.0.0.1"` | `localhost` ではなくリテラルの IP を使うこと(`localhost` は `::1` に解決されることがある)。ループバック以外は `apiKey` が必須 — なければ起動を拒否。 |
| `port` | `4000` | |
| `apiKey` | *(なし)* | 受信側のベアラートークン。(プロバイダーキーと違い)**起動時に一度だけ**解決される。`Authorization: Bearer ...` または `x-api-key` で受け付ける。`/health` は常に開放。 |

## providers.<id>

id は `model` 文字列の最初の `/` より前に現れる部分です。同じ
アップストリームを複数の id で登録して、異なるプロトコルを公開できます。

| フィールド | デフォルト | 補足 |
|---|---|---|
| `baseUrl` | *(`http` では必須)* | 末尾スラッシュなし。`anthropic-messages` はホストのルート(`https://api.anthropic.com`)— ゲートウェイが `/v1/messages` を付加する。OpenAI 系はバージョンプレフィックス(`…/v1`)まで含める — ゲートウェイが `/chat/completions` または `/responses` を付加する。 |
| `api` | *(必須)* | `openai-chat` \| `openai-responses` \| `anthropic-messages`。あるルートに、その `api` とは異なるプロトコルを話すクライアントから到達できるのは、ゲートウェイがその組み合わせを変換できる場合だけ — 現時点では `anthropic-messages` 受信 → `openai-chat` 送信のみ(つまり Claude Code から任意の OpenAI 互換プロバイダーへ)。それ以外の組み合わせは `400` になる。 |
| `apiKey` | *(なし)* | リテラル文字列 \| `"${ENV_VAR}"` \| `"keychain:<name>"`(macOS Keychain、サービス名 `llm-gateway/<name>`) \| `"command:<cmd>"`(コマンドの標準出力、例: `command:gh auth token`)。**リクエスト試行ごとに**解決されるため、ローテーションは即時反映 — これが `command:` 形式の存在理由でもある。`serve` プロセスの環境変数は起動時に固定され、外部から更新できないため `${VAR}` ではローテーションするトークンを扱えない。コマンドは試行のたびに実行されるので、高速なものにすること。 |
| `headers` | `{}` | 追加リクエストヘッダー。例: OpenRouter の任意ヘッダー `HTTP-Referer` / `X-Title`。 |
| `transport` | `"http"` | `"http"` \| `"claude-cli"` \| `"codex-cli"`。CLI transport はリクエストを送る代わりにローカルのバイナリを実行する — これがサブスクリプションでゲートウェイのトラフィックを処理させる方法。この場合 `baseUrl` と `apiKey` は使われず(CLI 自身がログイン済み)、`api` は CLI の出力に応じて固定される: `claude-cli` なら `anthropic-messages`、`codex-cli` なら `openai-chat`。モデル部の `default` は「CLI が設定されている通りのもの」を意味する。README の「サブスクリプションベースのプロバイダー」を参照。 |
| `agentArgs` | `[]` | agent CLI のコマンドラインに追加される引数(`--add-dir`、別の `--permission-mode` など)。`http` transport では無視される。 |
| `injectUsage` | `true` | ストリーミングの `openai-chat` のみ: トークン数が取れるよう `stream_options.include_usage` を付加する。ストリーム末尾に usage のみのチャンクが 1 つ追加される。 |

## routes.<name>

name はクライアントが `model` として送るものです。`:` と `/` は使えません。
末尾の `*` は今でもプレフィックスワイルドカードとして受け付けますが、
ワイルドカード route は今や手書き設定向けの上級者用 escape hatch です。
`init` は生成せず、`GET /v1/models` にも載らず、分類器も採点しません。

ワイルドカードではない route は、予約済みの `default` を含めて、
すべて分類候補になります。

| フィールド | デフォルト | 補足 |
|---|---|---|
| `title` | *(なし)* | 表示用。 |
| `description` | *(ワイルドカードではない route では必須)* | インラインテキスト、または `./` `../` `/` `~/` で始まる場合はパス(相対パスは設定ディレクトリ基準)。これ自体が分類コーパスであり、各リクエストの最後の user message はすべての非ワイルドカード route の `description` と比較される。「この route がいつ勝つべきか」を書くこと。 |
| `model.default` | *(必須)* | `"<provider>/<model>"`。**最初の** `/` でのみ分割される — `openrouter/anthropic/claude-x` も `ollama-cloud/glm:cloud` もパースできる。モデル部の `*` が展開されるのは、ワイルドカード route が実際に解決されたときだけ。 |
| `model.fallbacks` | `[]` | 順番に試行。最初のレスポンスバイトより前、かつ接続失敗 / タイムアウト / 408 / 429 / 5xx のときのみ。default と同じ `api` のプロバイダーであること。 |

### 予約済み route: `default`

`default` という名前の route は **必須** です。これを省略したり、
ワイルドカードにしようとする設定は validation で拒否されます。

`default` には 2 つの役目があります:

1. 固定分類閾値 `0.45` をどの候補も超えなかったときの catch-all。
2. 自身の `description` と `model` を持つ普通の route として、
   その実力で分類に勝つこともできる候補。

分類がそもそも走れない場合 — たとえば既定の `semantic` feature を外した
ビルド(`--no-default-features`) — も、リクエストは `default` に落ちます。

### 分類の挙動

- 通常ビルドでは常時オン。`semantic` は **既定の cargo feature**。
- `llm-gateway init` は `config.json` を書く前に埋め込みモデルを必ず
  ダウンロードする。
- クライアントが送る `model` 文字列は route 選択では無視される。残るのは
  クライアント側の UX と、trace ログの `requested_model` だけ。
- 類似度は static な `model2vec-rs` 埋め込みと固定 cosine 閾値 `0.45`
  (`src/semantic/index.rs`)を使う。route ごとの閾値設定はない。

## launch.<client> (任意の上級者向けキー)

通常 `init` が生成する `config.json` では `launch` は丸ごと省略されます。
必要なのは launcher 固有の上書きが欲しいときだけです。

| フィールド | 対象 | 補足 |
|---|---|---|
| `extraArgs` | claude / codex / opencode | ユーザー指定の引数より前に挿入される。 |
| `wireApi` | codex | `"responses"`(既定)または `"chat"`。ゲートウェイは両エンドポイントを提供し、どちらを受け付けるかは Codex のバージョン次第。 |
| `models` | opencode | 公開する route 名。空 = ワイルドカード以外のすべての route。起動前に稼働中のゲートウェイに対して検証される。 |
| `overrideProviders` | opencode | `baseURL` をゲートウェイへ向け替える opencode 組み込み provider id。既定: `["openai", "anthropic"]`。 |

## logging

| フィールド | デフォルト | 補足 |
|---|---|---|
| `dir` | `"./logs"` | 設定ディレクトリからの相対パス。 |
| `usage` | `true` | `usage-YYYY-MM.jsonl`。プロキシしたリクエストごとに 1 行(トークンカウントのリクエストは記録されない)。 |
| `debug` | `false` | `trace-YYYY-MM-DD.jsonl`。**プロンプト本文を含む**完全なルーティング記録(200 文字で切り詰め; `serve --debug-full` で切り詰め無効)。CLI の `--debug` でも有効になる。 |

## レコード形式

`usage-*.jsonl`:
`ts, client, route, provider, model, attempt, in_tok, out_tok, cache_read_tok,
cache_write_tok, dur_ms, status(success|aborted|error), stream, error?`

`trace-*.jsonl`:
`ts, req_id, client, endpoint, requested_model, input{messages_n,
last_user_text?, tokens_est, tools, has_image, stream}, routing{mode,
matched_route, reason, …セマンティック時はスコアも}, resolved{provider, model, api, translation?},
attempts[{n, target, result, ms}], usage?{in_tok, out_tok}`

`routing.mode` は分類が走ったとき `semantic`、走れなかったとき
`no_classifier` で、その場合は `default` にフォールバックしたことを意味します。

`resolved.translation` はリクエストがプロトコルをまたいだ場合にのみ存在し
(例: `"anthropic-messages->openai-chat"`)、存在しなければレスポンスが
バイト単位で無加工のまま転送されたことを意味します。
