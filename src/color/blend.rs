use oklab::Oklab;

/// Linear interpolation between two Oklab colors.
pub fn lerp_oklab(start: Oklab, end: Oklab, t: f32) -> Oklab {
    Oklab {
        l: start.l + t * (end.l - start.l),
        a: start.a + t * (end.a - start.a),
        b: start.b + t * (end.b - start.b),
    }
}

/// Average a list of Oklab colors component-wise; `None` when empty.
pub fn average_oklab(colors: &[Oklab]) -> Option<Oklab> {
    if colors.is_empty() {
        return None;
    }
    let mut sum = Oklab {
        l: 0.0,
        a: 0.0,
        b: 0.0,
    };
    for c in colors {
        sum.l += c.l;
        sum.a += c.a;
        sum.b += c.b;
    }
    let inv_n = 1.0 / colors.len() as f32;
    Some(Oklab {
        l: sum.l * inv_n,
        a: sum.a * inv_n,
        b: sum.b * inv_n,
    })
}

/// Helper function for tests.
pub fn blend_weights(normalized_scores: &[f32], steepness: f32) -> Vec<f32> {
    if normalized_scores.is_empty() {
        return Vec::new();
    }
    let max_delta = normalized_scores
        .iter()
        .map(|&t| (t - 0.5).abs())
        .fold(0.0_f32, f32::max);

    let mut weights: Vec<f32> = if max_delta <= 1e-6 {
        vec![1.0 / normalized_scores.len() as f32; normalized_scores.len()]
    } else {
        let inv_max_delta = 1.0 / max_delta;
        normalized_scores
            .iter()
            .map(|&t| (((t - 0.5).abs()) * inv_max_delta).powf(steepness))
            .collect()
    };

    let total_weight: f32 = weights.iter().sum();
    if total_weight > 0.0 {
        let inv_total = 1.0 / total_weight;
        for w in weights.iter_mut() {
            *w *= inv_total;
        }
    }
    weights
}
