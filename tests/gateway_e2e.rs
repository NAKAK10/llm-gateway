//! End-to-end tests over real TCP: a mock upstream, the real router, real
//! streams. These are the tests that earn the claims in the README —
//! byte-identical passthrough, pre-first-byte fallback, and auth.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;

use llm_gateway::config::watch::SharedConfig;
use llm_gateway::config::{ApiKind, Config, ModelConfig, ProviderConfig, RouteConfig, SecretRef};
use llm_gateway::record::{RecordMode, Recorder};
use llm_gateway::server::live::LiveFeed;
use llm_gateway::server::{router, AppState};

/// A fixed SSE body with awkward blank lines and a multi-event layout —
/// anything the gateway re-framed would show up as a diff.
const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"日本語テスト\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\ndata: [DONE]\n\n";

/// What an `openai-chat` provider actually emits, for the cross-protocol
/// translation tests: flat deltas, a usage-only chunk, and `[DONE]` — none of
/// which an Anthropic client understands.
const CHAT_SSE_BODY: &str = concat!(
    r#"data: {"id":"chatcmpl-e2e","model":"qwen3.5","choices":[{"index":0,"delta":{"role":"assistant","content":"日本語"}}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-e2e","choices":[{"index":0,"delta":{"content":"テスト"},"finish_reason":"stop"}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-e2e","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// Same shape as `CHAT_SSE_BODY`, but with no usage chunk at all — what an
/// `openai-chat` upstream that never got `stream_options.include_usage`
/// sends, matching a provider configured with `injectUsage: false` (#22).
const CHAT_SSE_BODY_NO_USAGE: &str = concat!(
    r#"data: {"id":"chatcmpl-e2e","model":"qwen3.5","choices":[{"index":0,"delta":{"role":"assistant","content":"日本語"}}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-e2e","choices":[{"index":0,"delta":{"content":"テスト"},"finish_reason":"stop"}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone, Default)]
struct MockState {
    /// Bodies the mock upstream received, so tests can assert on the rewrite.
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn mock_chat(State(state): State<MockState>, body: axum::body::Bytes) -> Response {
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    state.requests.lock().unwrap().push(parsed);

    let mut response = Response::new(Body::from(SSE_BODY));
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    // A header the gateway must strip (the body is not actually chunked by us)
    // and one it must forward.
    response
        .headers_mut()
        .insert("x-mock-upstream", http::HeaderValue::from_static("yes"));
    response
}

async fn spawn_mock() -> (SocketAddr, MockState) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .route("/v1/models", get(|| async { "{\"data\":[]}" }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

/// A plain `anthropic-messages` upstream — no translation involved, so the
/// cross-protocol fallback test can assert this response reaches the client
/// byte-for-byte in its own protocol.
async fn mock_anthropic_messages(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    state.requests.lock().unwrap().push(parsed);

    let body = serde_json::json!({
        "id": "msg_e2e",
        "type": "message",
        "role": "assistant",
        "model": "haiku-mock",
        "content": [{"type": "text", "text": "pong from haiku"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3},
    });
    let mut response = Response::new(Body::from(body.to_string()));
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

async fn spawn_anthropic_mock() -> (SocketAddr, MockState) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/messages", post(mock_anthropic_messages))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

/// How the `openai-chat` mock answers, so one handler covers the streaming,
/// non-streaming and error shapes a translated route has to deal with.
#[derive(Clone, Copy)]
enum ChatMockMode {
    Sse,
    /// Streams with no usage chunk at all — an upstream that never got (or
    /// never honors) `stream_options.include_usage` (#22).
    SseNoUsage,
    Json,
    RateLimited,
}

#[derive(Clone)]
struct ChatMockState {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    mode: ChatMockMode,
}

async fn mock_openai_chat(State(state): State<ChatMockState>, body: axum::body::Bytes) -> Response {
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    state.requests.lock().unwrap().push(parsed);

    match state.mode {
        ChatMockMode::Sse => {
            let mut response = Response::new(Body::from(CHAT_SSE_BODY));
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/event-stream"),
            );
            response
        }
        ChatMockMode::SseNoUsage => {
            let mut response = Response::new(Body::from(CHAT_SSE_BODY_NO_USAGE));
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/event-stream"),
            );
            response
        }
        ChatMockMode::Json => {
            let body = serde_json::json!({
                "id": "chatcmpl-e2e",
                "model": "qwen3.5",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "こんにちは"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 11, "completion_tokens": 4},
            });
            let mut response = Response::new(Body::from(body.to_string()));
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            response
        }
        ChatMockMode::RateLimited => {
            let body = serde_json::json!({
                "error": {"message": "slow down", "type": "rate_limit_exceeded"},
            });
            let mut response = Response::new(Body::from(body.to_string()));
            *response.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
            response
        }
    }
}

async fn spawn_chat_mock(mode: ChatMockMode) -> (SocketAddr, ChatMockState) {
    let state = ChatMockState {
        requests: Arc::new(Mutex::new(Vec::new())),
        mode,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_openai_chat))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

/// A gateway whose only route is backed by an `openai-chat` provider, reached
/// over `/v1/messages` — i.e. the `launch claude` → Ollama shape.
fn translated_config(upstream: SocketAddr) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "chat-mock".into(),
        provider(&format!("http://{upstream}/v1"), ApiKind::OpenaiChat),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("chat-mock/qwen3.5", &[]),
    );
    config
}

fn provider(base: &str, api: ApiKind) -> ProviderConfig {
    ProviderConfig {
        base_url: base.to_string(),
        api,
        api_key: Some(SecretRef::new("mock-key")),
        headers: Default::default(),
        inject_usage: true,
        transport: Default::default(),
        agent_args: Vec::new(),
        timeout_seconds: None,
    }
}

/// Same as [`provider`], but with `injectUsage: false` — for #22's
/// "no usage expected" configuration.
fn provider_without_inject_usage(base: &str, api: ApiKind) -> ProviderConfig {
    ProviderConfig {
        inject_usage: false,
        ..provider(base, api)
    }
}

fn route_to(default: &str, fallbacks: &[&str]) -> RouteConfig {
    RouteConfig {
        model: ModelConfig {
            default: default.to_string(),
            fallbacks: fallbacks.iter().map(|s| s.to_string()).collect(),
        },
        ..Default::default()
    }
}

async fn spawn_gateway(config: Config, inbound_key: Option<&str>) -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Recorder::start(
        dir.path().to_path_buf(),
        RecordMode {
            usage: false,
            debug: false,
            debug_full: false,
        },
    )
    .unwrap();
    let state = AppState {
        config: SharedConfig::from_config(config, dir.path().join("config.json")),
        http: reqwest::Client::new(),
        recorder,
        inbound_key: inbound_key.map(String::from),
        #[cfg(feature = "semantic")]
        classifier: None,
        #[cfg(feature = "semantic")]
        basis_cache: std::sync::Arc::new(llm_gateway::server::ui::pca::BasisCache::new()),
        live: None,
        ui_token: None,
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // The tempdir must outlive the server; leak it for the test's lifetime.
    std::mem::forget(dir);
    addr
}

/// Same as [`spawn_gateway`], but with `usage-*.jsonl` recording on and the
/// logs directory returned, so a test can read back what `usage_missing`
/// ended up as (#22).
async fn spawn_gateway_recording_usage(config: Config) -> (SocketAddr, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let logs_dir = dir.path().to_path_buf();
    let recorder = Recorder::start(
        logs_dir.clone(),
        RecordMode {
            usage: true,
            debug: false,
            debug_full: false,
        },
    )
    .unwrap();
    let state = AppState {
        config: SharedConfig::from_config(config, dir.path().join("config.json")),
        http: reqwest::Client::new(),
        recorder,
        inbound_key: None,
        #[cfg(feature = "semantic")]
        classifier: None,
        #[cfg(feature = "semantic")]
        basis_cache: std::sync::Arc::new(llm_gateway::server::ui::pca::BasisCache::new()),
        live: None,
        ui_token: None,
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // The tempdir must outlive the server; leak it for the test's lifetime.
    std::mem::forget(dir);
    (addr, logs_dir)
}

/// Same as [`spawn_gateway`], but with `serve --ui`'s dashboard on — for
/// exercising `/ui`/`/api/*` and the live feed. Returns the [`LiveFeed`]
/// directly (not just the address) so a test can subscribe to it without a
/// real SSE client, plus the dashboard token `serve --ui` would have printed
/// at startup — a test that wants past `ui_guard` needs it to bootstrap a
/// session cookie via `/ui?token=…`, the same way a browser does.
async fn spawn_gateway_with_live(
    config: Config,
    inbound_key: Option<&str>,
) -> (SocketAddr, Arc<LiveFeed>, String) {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Recorder::start(
        dir.path().to_path_buf(),
        RecordMode {
            usage: false,
            debug: false,
            debug_full: false,
        },
    )
    .unwrap();
    let live = Arc::new(LiveFeed::new());
    let ui_token = uuid::Uuid::now_v7().to_string();
    let state = AppState {
        config: SharedConfig::from_config(config, dir.path().join("config.json")),
        http: reqwest::Client::new(),
        recorder,
        inbound_key: inbound_key.map(String::from),
        #[cfg(feature = "semantic")]
        classifier: None,
        #[cfg(feature = "semantic")]
        basis_cache: std::sync::Arc::new(llm_gateway::server::ui::pca::BasisCache::new()),
        live: Some(live.clone()),
        ui_token: Some(ui_token.clone()),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::mem::forget(dir);
    (addr, live, ui_token)
}

#[tokio::test]
async fn streamed_response_reaches_the_client_byte_for_byte() {
    let (upstream, mock) = spawn_mock().await;

    let mut config = Config::default();
    config.providers.insert(
        "mock".into(),
        provider(&format!("http://{upstream}/v1"), ApiKind::OpenaiChat),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("mock/real-model", &[]),
    );

    let addr = spawn_gateway(config, None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("x-gw-client", "e2e")
        .json(&serde_json::json!({
            "model": "default",
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    // Forwarded provider metadata survives; framing headers do not.
    assert_eq!(response.headers().get("x-mock-upstream").unwrap(), "yes");
    assert!(response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));

    let body = response.text().await.unwrap();
    assert_eq!(body, SSE_BODY, "response body must be byte-identical");

    // The request that reached the upstream had its model rewritten and
    // stream_options injected — and nothing else about it invented.
    let seen = mock.requests.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["model"], "real-model");
    assert_eq!(seen[0]["stream_options"]["include_usage"], true);
    assert_eq!(seen[0]["messages"][0]["content"], "ping");
}

#[tokio::test]
async fn fallback_reaches_the_second_target_when_the_first_is_dead() {
    let (upstream, mock) = spawn_mock().await;

    let mut config = Config::default();
    // Port 9 (discard) refuses connections — a permanently dead upstream.
    config.providers.insert(
        "dead".into(),
        provider("http://127.0.0.1:9/v1", ApiKind::OpenaiChat),
    );
    config.providers.insert(
        "mock".into(),
        provider(&format!("http://{upstream}/v1"), ApiKind::OpenaiChat),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("dead/primary-model", &["mock/backup-model"]),
    );

    let addr = spawn_gateway(config, None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "default",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let seen = mock.requests.lock().unwrap();
    assert_eq!(seen.len(), 1, "fallback target must have been called once");
    assert_eq!(seen[0]["model"], "backup-model");
}

/// The point of cross-protocol fallback: a route's default and fallback no
/// longer have to speak the same protocol. Here the client is Claude Code
/// (`anthropic-messages`), the dead default is `openai-chat`, and the
/// fallback that actually answers is `anthropic-messages` — its response
/// must reach the client byte-for-byte, with no translation applied, exactly
/// like any other same-protocol passthrough.
#[tokio::test]
async fn cross_protocol_fallback_reaches_an_anthropic_fallback_untranslated() {
    let (anthropic_upstream, anthropic_mock) = spawn_anthropic_mock().await;

    let mut config = Config::default();
    // Port 9 (discard) refuses connections — a permanently dead upstream.
    config.providers.insert(
        "dead-chat".into(),
        provider("http://127.0.0.1:9/v1", ApiKind::OpenaiChat),
    );
    config.providers.insert(
        "haiku".into(),
        provider(
            &format!("http://{anthropic_upstream}"),
            ApiKind::AnthropicMessages,
        ),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("dead-chat/primary-model", &["haiku/haiku-mock"]),
    );

    let addr = spawn_gateway(config, None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({
            "model": "default",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    // Untranslated: the fallback's own Anthropic-shaped body, verbatim.
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "pong from haiku");

    let seen = anthropic_mock.requests.lock().unwrap();
    assert_eq!(
        seen.len(),
        1,
        "the anthropic-messages fallback must have been called once"
    );
    assert_eq!(seen[0]["model"], "haiku-mock");
}

#[tokio::test]
async fn unknown_model_is_a_404_with_a_hint() {
    let addr = spawn_gateway(Config::default(), None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({"model": "nope", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("/v1/models"));
}

#[tokio::test]
async fn models_endpoint_lists_every_route() {
    let mut config = Config::default();
    config.providers.insert(
        "mock".into(),
        provider("http://127.0.0.1:9/v1", ApiKind::OpenaiChat),
    );
    config
        .routes
        .insert("role-a".into(), route_to("mock/m", &[]));

    let addr = spawn_gateway(config, None).await;
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["role-a"]);
}

#[tokio::test]
async fn inbound_key_gates_every_v1_route_but_not_health() {
    let addr = spawn_gateway(Config::default(), Some("gw-secret")).await;
    let client = reqwest::Client::new();

    let denied = client
        .get(format!("http://{addr}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);

    let bearer = client
        .get(format!("http://{addr}/v1/models"))
        .bearer_auth("gw-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), 200);

    // Claude Code may authenticate with x-api-key instead.
    let api_key = client
        .get(format!("http://{addr}/v1/models"))
        .header("x-api-key", "gw-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(api_key.status(), 200);

    let health = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
}

#[tokio::test]
async fn protocol_mismatch_is_a_400_not_a_confusing_upstream_error() {
    let mut config = Config::default();
    config.providers.insert(
        "mock".into(),
        provider("http://127.0.0.1:9/v1", ApiKind::AnthropicMessages),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("mock/m", &[]),
    );

    let addr = spawn_gateway(config, None).await;
    // Calling the *Chat* endpoint with an anthropic-only-backed route must be
    // refused up front: unlike `openai-responses`, `openai-chat` has no
    // translation to `anthropic-messages`.
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({"model": "default", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("anthropic-messages"));
}

/// The point of issue #3: Claude Code only ever speaks `/v1/messages`, and
/// every Ollama/Groq/DeepSeek-class provider only speaks `openai-chat`. This is
/// that pair, end to end over real TCP.
#[tokio::test]
async fn an_anthropic_client_streams_from_an_openai_chat_provider() {
    let (upstream, mock) = spawn_chat_mock(ChatMockMode::Sse).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header("x-gw-client", "claude-code")
        .json(&serde_json::json!({
            "model": "default",
            "max_tokens": 1024,
            "stream": true,
            "system": [{"type": "text", "text": "You are terse.", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "ping"}]}],
            "tools": [{"name": "read_file", "description": "read a file", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
            "top_k": 5,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();

    // What the client sees must be a well-formed Anthropic event sequence.
    for event in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: content_block_stop",
        "event: message_delta",
        "event: message_stop",
    ] {
        assert!(body.contains(event), "missing {event} in:\n{body}");
    }
    assert!(body.contains("日本語"), "{body}");
    assert!(body.contains("テスト"), "{body}");
    // `[DONE]` is not part of the Anthropic protocol.
    assert!(!body.contains("[DONE]"), "{body}");
    // The final usage must reach the client, restated Anthropic-style.
    assert!(body.contains("\"input_tokens\":11"), "{body}");
    assert!(body.contains("\"output_tokens\":4"), "{body}");

    // And what reached the upstream must be a plain `openai-chat` request.
    let seen = mock.requests.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0];
    assert_eq!(sent["model"], "qwen3.5");
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "You are terse.");
    assert_eq!(sent["messages"][1]["role"], "user");
    assert_eq!(sent["messages"][1]["content"], "ping");
    assert_eq!(sent["max_tokens"], 1024);
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
    assert_eq!(sent["tool_choice"], "auto");
    // Usage injection still applies — it is what makes accounting work here.
    assert_eq!(sent["stream_options"]["include_usage"], true);
    // Anthropic-only keys must not leak upstream: strict servers 400 on them.
    assert!(sent.get("system").is_none(), "{sent}");
    assert!(sent.get("top_k").is_none(), "{sent}");
}

#[tokio::test]
async fn a_non_streaming_translated_response_arrives_as_an_anthropic_message() {
    let (upstream, _mock) = spawn_chat_mock(ChatMockMode::Json).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({
            "model": "default",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "こんにちは");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 11);
    assert_eq!(body["usage"]["output_tokens"], 4);
    // The upstream's own model name is the honest answer.
    assert_eq!(body["model"], "qwen3.5");
}

#[tokio::test]
async fn an_upstream_error_reaches_an_anthropic_client_in_its_own_envelope() {
    let (upstream, _mock) = spawn_chat_mock(ChatMockMode::RateLimited).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({
            "model": "default",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    // Claude Code branches on this type to decide whether to back off.
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["message"], "slow down");
}

/// `openai-chat` has no token-counting endpoint, so the gateway answers this
/// one itself rather than failing — Claude Code sizes its context window from
/// the number and degrades badly without it.
#[tokio::test]
async fn count_tokens_on_a_translated_route_is_answered_locally() {
    let (upstream, mock) = spawn_chat_mock(ChatMockMode::Json).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages/count_tokens"))
        .json(&serde_json::json!({
            "model": "default",
            "messages": [{"role": "user", "content": "count these tokens please"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["input_tokens"].as_u64().unwrap() > 0, "{body}");
    assert!(
        mock.requests.lock().unwrap().is_empty(),
        "the provider must never see a count_tokens request it cannot answer"
    );
}

/// The point of `Translation::ResponsesToChat`: Codex CLI only ever speaks
/// `/v1/responses`, and the same `openai-chat`-only providers `launch claude`
/// already reaches need to be reachable from it too. This is that pair, end
/// to end over real TCP, streaming.
#[tokio::test]
async fn a_responses_client_streams_from_an_openai_chat_provider() {
    let (upstream, mock) = spawn_chat_mock(ChatMockMode::Sse).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/responses"))
        .header("x-gw-client", "codex")
        .json(&serde_json::json!({
            "model": "default",
            "stream": true,
            "instructions": "You are terse.",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "ping"}],
            }],
            "tools": [{
                "type": "function",
                "name": "read_file",
                "description": "read a file",
                "parameters": {"type": "object"},
            }],
            "tool_choice": "auto",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();

    // What the client sees must be a well-formed Responses event sequence.
    for event in [
        "event: response.created",
        "event: response.output_item.added",
        "event: response.content_part.added",
        "event: response.output_text.delta",
        "event: response.content_part.done",
        "event: response.completed",
    ] {
        assert!(body.contains(event), "missing {event} in:\n{body}");
    }
    assert!(body.contains("日本語"), "{body}");
    assert!(body.contains("テスト"), "{body}");
    // `[DONE]` is not part of the Responses protocol either.
    assert!(!body.contains("[DONE]"), "{body}");
    // The final usage must reach the client, restated Responses-style.
    assert!(body.contains("\"input_tokens\":11"), "{body}");
    assert!(body.contains("\"output_tokens\":4"), "{body}");

    // And what reached the upstream must be a plain `openai-chat` request.
    let seen = mock.requests.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0];
    assert_eq!(sent["model"], "qwen3.5");
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "You are terse.");
    assert_eq!(sent["messages"][1]["role"], "user");
    assert_eq!(sent["messages"][1]["content"], "ping");
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
    assert_eq!(sent["tool_choice"], "auto");
    // Usage injection still applies.
    assert_eq!(sent["stream_options"]["include_usage"], true);
    // Responses-only keys must not leak upstream.
    assert!(sent.get("input").is_none(), "{sent}");
    assert!(sent.get("instructions").is_none(), "{sent}");
}

#[tokio::test]
async fn a_non_streaming_responses_client_gets_an_output_text_response() {
    let (upstream, _mock) = spawn_chat_mock(ChatMockMode::Json).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "default",
            "input": "ping",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["type"], "message");
    assert_eq!(body["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(body["output"][0]["content"][0]["text"], "こんにちは");
    assert_eq!(body["usage"]["input_tokens"], 11);
    assert_eq!(body["usage"]["output_tokens"], 4);
    // The upstream's own model name is the honest answer.
    assert_eq!(body["model"], "qwen3.5");
}

#[tokio::test]
async fn an_upstream_error_reaches_a_responses_client_in_an_openai_envelope() {
    let (upstream, _mock) = spawn_chat_mock(ChatMockMode::RateLimited).await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "default",
            "stream": true,
            "input": "ping",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "slow down");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    // No top-level `type` field — that is an Anthropic envelope detail, and
    // this client speaks OpenAI's own error shape instead.
    assert!(body.get("type").is_none());
}

/// The bug `Config::auto_mode` fixes, end to end: Claude Code's internal
/// `<transcript>`-prefixed auto-mode judgment request must reach the
/// operator-pinned fast target directly — never the reserved `default`
/// route, which in a real deployment can be a multi-second subprocess
/// target that starves this fast yes/no judgment (see the 2026-08-01 entry
/// in `docs/decisions.md`). `route::resolve_model` is exercised here through
/// the real `proxy()` handler, not just in isolation — this is what proves
/// the second, route-name lookup that `route::resolve` would otherwise do is
/// actually skipped: the requested model name below matches no configured
/// route, so a fallback to `default` would hit `slow`, and only the direct
/// `auto_mode` path can reach `fast` at all.
///
/// `semantic`-only: a `--no-default-features` build never inspects the
/// request's text at all (`classify_request`'s non-`semantic` variant always
/// falls back to `default` unconditionally, same as before `auto_mode`
/// existed), so there is no `<transcript>` bypass to exercise there.
#[cfg(feature = "semantic")]
#[tokio::test]
async fn transcript_prefixed_request_bypasses_default_and_reaches_the_configured_auto_mode_target()
{
    let (slow_upstream, slow_mock) = spawn_mock().await;
    let (fast_upstream, fast_mock) = spawn_mock().await;

    let mut config = Config::default();
    config.providers.insert(
        "slow".into(),
        provider(&format!("http://{slow_upstream}/v1"), ApiKind::OpenaiChat),
    );
    config.providers.insert(
        "fast".into(),
        provider(&format!("http://{fast_upstream}/v1"), ApiKind::OpenaiChat),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("slow/real-model", &[]),
    );
    config.auto_mode = Some(ModelConfig {
        default: "fast/haiku-mock".to_string(),
        fallbacks: Vec::new(),
    });

    let addr = spawn_gateway(config, None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            // Matches no configured route — without `auto_mode`, this would
            // fall back to `default` (`slow`). With it, the requested model
            // name is never even looked up.
            "model": "claude-opus-4-not-a-configured-route",
            "messages": [
                {"role": "user", "content": "<transcript>\nsome tool call history\n</transcript>\nis this safe?"},
            ],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let fast_seen = fast_mock.requests.lock().unwrap();
    assert_eq!(fast_seen.len(), 1, "the fast auto_mode target must be hit");
    assert_eq!(fast_seen[0]["model"], "haiku-mock");

    let slow_seen = slow_mock.requests.lock().unwrap();
    assert!(
        slow_seen.is_empty(),
        "the shared `default` route must never see this request: {:?}",
        *slow_seen
    );
}

/// The dashboard must not exist at all — not "exist but say disabled" — when
/// `serve --ui` was never passed. See `router`'s doc comment.
#[tokio::test]
async fn ui_routes_are_absent_when_the_dashboard_is_off() {
    let (upstream, _mock) = spawn_mock().await;
    let addr = spawn_gateway(translated_config(upstream), None).await;
    let client = reqwest::Client::new();

    for path in ["/ui", "/api/usage", "/api/live", "/api/routes/vectors"] {
        let response = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path} should not exist");
    }
}

/// Trades a dashboard token for its session cookie, exactly the way a
/// browser opening the URL `serve --ui` prints at startup would — used by
/// every test below that only cares about what happens *after* the
/// dashboard is authenticated. Asserts the bootstrap itself succeeded, so a
/// regression there fails loudly at the first caller rather than as a
/// confusing downstream 401.
async fn bootstrap_ui_cookie(client: &reqwest::Client, addr: SocketAddr, token: &str) -> String {
    let page = client
        .get(format!("http://{addr}/ui?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let cookie = page
        .headers()
        .get(http::header::SET_COOKIE)
        .expect("a valid token should mint a session cookie")
        .to_str()
        .unwrap();
    cookie.split(';').next().unwrap().to_string()
}

/// H1/H2: without the dashboard token — no header, no cookie, nothing a
/// browser could not already do on its own — every dashboard route refuses
/// the request. Before the fix these were reachable with zero auth whenever
/// `server.apiKey` was unset, and unreachable from a browser whenever it was
/// set; this is the "closed by default" half of the fix.
#[tokio::test]
async fn ui_routes_refuse_requests_without_the_dashboard_token() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, _live, _token) = spawn_gateway_with_live(translated_config(upstream), None).await;
    let client = reqwest::Client::new();

    for path in ["/ui", "/api/usage", "/api/live"] {
        let response = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            401,
            "{path} should require the dashboard token"
        );
    }
}

/// The bootstrap flow itself: `GET /ui?token=<token>` hands back a
/// `Set-Cookie`, and that cookie — no header at all — is then enough to
/// reach `/api/usage`. This is the exact path a browser takes, since it
/// cannot attach `Authorization`/`x-api-key` to a page navigation.
#[tokio::test]
async fn the_dashboard_token_trades_for_a_cookie_that_then_authenticates() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, _live, token) = spawn_gateway_with_live(translated_config(upstream), None).await;
    let client = reqwest::Client::new();

    let cookie = bootstrap_ui_cookie(&client, addr, &token).await;

    let usage = client
        .get(format!("http://{addr}/api/usage"))
        .header(http::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(usage.status(), 200);
    let usage_body: serde_json::Value = usage.json().await.unwrap();
    assert!(usage_body["rows"].is_array());
    assert!(usage_body["total"].is_object());

    // No classifier is loaded in this test state (see `spawn_gateway_with_live`),
    // so the map is legitimately empty — this only checks the endpoint answers
    // with the right shape.
    let vectors = client
        .get(format!("http://{addr}/api/routes/vectors"))
        .header(http::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(vectors.status(), 200);
    let vectors_body: serde_json::Value = vectors.json().await.unwrap();
    assert!(vectors_body["routes"].is_array());
}

/// The other half of "existing header auth keeps working": a `curl` script
/// carrying the configured `server.apiKey` reaches `/api/usage` without ever
/// touching the token/cookie dance — the proxy path's own credential is
/// still honored on the dashboard routes (see `ui::ui_guard`).
#[tokio::test]
async fn a_configured_api_key_authenticates_the_dashboard_too() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, _live, _token) =
        spawn_gateway_with_live(translated_config(upstream), Some("gw-secret")).await;

    let usage = reqwest::Client::new()
        .get(format!("http://{addr}/api/usage"))
        .bearer_auth("gw-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(usage.status(), 200);
}

/// H2: a `Host` header naming anything other than this loopback listener is
/// refused outright — the actual defense against DNS rebinding, since a
/// rebound page still sends its *original* hostname in `Host` even once the
/// browser has resolved it to 127.0.0.1. A valid token is not enough to get
/// past this check.
#[tokio::test]
async fn a_non_loopback_host_header_is_refused_even_with_a_valid_token() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, _live, token) = spawn_gateway_with_live(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/ui?token={token}"))
        .header(http::header::HOST, "evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

/// H3 regression: the dashboard's own "all" checkbox
/// (`assets/index.html`) sends `all=1`, which `str::parse::<bool>()` (what
/// `serde_urlencoded` used for a plain `bool` field) rejects outright — every
/// checked request was a guaranteed 400. See `ui::deserialize_truthy`.
#[tokio::test]
async fn usage_query_accepts_all_1_as_a_truthy_value() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, _live, token) = spawn_gateway_with_live(translated_config(upstream), None).await;
    let client = reqwest::Client::new();
    let cookie = bootstrap_ui_cookie(&client, addr, &token).await;

    let response = client
        .get(format!("http://{addr}/api/usage?all=1"))
        .header(http::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// A request through the ordinary proxy path publishes a
/// [`llm_gateway::server::live::LiveEvent`] carrying the prompt preview and
/// the route/model it triggered — the core "what just got routed" feature.
#[tokio::test]
async fn a_completed_request_publishes_a_live_event() {
    let (upstream, _mock) = spawn_mock().await;
    let (addr, live, _token) = spawn_gateway_with_live(translated_config(upstream), None).await;
    let mut rx = live.subscribe();

    reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "anything",
            "messages": [{"role": "user", "content": "hello from the live feed test"}],
        }))
        .send()
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("a live event should arrive promptly")
        .expect("the channel should not have closed");

    assert_eq!(event.provider, "chat-mock");
    assert_eq!(event.model, "qwen3.5");
    assert_eq!(event.status, "success");
    assert_eq!(
        event.prompt_preview.as_deref(),
        Some("hello from the live feed test")
    );
}

/// A build that turns the dashboard off entirely (`--ui` never passed)
/// pays nothing for it: `state.live` is `None`, so the proxy path never
/// even considers building a live event. Exercised indirectly by
/// `ui_routes_are_absent_when_the_dashboard_is_off` for the router side;
/// this checks the proxy side reaches the same request successfully with
/// `live: None`, i.e. nothing panics or misbehaves when there is no
/// subscriber to publish to.
#[tokio::test]
async fn requests_still_succeed_with_the_dashboard_off() {
    let (upstream, _mock) = spawn_mock().await;
    let addr = spawn_gateway(translated_config(upstream), None).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "anything",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

/// Reads back the one `usage-*.jsonl` line the recorder wrote under `dir`,
/// polling briefly since the write lands on a background task (see
/// `record::Recorder`).
async fn read_usage_record(dir: &std::path::Path) -> llm_gateway::record::usage_log::UsageRecord {
    let now = time::OffsetDateTime::now_utc();
    let path = dir.join(llm_gateway::record::usage_log::file_name(
        now.year(),
        now.month() as u8,
    ));
    let mut contents = String::new();
    for _ in 0..100 {
        if let Ok(c) = std::fs::read_to_string(&path) {
            if !c.is_empty() {
                contents = c;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let line = contents.lines().next().expect("usage record was written");
    serde_json::from_str(line).unwrap()
}

/// #22: a provider configured with `injectUsage: false`, given a client that
/// never asked for `stream_options` itself, never gets a `usage` object back
/// from an `openai-chat` stream — that is the expected shape of this
/// configuration, not a failed extraction, so `usage_missing` must stay
/// `false` for it.
#[tokio::test]
async fn inject_usage_false_with_no_client_stream_options_does_not_mark_usage_missing() {
    let (upstream, _mock) = spawn_chat_mock(ChatMockMode::SseNoUsage).await;
    let mut config = Config::default();
    config.providers.insert(
        "chat-mock".into(),
        provider_without_inject_usage(&format!("http://{upstream}/v1"), ApiKind::OpenaiChat),
    );
    config.routes.insert(
        llm_gateway::config::DEFAULT_ROUTE.into(),
        route_to("chat-mock/qwen3.5", &[]),
    );
    let (addr, logs_dir) = spawn_gateway_recording_usage(config).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({
            "model": "default",
            "max_tokens": 1024,
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();

    let record = read_usage_record(&logs_dir).await;
    assert_eq!(record.status, "success");
    assert!(
        !record.usage_missing,
        "a provider with injectUsage: false and no client stream_options is expected to have no usage"
    );
}

/// Same shape as above, but with `injectUsage: true` (the default) — the
/// upstream mock still never sends usage (simulating a genuinely broken
/// upstream), and this time that really is an extraction failure:
/// `usage_missing` must still fire, so #22's fix only silences the case it
/// was meant to.
#[tokio::test]
async fn inject_usage_true_with_missing_upstream_usage_still_marks_usage_missing() {
    let (upstream, _mock) = spawn_chat_mock(ChatMockMode::SseNoUsage).await;
    let config = translated_config(upstream);
    let (addr, logs_dir) = spawn_gateway_recording_usage(config).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({
            "model": "default",
            "max_tokens": 1024,
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();

    let record = read_usage_record(&logs_dir).await;
    assert_eq!(record.status, "success");
    assert!(
        record.usage_missing,
        "injectUsage: true still expects usage, so a stream with none is a real extraction failure"
    );
}
