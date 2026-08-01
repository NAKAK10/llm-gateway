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
| `api` | *(required)* | `openai-chat` \| `openai-responses` \| `anthropic-messages`. A route may be reached from a client speaking a different protocol only when the gateway can translate the pair — today `anthropic-messages` in → `openai-chat` out (Claude Code to any OpenAI-compatible provider) and `openai-responses` in → `openai-chat` out (Codex CLI to the same providers — needed since Codex CLI 0.145+ requires `wire_api = "responses"` and no longer accepts `"chat"`). Anything else is a `400`. |
| `apiKey` | *(none)* | Literal string \| `"${ENV_VAR}"` \| `"keychain:<name>"` (macOS Keychain, service `llm-gateway/<name>`) \| `"command:<cmd>"` (stdout of a command, e.g. `command:gh auth token`). Resolved **per request attempt**, so rotation applies live — which is the point of the `command:` form, since a `serve` process's environment is fixed at startup and cannot be updated from outside. A command runs on every attempt, so keep it fast. |
| `headers` | `{}` | Extra request headers, e.g. OpenRouter's optional `HTTP-Referer` / `X-Title`. |
| `transport` | `"http"` | `"http"` \| `"claude-cli"` \| `"codex-cli"`. The CLI transports run a local binary instead of making a request, which is how a subscription can serve gateway traffic — `baseUrl` and `apiKey` are then unused (the CLI holds its own login), and `api` is fixed by the CLI's output: `anthropic-messages` for `claude-cli`, `openai-chat` for `codex-cli`. A model part of `default` means "whatever the CLI is configured to use". See the README's "Subscription-backed providers". |
| `agentArgs` | `[]` | Extra arguments appended to an agent CLI's command line (`--add-dir`, a different `--permission-mode`). Ignored by the `http` transport. |
| `injectUsage` | `true` | Streamed `openai-chat` only: adds `stream_options.include_usage` so token counts exist. Appends one usage-only chunk at stream end. |

## routes.<name>

The name is what clients send as `model`. Must not contain `*`, `:`, or `/` —
route names are matched exactly; a `*` anywhere in the name (a wildcard route
name, e.g. `claude-*`) fails config validation.

Every route participates as a classification candidate, including the
reserved `default` route.

