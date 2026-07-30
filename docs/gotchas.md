# Gotchas

[English](gotchas.md) | [日本語](ja/gotchas.md)

Traps we already know about. If you hit one that isn't here, add it.

## Streaming / proxying

- **Never copy `content-length`, `transfer-encoding` or `content-encoding`
  from the upstream response.** axum re-frames the body; a stale value makes
  clients hang or try to inflate plain text. (`server/passthrough.rs` strips
  them.)
- **Never enable reqwest's gzip/brotli features.** A decoder between us and
  the wire re-buffers chunks — SSE token streaming turns into long stalls. We
  send `accept-encoding: identity` instead.
- **Timeout on first byte, not the whole request.** `reqwest::timeout()` kills
  long, healthy generations mid-stream. `connect_timeout` + a header deadline
  (`FIRST_BYTE_TIMEOUT`) is the correct pair.
- **Fallback cannot happen after the stream starts.** The 200 and first chunk
  are already sent. This is physics, not a missing feature; document it in any
  user-facing description of fallback.
- **A client disconnect still costs tokens upstream.** Record usage from the
  stream's `Drop`, never from the handler's happy path (`usage/tee.rs`).
- **`stream_options.include_usage` must be injected** for streamed OpenAI-chat
  requests, or usage is silently zero forever — which looks exactly like
  "works fine". Providers that choke on it: set `injectUsage: false`.

## launch / clients

- **Claude Code: `settings.json` `env` beats the shell environment.** If the
  user ever adds `ANTHROPIC_BASE_URL` there, `launch claude` silently stops
  redirecting. We detect and warn (`launch/claude.rs::detect_conflicts`);
  `--isolate` (`--setting-sources project`) is the hammer, not the default.
- **Codex: no env var redirects its upstream.** `OPENAI_BASE_URL` does not
  exist. Only `-c model_providers.…` works, values parsed as TOML → strings
  need embedded double quotes.
- **Codex: `--ignore-user-config` exists only on `codex exec`.** TUI runs
  cannot be isolated. Asymmetry is warned about, not hidden.
- **Codex: `disable_response_storage=true` always.** OpenRouter's `/v1/responses`
  is stateless and 400s on a non-null `previous_response_id` — without this,
  an OpenRouter fallback dies on turn 2 of every conversation.
- **opencode: `models` keys must equal `GET /v1/models` ids exactly.** On
  mismatch it shows nothing and says nothing. `launch opencode` verifies
  against the live gateway before starting the child.
- **opencode: `OPENCODE_CONFIG` loses to project configs.** Only
  `OPENCODE_CONFIG_CONTENT` reliably wins, and `{env:VAR}`/`{file:…}` are NOT
  expanded inside it (anomalyco/opencode#13219) — the key is embedded literally,
  which is why that env var is in the redaction list.
- **OpenClaw: provider `models` allowlist.** A route name missing from the
  allowlist simply doesn't exist as far as OpenClaw is concerned; the error
  says nothing useful. Update the allowlist when adding routes.
- **OpenClaw: double fallback.** OpenClaw has its own model fallback chain.
  Keep model-level fallback in the gateway; leave OpenClaw's `fallbacks` as a
  single "gateway is down → old direct route" escape hatch, or every failure
  retries twice everywhere.
- **OpenClaw: cron runs have no shell environment.** `${VAR}` key references
  resolve in your terminal and 401 at 09:01. Put keys in the daemon's own
  startup environment.
- **`localhost` may resolve to `::1`.** Every config and doc uses `127.0.0.1`.

## Cross-protocol translation

Only one direction exists: `anthropic-messages` in → `openai-chat` out
(`src/translate/`). It runs *only* when the client's protocol and the provider's
differ; same-protocol traffic never enters this code path.

- **The byte-for-byte guarantee does not hold on a translated route.** Say so in
  any user-facing description, and check `llm-gateway trace` for
  `xlat=anthropic-messages->openai-chat` before debugging a "weird output" report.
- **Silently dropped, because the target protocol has nowhere to put them:**
  prompt caching (`cache_control`, and `cache_creation_input_tokens` is always
  0), `thinking` blocks and the `thinking` request config, citations,
  `document`/`search_result` content blocks, `top_k`, and Anthropic server-side
  tools (`web_search_*`, `bash_*`, `text_editor_*` — they run inside
  Anthropic's infrastructure, so no other provider could execute them anyway).
- **`reasoning_content` / `reasoning` deltas are dropped, not converted.** A
  real Anthropic `thinking` block carries a `signature` only Anthropic can
  produce; forwarding the reasoning as ordinary text would present it as the
  answer. A thinking-heavy local model therefore looks silent until it starts
  answering — and with a small `max_tokens` it can return **no text at all**
  (measured: `qwen3.5:4b` with `max_tokens: 64` spent all 64 tokens on
  `reasoning` and returned an empty `content`). That is the upstream's answer,
  not a gateway bug: check the provider directly before hunting for one.
- **`finish_reason: "stop"` alongside `tool_calls` is common** (Ollama, several
  OpenAI-compatible servers). The Anthropic side must report
  `stop_reason: "tool_use"` anyway, or the client never executes the tool call
  it was just handed. Same rule in the streaming and non-streaming translator.
- **`function.arguments` is a JSON *string* on the OpenAI side and an object on
  the Anthropic side.** In a stream the fragments must be forwarded verbatim as
  `input_json_delta.partial_json` — a fragment is not valid JSON on its own, and
  re-serialising it corrupts the call.
- **Ollama omits `index` and `id` on streamed tool calls.** Keying blocks by
  `index` alone merges two calls into one; ids have to be synthesized.
- **The terminal events must be emitted on every path.** `[DONE]`, a
  `finish_reason`, a mid-stream `{"error":…}` frame, or an upstream that just
  stops — the client must always get `content_block_stop` + `message_delta` +
  `message_stop`, or it waits forever.
- **`/v1/messages/count_tokens` cannot be forwarded** (`openai-chat` has no such
  endpoint) and is answered with a local estimate. It is an estimate, not a
  count: `result: "estimated_locally"` in the trace log is how you tell.
- **Usage accounting is unaffected** — `usage/tee.rs` observes the upstream
  bytes below the translation layer. If you ever move that observer above it,
  every translated request starts reporting the *translator's* numbers.

## Config / security

- `config.json` can hold literal keys → `0600` on create, warned on drift,
  masked in `config show` and `launch --print`, `config gitignore` template.
- Non-loopback bind without `server.apiKey` is refused at startup: one key
  guards every provider credential behind the port.
- `server.host`/`server.port`/`server.apiKey` do **not** hot-reload (the
  listener and its identity are fixed at bind time). Everything else does.
- A failed reload keeps the old config live — by design. Check stderr before
  assuming your edit applied; the log line tells you what changed.
- Route names must not contain `:` or `/` (breaks opencode model keys, Codex
  TOML keys and URL paths). Model *values* may contain both — parsing splits
  on the first `/` only, so `openrouter/anthropic/claude-…` and
  `ollama-cloud/glm-5.2:cloud` both work.
- `--debug` writes prompt text (truncated at 200 chars; `--debug-full` doesn't
  truncate). Business conversations end up in `logs/` in the clear.

## Rust specifics

- axum 0.7+ removed `StreamBody`; `Body::from_stream` is the way.
- `notify` on macOS (FSEvents) mishandles files you don't own → the watcher
  watches the parent directory and filters by file name, with a 300 ms
  debounce for editors' atomic saves.
- `dirs`/`directories` return `~/Library/Application Support` on macOS and
  won't honour XDG. `etcetera::choose_base_strategy()` is why config lands in
  `~/.config`.
