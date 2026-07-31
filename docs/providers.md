# Providers

[English](providers.md) | [日本語](ja/providers.md)

A provider is nothing more than `baseUrl` + `api` + `apiKey` in `config.json`
— there is no per-provider code in the gateway. Everything below is therefore
copy-paste configuration, not a compatibility matrix: **any** endpoint that
speaks one of the three wire protocols works, listed here or not.

`llm-gateway init` can scaffold every provider on this page; `llm-gateway
providers` probes each configured one and tells you whether the key resolved
and the endpoint answered.

## Cheat sheet

| provider | `baseUrl` | `api` | key env var |
|---|---|---|---|
| Anthropic | `https://api.anthropic.com` | `anthropic-messages` | `ANTHROPIC_API_KEY` |
| OpenAI | `https://api.openai.com/v1` | `openai-responses` | `OPENAI_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | `openai-chat` / `anthropic-messages` | `OPENROUTER_API_KEY` |
| GitHub Copilot | `https://api.githubcopilot.com` | `openai-chat` | *(a GitHub token — see below)* |
| Google Gemini | `https://generativelanguage.googleapis.com/v1beta/openai` | `openai-chat` | `GEMINI_API_KEY` |
| xAI (Grok) | `https://api.x.ai/v1` | `openai-chat` | `XAI_API_KEY` |
| Mistral | `https://api.mistral.ai/v1` | `openai-chat` | `MISTRAL_API_KEY` |
| DeepSeek | `https://api.deepseek.com/v1` | `openai-chat` | `DEEPSEEK_API_KEY` |
| Groq | `https://api.groq.com/openai/v1` | `openai-chat` | `GROQ_API_KEY` |
| Together AI | `https://api.together.xyz/v1` | `openai-chat` | `TOGETHER_API_KEY` |
| Sakana AI (Fugu) | `https://api.sakana.ai/v1` | `openai-chat` | `SAKANA_API_KEY` |
| PLaMo (Preferred Networks) | `https://api.platform.preferredai.jp/v1` | `openai-chat` | `PLAMO_API_KEY` |
| Ollama Cloud | `https://ollama.com/v1` | `openai-chat` | `OLLAMA_API_KEY` |
| Ollama (local) | `http://127.0.0.1:11434/v1` | `openai-chat` | *(none)* |