| field | default | notes |
|---|---|---|
| `title` | *(none)* | Display only. |
| `description` | *(required)* | `string` or `string[]`. Each entry is inline text, or a path when it starts with `./` `../` `/` `~/` (relative paths resolve against the config dir). This is the classification corpus: every request's newest user text (walking back through history when the newest scores below the threshold — see "Classification behavior") is embedded and compared against every route's `description`. Write it as "when should this route win?" Write it **in the language you give instructions in** — the embedding model aligns meaning weakly across languages, so a description in a different language than the request scores far lower (see the README's [Content-classified routing](../README.md#content-classified-routing)). A `string[]` embeds each entry separately and scores the route by the **max cosine across all variants** — the typical use is one variant per language, so mixed-language traffic (a human writing Japanese, a sub-agent or harness sending English) matches whichever variant fits, instead of diluting both in one mean-pooled string. `llm-gateway init` generates every `description`, including `default`'s, in whichever language you tell it you write instructions in — and as a two-entry array (`[that language, English]`) whenever the chosen language isn't English. |
| `model.default` | *(required)* | `"<provider>/<model>"`, split on the **first** `/` only — `openrouter/anthropic/claude-x` and `ollama-cloud/glm:cloud` both parse. No `*` allowed in the model part; every model must be explicit. |
| `model.fallbacks` | `[]` | Tried in order, only before the first response byte, only on connect failure / timeout / 408 / 429 / 5xx. May use a provider with a different `api` than the default — reachability from the client's protocol is checked per attempt at request time (see [Cross-protocol routing](../README.md#cross-protocol-routing)), not by `config check`; a target the client's protocol cannot reach is skipped. |

### Reserved route: `default`

A route literally named `default` is **required**. Validation rejects configs
that omit it.

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
- Before any user text is looked at, the request's **system prompt** — if it
  has one — is classified on its own, at a stricter cosine threshold `0.50`
  (`SYSTEM_CLASSIFICATION_THRESHOLD`, `src/semantic/index.rs`). Extraction is
  per `ApiKind`: Anthropic Messages' `system` field, OpenAI Chat's leading
  `system`/`developer` message, or OpenAI Responses' `instructions` field
  (falling back to a leading `system`/`developer` item in `input` when
  `instructions` is empty — see `system_prompt_text`,
  `src/server/proxy.rs`). A match short-circuits the newest-user-text walk
  below entirely — `routing.mode = "semantic_system"` — because a system
  prompt (an agent's own role definition, e.g. a Claude Code subagent's
  `.claude/agents/*.md` prompt) is a stronger signal than user text, which
  can be pulled toward an unrelated route by whatever object the
  instruction happens to mention. The same 800-character / 64-token
  embedding bound as user text applies (see `Embedder::embed`), so only the
  *beginning* of a long system prompt ever reaches the classifier — the
  threshold is stricter than the user-text one specifically so that a
  harness's own generic preamble ("You are Claude Code, ...") does not
  clear it and hijack every request of a session. No system prompt, an
  empty one, one that strips down to blank after
  `<system-reminder>...</system-reminder>` removal (see below), or a miss
  against `0.50` all fall through unaffected to the newest-user-text walk.
- Among user texts, the newest is classified first; a match routes the
  request and a genuine topic change always wins immediately. When it scores
  below the
  threshold — or the newest user message has no text at all (an agentic
  `tool_result` turn) — classification walks back through up to 8 earlier user
  texts and takes the most recent one that clears the bar, so a conversation
  keeps its route across ambiguous turns. The walk is stateless: it reruns from
  the request's own message history every time, so the same request always
  classifies the same way regardless of gateway restarts or config reloads.
- A route's score is the **max cosine across its `description` variants**
  when `description` is a `string[]`. Each variant is embedded independently
  at config-load time; a plain `string` is the one-element case of the same
  scoring rule.
- A request whose newest user text begins with `<transcript>` — Claude Code's
  own internal auto-mode judgment call, not a real user turn — skips
  classification entirely rather than being scored against `description`s;
  see "autoMode" above for where it resolves instead. This check runs
  **before** system-prompt classification too, so it is checked first
  overall: `<transcript>` bypass, then system prompt, then the user-text
  walk.
- Before any text is embedded, `<system-reminder>...</system-reminder>` blocks
  are stripped from it (`src/server/proxy.rs`, `classification_texts` for user
  text, `system_prompt_text` for the system prompt) — this is harness-injected
  context, not the user's (or agent definition's) own words, and it would
  otherwise skew both the newest-text score and every text the walk-back
  tries. A user message (or a system prompt) left blank after stripping
  counts as no text and is skipped — exactly like a textless `tool_result`
  turn for user text, or "no system prompt at all" for the system prompt.
  This only changes the classification input: the payload forwarded to the
  provider is never modified. A block with no closing tag has everything
  from `<system-reminder>` to the end of the text removed.

## autoMode (optional)

| field | default | notes |
|---|---|---|
| `autoMode.default` | *(none)* | Same shape as `routes.<name>.model.default`: `"<provider>/<model>"`, no wildcard. |
| `autoMode.fallbacks` | `[]` | Same shape as `routes.<name>.model.fallbacks`. |

Pins the target for Claude Code's own **internal** auto-mode judgment
requests — the yes/no permission-approval calls its harness makes to decide
whether an action needs the user's confirmation, sent through the same
gateway endpoint a real turn would use, marked by a `<transcript>`-prefixed
message. These are never classified against `routes.*.description` (see
"Classification behavior" below) and never depend on a route name or the
client's requested `model` string — `autoMode`, when set, is resolved
directly, exactly like a route's `model` but without a route-name lookup at
all, so it can be pointed at a fast, cheap model regardless of what model
name Claude Code's internal classifier happens to send. The gateway does not
fabricate the judgment itself; a real model still answers it, just the one
you pin here instead of whichever target the fallback below would have used.

