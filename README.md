# llm-gateway

One local endpoint in front of every agent CLI.

`llm-gateway` speaks the three wire protocols its clients need — Anthropic
Messages (`/v1/messages`), OpenAI Chat (`/v1/chat/completions`) and OpenAI
Responses (`/v1/responses`) — rewrites only the `model` field of each request,
and streams the response back **byte-for-byte unmodified**. Model selection,
fallback, cost accounting and auditable routing decisions all live in one
config file.

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
cargo install --path .
# or, once releases exist:
# brew install NAKAK10/tap/llm-gateway
```

## Quick start

```sh
llm-gateway init            # interactive; writes ~/.config/llm-gateway/config.json (chmod 600)
llm-gateway serve           # start the gateway on 127.0.0.1:4000
llm-gateway launch claude   # start Claude Code through the gateway
llm-gateway stats           # what was spent, per route
```

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
    claude:   { model: "claude-sonnet-4-6", extraArgs: [] },
    codex:    { model: "gpt-5.6", wireApi: "responses", extraArgs: [] },
    opencode: { model: "role-default", models: [], extraArgs: [] },
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
