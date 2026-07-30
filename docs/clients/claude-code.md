# Claude Code — manual setup

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

## Rollback

Delete the `ANTHROPIC_BASE_URL` line and restart Claude Code. One line.