**Unset** (the default) keeps the pre-existing behavior: such a request
resolves by the client-sent model name if it happens to match a route, or
the reserved `default` route otherwise. That fallback can be a problem in
practice — if `default` (or the requested model name) points at something
slow (a multi-second `claude-cli`/`codex-cli` subprocess, say), Claude
Code's own timeout for this judgment can trip and the action gets rejected
with "Auto mode could not evaluate this action" (see the 2026-08-01 entry in
`docs/decisions.md`). Setting `autoMode` to a fast, ordinary model sidesteps
that regardless of what `default` is doing.

`llm-gateway init` asks whether to configure this (recommended) once
provider/role selection is done, offering the providers you already picked —
preferring a fast alias (`haiku`) over the usual `sonnet` default when a
`claude-cli` subscription's model list is shown, since speed matters more
than strength here.

## launch.<client> (optional advanced key)

A normal generated `config.json` omits `launch` entirely. Add it only when you
need launcher-specific overrides.

| field | applies to | notes |
|---|---|---|
| `extraArgs` | claude / codex / opencode | Inserted before user-supplied arguments. |
| `wireApi` | codex | `"responses"` (default) or `"chat"`. The gateway serves both endpoints; which one your Codex accepts depends on its version — Codex CLI 0.145+ removed `"chat"` entirely and requires `"responses"`, and the gateway now translates `openai-responses → openai-chat` per attempt so an `openai-chat`-only provider is still reachable with `wireApi` left at the default. |
| `models` | opencode | Route names to expose. Empty = every route. Verified against the live gateway before starting. |
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
last_user_text?, system_text?, tokens_est, tools, has_image, stream},
routing{mode, matched_route, reason, decided_by_text?, walk?, system_score?,
…scores when semantic}, resolved{provider, model, api, translation?},
attempts[{n, target, result, ms}], usage?{in_tok, out_tok, cache_read_tok,
cache_write_tok}`

`routing.mode` is `semantic_system` when the request's **system prompt**
decided the route on its own — cleared the stricter system-prompt threshold
`0.50` before any user text was consulted at all (see "Classification
behavior" above) — `semantic` when the newest user text decided the route
(match or below-threshold fallback), `semantic_history` when the newest text
scored below the threshold and an earlier user text matched instead (the
`reason` says how far back), `no_text` when the request carried no classifiable
user text at all, `no_classifier` when classification could not run,
`manual` when the client sent `x-gw-auto-route: 0` (classification skipped
on purpose, routed by the requested model name), and `utility_bypass` for a
client-internal `<transcript>`-prefixed request (e.g. Claude Code's
auto-mode judgment) — `reason` distinguishes its three possible resolutions:
pinned to the configured `autoMode` target (see "autoMode" above, in which
case `matched_route` reads `<auto-mode>`, a display-only label rather than a
real route name), resolved by the requested model name when `autoMode` is
unset but that name matches a route, or falling back to `default` when
neither applies. Every mode except `semantic_system`, `semantic`'s match,
`semantic_history`, and a `manual`/`utility_bypass` resolved by name or
`autoMode` means the request fell back to `default`.

`routing.decided_by_text` is the first 200 characters of whichever text
actually decided the route — present only on a match (`semantic_system`,
`semantic`, or `semantic_history`), absent on a `default` fallback. For
`semantic_system` this is the system prompt itself, not a user message.
`routing.system_score` is the system prompt's top candidate score, recorded
whenever system-prompt classification was *attempted* — even when it missed
`0.50` and the request fell through to the user-text walk (`system_score` is
then still present, but `mode` will be `semantic`/`semantic_history`/etc.
instead) — so it doubles as tuning data for the threshold; absent when the
request carried no system prompt or no classifier was loaded. `routing.walk`
lists every text the history walk tried, in order, as `{texts_back, top_score}`
pairs, so a single trace line shows which text won and how the walk got there
without having to re-derive it from scores alone.

`resolved.translation` is present only when the request crossed protocols
(e.g. `"anthropic-messages->openai-chat"` or `"openai-responses->openai-chat"`);
its absence means the response was
forwarded byte-for-byte. It always describes the target in `resolved` — i.e.
whichever attempt actually served the response — not the route's `default`,
since a fallback may sit on the other side of a protocol boundary.
