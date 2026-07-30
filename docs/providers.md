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

The `claude-*` wildcard route (`model: { default: "anthropic/*" }`) forwards
whatever id Claude Code asks for, so new Anthropic model ids need no config
change.

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
// Same upstream, Anthropic wire protocol — lets `claude-*` fall back to it
// without crossing ApiKinds:
"openrouter-anthropic": {
  baseUrl: "https://openrouter.ai/api/v1",
  api: "anthropic-messages",
  apiKey: "${OPENROUTER_API_KEY}",
},
```

Model ids contain a `/` (`anthropic/claude-sonnet-4.6`); route targets split
on the *first* `/` only, so `openrouter/anthropic/claude-sonnet-4.6` parses
fine.

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
as a fallback for `claude-*` routes.

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