The `api` value decides which endpoints can reach a provider — with one
crossing: an `anthropic-messages` client (Claude Code) can reach any
`openai-chat` provider here, because that direction is translated. See
[Cross-protocol routing](../README.md#cross-protocol-routing) for what such a
route gives up.

`baseUrl` rules (see `docs/config-reference.md`): no trailing slash;
`anthropic-messages` providers give the host root (the gateway appends
`/v1/messages`), OpenAI-kind providers include the `/v1`-style prefix (the
gateway appends `/chat/completions` or `/responses`).

## Notes per provider

### Anthropic

```json5
anthropic: {
  baseUrl: "https://api.anthropic.com",
  api: "anthropic-messages",
  apiKey: "${ANTHROPIC_API_KEY}",
},
```

`init` now scaffolds a classifiable route named after the *role* the wizard
assigns this provider to (e.g. `role-architect`, `role-implementer` — see
[`AgentRole`](../src/cli/init.rs) in the wizard), with a real `description`
and an explicit `model: { default: "anthropic/claude-sonnet-4-6" }` — the
wizard fetches the provider's model list over its API where possible and lets
you pick one, falling back to a pre-filled text prompt when it can't. Route
*selection* is done by classification against that description, not by a
`claude-*` wildcard being the normal path, and the model itself can no longer
be a `*` wildcard either: since routing no longer looks at the client's
requested model string, there is nothing left for one to substitute.

### OpenAI

```json5
openai: {
  baseUrl: "https://api.openai.com/v1",
  api: "openai-responses",
  apiKey: "${OPENAI_API_KEY}",
},
```

Codex speaks the Responses API by default. Register a second id with
`api: "openai-chat"` if you also want it as a chat-completions fallback
target for other routes (fallbacks may not cross protocols).

### OpenRouter

```json5
openrouter: {
  baseUrl: "https://openrouter.ai/api/v1",
  api: "openai-chat",
  apiKey: "${OPENROUTER_API_KEY}",
  headers: { "X-Title": "llm-gateway" },   // optional attribution
},
// Same upstream, same account, Anthropic wire protocol instead of
// openai-chat — lets an Anthropic-speaking route fall back to it without
// crossing ApiKinds.
// Note the root: `/api`, not `/api/v1` — the gateway appends `/v1/messages`
// itself, and reusing the `openai-chat` id's baseUrl here would double up to
// `/api/v1/v1/messages`.
"openrouter-anthropic": {
  baseUrl: "https://openrouter.ai/api",
  api: "anthropic-messages",
  apiKey: "${OPENROUTER_API_KEY}",
},
```

Two ids for one account looks redundant at a glance; it exists because a
provider entry couples one upstream to exactly one wire protocol (`api`) —
by design, so that `route.model.fallbacks` never has to cross protocols
mid-request (see `src/config/mod.rs`). Whether OpenRouter itself takes an
`openai-chat` or an `anthropic-messages` request depends on which path you
POST to, not on the credential, so the same key is simply registered twice.
`init` only adds `openrouter-anthropic` when you pick OpenRouter as a
fallback for Claude Code.

Model ids contain a `/` (`anthropic/claude-sonnet-4.6`); route targets split
on the *first* `/` only, so `openrouter/anthropic/claude-sonnet-4.6` parses
fine.

### GitHub Copilot

Your Copilot subscription, reachable from any client the gateway fronts —
including Claude Code, via [cross-protocol
translation](../README.md#cross-protocol-routing).

```json5
"github-copilot": {
  baseUrl: "https://api.githubcopilot.com",
  api: "openai-chat",
  // `gh` refreshes this token on its own schedule, so read it per request
  // instead of copying it. Add `--user <login>` if you have several accounts.
  apiKey: "command:gh auth token",
  headers: { "X-GitHub-Api-Version": "2026-06-01" },
},
```

`llm-gateway init` offers this provider and fills in exactly the above,
including the `command:` reference, whenever `gh` is on your `PATH`.

The credential is an ordinary GitHub token — there is no separate Copilot API
key and no token-exchange step. Any of these work:

- `gh auth login`, then `command:gh auth token` (recommended: it never goes
  stale).
- A personal access token in `GITHUB_COPILOT_TOKEN`, referenced as
  `"${GITHUB_COPILOT_TOKEN}"`.
- Whatever token an editor integration already stored, pasted into the
  Keychain (`keychain:github-copilot`).

`X-GitHub-Api-Version` is not required — a bare `Authorization: Bearer` works —
but pinning it means a future default change on GitHub's side cannot alter the
response shape mid-session.

**Which models you can actually use is narrower than the list.**
`GET https://api.githubcopilot.com/models` returns everything Copilot knows
about, including models your plan cannot touch; those answer
`400 model_not_supported`. The `policy.state` field is the closer signal, and
premium models generally need enabling in your GitHub Copilot settings first.
Check with:

```sh
curl -s https://api.githubcopilot.com/models \
  -H "Authorization: Bearer $(gh auth token)" \
  -H "X-GitHub-Api-Version: 2026-06-01" \
  | jq -r '.data[] | select(.capabilities.type=="chat") | "\(.id)\t\(.policy.state // "-")"'
```

Two more things worth knowing:

- Requests are billed against your Copilot quota like any other Copilot usage.
  The gateway does not send Copilot's `x-initiator` / `Openai-Intent`
  classification headers, because their correct value depends on the individual
  request and a constant would be wrong half the time.
- Copilot also advertises `/v1/messages` for its Claude models, which would
  mean no translation at all. The gateway cannot use it yet: that endpoint
  requires `Authorization: Bearer`, while an `anthropic-messages` provider is
  authenticated with `x-api-key`. Tracked as a follow-up.

### Google Gemini

```json5
gemini: {
  baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
  api: "openai-chat",
  apiKey: "${GEMINI_API_KEY}",
},
```

This is Google's OpenAI-compatibility endpoint for the Gemini API
(models like `gemini-2.5-pro`).

### xAI (Grok)

```json5
xai: {
  baseUrl: "https://api.x.ai/v1",
  api: "openai-chat",
  apiKey: "${XAI_API_KEY}",
},
```

### Mistral

```json5
mistral: {
  baseUrl: "https://api.mistral.ai/v1",
  api: "openai-chat",
  apiKey: "${MISTRAL_API_KEY}",
},
```

### DeepSeek

```json5
deepseek: {
  baseUrl: "https://api.deepseek.com/v1",
  api: "openai-chat",
  apiKey: "${DEEPSEEK_API_KEY}",
},
```

### Groq

```json5
groq: {
  baseUrl: "https://api.groq.com/openai/v1",
  api: "openai-chat",
  apiKey: "${GROQ_API_KEY}",
},
```

Note the non-standard `/openai/v1` prefix.

### Together AI

```json5
together: {
  baseUrl: "https://api.together.xyz/v1",
  api: "openai-chat",
  apiKey: "${TOGETHER_API_KEY}",
},
```

### Sakana AI (Fugu)

```json5
sakana: {
  baseUrl: "https://api.sakana.ai/v1",
  api: "openai-chat",
  apiKey: "${SAKANA_API_KEY}",
},
```

Sakana AI's Fugu is an orchestration model behind an OpenAI-compatible API —
models `fugu` and `fugu-ultra`. Get a key at
[console.sakana.ai](https://console.sakana.ai); the console dashboard shows
the base URL for your account, so prefer that value if it differs from the
one above. Fugu also exposes an Anthropic-compatible Messages endpoint —
register a second provider id with `api: "anthropic-messages"` if you want it
as a fallback for an Anthropic-speaking route.

```json5
routes: {
  "role-orchestrator": {
    model: { default: "sakana/fugu-ultra", fallbacks: ["sakana/fugu"] },
  },
},
```

### PLaMo (Preferred Networks)

```json5
plamo: {
  baseUrl: "https://api.platform.preferredai.jp/v1",
  api: "openai-chat",
  apiKey: "${PLAMO_API_KEY}",
},
```

Japanese-focused LLM with an OpenAI-compatible API
([docs.plamo.preferredai.jp](https://docs.plamo.preferredai.jp/)).

### Ollama

```json5
"ollama-cloud": {
  baseUrl: "https://ollama.com/v1",
  api: "openai-chat",
  apiKey: "${OLLAMA_API_KEY}",
},
"ollama-local": {
  baseUrl: "http://127.0.0.1:11434/v1",
  api: "openai-chat",
  apiKey: "local",   // placeholder — the local server ignores it
},
```

Model *values* may contain `:` (`ollama-cloud/glm-5.2:cloud`) — only route
*names* forbid it.

## Anything else

Any OpenAI-compatible endpoint (vLLM, LM Studio, llama.cpp server, LiteLLM,
a cloud vendor's compat layer, …) is one `providers` entry away:

```json5
"my-vllm": {
  baseUrl: "http://127.0.0.1:8000/v1",
  api: "openai-chat",
  apiKey: "local",
},
```

Then point a route at `my-vllm/<model>` and verify with
`llm-gateway providers`.
