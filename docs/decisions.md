# Design decisions (ADR)

Newest first. Each entry records *why*, because the code alone can't.

## 2026-07-31 — Model wildcards abolished; `init` stops scaffolding a dead subscription route

`stats`'s per-model table started showing route names (`role-anthropic`,
`default`) instead of real model ids. Tracing it back turned up a second,
unrelated bug in the same area, both fixed here.

**Bug: a route's `*` model wildcard silently sent the route name upstream as
the model.** `ModelRef::expand(requested)` (`src/config/mod.rs`) substituted
`*` in a route's model with whatever `requested` was — a holdover from when
routing matched the client's requested model string by prefix. Since "Always
classify" (the entry above this one) made `requested` always the *matched
route name* instead, a route configured as `anthropic/*` sent the literal
string `"role-anthropic"` upstream as the model on every request — a
guaranteed failure, and the reason `stats` showed route names where a model
belonged. `ModelRef::expand` is now gone, `route::resolve` uses the parsed
model as-is, and `src/config/validate.rs` rejects any route model containing
`*` outright: routing is decided purely by content classification now, so
there is nothing left for a `*` to stand in for. Route-*name* wildcards
(`claude-*` → prefix matching) are unrelated and still work exactly as
before.

**Bug: choosing "Subscription" for a provider in `init` scaffolded a second,
always-broken route anyway.** `build_config_with_auth` used to write the
plain API-key provider and `role-<id>` route for *every* selected provider,
even one whose credential the user just said was a subscription — with no
key to put in it, that route's `apiKey` was always empty and every request
through it always failed. `init` now skips the plain provider and route
entirely when Subscription is chosen for that provider; add it back by hand
later if a route that needs tools (which the subscription transport does not
forward) turns out to be wanted alongside it.

**The wizard now asks for an explicit model per route, and `init.rs`'s
auto-fallback wildcards are gone.** Each selected non-subscription provider's
`role-<id>` route needs a real `<provider>/<model>` now; the wizard prompts
for one, pre-filled with `KnownProvider::default_model()`'s suggestion.
`init`'s auto-added cross-provider fallbacks
(`openrouter-anthropic/anthropic/*`, `openrouter/openai/*`) used the same
mechanism and are dropped rather than replaced — guessing a matching model on
a different provider was never reliable, and a fallback is now something a
user opts into by hand. `KnownProvider::subscription_model()` for Anthropic
changed from the alias `"sonnet"` to the full id `"claude-sonnet-5"`: the
natural-looking next guess, `"sonnet-5"`, is not a valid alias and resolves to
`model_not_found` — confirmed by direct `claude -p` invocation before landing
this.

## 2026-07-31 — `init` asks before it regenerates, instead of just refusing

Running `init` over an existing `config.json` used to print "edit it directly"
and exit — safe, but unhelpful the moment someone actually wants a clean
restart (a new schema, a botched hand-edit, starting over). The owner asked
for a real choice instead of a wall.

**It confirms, then backs up, then proceeds.** `cli::init::run` now shows a
`cliclack::confirm` spelling out the consequence — "every provider, route and
key currently in the file will be replaced" — before touching anything.
Answering no leaves the file untouched and exits, same as before. Answering
yes copies the existing file to `config.json.<rfc3339-timestamp>.bak` next to
it (see `backup_path_for`) and only then runs the normal wizard. The timestamp,
not a fixed `.bak` name, is deliberate: confirming a regeneration twice in a
row must not silently discard the first backup.

## 2026-07-31 — Always classify; remove route-selection theatrics from the config

The old split brain — "exact/wildcard routing is the real thing, semantic
routing is an opt-in extra" — did not survive contact with the codebase. The
new rule is simpler: classify every request, keep one reserved fallback, and
stop pretending the client's `model` string is a policy API.

**Always-classify beat opt-in because the gateway only had one interesting
routing question left.** `src/server/proxy.rs::classify_request` now embeds the
last user message for every inbound request, scores it against every
non-wildcard route description via `src/semantic/index.rs`, and uses the top
candidate only if it clears `CLASSIFICATION_THRESHOLD` (`0.45`). That is easier
to explain, easier to trace, and harder to misconfigure than a special
`routes[].semantic` side path.

