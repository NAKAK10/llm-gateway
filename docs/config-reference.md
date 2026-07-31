# config.json reference

[English](config-reference.md) | [日本語](ja/config-reference.md)

Location: `~/.config/llm-gateway/config.json` (override the directory with
`LLM_GATEWAY_CONFIG_DIR`). Parsed as JSON5: comments, trailing commas and
unquoted keys are fine. Unknown fields are **rejected**, so typos fail loudly
instead of being ignored.

Hot reload: every field except `server.*` applies on save. A failed parse or
validation keeps the previous config live and logs why.

**Breaking schema change:** there is no migration path from the old config
format. Delete `~/.config/llm-gateway/config.json` (or the whole
`~/.config/llm-gateway/` directory) and re-run `llm-gateway init`.

For normal use the top-level shape is just **four keys**:
`server`, `providers`, `routes`, `logging`.

`launch` still exists, but only as an optional hand-edited escape hatch for
launcher quirks; `init` does not write it anymore.

## server

| field | default | notes |
|---|---|---|
| `host` | `"127.0.0.1"` | Use the literal IP, not `localhost` (which may resolve to `::1`). Non-loopback requires `apiKey` — startup is refused otherwise. |
| `port` | `4000` | |
| `apiKey` | *(none)* | Inbound bearer token. Resolved **once at startup** (unlike provider keys). Accepted as `Authorization: Bearer ...` or `x-api-key`. `/health` stays open. |

## providers.<id>

The id is what appears before the first `/` in `model` strings. The same
upstream may be registered under several ids to expose different protocols.

| field | default | notes |
|---|---|---|
| `baseUrl` | *(required for `http`)* | No trailing slash. For `anthropic-messages` this is the host root (`https://api.anthropic.com`) — the gateway appends `/v1/messages`. For the OpenAI kinds include the version prefix (`…/v1`) — the gateway appends `/chat/completions` or `/responses`. |
| `api` | *(required)* | `openai-chat` \| `openai-responses` \| `anthropic-messages`. A route may be reached from a client speaking a different protocol only when the gateway can translate the pair — today `anthropic-messages` in → `openai-chat` out, i.e. Claude Code to any OpenAI-compatible provider. Anything else is a `400`. |
| `apiKey` | *(none)* | Literal string \| `"${ENV_VAR}"` \| `"keychain:<name>"` (macOS Keychain, service `llm-gateway/<name>`) \| `"command:<cmd>"` (stdout of a command, e.g. `command:gh auth token`). Resolved **per request attempt**, so rotation applies live — which is the point of the `command:` form, since a `serve` process's environment is fixed at startup and cannot be updated from outside. A command runs on every attempt, so keep it fast. |
| `headers` | `{}` | Extra request headers, e.g. OpenRouter's optional `HTTP-Referer` / `X-Title`. |
| `transport` | `"http"` | `"http"` \| `"claude-cli"` \| `"codex-cli"`. The CLI transports run a local binary instead of making a request, which is how a subscription can serve gateway traffic — `baseUrl` and `apiKey` are then unused (the CLI holds its own login), and `api` is fixed by the CLI's output: `anthropic-messages` for `claude-cli`, `openai-chat` for `codex-cli`. A model part of `default` means "whatever the CLI is configured to use". See the README's "Subscription-backed providers". |
| `agentArgs` | `[]` | Extra arguments appended to an agent CLI's command line (`--add-dir`, a different `--permission-mode`). Ignored by the `http` transport. |
| `injectUsage` | `true` | Streamed `openai-chat` only: adds `stream_options.include_usage` so token counts exist. Appends one usage-only chunk at stream end. |

## routes.<name>

The name is what clients send as `model`. Must not contain `:` or `/`.
A trailing `*` is still accepted as a prefix wildcard, but wildcard routes are
an advanced hand-written escape hatch now: `init` does not generate them,
`GET /v1/models` does not list them, and classification never scores them.

Every **non-wildcard** route participates as a classification candidate,
including the reserved `default` route.

