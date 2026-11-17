use rayon::prelude::*;
use std::sync::Arc;

use crate::PreviewTextureData;

const EPSILON: f32 = 1.0e-4;
const GAMMA: f32 = 2.2;
const MAX_INPUT_DENSITY: f32 = 6.0;
const MAX_RENDER_DENSITY: f32 = 8.0;
const MAX_ANIMATED_PIXELS: usize = 1_500_000;
const DEVELOPMENT_FRAMES: usize = 50;
const CROSSFADE_FRAMES: usize = 10;

type Rgb = [f32; 3];

struct PixelDensity {
    density: Rgb,
    chroma: Rgb,
    neutral: f32,
}

pub(super) struct Development {
    size: [usize; 2],
    layers: Vec<[Rgb; 9]>,
}

impl Development {
    pub fn prepare(image: &PreviewTextureData) -> Self {
        let pixels: Vec<_> = image
            .rgba
            .par_chunks_exact(4)
            .map(|rgba| {
                let density = [rgba[0], rgba[1], rgba[2]]
                    .map(|code| {
                        -(f32::from(code) / 255.0)
                            .clamp(EPSILON, 1.0)
                            .powf(GAMMA)
                            .ln()
                    })
                    .map(|value| value.clamp(0.0, MAX_INPUT_DENSITY));
                let neutral = density.into_iter().fold(f32::INFINITY, f32::min);
                let chroma = density.map(|value| value - neutral);
                PixelDensity {
                    density,
                    chroma,
                    neutral,
                }
            })
            .collect();
        let maxima = pixels
            .par_iter()
            .map(|p| {
                [
                    p.density[0],
                    p.density[1],
                    p.density[2],
                    p.chroma[0],
                    p.chroma[1],
                    p.chroma[2],
                    p.neutral,
                ]
            })
            .reduce(|| [0.0; 7], |a, b| std::array::from_fn(|i| a[i].max(b[i])));
        let layers = pixels
            .into_par_iter()
            .map(|p| build_layers(p, maxima))
            .collect();
        Self {
            size: image.size,
            layers,
        }
    }

    pub fn frame(&self, t: f64) -> PreviewTextureData {
        let coefficients = layer_coefficients(t);
        let mut rgba = vec![255; self.layers.len() * 4];
        rgba.par_chunks_exact_mut(4)
            .zip(&self.layers)
            .for_each(|(rgba, layers)| {
                let density: Rgb = std::array::from_fn(|channel| {
                    layers
                        .iter()
                        .zip(coefficients)
                        .map(|(layer, weight)| layer[channel] * weight)
                        .sum()
                });
                for (code, density) in rgba.iter_mut().zip(density) {
                    let light = (-density.clamp(0.0, MAX_RENDER_DENSITY) / GAMMA).exp();
                    *code = (light * 255.0).round_ties_even() as u8;
                }
            });
        PreviewTextureData {
            size: self.size,
            rgba,
        }
    }
}

fn build_layers(pixel: PixelDensity, maxima: [f32; 7]) -> [Rgb; 9] {
    let [a, b, c] = pixel.chroma;
    let density_latent: Rgb = std::array::from_fn(|i| pixel.density[i] / (maxima[i] + EPSILON));
    let chroma_latent: Rgb = std::array::from_fn(|i| pixel.chroma[i] / (maxima[i + 3] + EPSILON));
    let neutral_latent = pixel.neutral / (maxima[6] + EPSILON);
    let shadow = (0.34 * pixel.density[0] + 0.39 * pixel.density[1] + 0.27 * pixel.density[2])
        .clamp(0.0, 1.75)
        / 1.75;
    let highlight = 1.0 - shadow;
    let chroma_strength = chroma_latent.into_iter().fold(0.0, f32::max);
    let warmth = ((b + 1.25 * c - 0.85 * a) / (a + b + c + EPSILON)).clamp(0.0, 1.0);

    let receive = [0.04, 0.08, 0.15].map(|v| (0.30 + 0.70 * highlight) * v);
    let silver_map = (0.24 + 0.46 * shadow + 0.30 * neutral_latent).clamp(0.0, 1.0);
    let silver = std::array::from_fn(|i| {
        (silver_map * [0.18, 0.19, 0.20][i] + chroma_latent[(i + 1) % 3] * [0.03, 0.02, 0.04][i])
            .clamp(0.0, 0.28)
    });
    let anti = std::array::from_fn(|i| {
        (highlight * [0.10, 0.06, 0.02][i]
            + (1.0 - density_latent[(i + 2) % 3]) * [0.13, 0.07, 0.18][i]
            + chroma_latent[(i + 1) % 3] * [0.04, 0.02, 0.06][i])
            * (0.40 + 0.60 * highlight)
    });
    let warm = [0.02, 0.08, 0.19]
        .map(|v| (0.24 + 0.42 * highlight + 0.34 * warmth.max(chroma_strength)) * v);
    let stain = [0.03, 0.06, 0.11].map(|v| highlight * v);
    [
        [pixel.neutral; 3],
        [0.0, 0.0, c],
        [0.0, b, 0.0],
        [a, 0.0, 0.0],
        receive,
        silver,
        anti,
        warm,
        stain,
    ]
}

fn normalized_sigmoid(t: f64, center: f64, slope: f64) -> f64 {
    let sigmoid = |t: f64| 1.0 / (1.0 + (-t.clamp(-60.0, 60.0)).exp());
    let start = sigmoid(-center * slope);
    let end = sigmoid((1.0 - center) * slope);
    let value = sigmoid((t - center) * slope);
    ((value - start) / (end - start + 1e-4)).clamp(0.0, 1.0)
}

