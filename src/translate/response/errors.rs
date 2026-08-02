//! Upstream and gateway error bodies → each client protocol's own envelope.

use serde_json::{json, Value};

/// Pull a human-readable message out of an upstream `openai-chat` error
/// body, tolerating the shapes real servers actually send: the common
/// `{"error": {"message": "…", …}}` object, Ollama's bare
/// `{"error": "plain string"}`, and anything else (in which case the status
/// and the raw body beat saying nothing).
fn chat_error_message(body: &Value, status: u16) -> String {
    let error = body.get("error");
    let message = match error {
        Some(Value::Object(_)) => error
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    message.unwrap_or_else(|| format!("upstream returned HTTP {status}: {body}"))
}

/// Translate an upstream error body into the Anthropic error envelope.
///
/// Never panics: even a body that is not the expected `{"error": …}` shape
/// (or is entirely unreadable) produces a valid envelope carrying whatever
/// information could be recovered, so the operator can still tell what went
/// wrong upstream.
pub fn chat_error_to_anthropic(body: &Value, status: u16) -> Value {
    json!({
        "type": "error",
        "error": {
            "type": error_type_from_status(status),
            "message": chat_error_message(body, status),
        },
    })
}

/// Translate an upstream error body into the OpenAI error envelope Responses
/// clients (Codex CLI) expect. Same robustness contract as
/// [`chat_error_to_anthropic`]: never panics, and an unreadable body still
/// produces a valid envelope.
pub fn chat_error_to_responses(body: &Value, status: u16) -> Value {
    json!({
        "error": {
            "message": chat_error_message(body, status),
            "type": error_type_from_status(status),
            "code": Value::Null,
        },
    })
}

/// Pull a human-readable message out of an upstream `anthropic-messages`
/// error body, tolerating a missing or malformed shape the same way
/// [`chat_error_message`] does for `openai-chat`: the documented
/// `{"type":"error","error":{"type":...,"message":"…"}}` envelope, or the
/// status and raw body when even that cannot be read.
fn anthropic_error_message(body: &Value, status: u16) -> String {
    body.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("upstream returned HTTP {status}: {body}"))
}

/// Translate an upstream Anthropic error body into the OpenAI error envelope
/// Responses clients (Codex CLI) expect. Same robustness contract as
/// [`chat_error_to_responses`]: never panics, and an unreadable body still
/// produces a valid envelope. The upstream's own `error.type` string is
/// preferred when present, falling back to [`error_type_from_status`] —
/// the same precedence [`chat_error_to_responses`] applies for `openai-chat`.
pub fn anthropic_error_to_responses(body: &Value, status: u16) -> Value {
    let error_type = body
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| error_type_from_status(status).to_string());
    json!({
        "error": {
            "message": anthropic_error_message(body, status),
            "type": error_type,
            "code": Value::Null,
        },
    })
}

/// Translate a Responses-shaped error body into the Anthropic error
/// envelope — the reverse of [`anthropic_error_to_responses`].
///
/// Not wired into any [`crate::translate::Translation`] arm: `error()` only
/// ever translates an *upstream* error body into the *client's* shape, and
/// for [`crate::translate::Translation::ResponsesToAnthropic`] the upstream
/// speaks `anthropic-messages`, not `openai-responses` — see that variant's
/// `error()` arm, which calls [`anthropic_error_to_responses`] instead. This
/// exists purely for symmetry/completeness, the same reason a translation
/// module keeps both directions of a mapping even when only one is reachable
/// today.
pub fn responses_error_to_anthropic(body: &Value, status: u16) -> Value {
    let message = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("upstream returned HTTP {status}: {body}"));
    let error_type = body
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| error_type_from_status(status));
    json!({
        "type": "error",
        "error": { "type": error_type, "message": message },
    })
}

/// The gateway's own error, in the Anthropic envelope — for a failure inside
/// translation itself rather than one the upstream reported (see
/// [`chat_error_to_anthropic`] for that case). `type: api_error` because this
/// is the gateway's own failure, not the client's.
pub fn anthropic_gateway_error(message: &str) -> Value {
    json!({
        "type": "error",
        "error": { "type": "api_error", "message": message },
    })
}

/// The gateway's own error, in the OpenAI envelope Responses clients expect
/// — the [`chat_error_to_responses`] counterpart of
/// [`anthropic_gateway_error`].
pub fn responses_gateway_error(message: &str) -> Value {
    json!({
        "error": { "message": message, "type": "api_error", "code": Value::Null },
    })
}

