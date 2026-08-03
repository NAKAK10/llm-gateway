# Design decisions (ADR)

Newest first. Each entry records *why*, because the code alone can't.

## 2026-08-03 — `tool_result` content joins the history walk instead of being treated as blank

Reported failure: pasting a bare URL ("見て: https://github.com/.../issues/N")
with no other text classifies against whatever route fits a short, trivial
request — reasonably, since that's genuinely all the text says. The problem
surfaces afterward: once an agent's own tools fetch what the URL points to
(`gh issue view`, a web fetch, …) and the result lands in conversation
history as a `tool_result` block, that content — which might reveal the task
is not trivial at all — never gets a chance to influence routing on later
turns either. `classification_texts`/`content_text` only ever read `type:
"text"` blocks from `role: "user"` messages; a `tool_result` block
(Anthropic), a `role: "tool"` message (OpenAI Chat), or a
`function_call_output` item (OpenAI Responses) contributed nothing, by
design — an existing test asserted exactly that.

**First design considered, and rejected after tracing through the actual
scenario: a last-resort third tier**, tried only after the existing
system-prompt tier and user-text history walk both miss, leaving those two
tiers' order, thresholds, and tests completely untouched. This does not fix
the bug. On the turn right after the fetch, the newest *user-role* message in
the request is the `tool_result` — but the walk already treats a textless
`tool_result` as blank and keeps going *past* it, straight back to the
original "見て: <url>" message, which still clears the trivial route's
threshold on its own. The user-text tier never misses in this scenario, so a
tier tried only after it exhausts never runs, and the session stays pinned
to the trivial route forever.

Researched how comparable OSS/commercial routers handle "more signal becomes
available as a session progresses": RouteLLM, Aurelio Labs'
`semantic-router`, LiteLLM Router, FrugalGPT, Not Diamond, OpenRouter's
auto-router, and Cursor's router. None widen a single classification pass —
FrugalGPT scores a cheap model's own output and escalates only if it looks
insufficient; Cursor re-runs its classifier on each new agent turn's
accumulated context rather than pinning one decision for a whole session.
That pattern — "a separate, later decision point" — is what motivated the
rejected tier design above. It turned out to be the wrong shape for this
specific bug regardless: the issue here isn't that a second decision point is
missing, it's that the *existing* history walk (built in the 2026-07-31 entry
below specifically to walk past a turn with no real signal, "continue," a
bare `tool_result`, to find "the instruction such a turn is continuing") was
built on an assumption that no longer holds universally: that a `tool_result`
never *is* that instruction, only ever masks one. Sometimes it is — the
fetched content can be more informative than what it followed.

**Built: `classification_texts` no longer filters to `role: "user"` +
`type: "text"` only.** `role: "tool"` (OpenAI Chat) and `function_call_output`
items (OpenAI Responses, which carry no `role` field and so were dropped
outright before) now count, and Anthropic's `tool_result` blocks (nested
inside a `role: "user"` message) are read via a new
`classification_content_text` — a fork of `content_text`, not a change to it,
because `content_text` is shared with `system_prompt_text` (where a
`tool_result` block never appears anyway) and `last_user_text` (`extract_input`'s
trace-log "what did the user last say" field — changing what that field means
is a separate decision this entry does not make). The walk itself, its
threshold (`CLASSIFICATION_THRESHOLD = 0.45`, same as ordinary user text — real
external content, not harness boilerplate, so it doesn't need the stricter
system-prompt bar), and `HISTORY_WALK_LIMIT` are all unchanged; only what
counts as a candidate text at each history position changed. Being
newest-first is what makes this safe rather than a free-for-all: fetched
content only wins if it's more recent than whatever would otherwise have
decided the route — the same recency rule that already governs plain
conversation. No new `SemanticOutcome` variant or trace field was needed
either — `Matched { texts_back }` still means exactly what it did, and
`decided_by_text` (truncated to 200 chars) already shows, after the fact,
whether a match came from ordinary conversation or fetched tool content.
Anthropic's own `tool_result`-content string-or-array-of-blocks handling
already existed in `translate/request/anthropic_to_chat.rs`'s
`tool_result_text`/`text_blocks` — made `pub(crate)` and reused rather than
reimplemented.

**Accepted tradeoff, not fully mitigated:** tool output can be noisy (a large
diff, raw command output) and could in principle false-positive against an
unrelated route's `description`, the same way `<system-reminder>` boilerplate
did before it was special-cased (2026-07-31 entry below). Unlike that case,
there's no fixed, recognizable string to strip — genuine tool output varies
per project and per tool. No new mitigation was added beyond what already
protects ordinary noisy user text: the embedder's own 800-char/64-token
bound per candidate, and requiring a candidate to actually clear
`CLASSIFICATION_THRESHOLD` before it wins.

**Still forward-looking only, same as any per-request router:** this cannot
change which model answered the turn where the URL was first pasted, before
anything was fetched — only later turns, once the fetched content is itself
the newest classifiable text in history.

## 2026-08-03 — A utility-bypass call gets a real brevity floor and skips session persistence

The owner's real gateway showed 26 `<auto-mode>` (`utility_bypass`) requests
in a 5-minute window, all `attempt: 1` / `status: success` — no gateway-side
retry, fallback, or error anywhere. They arrived in tight, non-overlapping
groups of 5 identical requests (same payload bytes, confirmed via
`tokens_est`): request N+1 started essentially the instant request N's
response was delivered. Output length varied wildly for the *same* judgment,
from 5 tokens up to 1425, over 4–19s per call. This is Claude Code's own
internal auto-mode permission classifier re-issuing an identical judgment
repeatedly — the same "Auto mode could not evaluate this action" failure
mode the 2026-08-01 `Config::auto_mode` entry below already documents,
happening even with `autoMode` pinned to a dedicated, non-shared target.

