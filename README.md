# llm-gateway

English | [日本語](README.ja.md)

One local endpoint in front of every agent CLI.

`llm-gateway` speaks the three wire protocols its clients need — Anthropic
Messages (`/v1/messages`), OpenAI Chat (`/v1/chat/completions`) and OpenAI
Responses (`/v1/responses`) — rewrites only the `model` field of each request,
and streams the response back **byte-for-byte unmodified**. Model selection,
fallback, cost accounting and auditable routing decisions all live in one
config file.

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
llm-gateway init            # interactive; writes ~/.config/llm-gateway/config.json (chmod 600)
llm-gateway serve           # start the gateway on 127.0.0.1:4000
llm-gateway launch claude   # start Claude Code through the gateway
llm-gateway stats           # what was spent, per route
```

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

## Per-agent models

Sub-agents that pin their own model keep working, with **zero changes to the
agent files** — every request flows through the gateway:

| client | agent model source | how it reaches the gateway |
|---|---|---|
| Claude Code | subagent `model:` frontmatter | env redirect covers the whole process; ids resolve via `claude-*` |
| Codex CLI | `~/.codex/agents/*.toml` `model =` | provider is global, models pass through via `gpt-*` |
| opencode | `agents/*.md` `model: openai/…` | `launch` also redirects the built-in providers named in `launch.opencode.overrideProviders` (default `openai`, `anthropic`), because opencode picks a provider per model reference — without this, pinned agents would silently bypass the gateway |

Routing is by model name by default: an exact route name wins, otherwise the
longest wildcard prefix. Content-based (semantic) routing, which picks a route
from the request itself, is available for routes that opt in — see
[Semantic routing](#semantic-routing) below.

## Semantic routing

Content-based routing picks a route from the *content* of the request instead
of its `model` name. It runs only for a route that carries a `semantic` block;
everything else stays exact-name-first, then longest-wildcard-prefix.

**Requires a build with the `semantic` cargo feature** — the Homebrew binary
has it, `cargo install` without `--features semantic` does not. The feature is
opt-in at build time because the embedding model is ~500MB. A binary without it
warns at startup and forwards such routes to their own `model` directly, so the
config stays valid either way. The model is downloaded on first `serve` with a
`semantic` route configured, and is only loaded into memory when one exists.

`routes[].semantic` is an optional field on any route:

| field | type | default | meaning |
|---|---|---|---|
| `candidates` | `string[]` | `[]` | Route names eligible for selection. Empty means "every other route that has a `description`". |
| `threshold` | `number` | `0.45` | If the top-1 cosine similarity of the request against the candidates falls below this, the auto route's own `model` is used instead. |

Design points:

- **The auto route's own `model` is where requests land when no candidate
  clears the threshold** — so a route with `semantic` still needs a `model`,
  exactly like any other route.
- **An explicit route name is never overridden.** Classification only runs
  when a request names a route that itself carries `semantic`. This
  continues the existing rule that an exact route name always wins and is
  always predictable (see `src/route.rs`, Phase 2 in `docs/roadmap.md`).
- **Candidates must have a `description`** — that's the classification
  corpus (long descriptions can live in `llm/*.md`, as today).
- **Candidates the incoming request cannot reach are excluded at match
  time** — a request to `/v1/chat/completions` will never resolve to an
  `anthropic-messages` candidate, because nothing translates in that
  direction. A Claude Code request *can* pick an `openai-chat` candidate,
  since that direction is translated (see
  [Cross-protocol routing](#cross-protocol-routing)).
- **Route names with `semantic` cannot use a wildcard (`*`).**

```json5
routes: {
  "auto": {
    semantic: {
      candidates: ["role-light", "role-deep", "role-code"],
      threshold: 0.45,
    },
    // Where requests land when no candidate clears the threshold.
    model: {
      default: "ollama-local/qwen3:8b",
      fallbacks: ["openrouter/qwen/qwen3-8b"],
    },
  },

  "role-light": {
    description: "Short, well-defined chores: summarizing, formatting, commit messages, naming",
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
    description: "Code generation, refactoring, test writing, bug fixes",
    model: {
      default: "openrouter/qwen/qwen3-coder",
      fallbacks: ["deepseek/deepseek-coder"],
    },
  },
}
```

## Cross-protocol routing

Claude Code only ever speaks Anthropic Messages, and almost every cheap or
local provider only speaks OpenAI Chat. So one direction is translated:

| client speaks | provider speaks | result |
|---|---|---|
| `anthropic-messages` | `openai-chat` | translated — Claude Code reaches Ollama, Groq, DeepSeek, Gemini, Mistral, Together, Sakana AI, PLaMo |
| same on both sides | — | byte-for-byte passthrough, as before |
| anything else | — | `400` with an explanation, as before |

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
},
routes: {
  // Reached from Claude Code with: llm-gateway launch claude --model role-cheap
  "role-cheap": { model: { default: "ollama-local/qwen3:8b" } },
}
```

What a translated route costs you:

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
  output looks subtly off.
- Usage accounting is *not* affected: token counts are read from the upstream
  bytes before translation.

Full list of what is and is not carried across: `docs/gotchas.md`.

## Subscription-backed providers

A subscription is not an API key. A Claude Pro/Max plan authenticates *Claude
Code*, and no credential the gateway could hold would let it speak to Anthropic
on that plan's behalf. So for that one case the gateway does not hold a
credential — it runs the official client, which already has the login:

```json5
providers: {
  // No baseUrl, no apiKey: the CLI authenticates itself.
  "claude-subscription": { api: "anthropic-messages", transport: "claude-cli" },
},
routes: {
  "role-sub": { model: { default: "claude-subscription/sonnet" } },
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

Every `openai-chat` provider in this table is reachable from Claude Code too —
see [Cross-protocol routing](#cross-protocol-routing).

## Configuration reference

Everything lives in `~/.config/llm-gateway/` (override with
`LLM_GATEWAY_CONFIG_DIR`). `config.json` is parsed as **JSON5** — comments and
trailing commas are allowed. Changes are hot-reloaded; a broken edit keeps the
previous config serving and logs the error.

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
      apiKey: "sk-…",         // literal | "${ENV_VAR}" | "keychain:<name>"
      headers: { "X-Title": "llm-gateway" },   // optional extra headers
      injectUsage: true,      // add stream_options.include_usage to streamed chat
    },
  },
  routes: {
    "<name>": {               // what clients put in `model`; `:` and `/` forbidden
      title: "…",
      description: "text or ./llm/file.md",   // becomes the semantic-routing corpus later
      model: {
        default: "<provider>/<model>",        // split on the FIRST `/` only
        fallbacks: ["<provider>/<model>"],    // same protocol as default; tried before first byte
      },
    },
    "claude-*": {             // wildcard: `*` expands to the requested model
      model: { default: "anthropic/*" },
    },
  },
  launch: {
    // `model` is only the client's MAIN/default model, and it is a route
    // name — a role route (`role-strategy`) or a passthrough id caught by a
    // wildcard. Per-agent models are untouched; see "Per-agent models" below.
    claude:   { model: "claude-sonnet-4-6", extraArgs: [] },
    codex:    { model: "gpt-5.6", wireApi: "responses", extraArgs: [] },
    opencode: { model: "role-default", models: [],
                overrideProviders: ["openai", "anthropic"], extraArgs: [] },
  },
  logging: {
    dir: "./logs",            // relative to the config dir
    usage: true,              // usage-YYYY-MM.jsonl, one line per request
    debug: false,             // trace-YYYY-MM-DD.jsonl — records prompt text!
  },
}
```

| field | notes |
|---|---|
| `server.apiKey` | resolved once at startup; changing it needs a restart. Required when `host` is not loopback — one key guards every provider credential. |
| `providers.<id>.apiKey` | resolved per request attempt, so env/Keychain rotation is picked up live. |
| `providers.<id>.api` | fallbacks may not cross protocols; `config check` enforces it. |
| `routes.<name>` | exact match wins over wildcards; among wildcards the longest prefix wins. |
| `description` | treated as a file path when it starts with `./`, `../`, `/` or `~/`. |
| `logging.debug` | `--debug` truncates user text to 200 chars; `--debug-full` keeps everything. Plain-text prompts on disk — enable deliberately. |

## Commands

```
llm-gateway serve [--debug] [--debug-full] [--port N]
llm-gateway init
llm-gateway launch <claude|codex|opencode> [--model R] [--isolate] [--print] [-- ARGS]
llm-gateway config check|show|gitignore
llm-gateway stats [--by route|client|provider|model|day] [--since D] [--until D]
llm-gateway trace [--tail] [--route R] [--client C]
llm-gateway providers
```

## What fallback does (and does not) do

Fallback triggers on connection failure, header timeout, 408, 429 and 5xx —
**before the first response byte**. Once a response starts streaming it is
committed; a mid-generation failure cannot switch providers. Same-protocol
fallbacks only; for cross-vendor redundancy on the Anthropic protocol, point a
fallback at OpenRouter's Anthropic-compatible endpoint.

## Manual client setup

`launch` is the supported path, but every client can also be configured by hand
— see `docs/clients/` for Claude Code, Codex CLI, opencode and OpenClaw. The
gateway never edits those files either way.

## Security notes

- `config.json` may contain literal API keys: it is created `0600`, checked by
  `config check`, masked by `config show`, and covered by `config gitignore`.
- Binding to anything but loopback without `server.apiKey` is refused at startup.
- `--debug` writes prompt text to `logs/`. Treat that directory accordingly.

## License

MIT OR Apache-2.0.
