//! The vector table semantic routing classifies requests against, plus the
//! classifier itself.
//!
//! A [`RouteIndex`] holds one embedding per candidate route, computed once
//! from `routes[].description` rather than per request (see the module docs
//! on [`Classifier`] for how it stays in step with config hot-reloads). A
//! [`Classifier`] owns the embedding model and the current index together,
//! and is the only public entry point: [`Classifier::classify`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arc_swap::ArcSwap;

use crate::config::watch::SharedConfig;
use crate::config::{ApiKind, Config, ModelRef};
use crate::semantic::embed::Embedder;

/// Cosine similarity a candidate must clear to win classification. Below
/// this, [`Classifier::classify`] returns no winner and the caller falls
/// back to the reserved `default` route (`crate::config::DEFAULT_ROUTE`).
///
/// Fixed rather than configurable per route: every route is now a
/// classification candidate by default, so there is no longer a natural
/// per-route place to hang a threshold on, and one shared cutoff is simpler
/// to reason about than N independently-tuned ones. `0.45` is carried over
/// from the old opt-in design's default, which was chosen empirically to
/// separate genuinely on-topic descriptions from unrelated ones with the
/// `potion-multilingual-128M` embedding model.
pub const CLASSIFICATION_THRESHOLD: f32 = 0.45;

/// One candidate available to the classifier.
///
/// `vectors` holds one already-L2-normalized embedding (see `Embedder::load`)
/// per entry in the route's `description` — see [`crate::config::Description`]
/// for why a route may have more than one — and is never empty for a
/// candidate that made it into a [`RouteIndex`] (see [`embed_candidate`]). A
/// candidate's score against a query is the **maximum** cosine to any of
/// these, so a route described in two languages wins on whichever variant
/// the request text actually matches, rather than an averaged-down single
/// embedding.
///
/// `apis` holds the protocol the candidate's `model.default` speaks plus one
/// for every resolvable `model.fallbacks` entry (default first) — a candidate
/// the request cannot reach through *any* of them — not the same protocol,
/// and not translatable to it — can be excluded before scoring rather than
/// after. A route's fallbacks may speak a different protocol than its default
/// now (cross-protocol fallback), so a candidate whose default the request
/// cannot reach directly may still be reachable through one of its
/// fallbacks.
#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    vectors: Vec<Vec<f32>>,
    apis: Vec<ApiKind>,
}

/// Vector table over every classifiable route, built from a single
/// [`Config`] snapshot.
///
/// Cheap to rebuild wholesale: at most a few dozen routes, each costing one
/// [`Embedder::embed`] call (well under a millisecond) plus a `Vec` push, so
/// [`Classifier`] rebuilds this from scratch on every config change rather
/// than diffing it.
pub struct RouteIndex {
    candidates: Vec<Candidate>,
}

impl RouteIndex {
    /// Build the index from every route in `config` — every route is a
    /// classification candidate now, including the reserved `default` route
    /// (see `crate::config::DEFAULT_ROUTE`).
    ///
    /// A candidate that turns out to have no description or an unresolvable
    /// `model.default` is silently skipped rather than causing a build
    /// failure: `config` reaching this point has already passed
    /// `validate::validate`, which rejects exactly those cases for any
    /// config that is actually live, so this only needs to be defensive.
    pub fn build(config: &Config, embedder: &Embedder) -> Self {
        let candidates = config
            .routes
            .keys()
            .filter_map(|name| embed_candidate(config, embedder, name))
            .collect();

        Self { candidates }
    }
}

