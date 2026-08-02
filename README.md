# llm-gateway

English | [日本語](README.ja.md)

One local endpoint in front of every agent CLI.

`llm-gateway` speaks the three wire protocols its clients need — Anthropic
Messages (`/v1/messages`), OpenAI Chat (`/v1/chat/completions`) and OpenAI
Responses (`/v1/responses`) — classifies every inbound request against your
routes' `description` text, rewrites only the upstream `model` field it sends,
and streams the response back **byte-for-byte unmodified**. Content-based
routing, fallback, cost accounting and auditable routing decisions all live in
one config file.

The one deliberate exception is a [cross-protocol route](#cross-protocol-routing):
when the client's protocol and the provider's differ, the request and response
*are* rebuilt, because the alternative is that the pair simply does not work.
Same-protocol traffic — still the overwhelming majority — is untouched.

```
llm-gateway launch claude    ─┐
llm-gateway launch codex      ┼→  llm-gateway serve :4000  →  anthropic / openai /
llm-gateway launch opencode  ─┘        route → fallback →      openrouter / ollama …
OpenClaw (manual setup)      ─┘        record
```

Clients are started with `launch`, which injects the redirect via environment
variables / CLI overrides — **no client config file is ever modified**.

Each `launch` asks once, per session, whether the gateway should
auto-classify requests by content ("yes", the default and historical
behaviour) or route by the model name the agent actually sent ("no").
Answer non-interactively with `--auto` / `--no-auto`; without either, a
terminal prompt asks (skipped, defaulting to "yes", when stdin is not a
terminal).

## Install

```sh
brew install NAKAK10/tap/llm-gateway
# or from source:
cargo install --git https://github.com/NAKAK10/llm-gateway
```

## Releasing (maintainers)

`dev` is the default branch. To ship a release: bump `version` in
`Cargo.toml` on `dev`, then merge `dev` into `main`. The release workflow
builds macOS binaries (arm64 + x86_64), publishes `v{version}` on GitHub
Releases, and updates the formula in
[NAKAK10/homebrew-tap](https://github.com/NAKAK10/homebrew-tap)
automatically. Merging without a version bump releases nothing (the tag
already exists), so docs-only merges are safe.

## Quick start

```sh
llm-gateway init            # interactive; downloads the embedding model, then writes ~/.config/llm-gateway/config.json (chmod 600)
llm-gateway serve           # start the gateway on 127.0.0.1:4000
llm-gateway launch claude   # start Claude Code through the gateway
llm-gateway stats           # what was spent, per route
llm-gateway update          # upgrade to the latest release
```

Before asking which role to configure, `init` asks one more question: "Which
language do you mainly write instructions in?" — English, 日本語, 中文, 한국어, or
Español. Every route's `description` it scaffolds, including `default`'s, is
generated in that language — and, when you pick anything other than English,
as a two-entry array of `[that language, English]`, so English-only traffic
from sub-agents or the harness itself still lands on the right route. See
[Content-classified routing](#content-classified-routing) for why this
matters.

**Breaking config change:** there is no migration shim for the old schema.
Delete `~/.config/llm-gateway/config.json` (or the whole
`~/.config/llm-gateway/` directory) and re-run `llm-gateway init`.

## Supported clients

| client | how |
|---|---|
| Claude Code | `llm-gateway launch claude` |
| Codex CLI | `llm-gateway launch codex` |
| opencode | `llm-gateway launch opencode` |
| OpenClaw | manual setup — see `docs/clients/openclaw.md` |

`launch` injects the redirect at start time; nothing is written to the
client's config. Manual (permanent) setup for every client is documented in
`docs/clients/`.

## Per-agent model strings

Sub-agents that pin their own model keep working, with **zero changes to the
agent files** — every request still flows through the gateway. What changed is
that the pinned string no longer chooses a route: it is just whatever the
client needs to stay happy.

| client | where the client keeps its own model string | what that means now |
|---|---|---|
| Claude Code | subagent `model:` frontmatter, or Claude's own `/model` UI | the string is sent and logged, but route selection ignores it |
| Codex CLI | `~/.codex/agents/*.toml` `model =` | Codex still needs a model name; the gateway classifies by content instead |
| opencode | `agents/*.md` `model: openai/…` | `launch` still redirects built-in providers so pinned agents do not bypass the gateway |

## Content-classified routing

Classification is now always on. For every inbound request — before the
newest user text is even looked at — the gateway first tries the request's
**system prompt** (see [System-prompt classification](#system-prompt-classification)
below); only when that does not decide the route does it fall through to the
newest user text.

For that fallback, the gateway embeds the **newest user text**, compares it
against every route's `description` with static `model2vec-rs` embeddings,
and picks the top match if it clears the fixed cosine threshold **0.45**.

When the newest user text does not clear the bar — or the newest user message
carries no text at all, which is the normal state of an agentic turn whose last
message is a `tool_result` — the gateway **walks back through earlier user
texts** (up to 8) and takes the most recent one that clears the threshold. A
conversation keeps its route across "continue"-style turns and tool-result
turns without the gateway holding any per-conversation state: the history that
arrives with every request is the state. A genuine topic change still wins
immediately, because the newest text is always tried first. If nothing in the
walk clears the bar — or classification cannot run at all — the reserved
`default` route is used. Before any of this, every candidate text has
`<system-reminder>...</system-reminder>` blocks stripped out — harness
boilerplate (Claude Code injects one into every session's first user message),
not the user's own words — and a message left blank afterward counts as no
text and is skipped just like a textless `tool_result` turn; only the
classification input is affected, the payload sent to the provider never
changes.

Important consequences:

- **The client's requested `model` never picks a route.** It survives only for
  the client's own UI and for trace logs' `requested_model` field.
- **Normal builds always include classification.** `semantic` is a default cargo
  feature, so Homebrew and plain `cargo install` builds behave the same.
- **`cargo install --no-default-features` is the opt-out.** That smaller build
  skips classification entirely and always routes to `default`.
- **`llm-gateway init` always downloads the embedding model** (roughly 500 MB)
  before it writes `config.json`.
- **Every route needs a real `description`.** That text is both documentation
  and the classification corpus; boilerplate descriptions produce boilerplate
  routing.
- **Write `description` in the language you give instructions in.** The
  embedding model (`potion-multilingual-128M`) aligns meaning weakly across
  languages: measured cosine similarity between a Japanese instruction and an
  English `description` sits around 0.19–0.26 — well under the 0.45 threshold
  — while same-language pairs measure 0.55–0.79. `llm-gateway init` asks which
  language you mainly write instructions in and generates every route's
  `description`, including `default`'s, in that language.
- **`description` also accepts an array of strings — one variant per
  language.** Each entry is embedded separately, and a route's score is the
  **max cosine across all its variants**. This matters because real traffic
  mixes languages: a human writing Japanese, but sub-agent prompts and
  harness-injected text that are overwhelmingly English regardless. Writing
  both languages into a single `description` does not fix that — mean-pooled
  through the embedding model's 64-token window, the same Japanese
  instruction's cosine dropped from **0.550** (Japanese-only description) to
  **0.433** (Japanese and English concatenated) — under threshold, because
  concatenation dilutes both languages toward a centroid that matches neither
  as well as either did alone. Separate variants keep each language's full
  score. A plain string is still valid and behaves like a one-element array.
  `llm-gateway init` scaffolds `[chosen language, English]` whenever you pick
  anything other than English.
- **Every route's model must be explicit — `"<provider>/<model>"`, no `*`.**
  A model string can no longer borrow the client's requested model name: since
  routing is decided purely by content classification, there is nothing left
  for a `*` to substitute at request time, and one now fails config validation.
- **Wildcard route names (`claude-*` and the like) are rejected outright.**
  Every route name is matched exactly; a `*` anywhere in a route name fails
  config validation.
- **Claude Code's own internal auto-mode judgment requests skip
  classification entirely.** These are `<transcript>`-prefixed yes/no
  permission calls its harness makes to itself through the same gateway
  endpoint, not real user turns — classifying them against `description`s
  would route them arbitrarily. See `autoMode` below for where they resolve
  instead.
- **The request's system prompt is tried before any user text.** See
  [System-prompt classification](#system-prompt-classification) below —
  this is what lets a Claude Code subagent's own role definition decide
  the route instead of whatever the user's instruction happens to mention.

```json5
routes: {
  "default": {
    description: "Fallback for requests that do not clearly match any other route.",
    model: {
      default: "anthropic/claude-sonnet-4-6",
    },
  },

  "role-anthropic": {
    // Two variants, embedded separately, scored by max cosine — matches
    // both a Japanese-speaking human and English sub-agent/harness traffic.
    description: [
      "慎重な段階的思考と完全なツール対応を必要とする、複雑な推論・コーディング・マルチステップな agent 的タスク。",
      "Complex reasoning, coding, and multi-step agentic tasks that need careful step-by-step thinking and full tool support.",
    ],
    model: {
      default: "anthropic/claude-sonnet-4-6",
      fallbacks: ["openrouter-anthropic/anthropic/claude-sonnet-4-6"],
    },
  },

  "role-cheap": {
    description: "Short chores: summarizing, formatting, commit messages, and other latency-sensitive low-cost tasks.",
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["groq/llama-3.3-70b-versatile"],
    },
  },

  "role-code": {
    description: "Code generation, refactoring, test writing, and bug fixes.",
    model: {
      default: "openrouter/qwen/qwen3-coder",
      fallbacks: ["deepseek/deepseek-coder"],
    },
  },
}
```

### System-prompt classification

Some requests carry a stronger routing signal than anything the user typed:
the **system prompt** — an agent's own definition of its role. A Claude Code
Task subagent (Explore, a custom `.claude/agents/*.md` agent, …), Codex CLI,
and opencode all send one, and for a subagent it usually *is* the role
definition, verbatim. The gateway tries this before it ever looks at user
text — see `routing.mode = "semantic_system"` in [Record formats](docs/config-reference.md#record-formats).

Where it comes from depends on protocol: Anthropic Messages' dedicated
`system` field, OpenAI Chat's leading `system`/`developer` message, or OpenAI
Responses' `instructions` field (falling back to a leading `system`/
`developer` item in `input` when `instructions` is empty — opencode sends it
that way). Only the *beginning* of it ever reaches the classifier — the same
800-character / 64-token embedding limit that applies to user text (see
above) applies here too — which is exactly the shape a subagent definition
has: the role description comes first.

**The threshold is stricter: 0.50, not 0.45.** A genuine role definition
scores in the same range a same-language `description` match does
(0.55–0.79); a harness's own generic system-prompt preamble ("You are Claude
Code, an interactive CLI tool...") must not clear this bar, or every
ordinary main-loop request of a session would get pinned to whatever route
that preamble happens to resemble. If the system prompt does not clear 0.50
— or the request has none, or no classifier is loaded — routing falls
through to the newest-user-text walk described above, unaffected.

This was built after a real misroute: a Claude Code Explore subagent's own
investigation prompt ("コードベースを調査してください(LP作成タスクに向けて)")
scored 0.501 against `role-implementer` — pulled there by the object
("LP作成", a landing-page build) the *user's* instruction mentioned — while
the correct `role-explorer` scored only 0.331. The subagent's system prompt
(a read-only-exploration-agent definition) is unambiguous about which role
it is; the user's text, taken out of context, is not. See the 2026-08-01
entry in `docs/decisions.md` for the full writeup.

### `autoMode`: a fast, dedicated target for Claude Code's own internal judgment

`autoMode` is a top-level config key, independent of `routes` — it never
depends on a route name or the client's requested `model` string, only on
what you pin here:

```json5
autoMode: {
  default: "anthropic-subscription/haiku",
  // fallbacks: ["anthropic/claude-haiku-4-5"],
}
```

Same shape as a route's `model` (`default` + optional `fallbacks`), but with
nothing to classify: a `<transcript>`-prefixed auto-mode request resolves
straight to these targets, bypassing the route-name lookup that
`routes.default` (and every other route) goes through.

**Why this exists:** without `autoMode`, an auto-mode judgment request falls
back to resolving by the client-sent model name, or the reserved `default`
route if that name matches nothing. If `default` (or that model name) points
at something slow — a multi-second `claude-cli`/`codex-cli` subprocess, for
instance — Claude Code's own timeout for this fast yes/no judgment can trip,
and the action gets rejected with "Auto mode could not evaluate this
action." Setting `autoMode` to something fast and cheap sidesteps that
regardless of what `default` is doing — the gateway still asks a real model
to make the call, it just asks a quicker one. `llm-gateway init` offers to
set this up (recommended) once you have picked your providers, preferring a
fast alias like `haiku` over the usual `sonnet` default.

## Cross-protocol routing

Claude Code only ever speaks Anthropic Messages, Codex CLI only ever speaks
OpenAI Responses (0.145+ dropped `wire_api = "chat"` entirely — see
`docs/clients/codex.md`), and almost every cheap or local provider only
speaks OpenAI Chat. So two directions are translated — and the decision is
made **per target, not per route**: a route's `default` and each of its
`fallbacks` can each speak a different protocol, and every attempt is
translated or passed through independently based on what the client sent.

| client speaks | target speaks | result |
|---|---|---|
| `anthropic-messages` | `openai-chat` | translated — Claude Code reaches Ollama, Groq, DeepSeek, Gemini, Mistral, Together, Sakana AI, PLaMo |
| `openai-responses` | `openai-chat` | translated — Codex CLI reaches the same roster, which matters now that its `wire_api = "chat"` escape hatch is gone |
| same on both sides | — | byte-for-byte passthrough, as before |
| anything else | — | that target is skipped before the request is sent; `400` only if every target in the route turns out unreachable this way |

Because reachability is decided per target, a route can freely mix a default
and fallbacks that speak different protocols — for example, a free
`openai-chat` model as the default with a subscription `anthropic-messages`
provider as fallback:

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
  "anthropic-subscription": { api: "anthropic-messages", transport: "claude-cli" },
},
routes: {
  // Reached when classification decides this request fits the route.
  "role-cheap": {
    description: "Short chores: summarizing, formatting, commit messages",
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["anthropic-subscription/claude-sonnet-4-6"],
    },
  },
}
```

The reverse direction — an `openai-chat` client reaching an
`anthropic-messages` target — has no translation implemented yet, so that
target is always skipped for such a client regardless of where it sits in
`fallbacks` (see `docs/roadmap.md`). The same is true for any pairing between
`anthropic-messages` and `openai-responses`, in either direction — no client
speaks both protocols, so there is nothing to translate between them; only
the two `→ openai-chat` directions above exist.

What a translated *attempt* costs you (this is about whichever target
actually serves the request, not necessarily the route's `default`):

**`anthropic-messages → openai-chat`:**

- **Prompt caching, `thinking` blocks, citations and Anthropic server-side
  tools** (`web_search`, `bash`, `text_editor`) are dropped — the target
  protocol has nowhere to put them. `cache_creation_input_tokens` is always 0.
- **`/v1/messages/count_tokens` is answered locally with an estimate**, because
  `openai-chat` has no token-counting endpoint. Returning nothing would break
  Claude Code's context sizing; the estimate is marked
  `result: "estimated_locally"` in the trace log.
- **The response is rebuilt**, so it is not byte-identical to what the provider
  sent. `llm-gateway trace` marks those requests with
  `xlat=anthropic-messages->openai-chat` — always check that field first when
  output looks subtly off. On a fallback, `resolved.translation` reflects the
  target that actually answered, not the route's `default`.

**`openai-responses → openai-chat`:**

- **Codex-specific fields are dropped**: `reasoning`, `include`, `store` /
  `previous_response_id`, `prompt_cache_key` / `client_metadata`, `text`,
  `metadata`. Only flat `function` tool definitions survive — `local_shell`,
  `web_search` and a `namespace` grouping of tools are Codex's own extensions
  and no `openai-chat` provider can run them anyway.
- **The response is rebuilt** into a Responses `response` object
  (non-streaming) or a Responses SSE event sequence (streaming:
  `response.created`, `response.output_text.delta`,
  `response.function_call_arguments.delta`, `response.completed`, …).
  `llm-gateway trace` marks these `xlat=openai-responses->openai-chat`.

Usage accounting is *not* affected by either direction: token counts are read
from the upstream bytes before translation.

Full list of what is and is not carried across: `docs/gotchas.md`.

## Subscription-backed providers

A subscription is not an API key. A Claude Pro/Max plan authenticates *Claude
Code* and a ChatGPT plan authenticates *Codex*; no credential the gateway could
hold would let it speak to either provider on those plans' behalf. So for those
cases the gateway does not hold a credential — it runs the official client, which
already has the login. `llm-gateway init` asks which you want:

```
Anthropic: how do you pay for it?
  API key                    per-token billing; full API features
  Subscription (via `claude`)  no key; generation only — your tools are not passed through
```

| transport | runs | renders as | verified |
|---|---|---|---|
| `claude-cli` | `claude -p` | `anthropic-messages` | yes — streaming, tools-denied, end to end |
| `codex-cli` | `codex exec` | `openai-chat` | plumbing and error paths only; see below |

Choosing a subscription for a provider does not also scaffold that provider's
API-key entry — there is no key to hold for it, so one would only produce an
always-failing route with an empty credential. Add the API-key provider back
by hand later if you want a route that needs tools alongside the
subscription's generation-only one.

```json5
providers: {
  // No baseUrl, no apiKey: the CLI authenticates itself.
  "anthropic-subscription": { api: "anthropic-messages", transport: "claude-cli" },
  "openai-subscription": { api: "openai-chat", transport: "codex-cli" },
},
routes: {
  "default": {
    description: "Fallback for requests that do not clearly match any other route.",
    model: { default: "anthropic-subscription/claude-sonnet-5" },
  },
  "role-sub": {
    description: "Requests that should run on the local Claude subscription via the provider CLI. Generation only — caller tools are not passed through.",
    model: { default: "anthropic-subscription/claude-sonnet-5" },
  },
  // `default` in the model string means "whatever the CLI is configured to use"
  // — which models a ChatGPT plan allows is not knowable from here.
  "role-codex": {
    description: "Requests that should run on the local ChatGPT subscription via Codex CLI.",
    model: { default: "openai-subscription/default" },
  },
}
```

From there it is a provider like any other: routes, fallback, `trace`, `stats`.
`transport: "claude-cli"` is the only unusual line, and `llm-gateway providers`
reports it as reachable when the binary is installed.

The limits are the CLI's, and they are real:

- **Your tools do not reach it.** `claude -p` has Claude Code's own tools, not
  the ones in the request, and the gateway denies all of them so a provider call
  cannot touch your files. This is a generation upstream, not an agent loop.
- **One prompt.** A `messages` array is flattened into a labelled transcript.
- **~5s of process startup per call**, and `temperature` / `top_p` /
  `stop_sequences` / `max_tokens` are dropped — the CLI has no equivalents.
- **`codex-cli` cannot stream token by token.** Codex's events are item-level, so
  the answer arrives complete; the gateway still emits a well-formed stream, it
  just arrives at once. `claude-cli` streams properly.
- Requests count against your **subscription's** limits, not an API balance.

What does survive: real streaming (the CLI emits Anthropic stream events, which
are forwarded as-is), `usage` including cache counts, and `stop_reason`. Nothing
else in the gateway changes — the transport is the only difference.

## Supported providers

`llm-gateway init` can scaffold any of these out of the box. A provider is
just `baseUrl` + `api` + `apiKey`, so **any** OpenAI- or Anthropic-compatible
endpoint works even if it is not on this list — see `docs/providers.md` for
copy-paste config for each.

| provider | `baseUrl` | `api` | key env var |
|---|---|---|---|
| Anthropic | `https://api.anthropic.com` | `anthropic-messages` | `ANTHROPIC_API_KEY` |
| OpenAI | `https://api.openai.com/v1` | `openai-responses` | `OPENAI_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | `openai-chat` (also speaks `anthropic-messages`) | `OPENROUTER_API_KEY` |
| GitHub Copilot | `https://api.githubcopilot.com` | `openai-chat` | *(a GitHub token, e.g. `command:gh auth token`)* |
| Google Gemini | `https://generativelanguage.googleapis.com/v1beta/openai` | `openai-chat` | `GEMINI_API_KEY` |
| xAI (Grok) | `https://api.x.ai/v1` | `openai-chat` | `XAI_API_KEY` |
| Mistral | `https://api.mistral.ai/v1` | `openai-chat` | `MISTRAL_API_KEY` |
| DeepSeek | `https://api.deepseek.com/v1` | `openai-chat` | `DEEPSEEK_API_KEY` |
| Groq | `https://api.groq.com/openai/v1` | `openai-chat` | `GROQ_API_KEY` |
| Together AI | `https://api.together.xyz/v1` | `openai-chat` | `TOGETHER_API_KEY` |
| Sakana AI (Fugu) | `https://api.sakana.ai/v1` | `openai-chat` | `SAKANA_API_KEY` |
| PLaMo (Preferred Networks) | `https://api.platform.preferredai.jp/v1` | `openai-chat` | `PLAMO_API_KEY` |
| Ollama Cloud | `https://ollama.com/v1` | `openai-chat` | `OLLAMA_API_KEY` |
| Ollama (local) | `http://127.0.0.1:11434/v1` | `openai-chat` | *(none needed)* |

Every `openai-chat` provider in this table is reachable from Claude Code and
from Codex CLI too — see [Cross-protocol routing](#cross-protocol-routing).

## Configuration reference

Everything lives in `~/.config/llm-gateway/` (override with
`LLM_GATEWAY_CONFIG_DIR`). `config.json` is parsed as **JSON5** — comments and
trailing commas are allowed. Changes are hot-reloaded; a broken edit keeps the
previous config serving and logs the error.

For everyday use the schema is just **four top-level keys**:
`server`, `providers`, `routes`, `logging`.

```json5
{
  server: {
    host: "127.0.0.1",        // non-loopback binding REQUIRES apiKey
    port: 4000,
    apiKey: "${LLM_GATEWAY_KEY}",   // optional on loopback
  },
  providers: {
    "<id>": {
      baseUrl: "https://openrouter.ai/api/v1",
      api: "openai-chat",     // openai-chat | openai-responses | anthropic-messages
      apiKey: "sk-…",         // literal | "${ENV_VAR}" | "keychain:<name>" | "command:<cmd>"
      headers: { "X-Title": "llm-gateway" },
      injectUsage: true,
    },
  },
  routes: {
    "default": {
      description: "Fallback for requests that do not clearly match any other route.",
      model: {
        default: "<provider>/<model>",
      },
    },
    "role-openai": {
      description: "General-purpose assistant tasks, coding, and tool use via OpenAI's models.",
      model: {
        default: "openai/gpt-5.1",                     // split on the FIRST `/` only
        fallbacks: ["openrouter/openai/gpt-5.1"],      // may cross protocols; tried before first byte
      },
    },
  },
  autoMode: {                  // optional — see "autoMode" above
    default: "<provider>/<fast-model>",
  },
  logging: {
    dir: "./logs",
    usage: true,
    debug: false,             // trace-YYYY-MM-DD.jsonl — records prompt text!
    logging: false,           // console diagnostics (embedding prep, fallback attempts)
  },
}
```

Optional advanced key: `launch`. `init` no longer writes it, and most configs
never need it, but you can hand-edit it for per-client launcher tweaks:

```json5
launch: {
  claude:   { extraArgs: [] },
  codex:    { wireApi: "responses", extraArgs: [] },
  opencode: { models: [], overrideProviders: ["openai", "anthropic", "openrouter", "groq", "mistral", "deepseek", "xai", "togetherai"], extraArgs: [] },
}
```

That is the default `overrideProviders` list, shown explicitly. `google`,
`github-copilot` and `ollama` are opencode built-ins too, but adding them
here would not help — see [opencode manual setup](docs/clients/opencode.md)
for why each is excluded, and which providers can send fields the upstream
may reject with a 400 even once redirected.

| field | notes |
|---|---|
| `server.apiKey` | resolved once at startup; changing it needs a restart. Required when `host` is not loopback — one key guards every provider credential. |
| `providers.<id>.apiKey` | resolved per request attempt, so env/Keychain/`command:` rotation is picked up live. |
| `providers.<id>.api` | a route's `default` and `fallbacks` may each use a different `api`; reachability from the client's protocol is checked per attempt at request time, not by `config check`. |
| `routes.default` | required. It is the reserved catch-all when no route clears the classification threshold, and it also participates as a normal candidate. |
| `routes.<name>.description` | required on every route. Inline text or `./`/`../`/`/`/`~/` path; this is the classification corpus. |
| `routes.<name>.model.default` | `"<provider>/<model>"`, split on the first `/` only. |
| `routes.<name>.model.fallbacks` | may use a different `api` than the default; tried in order before the first response byte, skipping any target the client's protocol cannot reach (see [Cross-protocol routing](#cross-protocol-routing)). |
| `autoMode` | optional; unset by default. Same `default`/`fallbacks` shape as a route's `model`, but resolved directly — no route-name lookup — for Claude Code's own internal `<transcript>`-prefixed auto-mode judgment requests. See [`autoMode`](#automode-a-fast-dedicated-target-for-claude-codes-own-internal-judgment) above. |
| `launch` | optional advanced escape hatch only: Claude/Codex/opencode extra args, Codex `wireApi`, opencode `models`/`overrideProviders`. |
| `logging.debug` | `--debug` truncates user text to 200 chars; `--debug-full` keeps everything. Plain-text prompts on disk — enable deliberately. |
| `logging.logging` | off by default; set `true` to print `serve`'s console diagnostics (which route/provider was picked, embedding-model prep, per-attempt fallback outcomes) to stderr. An explicit `RUST_LOG` still wins. |
| `ui.enabled` | off by default; `--ui` ORs with it. Turns on the local dashboard at `GET /ui` — see [Dashboard](#dashboard-serve---ui). |

## Commands

```
llm-gateway serve [--debug] [--debug-full] [--port N] [--ui]
llm-gateway init
llm-gateway launch <claude|codex|opencode> [--isolate] [--auto|--no-auto] [--print] [-- ARGS]
llm-gateway config check|show|gitignore
llm-gateway stats [--by route|client|provider|model|day] [--since D] [--until D]
llm-gateway trace [--tail] [--route R] [--client C]
llm-gateway providers
llm-gateway update [--check]
```

`update` asks GitHub for the latest release and, if this build is behind, runs
the upgrade that matches how it was installed — `brew upgrade` for a Homebrew
install, `cargo install --force` for a `cargo install` one. It never overwrites
its own binary: that would leave a package manager believing the old version is
still there. For a hand-placed binary it prints the release link instead.
`--check` reports without changing anything.

### `--isolate` by client

`--isolate` means something different for each client — same flag name, three
different scopes of what actually stops loading:

| client | implementation | what is actually disabled |
|---|---|---|
| Claude Code | `--setting-sources project` | user settings are not read **at all** — permissions, hooks and model preferences are discarded along with everything else |
| Codex | `--ignore-user-config` | only on `codex exec`; on the TUI the flag is never added, so nothing is isolated (no equivalent option upstream — there is no workaround) |
| opencode | `--pure` | external plugins only; config files are still read |

For Claude Code, `--setting-sources project` is a heavier hammer than most
`launch` sessions need. If the only goal is avoiding a stale
`ANTHROPIC_BASE_URL` (or similar) in `~/.claude/settings.json`'s `env` block,
the conflict warning `launch claude` already prints on every run is usually
enough — reach for `--isolate` when permissions/hooks/model prefs from that
file are themselves the problem, not just its `env` block.

See [`docs/clients/claude-code.md`](docs/clients/claude-code.md),
[`docs/clients/codex.md`](docs/clients/codex.md) and
[`docs/clients/opencode.md`](docs/clients/opencode.md) for the full detail
behind each row.

`serve` binds before doing anything else. If the port is already taken —
almost always a previous `llm-gateway serve` still running — it identifies the
process (via `lsof`) and asks before touching anything:

```
▲  port 4000 is already in use by another process (pid 12345)
◆  kill it and start this one instead?
│  ● Yes / ○ No
```

Answering `No` leaves the other process alone and exits without starting;
answering `Yes` terminates it and binds. A non-interactive run (no terminal
attached) answers as if you said `No` rather than guessing.

## Dashboard (`serve --ui`)

`llm-gateway serve --ui` (or `ui.enabled: true` in `config.json`) exposes a
local dashboard at `GET /ui` — same listener as the proxy itself, so nothing
new is opened to the network beyond what `serve` already binds. On startup it
prints the URL to open, with a one-time token: `http://127.0.0.1:PORT/ui?token=…`.
Open that URL once and the browser gets a session cookie good for the rest of
the run; a configured `server.apiKey` also works via `Authorization`/
`x-api-key`, for scripted access. See [Security notes](#security-notes) for
why the dashboard needs its own token rather than just reusing `server.apiKey`.
Three views:

- **Live** — a real-time feed (Server-Sent Events, `GET /api/live`) of every
  completed request: the prompt that came in, which route classification
  picked, which provider/model actually answered, and how it turned out.
  Independent of `--debug`: it never touches disk, and disappears the moment
  nothing is subscribed — see the difference below.
- **Vector Map** — every route's classification embedding, projected to 2-D
  (`GET /api/routes/vectors`), with incoming requests plotted live on the
  same map as they're classified. Needs the `semantic` feature and a loaded
  classifier; without one the view says so rather than 404ing.
- **Usage** — the same aggregation `llm-gateway stats` prints, as a live table
  (`GET /api/usage?by=route|client|provider|model|day&since=...&until=...`).

The live feed is a different, lower-stakes decision than `--debug`: `--debug`
writes prompt text to `logs/trace-*.jsonl` on disk, permanently, until
retention prunes it — a decision worth making deliberately (see
[Security notes](#security-notes)). The dashboard's live feed is in-memory
only, per-tab, and gone the moment the tab closes or nothing is subscribed;
turning it on with `--ui` does not turn `--debug` on, and vice versa.

## What fallback does (and does not) do

Fallback triggers on connection failure, header timeout, 408, 429 and 5xx —
**before the first response byte**. Once a response starts streaming it is
committed; a mid-generation failure cannot switch providers. A fallback may
speak a different protocol than the default (see [Cross-protocol
routing](#cross-protocol-routing)); a target the client's protocol cannot
reach is skipped rather than tried. For cross-vendor redundancy on the
Anthropic protocol without any translation involved, point a fallback at
OpenRouter's Anthropic-compatible endpoint.

## Manual client setup

`launch` is the supported path, but every client can also be configured by hand
— see `docs/clients/` for Claude Code, Codex CLI, opencode and OpenClaw. The
gateway never edits those files either way.

## Security notes

- `config.json` may contain literal API keys: it is created `0600`, checked by
  `config check`, masked by `config show`, and covered by `config gitignore`.
- Binding to anything but loopback without `server.apiKey` is refused at startup.
- `--debug` writes prompt text to `logs/`. Treat that directory accordingly.
- `--ui` does not write anything to disk on its own, but it does need its own
  auth story: a browser cannot attach `Authorization`/`x-api-key` to a page
  load or an `EventSource`, so the dashboard can't just reuse `server.apiKey`
  the way the proxy routes do. Instead `serve --ui` prints a one-time token
  at startup as part of the dashboard URL (`/ui?token=…`); opening that URL
  trades the token for an `HttpOnly`/`SameSite=Strict` session cookie, which
  is what actually gates `/ui` and every `/api/*` route afterwards. A
  configured `server.apiKey` still works too, via the same headers as the
  proxy. Every dashboard route also refuses any `Host` other than
  `127.0.0.1`/`localhost`/`[::1]`, closing the DNS-rebinding hole a
  cookie-only scheme would otherwise leave open for any web page you visit
  while the dashboard is running.
- `--ui` combined with `--debug-full` sends **untruncated** prompt text over
  the live feed (`GET /api/live`), not just the 200-character preview `--ui`
  alone shows — `--debug-full` disables truncation everywhere it applies,
  including there. Anyone who can reach the dashboard while both are on sees
  full prompt text in real time, on top of what `--debug-full` already writes
  to `logs/trace-*.jsonl`.

## License

MIT OR Apache-2.0.