**`default` became the one explicit escape hatch.** `src/config/mod.rs`
reserves the route name, `src/config/validate.rs` requires it to exist and
forbids it from being a wildcard, and the classifier falls back to it both on a
low score and on classifier absence. It is still a real route with its own
`description` and `model`, so it can win honestly instead of being a dead bucket
at the bottom of the file.

**The config had to stop narrating removed choices.** The everyday schema is now
`server` + `providers` + `routes` + `logging`; `launch` survives only as the
rare hand-edit for extra CLI args, Codex's `wireApi`, and opencode's provider
overrides (`src/config/mod.rs`). `launch.<client>.model` went away because the
launchers in `src/launch/` always feed the fixed literal `default` to the child
process now. The client still wants *a* model-shaped string; the gateway no
longer treats it as authority.

**`semantic` becoming a default feature is what makes the docs honest.**
`Cargo.toml` now enables it by default, `src/cli/init.rs` downloads the
`potion-multilingual-128M` files unconditionally before writing `config.json`,
and the only opt-out is a `--no-default-features` build that always routes to
`default`. "Install classification if you feel like it later" was no longer a
true statement once `init` depended on the model being there.

**No migration path is deliberate, not forgotten.** The shape changed too much:
`routes[].semantic` disappeared, launcher `model` fields disappeared, `init`
stopped generating wildcard selector routes, and validation now demands a
literal `default` route plus descriptions on every non-wildcard route. The only
safe instruction is to delete `~/.config/llm-gateway/config.json` (or the whole
config directory from `src/paths.rs`) and re-run `llm-gateway init`.

## 2026-07-31 — `init`'s subscription and OpenRouter scaffolding, two bugs fixed

Flagged by the owner as "confusing": a fresh `config.json` can carry
`anthropic`, `anthropic-subscription`, `openrouter`, `openrouter-anthropic` —
one upstream's account under several ids. Investigating *why* surfaced two
real bugs, fixed here, plus one thing that is not a bug.

**Bug: `openrouter-anthropic`'s `baseUrl` doubled `/v1`.** `init` reused
`KnownProvider::OpenRouter.base_url()` (`https://openrouter.ai/api/v1`, meant
for the `openai-chat` id) for the Anthropic-protocol id too. `endpoint_url`
appends `/v1/messages` for any `anthropic-messages` provider, producing
`https://openrouter.ai/api/v1/v1/messages` — a URL that 404s. OpenRouter's
Anthropic-compatible root is `/api`, not `/api/v1`; `openrouter-anthropic` now
gets its own literal base URL instead of borrowing the other id's.

**Bug: two subscriptions collided on one route name.** Choosing both an
Anthropic and an OpenAI subscription made `init` write `role-subscription`
twice — the second `Config.routes.insert` silently discarded the first, so
the Anthropic route vanished from the generated file with no error. Each
subscription now gets `role-<id>-subscription` (`role-anthropic-subscription`,
`role-openai-subscription`).

