# Roadmap

## Now (MVP) — single endpoint + always-on classification + fallback + accounting

- [x] 3 wire protocols in, byte-identical passthrough out
- [x] content-based routing on every request: embed the last user message,
      score it against every non-wildcard route description, threshold 0.45,
      reserved `default` route on miss
- [x] `semantic` as a default cargo feature; `--no-default-features` as the
      small build that always routes to `default`
- [x] fallback before first byte, targets may cross protocols (reachability
      from the client's protocol is checked per attempt at request time)
- [x] usage observation without touching the response (`tee`)
- [x] `usage-*.jsonl` / `trace-*.jsonl` + `stats` / `trace` CLIs
- [x] hot reload that never takes a bad config live
- [x] `launch` for Claude Code / Codex / opencode — zero client-config edits
- [ ] real-world smoke: each client through the gateway + the reverse test
      (stop the gateway, confirm every client fails = nothing bypasses it)
- [x] release pipeline: merge dev→main builds macOS binaries, publishes the
      GitHub Release and updates NAKAK10/homebrew-tap (hand-rolled workflow,
      not cargo-dist — see decisions.md)

## Blocked on external facts

- **OpenClaw migration** — OpenClaw runs on a different machine; before it can
  point here we need (1) that machine identified, (2) reachability decided
  (Tailscale tailnet IP recommended; never a bare `0.0.0.0`), (3) the staged
  Day 0–4 plan in `docs/clients/openclaw.md` executed while the 09:20 watchdog
  is observable.
- **Codex `wire_api` ambiguity** — official docs list `chat` and `responses`;
  secondary sources say `chat` was removed 2026-02. The gateway serves both
  endpoints and `launch.codex.wireApi` switches, so measurement decides.

## Phase 2 — classification (shipped)

- [x] every non-wildcard route participates as a candidate; the reserved
      `default` route is required and can also win classification outright
- [x] engine: `model2vec-rs` + `potion-multilingual-128M` (static, no ONNX, 101
      languages, distilled from BGE-M3); upgrade path: `fastembed` `BGEM3`
- [x] `init` downloads the model up front instead of treating classification as
      an opt-in extra
- [x] trace `routing.mode = "semantic"` / `"no_classifier"` with per-candidate
      scores, threshold, and embedding time

## Phase 3 — cross-protocol translation

- [x] `anthropic-messages` in → `openai-chat` out (issue #3), request +
      streaming and non-streaming response + error envelope + a local
      `count_tokens` estimate. Written from scratch rather than pulling in
      `va-ai-api-bridge`; see decisions.md for why, and for what the
      translation drops.
- [ ] the reverse direction (`openai-chat` client → `anthropic-messages`
      provider). No unmet need yet: OpenRouter exposes an Anthropic-compatible
      endpoint, and the OpenAI-protocol clients already reach `openai-chat`
      providers directly.
- [ ] anything involving `openai-responses` (issue #4). Codex is the only
      client that speaks it, and `launch.codex.wireApi: "chat"` sidesteps the
      problem entirely.
- [ ] let a provider choose its auth header (`Authorization: Bearer` vs
      `x-api-key`), independently of its `api`. This is what stands between us
      and GitHub Copilot's own `/v1/messages` endpoint: Copilot advertises it
      for its Claude models, so Claude Code could reach them with **no
      translation at all** — full fidelity, byte-for-byte — but it requires
      Bearer, and an `anthropic-messages` provider currently authenticates with
      `x-api-key`.
- [x] cross-protocol *fallbacks* within one route. `proxy` now selects
      translation per target (attempt) instead of once per route from the
      first target, so a route's `default` and `fallbacks` may each speak a
      different protocol; a target the client's protocol cannot reach is
      skipped at request time rather than refused by config validation. See
      decisions.md.

## Phase 4 — agent CLIs as backends

Register `claude -p` / `codex exec` / `opencode run` as upstream "providers"
so subscription auth can serve API-shaped traffic. OpenClaw's
`agentRuntime.id: "claude-cli"` is the prior art.

- [x] `transport: "claude-cli"` — `claude -p` as a provider, streaming and
      non-streaming, with the recursion guards and tool denial documented in
      gotchas.md. Verified end to end against a real subscription.
- [x] `transport: "codex-cli"` — `codex exec` as a provider, rendered as
      `openai-chat` so every client can reach it. Plumbing and error paths
      verified end to end; a *successful* generation could not be verified on
      the machine at hand, where the ChatGPT account refuses every model id
      tried (including its own configured default).
- [ ] `opencode run`, for the same reason on that subscription.
- [ ] tool passthrough. Today a request's `tools` are dropped, which is what
      makes this a generation upstream rather than an agent one; the CLI would
      need to accept a foreign tool schema and surface `tool_use` blocks for
      that to change.

## Postgres? No.

Not until one of: someone else uses this gateway; per-client budget cutoffs
are needed; cost must be charged back; or a month's JSONL exceeds ~100 MB.
Until then flat files + `stats` win on every axis that matters here.