/// Embed every variant of one candidate route's description and resolve its
/// `ApiKind`. `None` if the description does not resolve to at least one
/// embeddable variant, or the model resolution step fails — see
/// [`RouteIndex::build`] for why that is expected to be unreachable on a live
/// config, not an error case worth surfacing here.
fn embed_candidate(
    config: &Config,
    embedder: &Embedder,
    candidate_name: &str,
) -> Option<Candidate> {
    let candidate_route = config.routes.get(candidate_name)?;
    let description = candidate_route.description.as_ref()?;
    let texts = description.texts().ok()?;
    // A variant that fails to embed (an empty tokenizer result, a caught
    // panic) is dropped rather than sinking the whole candidate — as long as
    // at least one variant survives, the candidate can still be scored on it.
    let vectors: Vec<Vec<f32>> = texts
        .iter()
        .filter_map(|text| embedder.embed(text))
        .collect();
    if vectors.is_empty() {
        return None;
    }

    // The default must resolve — same requirement as before — but a
    // fallback that does not (which should be unreachable on a validated
    // config) is simply left out of `apis` rather than sinking the whole
    // candidate.
    let default_api = ModelRef::parse(&candidate_route.model.default)
        .and_then(|model_ref| config.provider(&model_ref.provider))
        .map(|provider| provider.api)?;
    let mut apis = vec![default_api];
    apis.extend(candidate_route.model.fallbacks.iter().filter_map(|raw| {
        ModelRef::parse(raw)
            .and_then(|model_ref| config.provider(&model_ref.provider))
            .map(|provider| provider.api)
    }));

    Some(Candidate {
        name: candidate_name.to_string(),
        vectors,
        apis,
    })
}

/// Outcome of classifying one request against every candidate route.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// The winning candidate's route name and its cosine similarity to the
    /// request text, if the top-scoring candidate cleared
    /// [`CLASSIFICATION_THRESHOLD`]. `None` means the caller should fall
    /// back to the reserved `default` route — either no candidate cleared
    /// the bar, no candidate was reachable from `expected_api`, or `text`
    /// failed to embed.
    pub matched: Option<(String, f32)>,
    /// Every candidate considered (after excluding the unreachable ones —
    /// see [`rank`]), scored and sorted descending by score. Kept even when
    /// `matched` is `None`, so a caller building a trace record can still
    /// show what almost matched. Empty when `text` failed to embed or no
    /// candidate was reachable.
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

/// A candidate's score against `vector`: the **maximum** cosine similarity to
/// any of its `description` variants (see `Candidate`'s `vectors` field and
/// [`crate::config::Description`]). Panics if `vectors` is empty, which
/// [`embed_candidate`] never produces — every `Candidate` reaching [`rank`]
/// has at least one.
fn max_cosine(vector: &[f32], vectors: &[Vec<f32>]) -> f32 {
    vectors
        .iter()
        .map(|v| dot(vector, v))
        .fold(f32::NEG_INFINITY, f32::max)
}

/// Every candidate considered (after excluding the unreachable ones), scored
/// and sorted descending by score, plus the winner if one cleared
/// `threshold` — the pieces `rank` produces and `Classifier::classify`
/// assembles into a [`Verdict`].
struct Scored {
    all: Vec<(String, f32)>,
    winner: Option<(String, f32)>,
}

/// Score `candidates` against an already-embedded `vector`, keeping only
/// those the request can actually reach, and decide whether the winner clears
/// `threshold`.
///
/// Reachable means: at least one of the candidate's `apis` (its `model.default`
/// plus any `model.fallbacks`) speaks `expected_api` itself, or the gateway can
/// translate from `expected_api` to it (see [`crate::translate::Translation`]).
/// The translation case is what lets a request reached over `/v1/messages` pick
/// an `openai-chat` candidate — "send the cheap requests to local Ollama" is the
/// whole point of semantic routing for a Claude Code user, and before
/// translation existed that candidate could never win. Checking every entry in
/// `apis`, not just the default's, is what lets a route whose default the
/// request cannot reach still win through a reachable fallback — the same
/// cross-protocol fallback `crate::server::proxy` honors once a route is
/// picked.
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
        .filter(|c| {
            c.apis.iter().any(|&api| {
                api == expected_api
                    || crate::translate::Translation::select(expected_api, api).is_some()
            })
        })
        .map(|c| (c.name.clone(), max_cosine(vector, &c.vectors)))
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