**Not a bug, but worth writing down: why the duplicate ids exist at all.** A
`ProviderConfig` couples one upstream to exactly one `ApiKind` and one
`Transport`, by design (`src/config/mod.rs`) — so that
`route.model.fallbacks` never has to cross either mid-request. OpenRouter
answers two protocols and Anthropic can be reached by two mechanisms (a key,
or the subscription's CLI), and neither the code nor the config format has a
narrower way to say "same account, different shape" than a second id. A
provider-per-(protocol, transport) model is simpler to reach *from*, but
folding several shapes into one entry would touch `validate`, `route`,
`proxy`, `semantic::index`, `usage::tee` and every place that reads
`ProviderConfig.api` as a single value — a larger change than this pass, and
not undertaken here. `docs/providers.md` now says this out loud instead of
leaving the second id to look like an accident.

## 2026-07-31 — `codex exec` too, rendered as `openai-chat`, with the verification gap recorded

The same reasoning as the entry below, applied to a ChatGPT plan:
`transport: "codex-cli"` runs `codex exec --json`. Two decisions differ.

**It renders `openai-chat`, not the CLI's own shape.** Codex's events are not any
published wire format, so the gateway has to pick a protocol to present, and chat
completions is the one the most clients can reach — including Claude Code, through
the translation that already exists. One synthesizer, every client.

**Streaming is honest rather than simulated.** Codex emits *item-level* events: an
`agent_message` arrives complete. So a streaming request gets a well-formed
`openai-chat` stream that all arrives at once, and the text is deliberately *not*
chopped into fake deltas. A synthetic token stream would look better and tell the
client a lie about when the model produced what.

Tool denial differs too, because the tools differ: `claude -p` takes
`--allowedTools ""`, while Codex gets `--sandbox read-only` in an empty scratch
directory, which is the narrowest posture it offers.
`--dangerously-bypass-approvals-and-sandbox` is never passed.

**What could not be verified, and this matters:** a *successful* generation. The
ChatGPT account on the machine at hand refuses every model id tried — `gpt-5`,
`gpt-5.1`, `gpt-5-codex`, `gpt-5.1-codex`, and the user's own configured
`gpt-5.6-sol` — with "not supported when using Codex with a ChatGPT account".
Verified instead: the whole path up to and including that refusal, arriving at the
client as a clean `openai-chat` error carrying the CLI's own sentence. The success
path rests on the event shapes observed live plus unit fixtures, so the first
person with a working plan should re-check it. That is why the route scaffolded by
`init` uses `default` (no `-m` at all) rather than a model id guessed from here.

## 2026-07-31 — Subscriptions are served by running the official client, not by holding its credential

A Claude Pro/Max plan authenticates Claude Code. It is not an API key, and the
two ways to make it look like one are not equivalent:

- **Rejected: presenting the client's credential upstream.** Lifting Claude
  Code's OAuth token and sending it to `api.anthropic.com` as if the gateway were
  Claude Code is using a subscription outside what it is sold as. Not built, and
  the reason is written down here so it does not get "fixed" later.
- **Built: `transport: "claude-cli"`.** The gateway spawns `claude -p` and
  translates its output. The official client authenticates itself, with its own
  login, on the user's own machine — which is also what OpenClaw does
  (`agentRuntime.id: "claude-cli"`), the prior art this follows.

The shape that made it cheap: `--output-format stream-json --verbose
--include-partial-messages` emits the provider's **own Anthropic stream events**,
one per line. So streaming is unwrapping rather than rebuilding, and `usage`
(cache counts included) and `stop_reason` arrive as the model produced them. The
non-streaming path reads the single `assistant` event, taking `result`'s usage
because the assistant event's is a mid-run snapshot.

Design decisions worth keeping:

- **`transport` is a provider field, separate from `api`.** The CLI's output *is*
  `anthropic-messages`, so protocol and transport are different questions and
  validation enforces the pairing. Everything downstream — routes, fallback,
  translation, trace, stats — needed no knowledge of it.
- **`BodyStream` keeps `reqwest::Error` as its error type** even for a local
  body. A subprocess never produces one, and its failures are reported as an
  Anthropic error *body* (readable by the client) rather than a stream error
  (not). That single choice is why `usage::tee`, `translate::adapter` and
  `server::passthrough` were untouched by this feature.
- **The child is a text generator, not a session.** `--allowedTools ""` denies
  every tool (verified: it refuses without hanging on a permission prompt),
  `--setting-sources project` in an empty scratch directory loads no user
  settings, `--strict-mcp-config` loads no MCP servers, and the `ANTHROPIC_*`
  variables are removed from its environment. The last two guards exist for the
  same reason: `settings.json`'s `env` or an inherited shell could point the child
  back at this gateway, forever.
- **`count_tokens` never spawns.** Answering it would mean running a full
  generation, so a `claude-cli` target takes the local estimate path that
  `openai-chat` targets already use.

Accepted limits, documented rather than worked around: the caller's `tools` are
dropped (making this a generation upstream, not an agent one), a `messages` array
is flattened into a labelled transcript, sampling parameters have no CLI
equivalent, and process startup costs ~5s per call.

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
