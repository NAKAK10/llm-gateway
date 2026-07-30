//! `GET /v1/models` — the route list, in OpenAI's shape.
//!
//! This endpoint carries more weight than it looks. opencode requires every
//! model name in its config to appear here *verbatim*; on a mismatch it does not
//! error, it simply offers no models. So this is both the discovery mechanism and
//! the thing `launch opencode` checks against before starting anything.
//!
//! Wildcard routes are excluded — they are forwarding rules, not selectable
//! models, and listing `claude-*` would invite a client to request that literal
//! string.

use axum::extract::State;
use axum::response::Response;

use crate::server::AppState;

pub async fn handle(State(state): State<AppState>) -> Response {
    let _ = state;
    todo!("src/server/models.rs")
}

/// Build the response body for a set of route names.
///
/// Separate from the handler so `launch opencode` and the tests can build the
/// same list without an HTTP round trip.
pub fn body(routes: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "object": "list",
        "data": routes
            .iter()
            .map(|name| serde_json::json!({
                "id": name,
                "object": "model",
                "owned_by": "llm-gateway",
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_the_route_names_verbatim() {
        let v = body(&["role-ops", "role-writer"]);
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["role-ops", "role-writer"]);
    }
}
