//! The vector table semantic routing classifies requests against, plus the
//! classifier itself.
//!
//! A [`RouteIndex`] holds one embedding per candidate route, computed once
//! from `routes[].description` rather than per request (see the module docs
//! on [`Classifier`] for how it stays in step with config hot-reloads). A
//! [`Classifier`] owns the embedding model and the current index together,
//! and is the only public entry point: [`Classifier::classify`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arc_swap::ArcSwap;

use crate::config::validate::resolve_candidates;
use crate::config::watch::SharedConfig;
use crate::config::{ApiKind, Config, ModelRef};
use crate::semantic::embed::Embedder;

/// One candidate available to an auto route's classifier.
///
/// `vector` is already L2-normalized (see `Embedder::load`) and `api` is the
/// protocol the candidate's own `model.default` speaks, so a request whose
/// endpoint does not match can be excluded before scoring rather than after.
#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    vector: Vec<f32>,
    api: ApiKind,
}

/// Resolved candidates and threshold for one `semantic` ("auto") route.
struct RouteEntry {
    threshold: f32,
    candidates: Vec<Candidate>,
}

/// Vector table over every auto route's candidates, built from a single
/// [`Config`] snapshot.
///
/// Cheap to rebuild wholesale: at most a few dozen routes, each costing one
/// [`Embedder::embed`] call (well under a millisecond) plus a `Vec` push, so
/// [`Classifier`] rebuilds this from scratch on every config change rather
/// than diffing it.
pub struct RouteIndex {
    routes: HashMap<String, RouteEntry>,
}

impl RouteIndex {
    /// Build the index for every route in `config` that has a `semantic`
    /// block.
    ///
    /// A candidate that turns out to have no description or an unresolvable
    /// `model.default` is silently skipped rather than causing a build
    /// failure: `config` reaching this point has already passed
    /// `validate::validate`, which rejects exactly those cases for any
    /// config that is actually live, so this only needs to be defensive.
    pub fn build(config: &Config, embedder: &Embedder) -> Self {
        let mut routes = HashMap::new();

        for (route_name, route) in &config.routes {
            let Some(semantic) = &route.semantic else {
                continue;
            };

            let candidates = resolve_candidates(config, route_name, semantic)
                .into_iter()
                .filter_map(|candidate_name| embed_candidate(config, embedder, candidate_name))
                .collect();

            routes.insert(
                route_name.clone(),
                RouteEntry {
                    threshold: semantic.threshold,
                    candidates,
                },
            );
        }

        Self { routes }
    }

    fn entry(&self, route_name: &str) -> Option<&RouteEntry> {
        self.routes.get(route_name)
    }
}

/// Embed one candidate route's description and resolve its `ApiKind`.
/// `None` if either step is not possible — see [`RouteIndex::build`] for why
/// that is expected to be unreachable on a live config, not an error case
/// worth surfacing here.
fn embed_candidate(
    config: &Config,
    embedder: &Embedder,
    candidate_name: &str,
) -> Option<Candidate> {
    let candidate_route = config.routes.get(candidate_name)?;
    let description = candidate_route.description.as_ref()?;
    let text = description.text().ok()?;
    let vector = embedder.embed(&text)?;
    let api = ModelRef::parse(&candidate_route.model.default)
        .and_then(|model_ref| config.provider(&model_ref.provider))
        .map(|provider| provider.api)?;

    Some(Candidate {
        name: candidate_name.to_string(),
        vector,
        api,
    })
}

/// Outcome of classifying one request against an auto route's candidates.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// The winning candidate's route name and its cosine similarity to the
    /// request text, if the top-scoring candidate cleared the route's
    /// `threshold`. `None` means the caller should fall back to the auto
    /// route's own `model` — either no candidate cleared the bar, no
    /// candidate matched `expected_api`, or `text` failed to embed.
    pub matched: Option<(String, f32)>,
    /// Every candidate considered (after excluding `ApiKind` mismatches),
    /// scored and sorted descending by score. Kept even when `matched` is
    /// `None`, so a caller building a trace record can still show what
    /// almost matched. Empty when `text` failed to embed or no candidate
    /// matched `expected_api`.
    pub candidates: Vec<(String, f32)>,
    /// How long embedding the request text took.
    pub embed_ms: u64,
}