/// Classifies request text against every candidate route.
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

    /// Classify `text` against every candidate route.
    ///
    /// Always returns a `Verdict`: `matched` is `None` when embedding `text`
    /// fails, no candidate is reachable from `expected_api`, or the best
    /// score misses [`CLASSIFICATION_THRESHOLD`] — and in all of those
    /// cases the caller should fall back to the reserved `default` route
    /// (`crate::config::DEFAULT_ROUTE`). `candidates` is still populated
    /// where possible, for a caller that wants to record what was
    /// considered (e.g. tracing).
    pub fn classify(&self, text: &str, expected_api: ApiKind) -> Verdict {
        self.stale_check.sync(self.shared.generation(), || {
            let config = self.shared.get();
            self.index
                .store(Arc::new(RouteIndex::build(&config, &self.embedder)));
        });

        let index = self.index.load();

        let started = Instant::now();
        let vector = self.embedder.embed(text);
        let embed_ms = started.elapsed().as_millis() as u64;

        let Some(vector) = vector else {
            // Tokenizer panic or an empty result: nothing was scored, so
            // there is no candidate list to hand back either.
            return Verdict {
                matched: None,
                candidates: Vec::new(),
                embed_ms,
            };
        };

        let scored = rank(
            &vector,
            &index.candidates,
            expected_api,
            CLASSIFICATION_THRESHOLD,
        );
        Verdict {
            matched: scored.winner,
            candidates: scored.all,
            embed_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// A single-variant candidate — most tests only need one embedding.
    fn candidate(name: &str, vector: Vec<f32>, api: ApiKind) -> Candidate {
        multi_variant_candidate(name, vec![vector], api)
    }

    /// A candidate with one embedding per `description` variant, for exercising
    /// the max-cosine scoring [`max_cosine`] does across all of them.
    fn multi_variant_candidate(name: &str, vectors: Vec<Vec<f32>>, api: ApiKind) -> Candidate {
        Candidate {
            name: name.to_string(),
            vectors,
            apis: vec![api],
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
    fn rank_excludes_candidates_the_request_cannot_reach() {
        // `openai-chat` in, `anthropic-messages` candidate: no translation
        // exists in that direction, so the candidate is unreachable.
        let candidates = vec![
            candidate("unreachable", vec![1.0, 0.0], ApiKind::AnthropicMessages),
            candidate("reachable", vec![0.9, 0.1], ApiKind::OpenaiChat),
        ];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::OpenaiChat, 0.0);

        let (route, _) = scored.winner.expect("the reachable candidate clears 0.0");
        assert_eq!(route, "reachable");
        assert_eq!(scored.all.len(), 1, "{:?}", scored.all);
    }

    #[test]
    fn rank_has_no_winner_when_no_candidate_is_reachable() {
        let candidates = vec![candidate(
            "only",
            vec![1.0, 0.0],
            ApiKind::AnthropicMessages,
        )];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::OpenaiChat, 0.0);
        assert!(scored.winner.is_none());
        assert!(scored.all.is_empty());
    }

    /// The case cross-protocol translation exists for: a Claude Code request
    /// (`anthropic-messages`) picking a local Ollama candidate
    /// (`openai-chat`). Before translation this candidate was silently
    /// excluded, which made "route cheap work to Ollama" impossible for the
    /// one client that most needs it.
    #[test]
    fn rank_keeps_a_candidate_reachable_only_through_translation() {
        let candidates = vec![candidate(
            "ollama-local",
            vec![1.0, 0.0],
            ApiKind::OpenaiChat,
        )];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::AnthropicMessages, 0.5);

        let (route, _) = scored.winner.expect("a translatable candidate can win");
        assert_eq!(route, "ollama-local");
    }

    /// The case cross-protocol fallback adds: a route whose *default* speaks a
    /// protocol nothing translates to or from (`openai-responses`), but whose
    /// *fallback* speaks one that does. Before `apis` covered fallbacks too,
    /// this candidate was unreachable and could never win; now the fallback's
    /// reachability is enough.
    #[test]
    fn rank_keeps_a_candidate_reachable_only_through_its_fallback() {
        let mixed = Candidate {
            name: "mixed".to_string(),
            vectors: vec![vec![1.0, 0.0]],
            apis: vec![ApiKind::OpenaiResponses, ApiKind::OpenaiChat],
        };

        let scored = rank(&[1.0, 0.0], &[mixed], ApiKind::AnthropicMessages, 0.5);

        let (route, _) = scored
            .winner
            .expect("reachable through the fallback's protocol");
        assert_eq!(route, "mixed");
    }

    /// The point of `max_cosine`: a candidate's score is the best of its
    /// variants, not their average or the first one — a variant that scores
    /// low against this particular query must not drag a strong match down.
    #[test]
    fn max_cosine_takes_the_best_scoring_variant_not_the_average() {
        let vectors = vec![
            vec![0.0, 1.0], // orthogonal to the query: 0.0
            vec![1.0, 0.0], // identical to the query: 1.0
        ];
        let score = max_cosine(&[1.0, 0.0], &vectors);
        assert!((score - 1.0).abs() < 1e-6, "{score}");
    }

    /// `rank` must use the same maximum, not just `max_cosine` in isolation:
    /// a multi-variant candidate should outrank a single-variant one whose
    /// only embedding is a middling match.
    #[test]
    fn rank_scores_a_candidate_by_its_best_variant() {
        let candidates = vec![
            multi_variant_candidate(
                "multi",
                vec![vec![0.0, 1.0], vec![1.0, 0.0]],
                ApiKind::AnthropicMessages,
            ),
            candidate("single", vec![0.7, 0.7], ApiKind::AnthropicMessages),
        ];

        let scored = rank(&[1.0, 0.0], &candidates, ApiKind::AnthropicMessages, 0.0);

        let (route, score) = scored.winner.expect("clears threshold 0.0");
        assert_eq!(route, "multi");
        assert!((score - 1.0).abs() < 1e-6, "{score}");
    }

    /// The scenario multi-variant descriptions exist for (see
    /// `crate::config::Description`): a route embedded once per language
    /// clears the classification threshold whether the request text is
    /// Japanese or English, because each language gets scored against its
    /// own variant rather than one language-mixed embedding diluting both.
    /// Modeled with synthetic 2-D vectors rather than the real embedder — one
    /// axis stands in for "Japanese-shaped" text, the other for
    /// "English-shaped" text — so this stays a fast, model-independent unit
    /// test like the rest of `rank`'s coverage.
    #[test]
    fn a_route_with_ja_and_en_variants_matches_either_languages_query() {
        let ja_axis = vec![1.0, 0.0];
        let en_axis = vec![0.0, 1.0];
        let candidates = vec![multi_variant_candidate(
            "role-writer",
            vec![ja_axis.clone(), en_axis.clone()],
            ApiKind::AnthropicMessages,
        )];

        let ja_query_scored = rank(
            &ja_axis,
            &candidates,
            ApiKind::AnthropicMessages,
            CLASSIFICATION_THRESHOLD,
        );
        let (route, score) = ja_query_scored
            .winner
            .expect("a Japanese-shaped query clears threshold via the ja variant");
        assert_eq!(route, "role-writer");
        assert!((score - 1.0).abs() < 1e-6, "{score}");

        let en_query_scored = rank(
            &en_axis,
            &candidates,
            ApiKind::AnthropicMessages,
            CLASSIFICATION_THRESHOLD,
        );
        let (route, score) = en_query_scored
            .winner
            .expect("an English-shaped query clears threshold via the en variant");
        assert_eq!(route, "role-writer");
        assert!((score - 1.0).abs() < 1e-6, "{score}");
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
