# config.json リファレンス

[English](../config-reference.md) | 日本語

場所: `~/.config/llm-gateway/config.json`(ディレクトリは
`LLM_GATEWAY_CONFIG_DIR` で変更可)。JSON5 としてパースされるため、
コメント・末尾カンマ・クォートなしのキーが使えます。未知のフィールドは
**拒否**されるので、タイポは黙って無視されず、はっきり失敗します。

ホットリロード: `server.*` 以外のすべてのフィールドは保存時に反映されます。
パースや検証に失敗した場合は直前の設定が生き続け、理由がログに出ます。

## server

| フィールド | デフォルト | 補足 |
|---|---|---|
| `host` | `"127.0.0.1"` | `localhost` ではなくリテラルの IP を使うこと(`localhost` は `::1` に解決されることがある)。ループバック以外は `apiKey` が必須 — なければ起動を拒否。 |
| `port` | `4000` | |
| `apiKey` | *(なし)* | 受信側のベアラートークン。(プロバイダーキーと違い)**起動時に一度だけ**解決される。`Authorization: Bearer …` または `x-api-key` で受け付ける。`/health` は常に開放。 |

## providers.\<id\>

id は `model` 文字列の最初の `/` より前に現れる部分です。同じ
アップストリームを複数の id で登録して、異なるプロトコルを公開できます。

| フィールド | デフォルト | 補足 |
|---|---|---|
| `baseUrl` | *(必須)* | 末尾スラッシュなし。`anthropic-messages` はホストのルート(`https://api.anthropic.com`)— ゲートウェイが `/v1/messages` を付加する。OpenAI 系はバージョンプレフィックス(`…/v1`)まで含める — ゲートウェイが `/chat/completions` または `/responses` を付加する。 |
| `api` | *(必須)* | `openai-chat` \| `openai-responses` \| `anthropic-messages`。あるルートに、その `api` とは異なるプロトコルを話すクライアントから到達できるのは、ゲートウェイがその組み合わせを変換できる場合だけ — 現時点では `anthropic-messages` 受信 → `openai-chat` 送信のみ(つまり Claude Code から任意の OpenAI 互換プロバイダーへ)。それ以外の組み合わせは `400` になる。 |
| `apiKey` | *(なし)* | リテラル文字列 \| `"${ENV_VAR}"` \| `"keychain:<name>"`(macOS Keychain、サービス名 `llm-gateway/<name>`)\| `"command:<cmd>"`(コマンドの標準出力、例: `command:gh auth token`)。**リクエスト試行ごとに**解決されるため、ローテーションは即時反映 — これが `command:` 形式の存在理由でもある。`serve` プロセスの環境変数は起動時に固定され、外部から更新できないため `${VAR}` ではローテーションするトークンを扱えない。コマンドは試行のたびに実行されるので、高速なものにすること。 |
| `headers` | `{}` | 追加リクエストヘッダー。例: OpenRouter の任意ヘッダー `HTTP-Referer` / `X-Title`。 |
| `injectUsage` | `true` | ストリーミングの `openai-chat` のみ: トークン数が取れるよう `stream_options.include_usage` を付加する。ストリーム末尾に usage のみのチャンクが 1 つ追加される。 |

## routes.\<name\>

name はクライアントが `model` として送るものです。`:` と `/` は使えません。
末尾の `*` はプレフィックスワイルドカードになります。完全一致が
ワイルドカードに勝ち、ワイルドカード同士では最長プレフィックスが勝ちます。
ワイルドカードルートは `GET /v1/models` には載りません。

| フィールド | デフォルト | 補足 |
|---|---|---|
| `title` | *(なし)* | 表示用。 |
| `description` | *(なし)* | インラインテキスト、または `./` `../` `/` `~/` で始まる場合はパス(相対パスは設定ディレクトリ基準)。将来のセマンティックルーティングはこれに対して分類するので、「このルートはいつ選ばれるべきか」を書くこと。 |
| `model.default` | *(必須)* | `"<provider>/<model>"`。**最初の** `/` でのみ分割される — `openrouter/anthropic/claude-x` も `ollama-cloud/glm:cloud` もパースできる。モデル部の `*` はリクエストされた名前に置き換えられる。 |
| `model.fallbacks` | `[]` | 順番に試行。最初のレスポンスバイトより前、かつ接続失敗 / タイムアウト / 408 / 429 / 5xx のときのみ。default と同じ `api` のプロバイダーであること。 |

## launch.\<client\>

| フィールド | 対象 | 補足 |
|---|---|---|
| `model` | 全クライアント | クライアントが起動時に使うルート名。 |
| `extraArgs` | 全クライアント | ユーザー指定の引数より前に挿入される。 |
| `wireApi` | codex | `"responses"`(デフォルト)または `"chat"`。ゲートウェイは両エンドポイントを提供する。どちらを受け付けるかは Codex のバージョン次第。 |
| `models` | opencode | 公開するルート名。空 = ワイルドカード以外のすべてのルート。起動前に稼働中のゲートウェイに対して検証される。 |

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

`resolved.translation` はリクエストがプロトコルをまたいだ場合にのみ存在し
(例: `"anthropic-messages->openai-chat"`)、存在しなければレスポンスが
バイト単位で無加工のまま転送されたことを意味します。
