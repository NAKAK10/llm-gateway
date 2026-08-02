//! A 2-D projection of route embedding vectors, for `serve --ui`'s vector map.
//!
//! Hand-written PCA (power iteration with deflation) rather than a
//! linear-algebra crate: `model2vec-rs`'s 256-dimensional embeddings and "a
//! few dozen routes, each with a couple of language variants" put this well
//! within reach of plain nested loops over `Vec<f32>`, and a whole new
//! dependency for two eigenvectors is more than the problem needs.
//!
//! Deterministic on purpose — a fixed, data-derived starting vector and a
//! fixed iteration count, never anything random — so the same route set
//! always projects to the same axes. That matters here specifically because
//! the live feed (`crate::server::live`) projects an incoming request's
//! embedding through a *freshly fit* [`Basis`] independently of whichever
//! [`Basis`] the map view last fit: as long as the underlying route vectors
//! have not changed, both calls land on the same axes and the point is
//! comparable to the map already on screen.

/// Number of power-iteration steps per component. Convergence for this
/// problem size (dozens of vectors, 256 dimensions) is fast; this is
/// generous rather than tight; it exists to cap worst-case work, not because
/// more than a handful of iterations is usually needed.
const POWER_ITERATIONS: usize = 64;

use std::sync::{Arc, Mutex, PoisonError};

/// A fitted 2-D projection: a mean to center on and two principal axes to
/// project onto.
pub struct Basis {
    mean: Vec<f32>,
    components: [Vec<f32>; 2],
}

impl Basis {
    /// Fit a basis to `vectors`. `None` if `vectors` is empty, any vector has
    /// a different length than the first, or the first is zero-length —
    /// defensive only; every real caller feeds same-model embeddings.
    pub fn fit(vectors: &[Vec<f32>]) -> Option<Self> {
        let dim = vectors.first()?.len();
        if dim == 0 || vectors.iter().any(|v| v.len() != dim) {
            return None;
        }

        let mean = mean_vector(vectors, dim);
        let centered: Vec<Vec<f32>> = vectors.iter().map(|v| sub(v, &mean)).collect();

        let c1 = power_iteration(&centered, dim);
        // Second component: the same data with the first component's
        // variance removed, so the second power iteration converges to an
        // axis orthogonal to the first without needing per-step
        // re-orthogonalization.
        let deflated: Vec<Vec<f32>> = centered.iter().map(|v| deflate(v, &c1)).collect();
        let c2 = power_iteration(&deflated, dim);

        Some(Self {
            mean,
            components: [c1, c2],
        })
    }

    /// Project one vector (same dimensionality the basis was fit on) onto
    /// the two components.
    pub fn project(&self, vector: &[f32]) -> [f32; 2] {
        let centered = sub(vector, &self.mean);
        [
            dot(&centered, &self.components[0]),
            dot(&centered, &self.components[1]),
        ]
    }
}

/// Caches the last [`Basis`] a caller fit, keyed on `crate::config::watch::
/// SharedConfig::generation` — fitting is deterministic on the same route
/// vectors (see the module docs), and the route vectors only ever change on
/// a config reload, which is exactly what bumps that counter. Keyed on
/// generation rather than on the route-vector set itself: comparing vector
/// sets for equality would mean cloning and diffing every route's embedding
/// on every single request just to decide whether a refit is needed, which
/// defeats the point of caching in the first place, while the generation
/// counter is already sitting there as a single, already-incremented
/// integer (see `crate::semantic::index`'s `StaleCheck`, which makes the
/// same trade-off for the embedding index itself).
///
/// A coarser key than "did the route set change" — any reload bumps the
/// generation, even one that only touched `server.port` — so an unrelated
/// reload can trigger one avoidable refit. That is a rare, one-time cost
/// (the next call repopulates the cache), not a per-request one, so it is
/// not worth a finer-grained key.
pub struct BasisCache {
    cached: Mutex<Option<(u64, Arc<Basis>)>>,
}

impl BasisCache {
    pub fn new() -> Self {
        Self {
            cached: Mutex::new(None),
        }
    }