/// Cosine similarity as a plain dot product.
///
/// Both operands must already be L2-normalized (query vectors from
/// `Embedder::embed`, candidate vectors from `RouteIndex::build`) — this
/// function does not normalize, so a caller feeding it anything else would
/// silently get a similarity that is not cosine similarity at all.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Every candidate considered (after excluding `ApiKind` mismatches), scored
/// and sorted descending by score, plus the winner if one cleared
/// `threshold` — the pieces `rank` produces and `Classifier::classify`
/// assembles into a [`Verdict`].
struct Scored {
    all: Vec<(String, f32)>,
    winner: Option<(String, f32)>,
}

/// Score `candidates` against an already-embedded `vector`, keeping only
/// those whose `api` matches `expected_api`, and decide whether the winner
/// clears `threshold`.
///
/// `all` is returned regardless of whether anything clears `threshold` —
/// unlike an early `None`, this keeps sub-threshold candidates visible to a
/// caller that wants to record them (e.g. for tracing) even when the
/// classification decision itself is "fall back".
///
/// Pure and model-independent on purpose: this is the actual classification
/// decision (matching, ranking, thresholding), split out from `Classifier`
/// so it can be unit-tested without a loaded embedding model.
fn rank(vector: &[f32], candidates: &[Candidate], expected_api: ApiKind, threshold: f32) -> Scored {
    let mut scored: Vec<(String, f32)> = candidates
        .iter()
        .filter(|c| c.api == expected_api)
        .map(|c| (c.name.clone(), dot(vector, &c.vector)))
        .collect();

    scored.sort_by(|a, b| b.1.total_cmp(&a.1));

    let winner = scored
        .first()
        .cloned()
        .filter(|(_, score)| *score >= threshold);

    Scored {
        all: scored,
        winner,
    }
}

/// Decides, from a monotonically increasing generation counter, whether a
/// cached value needs rebuilding, and serializes concurrent rebuilders so
/// only one does the work while everyone else keeps using the
/// still-valid-if-slightly-stale cache instead of blocking on it.
///
/// Split out from [`Classifier`] so the generation-tracking mechanics can be
/// unit-tested with a plain counter, without a loaded embedding model.
struct StaleCheck {
    seen: AtomicU64,
    rebuild_lock: Mutex<()>,
}

impl StaleCheck {
    fn new(initial: u64) -> Self {
        Self {
            seen: AtomicU64::new(initial),
            rebuild_lock: Mutex::new(()),
        }
    }

    /// Calls `rebuild` if `current` differs from the last generation
    /// recorded here — but only once per change, even under concurrent
    /// callers: a caller that has to wait for `rebuild_lock` re-checks after
    /// acquiring it and skips `rebuild` if another thread already brought
    /// the recorded generation up to `current` while it was waiting.
    ///
    /// `generation` is only ever written while `rebuild_lock` is held, so
    /// the lock-free fast-path read below never observes a torn update:
    /// every visible value was fully committed by whichever call held the
    /// lock when it wrote it.
    fn sync(&self, current: u64, rebuild: impl FnOnce()) {
        if self.seen.load(Ordering::SeqCst) == current {
            return;
        }

        let _guard = self
            .rebuild_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.seen.load(Ordering::SeqCst) == current {
            return;
        }

        rebuild();
        self.seen.store(current, Ordering::SeqCst);
    }
}

/// Classifies request text against an auto route's candidates.
///
/// Owns the embedding model and the current [`RouteIndex`] together, and
/// keeps the index in step with config hot-reloads by comparing
/// `shared`'s [`SharedConfig::generation`] against the generation the index
/// was last built from — see [`StaleCheck`].
///
/// The rebuild this triggers must never happen on `notify`'s callback
/// thread, which is where `SharedConfig::reload` itself runs synchronously.
/// It does not: `reload` only bumps the generation counter, and the rebuild
/// happens lazily, inside [`Classifier::classify`], on whatever task called
/// it.
pub struct Classifier {
    shared: Arc<SharedConfig>,
    embedder: Embedder,
    index: ArcSwap<RouteIndex>,
    stale_check: StaleCheck,
}

impl Classifier {
    /// Build the initial index from `shared`'s config as of right now.
    pub fn new(shared: Arc<SharedConfig>, embedder: Embedder) -> Self {
        let generation = shared.generation();
        let index = RouteIndex::build(&shared.get(), &embedder);
        Self {
            shared,
            embedder,
            index: ArcSwap::from_pointee(index),
            stale_check: StaleCheck::new(generation),
        }
    }

