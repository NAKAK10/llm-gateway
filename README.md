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

Classification is now always on. For every inbound request, the gateway embeds
the **last user message**, compares it against every non-wildcard route's
`description` with static `model2vec-rs` embeddings, and picks the top match if
it clears the fixed cosine threshold **0.45**. If nothing clears the bar — or
classification cannot run at all — the reserved `default` route is used.

Important consequences:

- **The client's requested `model` never picks a route.** It survives only for
  the client's own UI and for trace logs' `requested_model` field.
- **Normal builds always include classification.** `semantic` is a default cargo
  feature, so Homebrew and plain `cargo install` builds behave the same.
- **`cargo install --no-default-features` is the opt-out.** That smaller build
  skips classification entirely and always routes to `default`.
- **`llm-gateway init` always downloads the embedding model** (roughly 500 MB)
  before it writes `config.json`.
- **Every non-wildcard route needs a real `description`.** That text is both
  documentation and the classification corpus; boilerplate descriptions produce
  boilerplate routing.
- **Every route's model must be explicit — `"<provider>/<model>"`, no `*`.**
  A model string can no longer borrow the client's requested model name: since
  routing is decided purely by content classification, there is nothing left
  for a `*` to substitute at request time, and one now fails config validation.
- **Wildcard route names are now an advanced hand-written escape hatch only.**
  `init` does not generate them, `GET /v1/models` does not list them, and the
  classifier never scores them.

```json5
routes: {
  "default": {
    description: "Fallback for requests that do not clearly match any other route.",
    model: {
      default: "anthropic/claude-sonnet-4-6",
    },
  },

  "role-anthropic": {
    description: "Complex reasoning, coding, and multi-step agentic tasks that need careful step-by-step thinking and full tool support.",
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
  // Reached when classification decides this request fits the route.
  "role-cheap": {
    description: "Short chores: summarizing, formatting, commit messages",
    model: { default: "ollama-local/qwen3:8b" },
  },
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

Every `openai-chat` provider in this table is reachable from Claude Code too —
see [Cross-protocol routing](#cross-protocol-routing).

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
        fallbacks: ["openrouter/openai/gpt-5.1"],      // same protocol as default; tried before first byte
      },
    },
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
  opencode: { models: [], overrideProviders: ["openai", "anthropic"], extraArgs: [] },
}
```

| field | notes |
|---|---|
| `server.apiKey` | resolved once at startup; changing it needs a restart. Required when `host` is not loopback — one key guards every provider credential. |
| `providers.<id>.apiKey` | resolved per request attempt, so env/Keychain/`command:` rotation is picked up live. |
| `providers.<id>.api` | fallbacks may not cross protocols; `config check` enforces it. |
| `routes.default` | required. It is the reserved catch-all when no route clears the classification threshold, and it also participates as a normal candidate. |
| `routes.<name>.description` | required on every non-wildcard route. Inline text or `./`/`../`/`/`/`~/` path; this is the classification corpus. |
| `routes.<name>.model.default` | `"<provider>/<model>"`, split on the first `/` only. |
| `routes.<name>.model.fallbacks` | same protocol as the default; tried in order before the first response byte. |
| `launch` | optional advanced escape hatch only: Claude/Codex/opencode extra args, Codex `wireApi`, opencode `models`/`overrideProviders`. |
| `logging.debug` | `--debug` truncates user text to 200 chars; `--debug-full` keeps everything. Plain-text prompts on disk — enable deliberately. |
| `logging.logging` | off by default; set `true` to print `serve`'s console diagnostics (which route/provider was picked, embedding-model prep, per-attempt fallback outcomes) to stderr. An explicit `RUST_LOG` still wins. |

## Commands

```
llm-gateway serve [--debug] [--debug-full] [--port N]
llm-gateway init
llm-gateway launch <claude|codex|opencode> [--isolate] [--print] [-- ARGS]
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
