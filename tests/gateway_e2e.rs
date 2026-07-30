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
use llm_gateway::server::{router, AppState};

/// A fixed SSE body with awkward blank lines and a multi-event layout —
/// anything the gateway re-framed would show up as a diff.
const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"日本語テスト\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\ndata: [DONE]\n\n";

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

fn provider(base: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: base.to_string(),
        api: ApiKind::OpenaiChat,
        api_key: Some(SecretRef::new("mock-key")),
        headers: Default::default(),
        inject_usage: true,
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
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // The tempdir must outlive the server; leak it for the test's lifetime.
    std::mem::forget(dir);
    addr
}

#[tokio::test]
async fn streamed_response_reaches_the_client_byte_for_byte() {
    let (upstream, mock) = spawn_mock().await;

    let mut config = Config::default();
    config
        .providers
        .insert("mock".into(), provider(&format!("http://{upstream}/v1")));
    config
        .routes
        .insert("role-test".into(), route_to("mock/real-model", &[]));

    let addr = spawn_gateway(config, None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("x-gw-client", "e2e")
        .json(&serde_json::json!({
            "model": "role-test",
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
    config
        .providers
        .insert("dead".into(), provider("http://127.0.0.1:9/v1"));
    config
        .providers
        .insert("mock".into(), provider(&format!("http://{upstream}/v1")));
    config.routes.insert(
        "role-fb".into(),
        route_to("dead/primary-model", &["mock/backup-model"]),
    );

    let addr = spawn_gateway(config, None).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "role-fb",
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
async fn models_endpoint_lists_routes_but_not_wildcards() {
    let mut config = Config::default();
    config
        .providers
        .insert("mock".into(), provider("http://127.0.0.1:9/v1"));
    config
        .routes
        .insert("role-a".into(), route_to("mock/m", &[]));
    config
        .routes
        .insert("claude-*".into(), route_to("mock/*", &[]));

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
    assert_eq!(ids, vec!["role-a"], "wildcards must not be advertised");
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
    config
        .providers
        .insert("mock".into(), provider("http://127.0.0.1:9/v1")); // openai-chat
    config
        .routes
        .insert("role-chat".into(), route_to("mock/m", &[]));

    let addr = spawn_gateway(config, None).await;
    // Calling the *Responses* endpoint with a chat-backed route must be
    // refused up front.
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/responses"))
        .json(&serde_json::json!({"model": "role-chat", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("openai-chat"));
}
