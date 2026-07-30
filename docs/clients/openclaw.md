# OpenClaw — manual setup (no `launch` support)

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
