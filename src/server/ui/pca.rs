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
    // A fixed, data-derived starting vector (the centroid) rather than a
    // random one — see the module docs on why this must be deterministic.
    // Falls back to an arbitrary fixed direction only in the degenerate case
    // (all-zero data: one vector, or every vector identical), where no
    // direction is more correct than any other.
    let mut v = mean_vector(vectors, dim);
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
}
