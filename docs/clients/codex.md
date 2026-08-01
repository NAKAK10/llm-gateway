# Codex CLI — manual setup

[English](codex.md) | [日本語](../ja/clients/codex.md)

`llm-gateway launch codex` is the supported path and needs none of this.
This page makes the redirect permanent by hand. The gateway never writes
`~/.codex/config.toml`.

## Point Codex at the gateway

Append to `~/.codex/config.toml` (**user-level** — a project-local
`.codex/config.toml` cannot redirect providers and is ignored for this):

```toml
[model_providers.gateway]
name = "LLM Gateway"
base_url = "http://127.0.0.1:4000/v1"
env_key = "LLM_GATEWAY_KEY"     # names an env var; the value goes in ~/.codex/.env
wire_api = "responses"           # required on Codex CLI 0.145+; older versions may also accept "chat"

[model_providers.gateway.http_headers]
"x-gw-client" = "codex"
```

And in `~/.codex/.env`:

```
LLM_GATEWAY_KEY=<server.apiKey value>
```

Then either opt in per run:

```sh
codex -c 'model_provider="gateway"' -c 'disable_response_storage=true'
```

or make it permanent by adding at the top of `config.toml`:

```toml
model_provider = "gateway"
disable_response_storage = true
```

Your `agents/*.toml` files can keep their existing `model = "gpt-…"` lines.
Codex still insists on a model string, but `llm-gateway` no longer uses it to
choose a route: every request is classified by content against route
`description`s, and the reserved `default` route catches anything that does not
clear the threshold.

Notes:

- `env_key` holds a **variable name**, not a key. GUI-launched Codex does not
  see your shell exports, which is why the value belongs in `~/.codex/.env`.
- `disable_response_storage = true` matters as soon as any fallback goes to
  OpenRouter: its `/v1/responses` is stateless and 400s on a non-null
  `previous_response_id` — every conversation would die on turn 2.
- `wire_api = "responses"` is required, not just preferred, on Codex CLI
  0.145.0+: `"chat"` support was dropped entirely and Codex refuses to start
  with it configured. Stay on `responses` even when your route's provider
  only speaks `openai-chat` (OpenRouter, Ollama, …) — the gateway translates
  `openai-responses → openai-chat` per attempt now
  (`Translation::ResponsesToChat`), so nothing downstream needs `wire_api =
  "chat"` to exist. Verified end to end against 0.145.0: a normal `codex exec`
  response and a full `exec_command` tool-calling round trip through an
  `openai-chat` fallback provider. Older Codex versions that still accept
  `"chat"` may keep using it, but there is no longer a reason to.

## Verify

```sh
codex exec -c 'model_provider="gateway"' "say ok"
llm-gateway stats --by client     # a codex row must appear
```

Reverse test: stop the gateway; the same command must **fail**.

## Rollback

Remove the `model_provider = "gateway"` line (or stop passing `-c`). The
provider block can stay — it is inert while unreferenced.