    /// Returns the `Basis` fit from `vectors()` at config `generation`,
    /// reusing the last fit — without calling `vectors()` at all — when
    /// `generation` matches what the cache already has. `vectors` is a
    /// closure rather than a plain slice so a cache hit skips not just
    /// `Basis::fit` but also whatever cloning the caller would otherwise do
    /// to assemble the vector list (`Classifier::route_vectors` clones every
    /// route's embeddings) — see #27.
    ///
    /// Holds its lock across `Basis::fit` on a miss, same as
    /// `crate::semantic::index`'s `StaleCheck` does across its own rebuild:
    /// a concurrent caller blocks briefly rather than duplicating the fit,
    /// which only matters right after a reload and is still bounded work
    /// (see `POWER_ITERATIONS`).
    pub fn get_or_fit(
        &self,
        generation: u64,
        vectors: impl FnOnce() -> Vec<Vec<f32>>,
    ) -> Option<Arc<Basis>> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((seen, basis)) = cached.as_ref() {
            if *seen == generation {
                return Some(Arc::clone(basis));
            }
        }

        let basis = Arc::new(Basis::fit(&vectors())?);
        *cached = Some((generation, Arc::clone(&basis)));
        Some(basis)
    }
}

impl Default for BasisCache {
    fn default() -> Self {
        Self::new()
    }
}

fn mean_vector(vectors: &[Vec<f32>], dim: usize) -> Vec<f32> {
    let mut mean = vec![0.0f32; dim];
    for v in vectors {
        for (m, x) in mean.iter_mut().zip(v) {
            *m += x;
        }
    }
    let n = vectors.len() as f32;
    for m in &mut mean {
        *m /= n;
    }
    mean
}

fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x - y).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(v: &[f32]) -> f32 {
    dot(v, v).sqrt()
}

