//! The `serve --ui` live feed: fan out "what just got routed" to however
//! many dashboard tabs are open.
//!
//! Deliberately not the same channel [`crate::record::Recorder`] uses for
//! disk writes. That channel exists to serialize usage/trace writes against
//! retention pruning (see `crate::record::Entry::Prune`); bolting an
//! unbounded number of SSE subscribers onto it would complicate a mechanism
//! that is kept simple on purpose. A `broadcast` channel is the right shape
//! instead: publishing is a non-blocking, fire-and-forget `send` that is
//! silently a no-op with zero subscribers — the same "never add request
//! latency" property `Recorder::usage`/`Recorder::trace` have — and each
//! dashboard tab gets its own independent `Receiver`, so one slow tab can
//! never block another or the request path.
//!
//! Publishing here is unconditional — never gated by `--debug` — which is
//! the whole point: `--debug`'s trade-off (prompt text on disk) is a
//! different, more consequential decision than showing the same data in a
//! localhost-only, in-memory-only dashboard for as long as the tab is open.
//! See `crate::server::proxy` for where an event's fields are assembled.

use serde::Serialize;
use tokio::sync::broadcast;

/// How many events a lagging subscriber can fall behind before older ones
/// are dropped for it specifically. A backgrounded dashboard tab missing a
/// few events under load is fine; a slow consumer must never make the
/// channel grow without bound or block a publisher, which is why this is
/// `broadcast` (drops the oldest) rather than an `mpsc` (blocks or grows).
const CHANNEL_CAPACITY: usize = 256;

/// Fan-out point for live events, held as `Arc<LiveFeed>` in [`crate::server::AppState`]
/// when `serve --ui` (or `config.ui.enabled`) turns the dashboard on; `None`
/// there otherwise; so a build with the dashboard off allocates nothing for it.
pub struct LiveFeed {
    tx: broadcast::Sender<LiveEvent>,
}

impl LiveFeed {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Broadcast one event. Never blocks, never surfaces an error: with no
    /// subscribers `send` returns an error that simply means "nobody was
    /// listening," which is the expected, common case and not worth logging.
    pub fn publish(&self, event: LiveEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.tx.subscribe()
    }

    /// Whether any tab is currently subscribed. `publish` is already a
    /// no-op with zero subscribers, but a caller that has to *build* the
    /// event first — assembling a `LiveEvent`'s `point` field, in
    /// particular, means a second embedding and a `Basis::fit` (see
    /// `crate::server::ui::project_point`) — needs this to skip that work
    /// too, rather than doing it only to have `publish` throw the result
    /// away. See #27.
    pub fn has_subscribers(&self) -> bool {
        self.tx.receiver_count() > 0
    }
}

impl Default for LiveFeed {
    fn default() -> Self {
        Self::new()
    }
}

/// One row of the live feed: what came in, which route/model it triggered,
/// and how it turned out. Broadcast only, kept only as long as a tab is
/// subscribed — never written to disk. See the module docs for why that stays
/// a separate decision from `--debug`.
#[derive(Debug, Clone, Serialize)]
pub struct LiveEvent {
    pub ts: String,
    /// UUIDv7, so events sort chronologically by id alone — same convention
    /// as `TraceRecord::req_id`, generated fresh per event here rather than
    /// shared with it (see `crate::server::proxy`).
    pub req_id: String,
    pub client: String,
    pub endpoint: String,
    pub requested_model: String,
    /// Truncated the same way `TraceInput::last_user_text` is (200
    /// characters unless `--debug-full`). `None` when the request carried no
    /// user text to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
    /// `explicit`, `semantic`, `semantic_history`, `semantic_system`,
    /// `manual`, `no_classifier`, `no_text`, or `utility_bypass` — same
    /// vocabulary as `TraceRouting::mode`.
    pub routing_mode: String,
    pub reason: String,
    pub matched_route: String,
    pub candidates: Vec<LiveCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    pub provider: String,
    pub model: String,
    pub api: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
    pub attempt: u32,
    pub streaming: bool,
    /// `success`, `error`, or `aborted` — same vocabulary as
    /// `UsageRecord::status`.
    pub status: String,
    pub dur_ms: u64,
    pub in_tok: u64,
    pub out_tok: u64,
    pub cache_read_tok: u64,
    pub cache_write_tok: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Where the text that decided routing lands in the current 2-D
    /// projection `GET /api/routes/vectors` returns — `None` when nothing
    /// decided routing by text (a manual, utility-bypass, or
    /// no-classifier/no-text request) or embedding it again for display
    /// failed. See `crate::server::ui::pca`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<[f32; 2]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveCandidate {
    pub route: String,
    pub score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(req_id: &str) -> LiveEvent {
        LiveEvent {
            ts: "2026-08-01T00:00:00Z".to_string(),
            req_id: req_id.to_string(),
            client: "claude-code".to_string(),
            endpoint: "/v1/messages".to_string(),
            requested_model: "claude-sonnet-4-6".to_string(),
            prompt_preview: Some("hello".to_string()),
            routing_mode: "semantic".to_string(),
            reason: "matched".to_string(),
            matched_route: "role-writer".to_string(),
            candidates: vec![LiveCandidate {
                route: "role-writer".to_string(),
                score: 0.9,
            }],
            score: Some(0.9),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api: "anthropic-messages".to_string(),
            translation: None,
            attempt: 1,
            streaming: false,
            status: "success".to_string(),
            dur_ms: 120,
            in_tok: 10,
            out_tok: 20,
            cache_read_tok: 0,
            cache_write_tok: 0,
            error: None,
            point: Some([0.1, -0.2]),
        }
    }

    #[test]
    fn a_publish_with_no_subscribers_does_not_panic_or_error_visibly() {
        let feed = LiveFeed::new();
        feed.publish(sample("1"));
    }

    #[test]
    fn a_subscriber_receives_a_published_event() {
        let feed = LiveFeed::new();
        let mut rx = feed.subscribe();
        feed.publish(sample("1"));

        let received = rx.try_recv().expect("event should be immediately ready");
        assert_eq!(received.req_id, "1");
        assert_eq!(received.matched_route, "role-writer");
    }

    #[test]
    fn every_subscriber_gets_its_own_copy() {
        let feed = LiveFeed::new();
        let mut a = feed.subscribe();
        let mut b = feed.subscribe();
        feed.publish(sample("1"));

        assert_eq!(a.try_recv().unwrap().req_id, "1");
        assert_eq!(b.try_recv().unwrap().req_id, "1");
    }

    #[test]
    fn has_subscribers_reflects_the_live_receiver_count() {
        let feed = LiveFeed::new();
        assert!(!feed.has_subscribers());

        let rx = feed.subscribe();
        assert!(feed.has_subscribers());

        drop(rx);
        assert!(!feed.has_subscribers());
    }

    #[test]
    fn a_subscriber_that_joins_after_publishing_sees_nothing_before_it() {
        let feed = LiveFeed::new();
        feed.publish(sample("missed"));
        let mut rx = feed.subscribe();
        feed.publish(sample("seen"));

        let received = rx.try_recv().expect("the post-subscribe event is ready");
        assert_eq!(received.req_id, "seen");
    }
}
