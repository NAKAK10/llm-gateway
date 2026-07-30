# Design decisions (ADR)

Newest first. Each entry records *why*, because the code alone can't.

## 2026-07-31 — GitHub Copilot is an ordinary provider; `command:` secrets exist for it

Copilot needs no special support in the gateway. `https://api.githubcopilot.com`
speaks `openai-chat` and accepts a plain GitHub token as
`Authorization: Bearer`, so with cross-protocol translation in place it is
reachable from every client the gateway fronts, Claude Code included. Verified
against the real API: `gpt-4.1` driving Claude Code through a full
tool_use → tool_result loop.

Two decisions came out of getting there:

- **A new `command:<cmd>` secret form**, rather than a Copilot-specific auth
  mechanism. The actual problem is generic: the credential is minted and
  rotated by another tool (`gh`), so any copy of it goes stale. `${VAR}` cannot
  fix that — a `serve` process's environment is frozen when it starts — while a
  command re-run per attempt is always current. `keychain:` already spawns a
  process per attempt, so this adds no new cost model. It also means `init` can
  offer Copilot without asking for a key at all.
- **No editor impersonation.** An earlier attempt went at
  `copilot_internal/v2/token` with VS Code-shaped identity headers, on the
  assumption that a token exchange was required, and got a `403` pointing at
  GitHub's terms. That assumption was simply wrong — the current API wants
  nothing but a Bearer token — and the correct fix was to stop guessing, read
  what a working client (opencode) actually sends, and send that: a Bearer
  token, an honest `User-Agent`, and a pinned `X-GitHub-Api-Version`.

Not implemented: Copilot's own `/v1/messages` endpoint, which would let Claude
Code reach its Claude models with no translation at all. It requires
`Authorization: Bearer`, and an `anthropic-messages` provider here authenticates
with `x-api-key`; giving a provider control over its auth header is the
prerequisite, and it could not be verified on the account at hand (that endpoint
answers `no_available_model_endpoints` there). Left as a follow-up rather than
shipped unverified.

## 2026-07-31 — One-way translation: Anthropic Messages in, OpenAI Chat out

Issue #3. `launch claude` had two possible destinations (Anthropic,
OpenRouter-as-Anthropic) because Claude Code only speaks `/v1/messages` and
every other provider in the table speaks `openai-chat`. "Send the cheap work to
local Ollama" — the most basic reason to run a gateway at all — was impossible
for the client most people use it with. So the `anthropic-messages` →
`openai-chat` direction is now translated (`src/translate/`).

Scope decisions:

- **One direction only.** The reverse (`openai-chat` client → Anthropic
  provider) has no unmet need: OpenRouter already exposes an
  Anthropic-compatible endpoint, and the OpenAI-protocol clients can all reach
  `openai-chat` providers directly. Responses ⇄ anything stays untranslated
  (issue #4).
- **The passthrough guarantee is kept where it applies.** Same-protocol
  requests do not touch the new code path at all: `proxy` picks
  `passthrough::respond(observed)` directly, and translation is only reachable
  for a pair that previously returned `400`. No working config changes
  behaviour.
- **Cross-protocol *fallback* is still refused by validation.** `proxy` selects
  one translation per route from its first target; a route mixing protocols
  would make the answer depend on which upstream happened to respond. Uniform
  target lists keep that unambiguous.
- **Usage accounting reads the upstream bytes, before translation.** The
  observer (`usage/tee.rs`) sits below the translation layer, so token counts
  come from what the provider actually reported rather than from a rebuilt
  body.
- **`count_tokens` is answered locally with an estimate.** `openai-chat` has no
  equivalent endpoint. A `400` would leave Claude Code unable to size its
  context window (it decides when to compact from that number), and forwarding
  the question to a *different* provider would answer with a token count for
  the wrong tokenizer. An approximate answer, marked in the trace log, is the
  least-wrong option.
- **Every translated request is marked in the trace log**
  (`resolved.translation`, shown as `xlat=…` by `llm-gateway trace`), because
  "why does this output look slightly different?" needs an answer that does not
  require reading the config.

Lossy by construction: prompt caching, `thinking` blocks, citations and
Anthropic server-side tools have no `openai-chat` representation and are
dropped. `openai-chat` `reasoning_content` is dropped in the other direction
rather than forged into a `thinking` block, which would need a `signature` only
Anthropic can produce. Documented in `docs/gotchas.md`.

Not evaluated further: `va-ai-api-bridge` (the roadmap's first candidate) would
have been another dependency for something that is ~700 lines here and needs to
match *these* clients' quirks exactly — `finish_reason: "stop"` alongside tool
calls, Ollama's missing `index`/`id` on tool calls, mid-stream error frames.

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
