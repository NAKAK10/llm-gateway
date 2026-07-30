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
