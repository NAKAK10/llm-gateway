# Roadmap

## Now (MVP) — single endpoint + fallback + accounting

- [x] 3 wire protocols in, byte-identical passthrough out
- [x] route resolution (exact → longest wildcard), `*` expansion
- [x] fallback before first byte, same-protocol only
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

## Phase 2 — semantic routing

Pick a route from the *content* of the request when the client asks for a
designated auto route (never overriding an explicit route name).

- embed `routes[].description` (that's why long ones live in `llm/*.md`) and
  the request's last user text; cosine top-1 over a threshold wins
- engine: `model2vec-rs` + `potion-multilingual-128M` (static, no ONNX, 101
  languages, distilled from BGE-M3); upgrade path: `fastembed` `BGEM3`
- trace `routing.mode = "semantic"` with per-candidate scores — the schema
  already has the fields (`candidates`, `score`, `threshold`, `embed_ms`)

## Phase 3 — cross-protocol translation

Anthropic Messages ⇄ OpenAI Chat ⇄ Responses, enabling cross-protocol
fallbacks. Evaluate `va-ai-api-bridge` (MIT) first; if its tool_use/thinking/
image/stop_reason round-trips fail, implement from scratch using
superagent-gateway's translate/stream sources as a behavioural spec (never as
code — no license). Lower urgency than originally planned: OpenRouter's
Anthropic-compatible endpoint already covers the main redundancy need.

## Phase 4 — agent CLIs as backends

Register `claude -p` / `codex exec` / `opencode run` as upstream "providers"
so subscription auth can serve API-shaped traffic. No OSS does this today;
OpenClaw's `agentRuntime.id: "claude-cli"` is the only prior art.

## Postgres? No.

Not until one of: someone else uses this gateway; per-client budget cutoffs
are needed; cost must be charged back; or a month's JSONL exceeds ~100 MB.
Until then flat files + `stats` win on every axis that matters here.
