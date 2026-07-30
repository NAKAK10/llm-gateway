# Design decisions (ADR)

Newest first. Each entry records *why*, because the code alone can't.

## 2026-07-30 — Hand-rolled release workflow instead of cargo-dist

The trigger model is "merge dev→main releases whatever version Cargo.toml
says", not "push a tag" — cargo-dist is tag-driven, so it would need an
auto-tagging shim anyway, and its generated workflow is ~2000 lines we would
own without understanding. The hand-rolled `release.yml` is ~100 lines:
version check (no-op when the tag exists, so docs-only merges are safe),
macOS arm64+x86_64 build, GitHub Release, formula regeneration pushed to
NAKAK10/homebrew-tap via a write-scoped deploy key (`TAP_DEPLOY_KEY` secret —
narrower than any PAT). Revisit cargo-dist if targets multiply.

## 2026-07-30 — Repo made public

Required for a normal Homebrew experience (binary downloads from Releases
need no auth). Pre-publication audit: full-history blob scan for key
patterns and personal data came back clean; the one finding — a committed
`target/` directory leaking local paths — was purged with filter-branch and
force-pushed before anything else happened. Commit identity is the public
NAKAK10 address.

## 2026-07-30 — Response bodies are never parsed or rebuilt

The gateway rewrites the request's `model` field and nothing else; responses
are streamed byte-for-byte (`reqwest::bytes_stream` → `axum::Body::from_stream`).

Why: the Anthropic ⇄ OpenAI translation layer is where every comparable
gateway has its worst bugs (Bifrost: duplicated `message_start` SSE frames,
system-message hoisting that破壊s prompt caching). By never re-serialising a
response, that entire bug class cannot exist here. Usage is extracted by a
read-only observer on a copy of the bytes (`usage/tee.rs`).

Consequence: fallbacks must stay within one wire protocol. Accepted, because
every current client/provider pair is same-protocol, and OpenRouter exposes an
Anthropic-compatible endpoint for cross-vendor redundancy without translation.

## 2026-07-30 — Rust, from scratch, no fork

- `superagent-ai/gateway` matched the requirements closely but has **no
  LICENSE file** → all rights reserved → forking is legally off the table.
  (Its `translate.rs`/`stream.rs` are read as a behavioural reference only.)
- `api7/aisix` (Apache-2.0) drags in etcd + admin API — too heavy for a
  single-binary local tool.
- `litellm-rs` accepts only OpenAI-shaped inbound traffic.
- LiteLLM (Python) was the earlier plan; dropped when the owner asked for a
  Rust implementation and `~/.config/llm-gateway/config.json`-style management.

## 2026-07-30 — `launch` instead of editing client configs

Requirement from the owner: never touch other tools' config files.
Verified mechanisms: Claude Code = env vars only; Codex = `-c` dotted overrides
(no env var can redirect its upstream); opencode = `OPENCODE_CONFIG_CONTENT`
(`OPENCODE_CONFIG` loses to project configs). OpenClaw is a daemon on another
machine → documented manual setup only (`docs/clients/openclaw.md`).

## 2026-07-30 — Fallback only before the first response byte

Once the status line and first chunk are sent the response is committed.
Retryable = connect failure / header timeout / 408 / 429 / 5xx. Client-fault
4xx is forwarded immediately (retrying a malformed request elsewhere burns
money and hides the real error). The last target's response is always
forwarded, even a 429 — the real upstream error beats a synthesized one.

## 2026-07-30 — Usage/trace recording happens on stream Drop

A client disconnect still costs tokens upstream, and a cancelled handler
future never reaches its own end. The destructor is the only place that runs
in every case, so `tee::observe` reports from `Drop` and records the outcome
(`success` / `aborted` / `error`) instead of pretending everything completed.

## 2026-07-30 — config.json may hold literal keys

The owner explicitly wants direct-value keys to be allowed. Mitigations:
file created `0600`, permission drift warned by `config check`, secrets masked
in `config show` and in `launch --print`, `config gitignore` template, and
`${ENV}` / `keychain:` forms for anyone who wants better.

## 2026-07-30 — `etcetera` for paths, not `dirs`/`directories`

`~/.config/llm-gateway/` on macOS was a hard requirement. `dirs`/`directories`
deliberately return `~/Library/Application Support` and refuse XDG semantics;
`etcetera::choose_base_strategy()` gives XDG everywhere but Windows.

## 2026-07-30 — Embedding choice for Phase 2 (semantic routing)

`model2vec-rs` + `potion-multilingual-128M` first: static embeddings (no ONNX
runtime dependency — `ort` is still 2.0.0-rc), 101 languages incl. Japanese,
256 dims, MIT, and it is a distillation of BGE-M3 — the model chosen as the
accuracy fallback (`fastembed`'s `BGEM3`) — so upgrading keeps the vocabulary
character. `routes[].description` doubles as the classification corpus, which
is why long descriptions live in dedicated `llm/*.md` files.
