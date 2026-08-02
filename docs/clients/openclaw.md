# OpenClaw — manual setup (no `launch` support)

[English](openclaw.md) | [日本語](../ja/clients/openclaw.md)

OpenClaw runs as a daemon with its own scheduler, typically on a **different
machine** than the gateway. There is no process for `launch` to start, so this
is a manual, staged migration — and because a daily content pipeline hangs off
its 09:01 cron, the staging is not optional.

## 0. Reachability first

`http://127.0.0.1:4000` does not exist from another host. Decide reachability
before touching OpenClaw:

- **Recommended: Tailscale.** Bind the gateway to the tailnet IP and set
  `server.apiKey` (the gateway refuses a non-loopback bind without one).
- **Never** a bare `0.0.0.0` on a LAN: one leaked key = every provider
  credential in your config, usable by anyone who finds the port.

Verify from the OpenClaw host before proceeding:

```sh
curl -s -H "Authorization: Bearer $KEY" http://<tailnet-ip>:4000/v1/models
```

## Auto-route, and why OpenClaw likely wants to turn it off

By default this gateway **ignores the model name a client sends** and picks
a route by classifying the request's content against every route's
`description` (see
[Content-classified routing](../../README.md#content-classified-routing)).
That fits a harness like Claude Code well, where the model string is mostly
cosmetic. It is the wrong default for a client like OpenClaw, whose agent
loop sets an explicit model name on every internal call (title generation,
per-agent judgment calls, and so on) — classification would second-guess
every one of those choices instead of respecting them.

The opt-out already exists and needs no new gateway feature: sending
`x-gw-auto-route: 0` on a request makes the gateway skip classification and
resolve whatever model name the client sent directly as a route name (see
`auto_route_requested` in `src/server/proxy.rs`). `llm-gateway launch` asks
about this interactively, once per session — but OpenClaw has no `launch`
support (it is a long-running daemon, often on a different machine, per the
intro above), so there is no prompt to answer. The header has to be pinned
on OpenClaw's side instead, so every request it sends carries it.

Whether — and how — OpenClaw lets you pin a fixed extra header per provider
is not something this doc can state; that depends on OpenClaw's own
configuration schema, which has not been verified here. Check OpenClaw's own
docs/config reference for a way to attach a static header to the `gateway`
provider block above, and set it to:

```
x-gw-auto-route: 0
```

**This also changes how the model name is resolved.** With auto-route off,
`find_route` in `src/route.rs` looks up the sent model name as a route key
with a plain, exact map lookup — no prefix match, no wildcard, no fuzzy
match (the same behavior documented for opencode's `overrideProviders` in
[`docs/clients/opencode.md`](opencode.md)). So every model name OpenClaw is
configured to send — the `models` list in step 1 below — must be the
**literal name of a route** in `config.json`. A model name with no matching
route 404s instead of falling through to classification.

A curl call that exercises the header directly (useful for confirming the
header actually reaches the gateway, independent of whatever OpenClaw does):

```sh
curl -s -H "Authorization: Bearer $KEY" \
     -H "x-gw-auto-route: 0" \
     -H "Content-Type: application/json" \
     -d '{"model":"role-researcher","messages":[{"role":"user","content":"ping"}]}' \
     http://<tailnet-ip>:4000/v1/chat/completions
```

With `x-gw-auto-route: 0` set, this resolves `role-researcher` as an exact
route name — no classification runs — so it only succeeds if `role-researcher`
is a real route in `config.json`. Drop the header (or set it to `1`) and the
same call goes through classification instead, ignoring the `model` field
entirely.

## 1. Add the provider (touches nothing that runs)

In `openclaw.json` on the OpenClaw host:

```json5
{
  models: {
    providers: {
      gateway: {
        name: "LLM Gateway",
        baseUrl: "http://<tailnet-ip>:4000/v1",
        apiKey: "<server.apiKey value>",   // cron has no shell env — a ${VAR}
                                           // reference resolves in your terminal
                                           // and 401s at 09:01. Literal, or put
                                           // the var in the daemon's own env.
        api: "openai-completions",
        // ★ Every route you want visible MUST be listed. A missing name
        //   simply doesn't exist as far as OpenClaw is concerned.
        models: ["role-manager", "role-researcher", "role-writer",
                 "role-reviewer", "role-publisher"],
      },
    },
  },
}
```

No agent points at it yet; the running system is unchanged. Confirm the models
are visible in OpenClaw's model list.

## 2. Migrate one low-stakes agent

Switch the **researcher** first (a failed research step degrades a run; it
does not kill it):

```json5
agents: {
  entries: {
    "ekkohappy-researcher": {
      model: {
        primary: "gateway/role-researcher",
        // ★ Keep the old direct route as the LAST-RESORT fallback.
        //   If the gateway machine is down, tomorrow's article still ships.
        fallbacks: ["ollama-cloud/deepseek-v4-flash"],
      },
    },
  },
},
```

Leave model-level fallback to the gateway; OpenClaw's `fallbacks` here is a
single "gateway unreachable → old direct path" escape hatch. Two fallback
systems both retrying the same failure doubles latency and cost.

Watch one full production run (the morning cron + its watchdog report), and
confirm the run shows up in `llm-gateway stats --by client`.

## 3. Then the rest, one per day

writer → reviewer → publisher → **controller last** (it drives the pipeline;
if it breaks, nothing else even starts). Rules that have already paid for
themselves:

- Switch on a day when a human will see the watchdog report. Not Friday
  night, not before a holiday.
- After each switch, trigger a **manual full run the same day**. The next
  morning's cron must never be the first test.

## Rollback

- Fastest: stop the gateway — every agent's `fallbacks` drops it back to the
  old direct route on its own.
- Clean: point `primary` back to the old `ollama-cloud/…` value (that is why
  step 2 keeps them in the file).
- Disaster (agent registrations lost — it has happened): re-register each
  agent with `openclaw agents add <name> --workspace … --model <old-model>`
  and recreate the crons. Keep a copy of `openclaw.json` from before step 1.