**Blaming the model was rejected, correctly.** `autoMode` here points at
`anthropic-subscription/sonnet` (a `claude-cli` subprocess), and the same
model runs fine through the same transport for ordinary sessions in this
setup — so "claude-cli/sonnet is slow, pin `autoMode` at something else"
would have been treating a symptom of a *different, more specific* bug as
the cause. The real gap was already half-documented in
`claude_cli.rs`'s own doc comments: `token_budget_hint` — the only guard
against "multi-hundred-token rambling on judgment-style utility calls"
(issue #17) — is best-effort (the model can ignore it) **and only fires when
the caller's own payload sets `max_tokens`**. Claude Code's internal
judgment request may not set it at all, in which case the hint silently does
nothing and the model is free to take as long as it likes. Even a 5-token
answer got retried in the real log, which fits a client-side "the whole
call took too long" give-up better than "the answer wasn't understood."

**Decision: thread `Target::is_utility_bypass` down to the actual `claude -p`
invocation, and use it for two purely mechanical fixes — nothing that
changes how carefully the judgment itself gets made.** The fact "this
request is the `<transcript>` bypass" was already fully computed inside
`classify_request` (`SemanticOutcome::UtilityBypass`, all three of its
resolutions) but discarded before reaching `agent::spawn` — it now survives
as a field on `route::Target`, set in one place
(`proxy::mark_utility_bypass_targets`, called right where `Resolution` is
built from `semantic_attempt`, covering all three resolutions without
repeating the mutation at each of `classify_request`'s three return sites).
`agent::claude_cli::token_budget_hint` now falls back to a fixed
`UTILITY_BYPASS_DEFAULT_TOKEN_BUDGET` (20) when `max_tokens` is absent *and*
`is_utility_bypass` is set — closing exactly the gap above — and `args()`
adds `--no-session-persistence` for the same targets: a one-shot judgment
that will never be resumed gains nothing from a saved session, so skipping
that disk write is a pure latency win with no behavioral change to the
model's answer. Both are no-ops for every ordinary claude-cli request:
`is_utility_bypass` defaults to `false` everywhere except this one bypass.

**Deliberately excluded: `--effort low`.** It would cut latency and tokens
further than either change above, but it would also reduce how carefully
Claude Code's own allow/deny judgment for a proposed agent action gets
reasoned about — a security-relevant decision, not an implementation detail
— and that trade is the operator's to make, not the gateway's to assume.
Confirmed with the owner and left out.

## 2026-08-03 — The live feed carries the whole prompt; the trace log clips it

`extract_input` used to apply `RecordMode::truncate_at()` (200 chars unless
`--debug-full`) while *building* the `TraceRecord`, and `serve --ui`'s live
feed reused that record's `last_user_text` verbatim. So the dashboard could
only ever show the first 200 characters of a prompt, with `--debug-full` — a
decision about **what goes on disk permanently** — as the only way to see
more. A judgment-style request whose interesting part is 30k tokens in was
undiagnosable from the dashboard, which is the one thing the dashboard is for.

**Decision: the record holds prompt text in full, and the 200-char clip moves
to the write boundary** (`Recorder::trace`). Truncation is a property of
`trace-*.jsonl`, not of the record: the live feed is in-memory, per-tab, and
gone the moment nothing is subscribed (see `server::live`'s module docs for
why that was already a separate decision from `--debug`), so it can carry
everything while the file keeps clipping. `LiveEvent` gained `prompt_full` and
`system_prompt` alongside the existing `prompt_preview` — the preview is what
the collapsed table row shows (always clipped, `--debug-full` or not: it is a
row in a table), `prompt_full` is what an expanded row reads, and it is left
`None` when the preview already *is* the whole text so a short prompt is not
sent twice. The consequence is recorded in the README's security notes:
reaching the dashboard now means seeing full prompt text in real time, which
is what the one-time token, the `HttpOnly` cookie and the `Host` check guard.

## 2026-08-02 — `input_tokens` excludes cached tokens on the client-facing Anthropic shape too

`usage::parse` normalizes OpenAI-shaped usage (`prompt_tokens`/`input_tokens`,
which already include `cached_tokens`) by subtracting the cached portion out,
so [`Usage::input_tokens`] means "new, non-cached input" for the gateway's own
accounting regardless of upstream protocol. That normalization only ever
touched the gateway's internal bookkeeping (`usage-*.jsonl`, `stats`) — the
translated response bodies handed back to a client on an `openai-chat →
anthropic-messages` route (`translate::response::chat_to_anthropic`,
`translate::stream::ChatToAnthropic`) still put the raw, cache-inclusive
`prompt_tokens` straight into `input_tokens` and reported
`cache_read_input_tokens` right alongside it.

**Decision: normalize the client-facing shape the same way.** Anthropic's own
API treats `input_tokens` and `cache_read_input_tokens` as mutually exclusive
— a client that sums the two (Claude Code does exactly this, to render
context usage) is following the real Anthropic contract, so it is the
gateway's inclusive `prompt_tokens → input_tokens` mapping that was wrong,
not the client's assumption. `chat_to_anthropic` and `ChatToAnthropic` now
subtract `cached_tokens` before writing `input_tokens`, the same subtraction
`usage::parse` already does — so a translated route's client-visible token
counts and the gateway's own `stats` agree. The Responses-shaped output
(`chat_to_responses`, `ChatToResponses`) is deliberately left alone: it
mirrors OpenAI's own nested `input_tokens` + `input_tokens_details
.cached_tokens` convention, which is already internally consistent on its own
terms.

The one thing this does *not* fix is old data: `usage-*.jsonl` records written
before the internal `usage::parse` normalization landed have `in_tok`
inclusive of cached tokens, and `stats` has no marker distinguishing them from
records written after. A `stats` run whose date range straddles that change
undercounts the cache-hit portion of the older days. See
`docs/config-reference.md`'s `usage-*.jsonl` field list for the same note
aimed at someone reading the logs directly.

## 2026-08-01 — System-prompt classification: an agent's own role definition decides routing before user text does

Semantic classification (the entries below, from "Always classify" onward)
only ever looked at the *user's* text. That is the wrong signal for an
agentic sub-request whose "user" turn is really an instruction from another
agent. Real example that surfaced this: a Claude Code Explore subagent's own
investigation prompt —

> コードベースを調査してください(LP作成タスクに向けて)
> ("Please investigate the codebase, for an LP [landing page] creation task")

— scored **0.501** against `role-implementer`, pulled there by the object
("LP作成", the landing-page task) the *user's* original instruction mentioned
several turns earlier, while the correct `role-explorer` scored only
**0.331**. The Explore subagent's own system prompt — a definition of a
read-only exploration agent, sent by Claude Code's Task tool on every
subagent invocation — was never consulted, even though it unambiguously
names the role. User text, taken out of the conversation it belongs to, is
an unreliable signal for "what role is this particular sub-request playing";
an agent's own system prompt is not.

**Built: system-prompt classification, tried before the user-text walk.**
`system_prompt_text` (`src/server/proxy.rs`) extracts the system prompt per
`ApiKind` — Anthropic Messages' dedicated `system` field (flattened via
`translate::request::system_text`, already `pub(crate)` for
`agent::claude_cli`'s benefit), OpenAI Chat's leading `system`/`developer`
message (`translate::request::message_text`), or OpenAI Responses'
`instructions` field, falling back to a leading `system`/`developer` item in
`input` when `instructions` is empty (opencode sends it that way) — then
strips `<system-reminder>...</system-reminder>` blocks the same way
`classification_texts` does for user text, on the same reasoning (see the
2026-07-31 entry below). `classify_request` tries this extracted text,
if any, immediately after the `<transcript>` bypass and before the
newest-user-text walk; a match short-circuits the walk entirely —
`SemanticOutcome::MatchedSystem`, `routing.mode = "semantic_system"` — and
user text is never even embedded for that request.

**A stricter, separate threshold: `SYSTEM_CLASSIFICATION_THRESHOLD = 0.50`,**
not the ordinary `CLASSIFICATION_THRESHOLD = 0.45`. A genuine role
definition scores in the same range a same-language `description` match does
(0.55–0.79, per the 2026-07-31 language-matching entry below) — comfortably
clearing 0.50 — while a harness's own generic system-prompt preamble ("You
are Claude Code, an interactive CLI tool...") must not clear it: that text is
identical across essentially every request of a session regardless of what
the session is actually doing, so a false positive here would pin an entire
session to one role instead of just one mis-scored turn (the failure mode
the 2026-07-31 `<system-reminder>`-stripping entry already fixed once for
user text). `Classifier::classify` is now a thin wrapper over the new
`Classifier::classify_with_threshold(text, expected_api, threshold)`, which
`rank` (already threshold-parameterized and unit-tested independently of any
loaded embedding model) takes the caller's threshold directly rather than
the fixed constant.

**The 800-character / 64-token embedding bound (`Embedder::embed`) applies
here exactly as it does to user text — this is deliberate, not a side
effect to work around.** Only the *beginning* of a system prompt ever
reaches the classifier. This is why the feature targets subagent
definitions specifically: a Claude Code subagent's `.claude/agents/*.md`
prompt (or the Task tool's built-in agents, like Explore) puts the role
description first, so it survives truncation intact; a harness's own long
preamble does not reliably distinguish itself from another harness's within
the first 800 characters, which is a second, independent reason the
threshold has to be strict — truncation alone does not guarantee the
boilerplate stays below it.

**Trace log gained two fields for observing this, both intentionally
"always attempted" rather than "only on a match."** `TraceInput.system_text`
(`src/record/trace_log.rs`) records the system prompt's first 200 characters
whenever one is present, regardless of outcome — the same truncation
`last_user_text` already gets. `TraceRouting.system_score` records the
system prompt's top candidate score whenever classification was
*attempted*, even on a miss that fell through to the user-text walk — so a
trace file becomes tuning data for `SYSTEM_CLASSIFICATION_THRESHOLD` without
needing `serve --debug-full`, the same design intent `walk` already served
for the user-text threshold. Both are `#[serde(default)]` so old trace files
without them still deserialize. `llm-gateway trace`'s `format_line` shows
`decided_by_text` for `semantic_system` the same way it already did for
`semantic_history` — the winning text is never otherwise visible on the
line.

**Not tested end-to-end with a real embedding model, same limitation the
user-text walk already has.** No test anywhere in this codebase — not the
existing `classify_request` tests, not `tests/gateway_e2e.rs` — loads the
real `potion-multilingual-128M` model; `AppState.classifier` is `None` in
every one of them, and `rank`'s pure, model-independent design (see its own
doc comment) exists specifically so classification logic can be unit-tested
without it. Coverage here follows the same split: `system_prompt_text`'s
per-protocol extraction is fully unit-tested, `classify_with_threshold`'s
threshold enforcement is tested at the `rank` level, and `routing_from`'s
`semantic_system` trace shape is tested directly — but "does a real
subagent system prompt actually clear 0.50 against a real route
description" rests on the same reasoning already applied to the
`description` language-matching numbers below (0.55–0.79 measured
same-language), not a new automated check.

## 2026-08-01 — `Config::auto_mode`: Claude Code's internal auto-mode judgment gets its own, route-independent target

A prior fix (`classify_request`'s `<transcript>` bypass, `src/server/proxy.rs`)
already kept Claude Code's internal auto-mode permission classifier out of
embedding classification entirely — same idea as `x-gw-auto-route: 0`: skip
embedding classification, resolve by the client-sent model name if it
happens to match a route, else fall back to `default`. In the owner's own
environment that fallback landed on
`default`, and `default` pointed at `anthropic-subscription/sonnet` — a
`claude-cli` subprocess whose first response routinely takes several
seconds to over twenty, with hundreds to 1500+ output tokens. Claude Code's
own auto-mode judgment expects a fast yes/no answer and gave up with "Auto
mode could not evaluate this action" — confirmed against the owner's real
gateway, not a hypothetical.

The `<transcript>` bypass's own fallback chain could not fix this by
itself: `default` is deliberately a shared, ordinary route (any request
that fails classification lands there too), so pointing it at something
fast would just move the problem onto whatever else falls back to it. What
auto-mode's judgment calls need is a target chosen independently of
`routes` altogether — never a route name, never the client's requested
model string, only what the operator explicitly pins for this one purpose.

**`Config::auto_mode: Option<ModelConfig>`** (`src/config/mod.rs`) is that
target — the same `default` + `fallbacks` shape a route's `model` already
has, reusing `ModelConfig` rather than inventing a parallel type.
`route::resolve_model` (`src/route.rs`) is the resolution logic extracted
out of `route::resolve` to make this possible: it turns a bare `ModelConfig`
straight into `Vec<Target>` without a route-name lookup, so `resolve` (via a
route's `model`) and the `auto_mode` path (via `Config::auto_mode`) share
one implementation instead of two. `classify_request`
(`src/server/proxy.rs`) tries `auto_mode` first inside the `<transcript>`
branch; only when it is unset (or, defensively, fails to resolve — `validate`
should already prevent that) does the request fall through to the
pre-existing requested-model-then-`default` chain. `SemanticOutcome::UtilityBypass`
grew a third state (`UtilityBypassResolution::AutoModeConfig`, alongside the
existing `RequestedModel`/`DefaultFallback`) so the trace log's `routing.reason`
names all three distinctly instead of collapsing "pinned to a fast operator
target" into the same wording as "fell back to `default`". The gateway never
fabricates the auto-mode verdict itself here — it only decides which real LLM
answers the judgment; a real model still makes the call, just a fast one
instead of whatever `default` happens to be.

`config/validate.rs` checks `auto_mode.default`/`fallbacks` the same way a
route's `model` is checked (malformed string, undefined provider, wildcard
model), reusing `resolve_target` under the fixed label `"autoMode"` rather
than a route name. `llm-gateway init` gained one more wizard step, asked
once role assignment is done: whether to pin a fast dedicated model for
auto-mode (default answer: yes), offered from whatever providers were
already selected — `haiku` preferred over `sonnet` when a subscription's
alias list is shown, since the whole point is speed over strength.

## 2026-08-01 — Wildcard route names abolished

Route-*name* wildcards (`claude-*` → longest-prefix match, see the
2026-07-31 entry below) are gone. Model-string wildcards were already
rejected — this closes the other half.

**Not a bug fix — an owner policy decision.** A hand-written `claude-*`
route is an easy way to intercept far more traffic than intended: it wins
on the raw client-sent model string for any request routed via
`x-gw-auto-route: 0` or the `<transcript>` utility bypass (the only two
places that ever resolved a wildcard route name — everything else routes by
content classification, which never scored wildcard routes), and a slightly
too-broad prefix silently swallows requests it was never meant to catch.
Banning it in `docs/` or a code comment is not enforcement; the owner asked
for the mechanism itself to be gone so the risk cannot resurface by a future
hand-edit of `config.json`.

**`src/config/validate.rs` now hard-errors on any route name containing
`*`**, at both `config check` and `serve`/hot-reload startup — a config with
a wildcard route name never becomes live. With wildcard route names
impossible in a validated `Config`, the matching logic that made them work
was dead weight and is deleted, not just unreachable: `route::find_route`
(`src/route.rs`) is exact-match only now, and `Resolution`/`MatchKind`
shrank to drop the `Wildcard` variant and the `kind` field entirely (nothing
outside tests ever read it). `Config::listable_routes`
(`src/config/mod.rs`) and `RouteIndex::build` (`src/semantic/index.rs`) both
lose their now-permanently-no-op `!name.contains('*')` filters for the same
reason.

## 2026-08-01 — Cache observability before cache transfer

Record of where prompt caching stands today: a client's cache hints
(`cache_control` blocks, `prompt_cache_key`) only survive when the request
passes through as the same protocol it arrived in. Cross-protocol translation
(`anthropic → chat`, `responses → chat`) rebuilds the outbound request field
by field from an allowlist — the same design `AnthropicToChat` and
`ResponsesToChat` use for every other unrecognized field (see the entries
above and near the bottom of this file) — so cache hints are dropped
structurally, not by oversight. This isn't a total loss: OpenAI-family
providers run their own automatic prefix caching server-side regardless of
what the request carries. `transport: "claude-cli"` is the one path immune
to all of this — the CLI manages its own cache, and its usage numbers,
cache counts included, arrive from the model untouched (see the
`claude-cli` entry below).

**Decision: build observability first, not transfer.** The next lever here
would be a provider opt-in that forwards `cache_control` / `prompt_cache_key`
through translation instead of dropping them. That isn't being built yet —
`in_tok`/`out_tok`'s new siblings `cache_read_tok`/`cache_write_tok` are
landing in trace usage and `stats` first, so real cache-hit-rate data exists
before deciding whether a transfer opt-in is worth the added per-provider
surface. Deferred, not rejected — this entry is the marker to revisit once
that data exists.

## 2026-08-01 — Second translation direction: `openai-responses` in → `openai-chat` out, for Codex CLI 0.145+

Codex CLI 0.145.0 dropped `wire_api = "chat"` entirely — a config naming it
now refuses to start, where earlier versions accepted either value. Codex
therefore only ever speaks `/v1/responses` from here on. In any config whose
Codex-facing route resolves (directly or via fallback) to a provider that
only speaks `openai-chat` — every local Ollama, Groq, DeepSeek, Gemini,
Mistral, Together, Sakana AI, and PLaMo entry in `docs/providers.md` — every
Codex request became an unconditional `400`: there was no longer any
`wire_api` value that reached those providers at all. The gateway already
solved exactly this shape of problem for Claude Code (issue #3,
`Translation::AnthropicToChat`, see the entry near the bottom of this file);
Codex needed the same fix for its own protocol pair.

**Built: `Translation::ResponsesToChat` (`src/translate/`), using
`AnthropicToChat` as the template.** Same three-way split — `request.rs`
builds a fresh `openai-chat` request field by field (never patches the
Responses body in place, for the same reason: an unrecognized key like
`reasoning` or `store` makes strict `openai-chat` servers answer `400`),
`response.rs` handles the non-streaming reverse translation, `stream.rs`
handles the streaming one. `instructions` becomes a leading `system`
message, `input` (string or the typed item array — `message`,
`function_call`, `function_call_output`) becomes `messages` (`developer`
role folds into `system`, chat has no third role), `max_output_tokens`
becomes `max_tokens`, and flat `{"type":"function",…}` tool definitions
translate directly. The reverse direction rebuilds a Responses `response`
object (non-streaming) or a Responses SSE event sequence — `response.created`
→ `response.output_text.delta` / `response.function_call_arguments.delta` →
`response.completed`, each carrying its own `sequence_number` — because
Responses clients dispatch on named event types the way Anthropic ones do,
not on a single `delta` shape the way `openai-chat` does.

**Dropped, and why:** `reasoning` (Codex's own extended-thinking config; no
reachable `openai-chat` provider implements it), `include` /
`prompt_cache_key` / `client_metadata` (Responses-specific caching and
telemetry hints with nothing to land in on the target side), `store` /
`previous_response_id` (server-side conversation state `openai-chat` has no
concept of), `text` (Responses' own structured-output/verbosity config,
incompatible with `openai-chat`'s), and — the one most specific to Codex —
non-`function` tool entries: `local_shell`, `web_search`, and a `namespace`
grouping of several function tools. Those last three are Codex's own
extensions, executed either by Codex itself or by OpenAI's infrastructure;
no `openai-chat` provider this gateway reaches could run them regardless of
translation, so silently dropping them (rather than erroring) is the same
call `AnthropicToChat` already makes for Anthropic's server-side tools.

**Refactored while adding the second direction, rather than duplicating
`AnthropicToChat`'s call sites:** `translate::adapter`'s `Mode::Sse` used to
hold a concrete `ChatToAnthropic` field; it now holds `Box<dyn
StreamConverter>` (`src/translate/stream.rs`), and `Translation::
stream_converter` picks the concrete converter per direction. The gateway's
own mid-translation error envelope (an unreadable or oversized upstream
body — as opposed to a real upstream error, which `Translation::error`
already handled per-direction) was hardcoded to the Anthropic shape
(`anthropic_error` in `adapter.rs`); it is now `Translation::gateway_error`,
dispatching per direction like every other `Translation` method. Neither
change alters `AnthropicToChat`'s behaviour — both are the same code paths,
now selected on `self` instead of assumed.

**Verified against a real Codex CLI 0.145.0** run through
`llm-gateway launch codex exec`, wired to an `openai-chat` provider
(OpenRouter): an ordinary response, and a full tool-calling round trip —
`exec_command`'s `function_call` out, `function_call_output` back in on the
next turn, matched by `call_id`. Not just unit fixtures; this is the same
bar `AnthropicToChat`'s launch entry and the `codex-cli` transport entry
below were held to.

**Not built: any direction touching `anthropic-messages` ⇄ `openai-responses`
directly.** No client speaks both protocols, so there is nothing to
translate between them — the roadmap's issue #4 line now reads "no client
needs this" rather than "not attempted yet".

## 2026-08-01 — `description` accepts an array of language variants; each is embedded separately and scored by max cosine

The previous entry fixed same-language routing but assumed traffic is
single-language per session. It isn't: a route's requests come from two
sources at once — the human, writing in whatever language `init` asked
about, and sub-agents / harness-injected text (tool descriptions, `CLAUDE.md`
boilerplate, tool-result echoes), which is overwhelmingly English regardless
of what the human writes. A `description` written only in the human's
language routes the human's own turns correctly but gives that English-shaped
traffic nothing to match — the previous entry's measured 0.19–0.26
cross-lingual cosine sends it to `default` every time.

Writing both languages into one `description` string doesn't fix this either.
Measured cosine similarity for the same Japanese instruction against its
matching description, mean-pooled through the embedding model's 64-token
window, went from **0.550** (Japanese only) down to **0.433** (Japanese and
English concatenated in one string) — under the fixed 0.45 threshold.
Concatenating languages into a single embedding pulls both toward a centroid
that matches neither as well as either matched alone.

**Built: `routes.<name>.description` accepts a string or a string array.**
Each array entry is embedded independently; classification scores a route by
the **max cosine across all its variants**, not one embedding of concatenated
text. A single string remains valid and behaves exactly like a one-element
array. Each entry follows the existing inline-text-or-path rule (`./` `../`
`/` `~/` for a path).

**`llm-gateway init` now scaffolds two variants when the chosen language
isn't English: `[chosen language, English]`.** Every generated route's
`description`, including `default`'s, becomes a two-element array, so
sub-agent and harness-originated English traffic still lands on the right
route without diluting the human-language match the way one merged string
would.

**Rejected: translate the request into English before embedding it.** Same
objection as the previous entry's rejected option, restated for the array
case — it requires an LLM call in the routing path before the routing
decision itself, which defeats the entire point of static, sub-millisecond
`model2vec-rs` classification. That is a latency, cost, and new-failure-point
tradeoff on every request, paid to reach the same place per-variant max-cosine
matching reaches for close to zero added cost: one extra stored embedding per
variant, compared at classification time, no extra model calls.

This also resolves the previous entry's "future option, not built" note about
per-sentence scoring: it's the same max-cosine mechanism, but scoped to whole
language variants rather than sentence fragments, because the dominant
dilution source turned out to be cross-language mixing, not general
multi-topic length.

## 2026-08-01 — `description` must be written in the request's language; `init` adds a language-selection step

Stripping `<system-reminder>` blocks (previous entry) fixed the boilerplate
mis-routing, but it uncovered a second, larger problem: with the injected
English preamble gone, every Japanese-language session started falling
through to `default` on essentially every turn. The route descriptions were
all written in English, and the requests were in Japanese.

Measured cause: the shipped embedding model, `potion-multilingual-128M`,
aligns meaning only weakly across languages. Cosine similarity between a
Japanese instruction and its matching English `description` measured
**0.19–0.26** — nowhere near the fixed **0.45** threshold — while the same
instruction against a Japanese-language `description` measured **0.55–0.79**.
The model is multilingual in the sense that it has vocabulary coverage for
each language, not in the sense that it places semantically equivalent
sentences from different languages near each other in embedding space.

**Built: `description` is now a language-matching contract, not just a
content one.** `llm-gateway init` gained a language-selection step — English,
日本語, 中文, 한국어, or Español — asked once, before role selection. Every
route's `description` the wizard scaffolds, including `default`'s, is
generated in the chosen language. Docs now say explicitly: write
`description` in whatever language you actually give the model instructions
in, because the embedding comparison is same-language-only in practice.

**Rejected: translate the request to English before embedding it.** This
would fix the mismatch without asking users anything, but it requires an LLM
call in the routing path before the routing decision itself — the entire
design point of static `model2vec-rs` embeddings is a sub-millisecond,
model-call-free classification step. A translation call on every request (or
even just on ones that miss the threshold) reintroduces the latency and cost
the static-embedding approach exists to avoid, and adds a second point of
failure ahead of the provider call classification is supposed to select.

**Future option, not built:** embed `description` as multiple per-sentence
vectors instead of one whole-string vector, and score a route by the max
cosine across its sentences rather than one embedding of the concatenated
text. This would not fix cross-lingual alignment, but it addresses a related
dilution problem — a long, multi-topic `description` embedding gets pulled
toward its centroid and can under-match a request that clearly matches only
one sentence of it. Not attempted here because the language mismatch was the
dominant failure mode by a wide margin.

## 2026-07-31 — Classification input strips `<system-reminder>` blocks; trace gains `decided_by_text` / `walk`

History walk-back (below) made an existing problem much worse: Claude Code
injects a `<system-reminder>` block — project `CLAUDE.md` and other harness
boilerplate — into the first user message of every session. That block is
long, near-identical across sessions, and in the case that surfaced this it
scored a stable **0.519** against one route's `description`, comfortably over
the fixed `0.45` threshold. Before walk-back, that only mis-routed the first
turn. After walk-back, every ambiguous turn ("continue", a bare tool result)
that fell through to the walk eventually reached message one and re-picked
the same route off the boilerplate instead of any real instruction — an
entire implementation session ended up pinned to a route meant for cheap
exploratory chores, never once routed by what the user actually asked.

**Built: strip `<system-reminder>...</system-reminder>` blocks from every
candidate text before it is embedded**, in `classification_texts`
(`src/server/proxy.rs`) — both the newest-text check and everything the
history walk tries. A message left blank after stripping is treated as
textless and skipped exactly like an agentic `tool_result` turn. Only the
classification input changes: the payload forwarded to the provider is
untouched, so the harness still sees its own preamble and behaves normally.

**An unterminated block strips to the end of the text, not just to where a
closing tag would be.** Text containing `<system-reminder>` with no matching
close is more likely to be a truncated or malformed injection than user
content that happens to contain the literal string; leaving the tail in would
let boilerplate back into the corpus anyway. Dropping the whole remainder is
the safer failure mode.

**Trace gained `routing.decided_by_text` and `routing.walk`** for the same
reason this bug was hard to see in the first place: the existing trace line
recorded `matched_route` and a score, but explaining *why* a route won meant
reasoning backward from an exact score coincidence — this case was only
caught by noticing the same 0.519 recurring across unrelated requests.
`decided_by_text` records the first 200 characters of whichever text actually
won (present only on a match); `walk` lists every text the walk-back tried as
`{texts_back, top_score}` pairs. The answer now sits on the trace line itself
instead of requiring re-derivation from scores alone.

## 2026-07-31 — Cross-protocol fallbacks: reachability moves from config validation to per-target request-time filtering

`route.model.fallbacks` used to be rejected outright by `config check` unless
every fallback's provider shared `api` with `model.default`. That blocked an
ordinary setup: a free `openai-chat` model (an OpenRouter or local Ollama
model, say) as the default, with a subscription-backed `anthropic-messages`
provider (`transport: "claude-cli"`) as the fallback — so an outage or a burst
of 5xx falls back to something with no per-token cost, as long as its
subscription seat is free. The two `api` values never matched, so the config
was refused before the gateway ever ran.

**Built: `filter_reachable_targets` in `src/server/proxy.rs`, per attempt.**
The old rule rejected on the wrong axis. At config-load time the gateway does
not know which protocol a *future request's client* will speak, and the same
route serves whichever client's request gets classified into it — there is no
single "the" protocol relationship between `default` and `fallbacks` to
validate once and for all. So the fixed same-`api` requirement was dropped
from `src/config/validate.rs`, and reachability became a per-attempt,
request-time question instead: for each target — `default` first, then
`fallbacks` in order — a target whose `api` matches the client's is a
passthrough; a target whose `api` differs is kept only if a translation for
that `client → provider` direction exists (today, only
`anthropic-messages → openai-chat`); anything else is dropped before
`upstream::send_with_fallback` ever sees it. A route only answers `400` if
every one of its targets gets dropped this way.

Translation selection moved from once-per-route to once-per-attempt for the
same reason: it used to be derived from the route's first target and reused
for every subsequent one, which was only correct because every target was
guaranteed to share a protocol. `resolved.translation` in the trace log now
reflects whichever target actually answered, not the route's `default` — a
fallback that crossed protocols to get there looks different in the trace
than one that didn't, by design.

**No new translation direction shipped.** This is a validation and
target-selection change, not a translator change: `openai-chat` client →
`anthropic-messages` provider is still untranslated, so that target is still
dropped for such a client — just at request time instead of at `config
check` time, with the same end result.

**Rejected: keep `config check` rejecting the mismatch, just loosen which
`api` pairs it allows.** Any such rule still assumes a route's targets have
one "correct" protocol relationship that config alone can decide. They don't:
whether a given `default`/`fallback` pair is fine depends on which protocol
the request's client happens to speak, and nothing stops the same route from
serving both an Anthropic Messages client and an OpenAI Chat client over its
lifetime. Static config validation cannot express "valid for this caller,
invalid for that one" — only a per-request, per-target check can, which is
why the check moved rather than being merely relaxed.

## 2026-07-31 — Ambiguous turns keep their route via history walk-back, not a sticky cache

Classifying only the last user message left agentic conversations falling to
`default` on most turns, for two distinct reasons. First, an agentic client's
newest user message is usually a bare `tool_result` with no text block at all,
so `classification_text` returned `None` and the request fell back before the
classifier even ran — misrecorded as `no_classifier` in the trace, though the
classifier was fine. Second, a short human turn ("continue", "yes, do that")
scores below the threshold on its own even though the conversation's task has
not changed. Either way, one ambiguous turn dropped the rest of the
conversation onto whatever `default` costs.

**Rejected: a per-conversation sticky cache** (key = hash of the first user
message, value = last route that cleared the threshold, in-memory, TTL'd).
It re-creates state the request already carries: every request arrives with
its full message history, so "the route this conversation last confirmed" is
recomputable from the request alone. The cache also brought real failure
modes for nothing — the key dies on context compaction (Claude Code rewrites
the first message into a summary), common openers ("hi") alias unrelated
sessions onto each other, a config hot-reload can leave the cache pointing at
a route name that no longer resolves (a client-visible 404), and a restart,
TTL expiry, or second gateway instance silently loses stickiness. Worst,
routing decisions would stop being reproducible from the trace log alone.

**Built: history walk-back in `classify_request`** (`src/server/proxy.rs`).
`classification_texts` extracts every user message's text, newest first,
skipping textless ones (`tool_result` turns, image-only messages, blank
strings). The newest text is classified first — so a genuine topic change
still wins immediately — and on a below-threshold score the walk tries
earlier texts, taking the first that clears the bar, bounded by
`HISTORY_WALK_LIMIT` (8; each embed is sub-millisecond, so the bound is a
pathology cap, not a budget). Same request, same config ⇒ same route, on any
gateway process at any time, with nothing to invalidate on hot-reload.

The trace vocabulary grew to keep the decision explainable:
`routing.mode = "semantic_history"` when an earlier text matched (the
`reason` says how far back), and `"no_text"` when the request had nothing to
classify — previously misreported as `no_classifier`. `SemanticAttempt`'s
three booleans became one `SemanticOutcome` enum because five modes were
being packed into `classified`/`matched`/`manual` combinations that could
express impossible states.

Out of scope, deliberately: what a route switch *loses* mid-conversation
(provider-specific state like prompt caches and thinking blocks) is a
translate/adapter concern, not a routing one, and is unchanged here.

## 2026-07-31 — route/fallback outcomes get their own console lines, `logging.logging` stays opt-in

The previous entry below made `serve`'s console diagnostics opt-in behind
`logging.logging` (default `false`). Turning that default to `true` was
tried and reverted: other tooling/agents already driving `serve` should not
have their stderr suddenly grow noisier on an upgrade they didn't ask for.
The default stays `false` — quiet unless a user opts in via `config.json` or
`RUST_LOG`.

**Two new `info!` lines make the routing decision itself visible once
`logging.logging` is turned on, not just its failures.** `classify_request`
(`src/server/proxy.rs`) now logs the winning route and its score (or, on a
fallback, the closest score that still missed the threshold) right after
every classification, and `proxy` logs which provider/model actually served
the request once `send_with_fallback` returns — including how many attempts
it took when a fallback fired. Before this, only failed attempts and startup
events were logged at all; a successful first-try request produced no
console line whatsoever, even with `logging.logging: true` set.

## 2026-07-31 — `serve` gets a quiet-by-default console log behind `logging.logging`

`serve` always printed its `tracing::info!` diagnostics — embedding-model
preparation, and a `warn!` per failed fallback attempt — regardless of
whether anyone wanted to see them, because the console filter defaulted to
`"info"` unconditionally.

**Added `logging.logging` (`bool`, default `false`).** `false` raises the
console filter's default level to `warn`, so only real problems (a broken
config hot-reload, "this build can't classify at all") reach stderr; `true`
lowers it back to `info`, showing the routine diagnostics again. An explicit
`RUST_LOG` still overrides either default, for whoever wants finer control
than one flag. Per-attempt fallback logging in `upstream::send_with_fallback`
moved from `warn!` to `info!` to make it part of this toggle — it is a normal,
expected part of trying the next target, not something that needs an
operator's attention by itself.

**The startup banner (`listening on ...`, and the `--debug` trace-recording
notice) moved to `eprintln!`, unconditionally.** Those are the one-time
confirmation that `serve` actually started with the flags it was given, not a
diagnostic stream — they should not disappear just because
`logging.logging` is left at its default.

## 2026-07-31 — `init` routes are named by role, not by provider

Every route `init` scaffolded was named after the *provider* serving it
(`role-anthropic`, `role-github-copilot`, `role-openrouter`). That reads as
meaningless once more than one provider is configured: the name says nothing
about what a request needs to be doing to reach it, only which vendor answers
it — and it is the route's `description`, not its name, that the classifier
actually matches against.

**Routes are now named after a functional role in a multi-agent workflow —
`AgentRole` (`src/cli/init.rs`): manager, architect, explorer, web-researcher,
browser-operator, implementer, reviewer, tester.** `init` asks which roles to
configure, then which provider serves each one, producing `role-manager`,
`role-architect`, and so on, each with a `description` written for the kind of
task that role does rather than boilerplate about the provider. Nothing stops
two roles from resolving to the same provider and model.

**Model selection now fetches the provider's own model list over its API
(`GET /models`, or `GET /v1/models` for Anthropic) and offers it as a
single-choice prompt, instead of always asking for free-form text.** A free
model id is easy to mistype and easy to leave stale as a provider retires
models. The wizard resolves a usable credential first (a typed key, a
discovered `gh auth token`, or the environment variable the config would
reference) and falls back to the previous pre-filled text prompt only when it
cannot reach the endpoint or parse its response — this is a nicety, not
something `init` should ever block on.

## 2026-07-31 — `serve` offers to free its port instead of failing cold

Restarting after an upgrade routinely hit `Address already in use` because the
previous `llm-gateway serve` was still running — and the message arrived only
*after* a multi-second classification model load, with no next step spelled
out beyond "go find and kill it yourself."

**The bind moves to the very top of `serve`, before the recorder or classifier
are touched, and a conflict now asks instead of just failing.**
`bind_or_offer_to_free_port` (`src/server/mod.rs`) tries the bind first; on
`AddrInUse` it shells out to `lsof -iTCP:<port> -sTCP:LISTEN -t` to find the
exact PID(s) holding that port, and uses `cliclack::confirm` — the same
prompt style `init` already uses — to ask before killing anything. `Yes`
sends each PID a plain `kill` (SIGTERM, so another `llm-gateway serve` shuts
its watcher and recorder down cleanly) and retries the bind for up to 3s to
give the kernel time to actually release the socket; `No`, or a still-taken
port after killing, aborts with an explanation rather than starting a second
instance or looping forever. A non-interactive run (no terminal — e.g. a
supervisor) gets an I/O error from `cliclack` immediately, which lands on the
same "don't start, don't kill anything" outcome as answering `No` — refusing
to guess is safer than guessing yes on someone's unattended process.

**Scoped to the exact port, not "any `llm-gateway` process."** A blind
`pkill llm-gateway` could take out an unrelated instance serving a different
port on purpose; asking `lsof` about this one port and killing only what it
reports keeps the blast radius to what is actually in the way.

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
