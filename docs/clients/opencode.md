# opencode — manual setup

[English](opencode.md) | [日本語](../ja/clients/opencode.md)

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
        "default": {}
      }
    }
  }
}
```

Then select `-m gateway/default`, or set it per agent in your `agents/*.md`
files (`model: gateway/default`). The client still needs some model id; the
gateway ignores that choice for routing and classifies by prompt content across
all configured route descriptions.

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

## `overrideProviders`: redirecting opencode's built-in providers

`llm-gateway launch opencode` (and `launch.opencode.overrideProviders` in
`config.json`) can also redirect opencode's *built-in* provider ids, so an
agent file or `opencode.json` that pins `model: openai/gpt-…` — instead of
`model: gateway/…` — still goes through the gateway. Redirecting only swaps
that provider's `options.baseURL`; its npm package, and therefore its native
wire protocol, is untouched. Whether that is safe depends on where the
package actually posts, since the gateway only serves three paths:
`/v1/messages`, `/v1/chat/completions` and `/v1/responses`.

| provider id | npm package | final path | redirect-safe? |
|---|---|---|---|
| `openai` | `@ai-sdk/openai` | `/v1/responses` | yes |
| `anthropic` | `@ai-sdk/anthropic` | `/v1/messages` | yes |
| `openrouter` | `@openrouter/ai-sdk-provider` | `/v1/chat/completions` | yes, but see dialect note below |
| `groq` | `@ai-sdk/groq` | `/v1/chat/completions` | yes, but see dialect note below |
| `mistral` | `@ai-sdk/mistral` | `/v1/chat/completions` | yes, but see dialect note below |
| `deepseek` | `@ai-sdk/openai-compatible` | `/v1/chat/completions` | yes (plain OpenAI shape) |
| `togetherai` | `@ai-sdk/togetherai` | `/v1/chat/completions` | yes (plain OpenAI shape) |
| `xai` | `@ai-sdk/xai` | `/v1/responses` or `/v1/chat/completions` | yes, either way |
| `google` | `@ai-sdk/google` | `/v1/models/{id}:generateContent` | **no** — no route matches this path; redirecting turns a working request into a 404 |
| `github-copilot` | opencode's own SDK + an auth plugin | not fixed | **no** — where it posts isn't stable enough to redirect |
| `ollama` | — | — | **not applicable** — has no id in models.dev to pin against |

The default `overrideProviders` list is the eight `yes` rows above. `google`,
`github-copilot` and `ollama` are deliberately absent — adding them to
`overrideProviders` yourself will not make them work; see the table for why.

**Dialect note:** `openrouter`, `groq`, `mistral` and `openai` itself can each
send request fields beyond plain OpenAI-compatible JSON (routing hints,
provider-specific sampling parameters, …). Once redirected, those fields are
forwarded to whatever this gateway's route resolves to upstream, which may
reject them with a 400 if that upstream doesn't understand them. That is
still preferable to the silent bypass this redirect exists to close — a 400
is a message, a bypass is not — but it means "redirected" is not the same
guarantee as "works with every model this provider offers."

**`x-gw-auto-route: 0` caveat:** with auto-routing off, the gateway looks up
a route by exact name match on whatever model id the client sent (see
`find_route` in `src/route.rs` — no prefix or fuzzy matching). A redirected
built-in provider forwards its *native* model name (e.g. `gpt-5`, not a
gateway route name), so unless a route happens to be named exactly that, the
request 404s. This already applies to the default `openai`/`anthropic`
redirects, not just the new ones — auto-routing on (the default) sidesteps it
entirely by classifying every request regardless of the model id sent.

`llm-gateway launch opencode` also scans agent files and `opencode.json` for
provider pins that fall outside `overrideProviders` and warns about them
before starting opencode — see `--help` output for what it checks.

## Verify

```sh
opencode run -m gateway/default "ping"
llm-gateway stats --by client     # an opencode row must appear
```

Reverse test: stop the gateway; the same command must **fail**.

## Rollback

Delete the `provider.gateway` block and any `gateway/…` model references.