/// Map an HTTP status to one of Anthropic's documented error `type` strings.
/// Claude Code branches on these — notably `rate_limit_error` and
/// `overloaded_error` drive its retry behaviour — so this mapping is
/// behaviour, not cosmetics.
fn error_type_from_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500 => "api_error",
        529 => "overloaded_error",
        // Any other 4xx: still the client's fault, so the generic
        // client-error type is the closest honest fit. Any other 5xx: the
        // gateway or the upstream's own fault, so the generic server-error
        // type. Written as a guarded catch-all (rather than an explicit
        // `..=499` range) because the literal arms above already cover the
        // specific codes inside that range and a range arm alongside them
        // reads as overlapping.
        other if (400..500).contains(&other) => "invalid_request_error",
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_object_error_message_is_used_directly() {
        let body = json!({
            "error": { "message": "model not found", "type": "invalid_request_error" },
        });

        let out = chat_error_to_anthropic(&body, 404);

        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "not_found_error");
        assert_eq!(out["error"]["message"], "model not found");
    }

    #[test]
    fn a_string_valued_error_is_used_directly() {
        let body = json!({ "error": "model is not running" });

        let out = chat_error_to_anthropic(&body, 500);

        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], "model is not running");
    }

    #[test]
    fn a_body_with_no_error_field_produces_a_message_from_the_status_and_body() {
        let body = json!({ "something_else": true });

        let out = chat_error_to_anthropic(&body, 500);

        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
        assert!(message.contains("something_else"), "{message}");
    }

    #[test]
    fn each_status_class_maps_to_its_documented_error_type() {
        let cases: &[(u16, &str)] = &[
            (400, "invalid_request_error"),
            (401, "authentication_error"),
            (403, "permission_error"),
            (404, "not_found_error"),
            (413, "request_too_large"),
            (429, "rate_limit_error"),
            (500, "api_error"),
            (529, "overloaded_error"),
            (422, "invalid_request_error"), // other 4xx
            (503, "api_error"),             // other 5xx
        ];

        for (status, expected) in cases {
            let body = json!({ "error": { "message": "x" } });
            let out = chat_error_to_anthropic(&body, *status);
            assert_eq!(out["error"]["type"], *expected, "status {status}");
        }
    }

    #[test]
    fn a_completely_empty_error_body_does_not_panic() {
        let out = chat_error_to_anthropic(&json!({}), 500);

        assert_eq!(out["type"], "error");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
    }

    #[test]
    fn an_object_error_message_translates_to_the_openai_envelope() {
        let body = json!({
            "error": { "message": "model not found", "type": "invalid_request_error" },
        });

        let out = chat_error_to_responses(&body, 404);

        assert_eq!(out["error"]["message"], "model not found");
        assert_eq!(out["error"]["type"], "not_found_error");
        // No top-level `type` field — that is an Anthropic envelope detail.
        assert!(out.get("type").is_none());
    }

    #[test]
    fn a_string_valued_error_translates_to_the_openai_envelope() {
        let body = json!({ "error": "model is not running" });

        let out = chat_error_to_responses(&body, 500);

        assert_eq!(out["error"]["message"], "model is not running");
        assert_eq!(out["error"]["type"], "api_error");
    }

    #[test]
    fn a_completely_empty_responses_error_body_does_not_panic() {
        let out = chat_error_to_responses(&json!({}), 500);

        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
    }

    #[test]
    fn an_anthropic_error_body_translates_to_the_openai_envelope() {
        let body = json!({
            "type": "error",
            "error": { "type": "not_found_error", "message": "model not found" },
        });

        let out = anthropic_error_to_responses(&body, 404);

        assert_eq!(out["error"]["message"], "model not found");
        assert_eq!(out["error"]["type"], "not_found_error");
        assert!(out.get("type").is_none());
    }

    #[test]
    fn anthropic_error_to_responses_falls_back_to_status_derived_type() {
        let body = json!({ "error": { "message": "boom" } });

        let out = anthropic_error_to_responses(&body, 429);

        assert_eq!(out["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn anthropic_error_to_responses_status_classes() {
        let cases: &[(u16, &str)] = &[
            (400, "invalid_request_error"),
            (429, "rate_limit_error"),
            (500, "api_error"),
        ];
        for (status, expected) in cases {
            let body = json!({ "error": { "message": "x" } });
            let out = anthropic_error_to_responses(&body, *status);
            assert_eq!(out["error"]["type"], *expected, "status {status}");
        }
    }

    #[test]
    fn a_completely_empty_anthropic_error_body_does_not_panic() {
        let out = anthropic_error_to_responses(&json!({}), 500);

        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
    }

    #[test]
    fn a_responses_error_body_translates_to_the_anthropic_envelope() {
        let body = json!({
            "error": { "message": "model not found", "type": "invalid_request_error", "code": null },
        });

        let out = responses_error_to_anthropic(&body, 400);

        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["message"], "model not found");
        assert_eq!(out["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn responses_error_to_anthropic_status_classes() {
        let cases: &[(u16, &str)] = &[
            (400, "invalid_request_error"),
            (429, "rate_limit_error"),
            (500, "api_error"),
        ];
        for (status, expected) in cases {
            let body = json!({ "error": { "message": "x" } });
            let out = responses_error_to_anthropic(&body, *status);
            assert_eq!(out["error"]["type"], *expected, "status {status}");
        }
    }

    #[test]
    fn a_completely_empty_responses_error_body_translates_without_panicking() {
        let out = responses_error_to_anthropic(&json!({}), 500);

        assert_eq!(out["type"], "error");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
    }

    #[test]
    fn gateway_errors_use_each_protocols_own_envelope() {
        let anthropic = anthropic_gateway_error("oversized body");
        assert_eq!(anthropic["type"], "error");
        assert_eq!(anthropic["error"]["type"], "api_error");
        assert_eq!(anthropic["error"]["message"], "oversized body");

        let responses = responses_gateway_error("oversized body");
        assert!(responses.get("type").is_none());
        assert_eq!(responses["error"]["type"], "api_error");
        assert_eq!(responses["error"]["message"], "oversized body");
    }
}
