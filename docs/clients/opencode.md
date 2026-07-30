# opencode — manual setup

`llm-gateway launch opencode` is the supported path and needs none of this.
This page makes the redirect permanent by hand. The gateway never writes
`~/.config/opencode/opencode.json`.

## Point opencode at the gateway

Add to `~/.config/opencode/opencode.json`:

```json
{
  "provider": {
    "gateway": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "LLM Gateway",
      "options": {
        "baseURL": "http://127.0.0.1:4000/v1",
        "apiKey": "{env:LLM_GATEWAY_KEY}",
        "headers": { "x-gw-client": "opencode" }
      },
      "models": {
        "role-default": {}
      }
    }
  }
}
```

Then select with `-m gateway/role-default`, or set it per agent in your
`agents/*.md` files (`model: gateway/role-default`).

Notes:

- **Every key under `models` must appear verbatim in the gateway's
  `GET /v1/models`.** On any mismatch opencode shows no models and prints no
  error — the single most time-wasting failure mode this client has. Check
  with:
  ```sh
  curl -s http://127.0.0.1:4000/v1/models | jq -r '.data[].id'
  ```
- `{env:VAR}` works in the *file* (unlike in `OPENCODE_CONFIG_CONTENT`), so
  the key does not need to be written literally here.
- `@ai-sdk/openai-compatible` speaks `/v1/chat/completions`. If you want the
  Responses API instead, use `@ai-sdk/openai` — the gateway serves both.

## Verify

```sh
opencode run -m gateway/role-default "ping"
llm-gateway stats --by client     # an opencode row must appear
```

Reverse test: stop the gateway; the same command must **fail**.

## Rollback

Delete the `provider.gateway` block and any `gateway/…` model references.
