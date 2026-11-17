//! Shared device coefficients for the per-channel film scatter and halation filters.

use spektrafilm_math::gaussian::EXPONENTIAL_GAUSSIAN_FIT;

use crate::HalationGpuParams;

// FIR covers sigma < 3, with a maximum radius of 9.
pub(crate) const CHANNEL_STRIDE: usize = 19;

pub(crate) struct GaussianRgb {
    /// FIR radius, or -1 for Young–van Vliet IIR.
    pub radii: [i32; 3],
    /// One fixed-width row per channel, with FIR taps or IIR coefficients.
    pub weights: [[f32; CHANNEL_STRIDE]; 3],
}

impl GaussianRgb {
    pub fn new(sigma: [f64; 3]) -> Self {
        let mut radii = [0; 3];
        let mut weights = [[0.0; CHANNEL_STRIDE]; 3];
        for (c, sigma) in sigma.into_iter().enumerate() {
            if sigma >= 3.0 {
                radii[c] = -1;
                weights[c][..3].copy_from_slice(&iir_differences(sigma));
            } else {
                let radius = (3.0 * sigma.max(0.0) + 0.5) as i32;
                radii[c] = radius;
                if radius == 0 {
                    weights[c][0] = 1.0;
                    continue;
                }
                let taps: Vec<f64> = (-radius..=radius)
                    .map(|x| (-0.5 * (f64::from(x) / sigma).powi(2)).exp())
                    .collect();
                let sum = taps.iter().sum::<f64>();
                for (out, tap) in weights[c].iter_mut().zip(taps) {
                    *out = (tap / sum) as f32;
                }
            }
        }
        Self { radii, weights }
    }
}

fn iir_differences(sigma: f64) -> [f32; 3] {
    let q = 0.98711 * sigma - 0.96330;
    let q2 = q * q;
    let q3 = q2 * q;
    let b0 = 1.57825 + 2.44413 * q + 1.4281 * q2 + 0.422205 * q3;
    let b2 = -(1.4281 * q2 + 1.26661 * q3) / b0;
    let b3 = 0.422205 * q3 / b0;
    // Algebraic form of (1 - b1 - b2 - b3), without cancellation.
    let gain = (1.57825 + 0.00001 * q2) / b0;
    // The device keeps first and second differences of the recurrence.
    // This avoids subtracting several nearly equal filtered samples at high σ.
    let first_difference = -(1.0 + b2 + 2.0 * b3);
    [gain as f32, first_difference as f32, b3 as f32]
}

pub(crate) fn scatter_tail(
    params: &HalationGpuParams,
) -> impl Iterator<Item = (GaussianRgb, [f32; 3])> {
    EXPONENTIAL_GAUSSIAN_FIT
        .into_iter()
        .map(|(amplitude, ratio)| {
            let sigma = params.scatter_tail_px.map(|decay| ratio * decay.max(1e-6));
            let contribution = params
                .scatter_tail_weight
                .map(|weight| params.scatter_amount * weight * amplitude as f32);
            (GaussianRgb::new(sigma), contribution)
        })
}

pub(crate) fn halation_bounces(
    params: &HalationGpuParams,
) -> impl Iterator<Item = (GaussianRgb, f32)> {
    let weights: Vec<f64> = (0..params.halation_n_bounces)
        .map(|k| params.halation_bounce_decay.powi(k as i32))
        .collect();
    let sum: f64 = weights.iter().sum();
    weights.into_iter().enumerate().map(move |(k, weight)| {
        let scale = ((k + 1) as f64).sqrt();
        let sigma = params.halation_first_sigma_px.map(|sigma| sigma * scale);
        (GaussianRgb::new(sigma), (weight / sum) as f32)
    })
}
