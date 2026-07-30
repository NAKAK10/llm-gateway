# config.json reference

[English](config-reference.md) | [日本語](ja/config-reference.md)

Location: `~/.config/llm-gateway/config.json` (override the directory with
`LLM_GATEWAY_CONFIG_DIR`). Parsed as JSON5: comments, trailing commas and
unquoted keys are fine. Unknown fields are **rejected**, so typos fail loudly
instead of being ignored.

Hot reload: every field except `server.*` applies on save. A failed parse or
validation keeps the previous config live and logs why.

## server

| field | default | notes |
|---|---|---|
| `host` | `"127.0.0.1"` | Use the literal IP, not `localhost` (which may resolve to `::1`). Non-loopback requires `apiKey` — startup is refused otherwise. |
| `port` | `4000` | |
| `apiKey` | *(none)* | Inbound bearer token. Resolved **once at startup** (unlike provider keys). Accepted as `Authorization: Bearer …` or `x-api-key`. `/health` stays open. |

## providers.\<id\>

The id is what appears before the first `/` in `model` strings. The same
upstream may be registered under several ids to expose different protocols.

| field | default | notes |
|---|---|---|
| `baseUrl` | *(required)* | No trailing slash. For `anthropic-messages` this is the host root (`https://api.anthropic.com`) — the gateway appends `/v1/messages`. For the OpenAI kinds include the version prefix (`…/v1`) — the gateway appends `/chat/completions` or `/responses`. |
| `api` | *(required)* | `openai-chat` \| `openai-responses` \| `anthropic-messages`. A route may be reached from a client speaking a different protocol only when the gateway can translate the pair — today `anthropic-messages` in → `openai-chat` out, i.e. Claude Code to any OpenAI-compatible provider. Anything else is a `400`. |
| `apiKey` | *(none)* | Literal string \| `"${ENV_VAR}"` \| `"keychain:<name>"` (macOS Keychain, service `llm-gateway/<name>`). Resolved **per request attempt**, so rotation applies live. |
| `headers` | `{}` | Extra request headers, e.g. OpenRouter's optional `HTTP-Referer` / `X-Title`. |
| `injectUsage` | `true` | Streamed `openai-chat` only: adds `stream_options.include_usage` so token counts exist. Appends one usage-only chunk at stream end. |

## routes.\<name\>

The name is what clients send as `model`. Must not contain `:` or `/`.
A trailing `*` makes it a prefix wildcard; exact matches beat wildcards, and
among wildcards the longest prefix wins. Wildcard routes are not listed in
`GET /v1/models`.

| field | default | notes |
|---|---|---|
| `title` | *(none)* | Display only. |
| `description` | *(none)* | Inline text, or a path when it starts with `./` `../` `/` `~/` (relative paths resolve against the config dir). Future semantic routing classifies against this — write it as "when should this route be picked". |
| `model.default` | *(required)* | `"<provider>/<model>"`, split on the **first** `/` only — `openrouter/anthropic/claude-x` and `ollama-cloud/glm:cloud` both parse. `*` in the model part is replaced by the requested name. |
| `model.fallbacks` | `[]` | Tried in order, only before the first response byte, only on connect failure / timeout / 408 / 429 / 5xx. Must use providers with the same `api` as the default. |
| `semantic.candidates` | `[]` | **Phase 2 design — schema finalized, classifier not implemented yet.** Route names eligible for selection when *this* route is requested by name. Empty means "every other route that has a `description`". Candidates must have a `description` (the classification corpus); candidates the incoming request's protocol can neither match nor be translated to are excluded at match time. An explicit route name is never overridden — classification only runs for a route that itself carries `semantic`. A route name with `semantic` cannot end in `*`. |
| `semantic.threshold` | `0.45` | **Phase 2 design.** If the top-1 cosine similarity against the candidates falls below this, `model` on this route is used instead — so a route with `semantic` still requires `model`. |

## launch.\<client\>

| field | applies to | notes |
|---|---|---|
| `model` | all | Route name the client starts on. |
| `extraArgs` | all | Inserted before user-supplied arguments. |
| `wireApi` | codex | `"responses"` (default) or `"chat"`. The gateway serves both endpoints; which one your Codex accepts depends on its version. |
| `models` | opencode | Route names to expose. Empty = every non-wildcard route. Verified against the live gateway before starting. |

## logging

| field | default | notes |
|---|---|---|
| `dir` | `"./logs"` | Relative to the config dir. |
| `usage` | `true` | `usage-YYYY-MM.jsonl`, one line per proxied request (token-counting requests are not recorded). |
| `debug` | `false` | `trace-YYYY-MM-DD.jsonl` with full routing decisions **including prompt text** (200-char truncation; `serve --debug-full` disables truncation). CLI `--debug` also enables this. |

## Record formats

`usage-*.jsonl`:
`ts, client, route, provider, model, attempt, in_tok, out_tok, cache_read_tok,
cache_write_tok, dur_ms, status(success|aborted|error), stream, error?`

`trace-*.jsonl`:
`ts, req_id, client, endpoint, requested_model, input{messages_n,
last_user_text?, tokens_est, tools, has_image, stream}, routing{mode,
matched_route, reason, …scores when semantic}, resolved{provider, model, api, translation?},
attempts[{n, target, result, ms}], usage?{in_tok, out_tok}`

`resolved.translation` is present only when the request crossed protocols
(e.g. `"anthropic-messages->openai-chat"`); its absence means the response was
forwarded byte-for-byte.
