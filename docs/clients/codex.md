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
wire_api = "responses"           # if this errors on your version, try "chat"

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

Your `agents/*.toml` files keep their `model = "gpt-…"` lines unchanged — the
gateway's `gpt-*` wildcard route forwards those ids as-is.

Notes:

- `env_key` holds a **variable name**, not a key. GUI-launched Codex does not
  see your shell exports, which is why the value belongs in `~/.codex/.env`.
- `disable_response_storage = true` matters as soon as any fallback goes to
  OpenRouter: its `/v1/responses` is stateless and 400s on a non-null
  `previous_response_id` — every conversation would die on turn 2.
- `wire_api` accepts `responses` on current versions; whether `chat` still
  exists differs between sources. The gateway serves both endpoints, so try
  `responses` first and fall back to `chat` only if Codex refuses.

## Verify

```sh
codex exec -c 'model_provider="gateway"' "say ok"
llm-gateway stats --by client     # a codex row must appear
```

Reverse test: stop the gateway; the same command must **fail**.

## Rollback

Remove the `model_provider = "gateway"` line (or stop passing `-c`). The
provider block can stay — it is inert while unreferenced.