fn normalize(v: &mut [f32]) {
    let n = norm(v);
    if n > 1e-9 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// Remove `component`'s contribution to `v` — the projection of `v` onto
/// `component` (assumed already unit length), subtracted out.
fn deflate(v: &[f32], component: &[f32]) -> Vec<f32> {
    let c = dot(v, component);
    v.iter().zip(component).map(|(x, y)| x - c * y).collect()
}

/// The dominant eigenvector of `vectors`' covariance, via power iteration —
/// computed as repeated `Cv = Xᵀ(Xv)` products (two matrix-vector multiplies
/// against the raw data) rather than ever forming the `dim × dim` covariance
/// matrix `C` itself, which is both unnecessary work and, at `dim = 256`,
/// considerably more of it.
fn power_iteration(vectors: &[Vec<f32>], dim: usize) -> Vec<f32> {
    // A fixed, data-derived starting vector rather than a random one — see
    // the module docs on why this must be deterministic. Deliberately *not*
    // the centroid (`mean_vector`), despite that having been the original
    // intent here: `power_iteration` is only ever called on already-centered
    // (`Basis::fit`'s `centered`) or already-deflated data, so that mean is
    // mathematically exact zero and `norm(&v) < 1e-9` never actually catches
    // it — what normalizing it produced was pure f32 rounding noise (~1e-7),
    // not a meaningful starting direction, and the fallback below never ran
    // in practice (#29). The row with the largest norm is real signal
    // instead: on typical data it is unlikely to be near-orthogonal to the
    // dominant eigenvector, so iteration converges quickly, and picking it
    // is deterministic (`Iterator::max_by`'s tie-break is a fixed function
    // of `vectors`' order, which does not change between calls on the same
    // input).
    let mut v = vectors
        .iter()
        .max_by(|a, b| norm(a).total_cmp(&norm(b)))
        .cloned()
        .unwrap_or_default();
    // The degenerate case this actually guards: every row (including
    // whichever has the largest norm) is numerically zero — one vector, or
    // every vector identical before centering/deflation. No direction is
    // more correct than any other there, so fall back to a fixed one.
    if norm(&v) < 1e-9 {
        v = vec![1.0; dim];
    }
    normalize(&mut v);

    for _ in 0..POWER_ITERATIONS {
        let mut next = vec![0.0f32; dim];
        for row in vectors {
            let coeff = dot(row, &v);
            for (n, x) in next.iter_mut().zip(row) {
                *n += coeff * x;
            }
        }
        if norm(&next) < 1e-9 {
            break; // No remaining signal on this axis — keep the last `v`.
        }
        normalize(&mut next);
        v = next;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_returns_none_for_empty_input() {
        assert!(Basis::fit(&[]).is_none());
    }

    #[test]
    fn fit_returns_none_for_mismatched_lengths() {
        let vectors = vec![vec![1.0, 0.0], vec![1.0, 0.0, 0.0]];
        assert!(Basis::fit(&vectors).is_none());
    }

    #[test]
    fn a_single_vector_projects_to_the_origin() {
        // No variance to speak of — every direction is equally arbitrary, so
        // the only reasonable projection is the origin (the vector equals
        // its own mean).
        let basis = Basis::fit(&[vec![1.0, 2.0, 3.0]]).unwrap();
        let [x, y] = basis.project(&[1.0, 2.0, 3.0]);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6, "{x} {y}");
    }

    #[test]
    fn the_dominant_axis_separates_the_two_far_apart_clusters() {
        // Two well-separated clusters along one axis: the first principal
        // component must point along it, so the two clusters end up far
        // apart on the projected x-axis and close together on y.
        let vectors = vec![
            vec![10.0, 0.0],
            vec![10.1, 0.1],
            vec![-10.0, 0.0],
            vec![-10.1, -0.1],
        ];
        let basis = Basis::fit(&vectors).unwrap();

        let [x_pos, _] = basis.project(&[10.0, 0.0]);
        let [x_neg, _] = basis.project(&[-10.0, 0.0]);
        assert!((x_pos - x_neg).abs() > 5.0, "{x_pos} {x_neg}");
    }

    #[test]
    fn fitting_twice_on_the_same_data_is_deterministic() {
        let vectors = vec![
            vec![1.0, 2.0, 3.0, 4.0],
            vec![4.0, 3.0, 2.0, 1.0],
            vec![0.5, -1.0, 2.0, 0.0],
            vec![-2.0, 1.0, 0.0, 3.0],
        ];
        let a = Basis::fit(&vectors).unwrap();
        let b = Basis::fit(&vectors).unwrap();

        let query = vec![1.5, 0.5, -1.0, 2.0];
        assert_eq!(a.project(&query), b.project(&query));
    }

    #[test]
    fn the_two_components_are_orthogonal() {
        let vectors = vec![
            vec![3.0, 1.0, 0.0],
            vec![1.0, 3.0, 0.0],
            vec![0.0, 0.0, 5.0],
            vec![0.0, 0.0, -5.0],
            vec![2.0, 2.0, 1.0],
        ];
        let basis = Basis::fit(&vectors).unwrap();
        let overlap = dot(&basis.components[0], &basis.components[1]);
        assert!(overlap.abs() < 1e-4, "{overlap}");
    }

    #[test]
    fn power_iteration_is_deterministic_across_repeated_calls() {
        // #29: the starting vector must be a fixed function of the input,
        // not something that drifts with f32 rounding noise.
        let vectors = vec![
            vec![1.0, 2.0, 3.0],
            vec![-1.0, 0.5, 2.0],
            vec![0.2, -3.0, 1.0],
        ];
        let a = power_iteration(&vectors, 3);
        let b = power_iteration(&vectors, 3);
        assert_eq!(a, b);
    }

    #[test]
    fn get_or_fit_reuses_the_cached_basis_for_the_same_generation() {
        let cache = BasisCache::new();
        let calls = std::cell::Cell::new(0);
        let vectors = || {
            calls.set(calls.get() + 1);
            vec![vec![1.0, 0.0], vec![-1.0, 0.0], vec![0.0, 1.0]]
        };

        let first = cache.get_or_fit(1, vectors).unwrap();
        let second = cache.get_or_fit(1, vectors).unwrap();

        // Same generation: `vectors` must only have run once, and both
        // calls hand back the very same fit (not just an equal one).
        assert_eq!(calls.get(), 1);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn get_or_fit_refits_when_the_generation_changes() {
        let cache = BasisCache::new();
        let calls = std::cell::Cell::new(0);
        let vectors = || {
            calls.set(calls.get() + 1);
            vec![vec![1.0, 0.0], vec![-1.0, 0.0], vec![0.0, 1.0]]
        };

        let first = cache.get_or_fit(1, vectors).unwrap();
        let second = cache.get_or_fit(2, vectors).unwrap();

        assert_eq!(calls.get(), 2);
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
