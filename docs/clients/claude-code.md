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

Keep whatever Claude-side `model` setting you already have if you like, but do
not mistake it for routing control anymore. Claude still wants a model string;
`llm-gateway` ignores that string when choosing a route and classifies every
request by content against route descriptions instead. `llm-gateway launch
claude` therefore feeds the fixed literal `default` to `ANTHROPIC_MODEL`, and
Claude's own `/model` picker is cosmetic as far as gateway routing is concerned.

Notes:

- `ANTHROPIC_AUTH_TOKEN` automatically becomes the HTTP bearer token. Do **not**
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
| `llm-gateway launch claude` | the gateway, with provider credentials | every configured provider, classification, fallback, cost accounting |
| the same, with a `claude-cli` provider in config | **the local `claude` binary**, with your subscription login | your plan *through* the gateway — classification and accounting included, minus what a CLI cannot carry |

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
  "role-sub": {
    description: "Requests that should run on my Claude subscription through the local Claude CLI. Generation only — caller tools are not passed through.",
    model: { default: "claude-subscription/claude-sonnet-5" },
  },
  "default": {
    description: "Fallback for requests that do not clearly match any other route.",
    model: { default: "anthropic/claude-sonnet-4-6" },
  },
}
```

The old launch-time route override is gone. If you want this route to win for a
certain class of requests, give it a description that genuinely distinguishes
those requests. Classification decides; the client-side model picker does not.

What that route cannot do is run **your** tools: `claude -p` has Claude Code's
own tools, not the ones in the request, and the gateway denies all of them
(`--allowedTools ""`) so a provider call cannot touch your files. So it is a
generation upstream, not an agent loop. `docs/gotchas.md` lists the rest
(multi-turn flattening, ~5s process startup per call, sampling parameters
dropped).

To reach **Claude models through the gateway with full API fidelity** instead —
tools, multi-turn, streaming exactly as the API defines it — buy them from a
provider that sells API access:

- **OpenRouter** — `openrouter-anthropic/anthropic/<model>`, the Anthropic wire
  protocol, so no translation.
- **GitHub Copilot** — a subscription with an official API, so a Copilot plan
  does serve gateway traffic. See `docs/providers.md`; the models must be
  enabled in your Copilot settings.

Leaving the Anthropic key empty during `init` is fine, and degrades
predictably: the reference stays as `${ANTHROPIC_API_KEY}`, and a target whose
credential does not resolve is **skipped in favour of the next fallback**
rather than failing the request (`key_unresolved` shows up in
`llm-gateway trace`). With the config `init` writes for Anthropic + OpenRouter,
that means Anthropic-flavoured traffic lands on OpenRouter until you set the
variable.

## Routing Claude Code to a non-Anthropic provider

Claude Code only speaks Anthropic Messages, but the gateway translates
`anthropic-messages` → `openai-chat`, so any OpenAI-compatible provider works
as a destination — Ollama (local or cloud), Groq, DeepSeek, Gemini, Mistral,
Together, Sakana AI, PLaMo.

Add a normal route with a real description (route names may not contain `:` or
`/`, so the provider's own model id stays on the right-hand side):

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
},
routes: {
  "default": {
    description: "Fallback for requests that do not clearly match any other route.",
    model: { default: "anthropic/claude-sonnet-4-6" },
  },
  "role-cheap": {
    description: "Short chores: summarizing, formatting, commit messages",
    model: { default: "ollama-local/qwen3:8b" },
  },
}
```

Then just use Claude Code normally. When the newest user text looks like a
better match for `role-cheap` than for the other routes, classification sends it
there — and an ambiguous follow-up turn ("continue", a bare tool result) keeps
the route of the last instruction that classified confidently, because
classification walks back through the conversation's own history. Claude's
`/model` UI does **not** force this route.

One thing worth knowing if you write descriptions that could plausibly match
Claude Code's own harness text: Claude Code injects a `<system-reminder>`
block (project `CLAUDE.md` and the like) into the first user message of every
session, and that block is stripped out of the classification input before
any comparison happens — so a description will not keep winning on the
strength of that boilerplate just because the walk-back reached all the way
back to message one.

What you give up on such a route is listed in the README
([Cross-protocol routing](../../README.md#cross-protocol-routing)); the short
version is prompt caching, thinking blocks, Anthropic server-side tools, and an
exact token count. `llm-gateway trace` marks these requests
`xlat=anthropic-messages->openai-chat`.

## `--isolate`

`llm-gateway launch claude --isolate` adds `--setting-sources project`, which
stops Claude Code from reading user-level settings **at all** — not just the
`env` block above. Permissions, hooks and model preferences from
`~/.claude/settings.json` are discarded along with everything else.

That is more than most sessions need. If the only reason to reach for
`--isolate` is a stale `ANTHROPIC_BASE_URL` (or similar) sitting in that
file's `env` block, the conflict warning `launch claude` already prints on
every run — without `--isolate` — is usually enough (see the notes above).
Reach for `--isolate` when permissions, hooks or model preferences from that
file are themselves the problem, not just its `env` block.

See the [README's `--isolate` by client table](../../README.md#--isolate-by-client)
for how this compares to Codex and opencode.

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
