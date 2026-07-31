# Claude Code — manual setup

[English](claude-code.md) | [日本語](../ja/clients/claude-code.md)

`llm-gateway launch claude` is the supported path and needs none of this.
This page is for making the redirect *permanent* by hand. The gateway itself
never writes this file.

## Point Claude Code at the gateway

Add to `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4000",
    "ANTHROPIC_AUTH_TOKEN": "<server.apiKey value>",
    "ANTHROPIC_CUSTOM_HEADERS": "x-gw-client: claude-code",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
  }
}
```

Keep your existing `"model"` setting — the gateway's `claude-*` wildcard route
forwards whatever id Claude Code resolves, including the small model used for
background requests, so nothing breaks when Anthropic ships new ids.

Notes:

- `ANTHROPIC_AUTH_TOKEN` gets `Bearer ` prepended automatically. Do **not**
  also set `ANTHROPIC_API_KEY` — pick one, or you will spend an afternoon
  discovering which header won.
- Values in the settings `env` block **override your shell environment**. That
  is why `launch claude` warns when this file already sets
  `ANTHROPIC_BASE_URL` — the two mechanisms fight, and settings.json wins.
- To avoid writing the token into the file, use `"apiKeyHelper"` with a script
  that prints it instead.

## "I have a Claude subscription, not an API key"

Then you have three modes, and the choice is per launch:

| you run | who authenticates | what you get |
|---|---|---|
| `claude` | Claude Code, with your subscription login | Anthropic's models on your plan. The gateway is not involved, so no routing, no fallback, no accounting. |
| `llm-gateway launch claude` | the gateway, with provider credentials | every configured provider, routing, fallback, cost accounting |
| the same, with a `claude-cli` route | **the local `claude` binary**, with your subscription login | your plan *through* the gateway — routing and accounting included, minus what a CLI cannot carry |

The third one is the answer to "I want my subscription, but I also want the
gateway". A Claude Pro/Max plan is a credential for Claude Code, not an API key,
so the gateway cannot present it upstream — but it can run the official client,
which already holds the login:

```json5
providers: {
  // No baseUrl, no apiKey: the CLI authenticates itself.
  "claude-subscription": { api: "anthropic-messages", transport: "claude-cli" },
},
routes: {
  "role-sub": { model: { default: "claude-subscription/sonnet" } },
}
```

```sh
llm-gateway launch claude --model role-sub
```

What that route cannot do is run **your** tools: `claude -p` has Claude Code's
own tools, not the ones in the request, and the gateway denies all of them
(`--allowedTools ""`) so a provider call cannot touch your files. So it is a
generation upstream, not an agent loop — good for a `role-*` route that writes
or summarises, wrong for the route your editor session runs on. `docs/gotchas.md`
lists the rest (multi-turn flattening, ~5s process startup per call, sampling
parameters dropped).

To reach **Claude models through the gateway with full API fidelity** instead —
tools, multi-turn, streaming exactly as the API defines it — buy them from a
provider that sells API access:

- **OpenRouter** — `openrouter-anthropic/anthropic/*`, the Anthropic wire
  protocol, so no translation (`init` scaffolds this as the `claude-*` route's
  fallback).
- **GitHub Copilot** — a subscription with an official API, so a Copilot plan
  does serve gateway traffic. See `docs/providers.md`; the models must be
  enabled in your Copilot settings.

Leaving the Anthropic key empty during `init` is fine, and degrades predictably:
the reference stays as `${ANTHROPIC_API_KEY}`, and a target whose credential
does not resolve is **skipped in favour of the next fallback** rather than
failing the request (`key_unresolved` shows up in `llm-gateway trace`). With the
config `init` writes for Anthropic + OpenRouter, that means Claude Code traffic
lands on OpenRouter until you set the variable.

## Routing Claude Code to a non-Anthropic provider

Claude Code only speaks Anthropic Messages, but the gateway translates
`anthropic-messages` → `openai-chat`, so any OpenAI-compatible provider works
as a destination — Ollama (local or cloud), Groq, DeepSeek, Gemini, Mistral,
Together, Sakana AI, PLaMo.

Add a route with a plain name (route names may not contain `:` or `/`, so the
provider's own model id stays on the right-hand side):

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
},
routes: {
  "role-cheap": {
    description: "Short chores: summarizing, formatting, commit messages",
    model: { default: "ollama-local/qwen3:8b" },
  },
}
```

Then pick it, either per session:

```sh
llm-gateway launch claude --model role-cheap
```

or from inside Claude Code with `/model role-cheap` — the name has to match the
route exactly.

What you give up on such a route is listed in the README
([Cross-protocol routing](../../README.md#cross-protocol-routing)); the short
version is prompt caching, thinking blocks, Anthropic server-side tools, and an
exact token count. `llm-gateway trace` marks these requests
`xlat=anthropic-messages->openai-chat`.

## Verify

```sh
llm-gateway trace --tail     # in another terminal, with serve --debug
claude -p "ping"             # a trace line with client=claude-code must appear
```

Then the reverse test — stop the gateway and confirm `claude -p "ping"`
**fails**. If it succeeds, it is talking to Anthropic directly and none of
your traffic is going through the gateway.

## Troubleshooting

| symptom | fix |
|---|---|
| 400 mentioning betas / `context_management` | `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` (temporarily, to isolate) |
| settings changes not taking effect | settings `env` overrides shell exports — check the file, not your shell |
| count_tokens errors | the gateway forwards `/v1/messages/count_tokens`; check `llm-gateway providers` |
| context size looks wrong on an `openai-chat` route | expected: that protocol has no counting endpoint, so the number is a local estimate |
| a local model seems to answer nothing, then everything | its `reasoning_content` is dropped in translation; only the answer is forwarded |

## Rollback

Delete the `ANTHROPIC_BASE_URL` line and restart Claude Code. One line.