fn delayed_rise(t: f64, center: f64, slope: f64, tail: f64) -> f64 {
    let rise = normalized_sigmoid(t, center, slope);
    rise + (1.0 - rise) * t.powf(tail)
}

fn pulse(t: f64, on: [f64; 2], off: [f64; 2], decay: f64) -> f64 {
    let rising = normalized_sigmoid(t, on[0], on[1]);
    let falling = 1.0 - normalized_sigmoid(t, off[0], off[1]);
    rising * falling * (1.0 - t).powf(decay)
}

fn layer_coefficients(t: f64) -> [f32; 9] {
    let t = t.clamp(0.0, 1.0);
    let body = delayed_rise(t, 0.19, 7.4, 3.8);
    [
        body * delayed_rise(t, 0.12, 8.0, 2.8),
        body * delayed_rise(t, 0.22, 11.0, 3.0),
        body * delayed_rise(t, 0.31, 10.2, 3.4),
        body * delayed_rise(t, 0.52, 9.4, 5.4),
        1.08 * (1.0 - t).powf(0.46),
        pulse(t, [0.05, 32.0], [0.44, 8.0], 0.24),
        1.22 * pulse(t, [0.10, 54.0], [0.28, 28.0], 0.18),
        0.96 * pulse(t, [0.12, 20.0], [0.80, 6.5], 0.34),
        0.68 * pulse(t, [0.03, 24.0], [0.98, 4.0], 0.08),
    ]
    .map(|coefficient| coefficient as f32)
}

pub(super) enum Transition {
    Develop(Development),
    Crossfade(Arc<PreviewTextureData>),
}

impl Transition {
    pub fn can_animate(size: [usize; 2]) -> bool {
        size[0] * size[1] <= MAX_ANIMATED_PIXELS
    }

    pub fn prepare(
        previous: Option<Arc<PreviewTextureData>>,
        target: &PreviewTextureData,
        was_visible: bool,
    ) -> Option<Self> {
        if !Self::can_animate(target.size) {
            return None;
        }
        if was_visible {
            previous
                .filter(|previous| previous.size == target.size)
                .map(Self::Crossfade)
        } else {
            Some(Self::Develop(Development::prepare(target)))
        }
    }

    pub fn frame(&self, target: &PreviewTextureData, step: usize) -> Option<PreviewTextureData> {
        match self {
            Self::Develop(state) if step < DEVELOPMENT_FRAMES - 1 => {
                let t = (step as f64 / (DEVELOPMENT_FRAMES - 1) as f64) as f32;
                Some(state.frame(f64::from(t)))
            }
            Self::Crossfade(source) if step < CROSSFADE_FRAMES => {
                let amount = step as f32 / CROSSFADE_FRAMES as f32;
                let rgba = source
                    .rgba
                    .par_iter()
                    .zip(&target.rgba)
                    .map(|(&a, &b)| {
                        let start = f32::from(a) / 255.0;
                        let end = f32::from(b) / 255.0;
                        (((1.0 - amount) * start + amount * end) * 255.0).round_ties_even() as u8
                    })
                    .collect();
                Some(PreviewTextureData {
                    size: target.size,
                    rgba,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_frames_match_python() {
        let rgb = [
            220, 87, 20, 255, 255, 255, 9, 43, 19, 0, 31, 255, 123, 108, 150, 24, 24, 26,
        ];
        let target = PreviewTextureData {
            size: [3, 2],
            rgba: rgb
                .chunks_exact(3)
                .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                .collect(),
        };
        let transition = Transition::prepare(None, &target, false).unwrap();
        for (step, expected) in [
            (
                0,
                [
                    254, 252, 249, 250, 245, 237, 254, 252, 249, 254, 252, 249, 253, 251, 248, 254,
                    252, 249,
                ],
            ),
            (
                12,
                [
                    228, 190, 112, 223, 222, 202, 130, 132, 102, 213, 169, 223, 197, 187, 190, 113,
                    111, 110,
                ],
            ),
            (
                26,
                [
                    218, 99, 25, 248, 240, 227, 32, 51, 24, 64, 42, 238, 141, 117, 151, 32, 31, 32,
                ],
            ),
            (
                48,
                [
                    220, 87, 20, 254, 253, 252, 17, 43, 19, 17, 31, 254, 123, 108, 149, 24, 24, 26,
                ],
            ),
        ] {
            let frame = transition.frame(&target, step).unwrap();
            let rgb: Vec<_> = frame
                .rgba
                .chunks_exact(4)
                .flat_map(|p| p[..3].iter().copied())
                .collect();
            assert_eq!(rgb, expected, "step {step}");
        }
        assert!(transition.frame(&target, 49).is_none());
    }

    #[test]
    fn incompatible_shapes_and_large_outputs_skip_crossfade() {
        let target = PreviewTextureData {
            size: [1, 1],
            rgba: vec![2, 250, 50, 255],
        };
        let original = PreviewTextureData {
            size: [1, 1],
            rgba: vec![50, 100, 150, 255],
        };
        let fade = Transition::prepare(Some(Arc::new(original)), &target, true).unwrap();
        assert_eq!(fade.frame(&target, 5).unwrap().rgba, [26, 175, 100, 255]);
        assert!(fade.frame(&target, 10).is_none());
        let differently_shaped = PreviewTextureData {
            size: [1, 2],
            rgba: vec![100; 8],
        };
        assert!(Transition::prepare(Some(Arc::new(differently_shaped)), &target, true).is_none());
        assert!(!Transition::can_animate([2000, 1000]));
    }
}