    /// Classify `text` against `auto_route`'s candidates.
    ///
    /// Returns `None` only when `auto_route` has no `semantic` block in the
    /// current index — the one case a caller cannot get anything useful
    /// back, since there is no `threshold` or candidate list to score
    /// against. Every other outcome is `Some(Verdict)`: `Verdict::matched`
    /// is `None` when embedding `text` fails, no candidate matches
    /// `expected_api`, or the best score misses `threshold`, and in all of
    /// those cases the caller should fall back to `auto_route`'s own
    /// `model` — but `Verdict::candidates` is still populated where
    /// possible, for a caller that wants to record what was considered.
    pub fn classify(&self, auto_route: &str, text: &str, expected_api: ApiKind) -> Option<Verdict> {
        self.stale_check.sync(self.shared.generation(), || {
            let config = self.shared.get();
            self.index
                .store(Arc::new(RouteIndex::build(&config, &self.embedder)));
        });

        let index = self.index.load();
        let entry = index.entry(auto_route)?;

        let started = Instant::now();
        let vector = self.embedder.embed(text);
        let embed_ms = started.elapsed().as_millis() as u64;

        let Some(vector) = vector else {
            // Tokenizer panic or an empty result: nothing was scored, so
            // there is no candidate list to hand back either.
            return Some(Verdict {
                matched: None,
                candidates: Vec::new(),
                embed_ms,
            });
        };

        let scored = rank(&vector, &entry.candidates, expected_api, entry.threshold);
        Some(Verdict {
            matched: scored.winner,
            candidates: scored.all,
            embed_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn candidate(name: &str, vector: Vec<f32>, api: ApiKind) -> Candidate {
        Candidate {
            name: name.to_string(),
            vector,
            api,
        }
    }

    #[test]
    fn dot_of_orthogonal_unit_vectors_is_zero() {
        assert_eq!(dot(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn dot_of_identical_unit_vectors_is_one() {
        let v = [0.6, 0.8];
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rank_picks_the_highest_scoring_candidate_and_sorts_descending() {
        let candidates = vec![
            candidate("low", vec![0.0, 1.0], ApiKind::AnthropicMessages),
            candidate("high", vec![1.0, 0.0], ApiKind::AnthropicMessages),
            candidate("mid", vec![0.7, 0.7], ApiKind::AnthropicMessages),
        ];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::AnthropicMessages, 0.0);

        let (route, score) = scored.winner.expect("top score clears threshold 0.0");
        assert_eq!(route, "high");
        assert!((score - 1.0).abs() < 1e-6);
        assert_eq!(
            scored
                .all
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["high", "mid", "low"]
        );
    }

    #[test]
    fn rank_has_no_winner_when_the_best_score_misses_threshold_but_keeps_all_scored() {
        let candidates = vec![candidate(
            "only",
            vec![0.0, 1.0],
            ApiKind::AnthropicMessages,
        )];

        // Orthogonal to the query vector: score 0.0, threshold 0.5.
        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::AnthropicMessages, 0.5);
        assert!(scored.winner.is_none());
        assert_eq!(
            scored.all.len(),
            1,
            "the sub-threshold candidate is still kept for tracing"
        );
    }

    #[test]
    fn rank_excludes_candidates_with_a_different_api_kind() {
        let candidates = vec![
            candidate("wrong-api", vec![1.0, 0.0], ApiKind::OpenaiChat),
            candidate("right-api", vec![0.9, 0.1], ApiKind::AnthropicMessages),
        ];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::AnthropicMessages, 0.0);

        let (route, _) = scored
            .winner
            .expect("the matching-api candidate clears 0.0");
        assert_eq!(route, "right-api");
        assert_eq!(scored.all.len(), 1, "{:?}", scored.all);
    }

    #[test]
    fn rank_has_no_winner_when_no_candidate_matches_the_expected_api() {
        let candidates = vec![candidate("only", vec![1.0, 0.0], ApiKind::OpenaiChat)];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::AnthropicMessages, 0.0);
        assert!(scored.winner.is_none());
        assert!(scored.all.is_empty());
    }

    #[test]
    fn stale_check_rebuilds_exactly_once_per_generation_change() {
        let check = StaleCheck::new(0);
        let calls = AtomicU32::new(0);

        check.sync(0, || {
            calls.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "same generation: no rebuild"
        );

        check.sync(1, || {
            calls.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "new generation: one rebuild"
        );

        check.sync(1, || {
            calls.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "same generation again: no rebuild"
        );

        check.sync(2, || {
            calls.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "another new generation: one more rebuild"
        );
    }
}
