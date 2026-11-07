/// Lawson-Hanson Non-Negative Least Squares (NNLS) solver using precomputed Gram matrix A^T A and A^T b.
use crate::global;

pub fn nnls_core(at_a: &[Vec<f32>], at_b: &[f32], max_iter: usize) -> Vec<f32> {
    let n = at_b.len();
    if n == 0 {
        return Vec::new();
    }

    let mut x = vec![0.0_f32; n];
    let mut passive = vec![false; n];
    let mut w = at_b.to_vec();

    let mut pass_indices = Vec::with_capacity(n);
    let mut z = vec![0.0_f32; n];
    let mut aug = Vec::new();

    for _ in 0..max_iter {
        let mut max_w = 0.0_f32;
        let mut max_idx = None;
        for i in 0..n {
            if !passive[i] && w[i] > max_w {
                max_w = w[i];
                max_idx = Some(i);
            }
        }

        let j = match max_idx {
            Some(idx) if max_w > 1e-6 => idx,
            _ => break,
        };

        passive[j] = true;

        loop {
            pass_indices.clear();
            for i in 0..n {
                if passive[i] {
                    pass_indices.push(i);
                }
            }

            let k = pass_indices.len();
            if k == 0 {
                break;
            }

            solve_sub_system_in_place(at_a, at_b, &pass_indices, &mut aug, &mut z);

            let mut all_pos = true;
            for &pi in &pass_indices {
                if z[pi] <= 1e-7 {
                    all_pos = false;
                    break;
                }
            }

            if all_pos {
                x.copy_from_slice(&z);
                break;
            }

            let mut alpha = f32::INFINITY;
            for &pi in &pass_indices {
                if z[pi] <= 1e-7 {
                    let denom = x[pi] - z[pi];
                    if denom.abs() > 1e-9 {
                        let a_val = x[pi] / denom;
                        if a_val < alpha {
                            alpha = a_val;
                        }
                    }
                }
            }

            if alpha.is_infinite() || alpha < 0.0 {
                alpha = 0.0;
            }

            for i in 0..n {
                x[i] += alpha * (z[i] - x[i]);
            }

            for &pi in &pass_indices {
                if x[pi].abs() <= 1e-6 {
                    x[pi] = 0.0;
                    passive[pi] = false;
                }
            }
        }

        for i in 0..n {
            let mut ax_i = 0.0_f32;
            for j in 0..n {
                ax_i += at_a[i][j] * x[j];
            }
            w[i] = at_b[i] - ax_i;
        }
    }

    x
}

/// Lawson-Hanson Non-Negative Least Squares (NNLS) solver.
/// Solves min || A x - b ||_2 s.t. x >= 0.
pub fn nnls(columns: &[Vec<f32>], b: &[f32], max_iter: usize) -> Vec<f32> {
    let n = columns.len();
    if n == 0 {
        return Vec::new();
    }

    let at_b: Vec<f32> = columns.iter().map(|col| global::dot(col, b)).collect();

    let mut at_a = vec![vec![0.0_f32; n]; n];
    for i in 0..n {
        for j in 0..n {
            at_a[i][j] = global::dot(&columns[i], &columns[j]);
        }
    }

    nnls_core(&at_a, &at_b, max_iter)
}

/// In-place solver for sub-system A^T A * z = A^T b on passive index set using flat augmented matrix.
fn solve_sub_system_in_place(
    at_a: &[Vec<f32>],
    at_b: &[f32],
    pass_indices: &[usize],
    aug: &mut Vec<f32>,
    z: &mut [f32],
) {
    let k = pass_indices.len();
    let stride = k + 1;
    aug.clear();
    aug.resize(k * stride, 0.0);

    for (r, &pi) in pass_indices.iter().enumerate() {
        for (c, &pj) in pass_indices.iter().enumerate() {
            aug[r * stride + c] = at_a[pi][pj];
        }
        aug[r * stride + k] = at_b[pi];
    }

    for i in 0..k {
        let mut max_row = i;
        let mut max_val = aug[i * stride + i].abs();
        for r in (i + 1)..k {
            let val = aug[r * stride + i].abs();
            if val > max_val {
                max_val = val;
                max_row = r;
            }
        }

        if max_row != i {
            for c in i..=k {
                aug.swap(i * stride + c, max_row * stride + c);
            }
        }

        let pivot = aug[i * stride + i];
        if pivot.abs() < 1e-9 {
            continue;
        }

        let inv_pivot = 1.0 / pivot;
        for c in i..=k {
            aug[i * stride + c] *= inv_pivot;
        }

        for r in 0..k {
            if r != i {
                let factor = aug[r * stride + i];
                for c in i..=k {
                    aug[r * stride + c] -= factor * aug[i * stride + c];
                }
            }
        }
    }

    z.fill(0.0);
    for (r, &pi) in pass_indices.iter().enumerate() {
        z[pi] = aug[r * stride + k];
    }
}