| field | default | notes |
|---|---|---|
| `title` | *(none)* | Display only. |
| `description` | *(required on non-wildcard routes)* | Inline text, or a path when it starts with `./` `../` `/` `~/` (relative paths resolve against the config dir). This is the classification corpus: every request's newest user text (walking back through history when the newest scores below the threshold — see "Classification behavior") is embedded and compared against every non-wildcard route's `description`. Write it as "when should this route win?" Write it **in the language you give instructions in** — the embedding model aligns meaning weakly across languages, so a description in a different language than the request scores far lower (see the README's [Content-classified routing](../README.md#content-classified-routing)). `llm-gateway init` generates every `description`, including `default`'s, in whichever language you tell it you write instructions in. |
| `model.default` | *(required)* | `"<provider>/<model>"`, split on the **first** `/` only — `openrouter/anthropic/claude-x` and `ollama-cloud/glm:cloud` both parse. `*` in the model part expands only when a wildcard route is actually resolved. |
| `model.fallbacks` | `[]` | Tried in order, only before the first response byte, only on connect failure / timeout / 408 / 429 / 5xx. May use a provider with a different `api` than the default — reachability from the client's protocol is checked per attempt at request time (see [Cross-protocol routing](../README.md#cross-protocol-routing)), not by `config check`; a target the client's protocol cannot reach is skipped. |

### Reserved route: `default`

A route literally named `default` is **required**. Validation rejects configs
that omit it or try to make it a wildcard.

`default` has two jobs:

1. it is the catch-all when no candidate clears the fixed classification
   threshold `0.45`; and
2. it is a perfectly ordinary route with its own `description` and `model`, so
   it can also win classification on its own merits.

If classification cannot run at all — for example a build without the default
`semantic` feature (`--no-default-features`) — requests also fall back to
`default`.

### Classification behavior

- Always on in normal builds. `semantic` is a **default cargo feature**.
- `llm-gateway init` downloads the embedding model unconditionally before it
  writes `config.json`.
- The client's requested `model` string is ignored for route selection. It is
  kept only for client-side UX and trace logs' `requested_model` field.
- Similarity uses static `model2vec-rs` embeddings with a fixed cosine
  threshold `0.45` (`src/semantic/index.rs`). There is no per-route threshold.
- The newest user text is classified first; a match routes the request and a
  genuine topic change always wins immediately. When it scores below the
  threshold — or the newest user message has no text at all (an agentic
  `tool_result` turn) — classification walks back through up to 8 earlier user
  texts and takes the most recent one that clears the bar, so a conversation
  keeps its route across ambiguous turns. The walk is stateless: it reruns from
  the request's own message history every time, so the same request always
  classifies the same way regardless of gateway restarts or config reloads.
- Before any text is embedded, `<system-reminder>...</system-reminder>` blocks
  are stripped from it (`src/server/proxy.rs`, `classification_texts`) — this
  is harness-injected context, not the user's own words, and it would
  otherwise skew both the newest-text score and every text the walk-back
  tries. A user message left blank after stripping counts as no text and is
  skipped exactly like a textless `tool_result` turn. This only changes the
  classification input: the payload forwarded to the provider is never
  modified. A block with no closing tag has everything from `<system-reminder>`
  to the end of the text removed.

## launch.<client> (optional advanced key)

A normal generated `config.json` omits `launch` entirely. Add it only when you
need launcher-specific overrides.

| field | applies to | notes |
|---|---|---|
| `extraArgs` | claude / codex / opencode | Inserted before user-supplied arguments. |
| `wireApi` | codex | `"responses"` (default) or `"chat"`. The gateway serves both endpoints; which one your Codex accepts depends on its version. |
| `models` | opencode | Route names to expose. Empty = every non-wildcard route. Verified against the live gateway before starting. |
| `overrideProviders` | opencode | Built-in opencode provider ids whose `baseURL` is redirected to the gateway. Default: `["openai", "anthropic"]`. |

## logging

| field | default | notes |
|---|---|---|
| `dir` | `"./logs"` | Relative to the config dir. |
| `usage` | `true` | `usage-YYYY-MM.jsonl`, one line per proxied request (token-counting requests are not recorded). |
| `debug` | `false` | `trace-YYYY-MM-DD.jsonl` with full routing decisions **including prompt text** (200-char truncation; `serve --debug-full` disables truncation). CLI `--debug` also enables this. |
| `logging` | `false` | Console (stderr) diagnostics from `serve` — which route/provider was picked, embedding-model preparation, and per-attempt fallback outcomes. Off by default so a plain gateway process stays quiet; set to `true` to see them. Unrelated to the on-disk `usage`/`debug` logs above, and an explicit `RUST_LOG` still overrides it. |

## Record formats

`usage-*.jsonl`:
`ts, client, route, provider, model, attempt, in_tok, out_tok, cache_read_tok,
cache_write_tok, dur_ms, status(success|aborted|error), stream, error?`

`trace-*.jsonl`:
`ts, req_id, client, endpoint, requested_model, input{messages_n,
last_user_text?, tokens_est, tools, has_image, stream}, routing{mode,
matched_route, reason, decided_by_text?, walk?, …scores when semantic}, resolved{provider, model, api, translation?},
attempts[{n, target, result, ms}], usage?{in_tok, out_tok}`

`routing.mode` is `semantic` when the newest user text decided the route
(match or below-threshold fallback), `semantic_history` when the newest text
scored below the threshold and an earlier user text matched instead (the
`reason` says how far back), `no_text` when the request carried no classifiable
user text at all, and `no_classifier` when classification could not run. Every
mode except `semantic`'s match and `semantic_history` means the request fell
back to `default`.

`routing.decided_by_text` is the first 200 characters of whichever text
actually decided the route — present only on a match (`semantic` or
`semantic_history`), absent on a `default` fallback. `routing.walk` lists
every text the history walk tried, in order, as `{texts_back, top_score}`
pairs, so a single trace line shows which text won and how the walk got there
without having to re-derive it from scores alone.

`resolved.translation` is present only when the request crossed protocols
(e.g. `"anthropic-messages->openai-chat"`); its absence means the response was
forwarded byte-for-byte. It always describes the target in `resolved` — i.e.
whichever attempt actually served the response — not the route's `default`,
since a fallback may sit on the other side of a protocol boundary.
