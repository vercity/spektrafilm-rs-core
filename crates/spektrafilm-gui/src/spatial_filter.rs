//! High-order output texture interpolation for the OpenGL and wgpu viewers.
//! The kernel grid follows the spatial filters used by VisPy's ImageVisual.

use std::{
    f64::consts::PI,
    sync::{Arc, LazyLock, mpsc},
    time::Duration,
};

use eframe::egui;
use rayon::prelude::*;

use crate::PreviewTextureData;

pub(super) const OUTPUT_INTERPOLATIONS: [&str; 7] = [
    "nearest", "linear", "cubic", "spline16", "spline36", "lanczos", "blackman",
];

pub(super) fn high_order(name: &str) -> bool {
    Filter::for_name(name).is_some()
}

const SAMPLES: usize = 1024;
const MAX_TAPS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Filter {
    Cubic,
    Spline16,
    Spline36,
    Lanczos,
    Blackman,
}

impl Filter {
    fn for_name(name: &str) -> Option<Self> {
        Some(match name {
            "cubic" => Self::Cubic,
            "spline16" => Self::Spline16,
            "spline36" => Self::Spline36,
            "lanczos" => Self::Lanczos,
            "blackman" => Self::Blackman,
            _ => return None,
        })
    }

    fn radius(self) -> usize {
        match self {
            Self::Cubic | Self::Spline16 => 2,
            Self::Spline36 => 3,
            Self::Lanczos | Self::Blackman => 4,
        }
    }

    fn weight(self, x: f64) -> f64 {
        let sinc = |x: f64| if x == 0.0 { 1.0 } else { x.sin() / x };
        match self {
            Self::Cubic => (2.0 - x).powi(3) / 6.0,
            Self::Spline16 if x < 1.0 => 1.0 + x * (-0.2 + x * (-1.8 + x)),
            Self::Spline16 => {
                let e = x - 1.0;
                e * (-7.0 / 15.0 + e * (0.8 - e / 3.0))
            }
            Self::Spline36 if x < 1.0 => {
                1.0 + x * (-3.0 / 209.0 + x * (-453.0 / 209.0 + x * 13.0 / 11.0))
            }
            Self::Spline36 if x < 2.0 => {
                let e = x - 1.0;
                e * (-156.0 / 209.0 + e * (270.0 / 209.0 - e * 6.0 / 11.0))
            }
            Self::Spline36 => {
                let e = x - 2.0;
                e * (26.0 / 209.0 + e * (-45.0 / 209.0 + e / 11.0))
            }
            Self::Lanczos => sinc(x * PI) * sinc(x * PI / 4.0),
            Self::Blackman => {
                let window = x * PI / 4.0;
                sinc(x * PI) * (0.42 + 0.5 * window.cos() + 0.08 * (2.0 * window).cos())
            }
        }
    }

    fn table(self) -> &'static [f32; SAMPLES] {
        static TABLES: LazyLock<[[f32; SAMPLES]; 5]> = LazyLock::new(|| {
            [
                Filter::Cubic,
                Filter::Spline16,
                Filter::Spline36,
                Filter::Lanczos,
                Filter::Blackman,
            ]
            .map(|filter| {
                let radius = filter.radius();
                let per_band = SAMPLES / radius;
                let mut weights: [f64; SAMPLES] = std::array::from_fn(|i| {
                    filter.weight(i as f64 * radius as f64 / (SAMPLES - 1) as f64)
                });
                let normalizers: Vec<_> = (0..per_band)
                    .map(|phase| {
                        (0..radius)
                            .map(|band| {
                                let index = band * per_band + phase;
                                weights[index] + weights[SAMPLES - 1 - index]
                            })
                            .sum::<f64>()
                    })
                    .collect();
                for band in weights.chunks_exact_mut(per_band) {
                    for (weight, normalizer) in band.iter_mut().zip(&normalizers) {
                        *weight /= normalizer;
                    }
                }
                weights.map(|weight| weight as f32)
            })
        });
        &TABLES[self as usize]
    }
}

#[derive(Clone, Copy)]
struct Taps {
    indices: [usize; MAX_TAPS],
    weights: [f32; MAX_TAPS],
}

fn kernel_sample(table: &[f32; SAMPLES], coordinate: f32) -> f32 {
    let sample = coordinate * SAMPLES as f32;
    let index = sample.floor() as usize;
    let amount = sample - index as f32;
    let a = table[index.min(SAMPLES - 1)];
    let b = table[(index + 1).min(SAMPLES - 1)];
    a + amount * (b - a)
}

fn taps(filter: Filter, coordinate: f32, length: usize) -> Taps {
    let mut taps = Taps {
        indices: [0; MAX_TAPS],
        weights: [0.0; MAX_TAPS],
    };
    let radius = filter.radius() as isize;
    let table = filter.table();
    let base = coordinate.floor() as isize;
    let fraction = coordinate - base as f32;
    // The shader accumulates left/right taps alternately.
    for k in 0..radius {
        for (pair, delta) in [(0, k + 1 - radius), (1, k + 1)] {
            let slot = k as usize * 2 + pair;
            taps.indices[slot] = (base + delta).clamp(0, length as isize - 1) as usize;
            taps.weights[slot] =
                kernel_sample(table, (delta as f32 - fraction).abs() / radius as f32);
        }
    }
    taps
}

fn render_region(
    source: &PreviewTextureData,
    filter: Filter,
    size: [usize; 2],
    first_uv: [f32; 2],
    step_uv: [f32; 2],
) -> PreviewTextureData {
    let [width, height] = size;
    let [source_width, source_height] = source.size;
    let taps_in_axis = filter.radius() * 2;
    let horizontal: Vec<_> = (0..width)
        .map(|x| {
            taps(
                filter,
                (first_uv[0] + x as f32 * step_uv[0]) * source_width as f32 - 0.5,
                source_width,
            )
        })
        .collect();
    let mut vertical: Vec<_> = (0..height)
        .map(|y| {
            taps(
                filter,
                (first_uv[1] + y as f32 * step_uv[1]) * source_height as f32 - 0.5,
                source_height,
            )
        })
        .collect();

    let mut used_rows: Vec<_> = vertical
        .iter()
        .flat_map(|row| row.indices[..taps_in_axis].iter().copied())
        .collect();
    used_rows.sort_unstable();
    used_rows.dedup();
    for tap in &mut vertical {
        for index in &mut tap.indices[..taps_in_axis] {
            *index = used_rows.binary_search(index).expect("source row retained");
        }
    }
    let rows: Vec<Vec<[f32; 3]>> = used_rows
        .par_iter()
        .map(|&y| {
            let row = &source.rgba[(y * source_width * 4)..][..source_width * 4];
            horizontal
                .iter()
                .map(|taps| {
                    let mut color = [0.0; 3];
                    for tap in 0..taps_in_axis {
                        let pixel = &row[(taps.indices[tap] * 4)..][..3];
                        for (sample, &component) in color.iter_mut().zip(pixel) {
                            *sample += taps.weights[tap] * f32::from(component);
                        }
                    }
                    color
                })
                .collect()
        })
        .collect();

    let mut rgba = vec![255; width * height * 4];
    rgba.par_chunks_exact_mut(width * 4)
        .zip(vertical)
        .for_each(|(row, taps)| {
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                let mut color = [0.0; 3];
                for tap in 0..taps_in_axis {
                    for (sample, component) in color.iter_mut().zip(rows[taps.indices[tap]][x]) {
                        *sample += taps.weights[tap] * component;
                    }
                }
                for (code, sample) in pixel.iter_mut().zip(color) {
                    *code = sample.clamp(0.0, 255.0).round_ties_even() as u8;
                }
            }
        });
    PreviewTextureData { size, rgba }
}

struct Request {
    source: Arc<PreviewTextureData>,
    filter: Filter,
    texel_rect: egui::Rect,
    image_rect: egui::Rect,
    pixels_per_point: f32,
}

impl PartialEq for Request {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source)
            && self.filter == other.filter
            && self.texel_rect == other.texel_rect
            && self.image_rect == other.image_rect
            && self.pixels_per_point == other.pixels_per_point
    }
}

impl Request {
    fn render(&self) -> PreviewTextureData {
        let size = [
            self.texel_rect.width() as usize,
            self.texel_rect.height() as usize,
        ];
        let scale = self.image_rect.size() * self.pixels_per_point;
        let uv_offset = self.texel_rect.min.to_vec2()
            - self.image_rect.min.to_vec2() * self.pixels_per_point
            + egui::Vec2::splat(0.5);
        render_region(
            &self.source,
            self.filter,
            size,
            [uv_offset.x / scale.x, uv_offset.y / scale.y],
            [1.0 / scale.x, 1.0 / scale.y],
        )
    }

    fn draw_rect(&self) -> egui::Rect {
        egui::Rect::from_min_max(
            self.texel_rect.min / self.pixels_per_point,
            self.texel_rect.max / self.pixels_per_point,
        )
    }
}

#[derive(Default)]
pub(super) struct FilteredViewport {
    pending: Option<(Request, mpsc::Receiver<PreviewTextureData>)>,
    saved: Option<(Request, egui::TextureHandle)>,
}

impl FilteredViewport {
    pub fn source_changed(&mut self) {
        self.saved = None;
    }

    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        source: Arc<PreviewTextureData>,
        image_rect: egui::Rect,
        viewport: egui::Rect,
        interpolation: &str,
    ) -> Option<(egui::TextureHandle, egui::Rect)> {
        let filter = Filter::for_name(interpolation)?;
        let visible = image_rect.intersect(viewport);
        if !visible.is_positive() {
            return None;
        }
        let pixels_per_point = ctx.pixels_per_point();
        let snap = |position: egui::Pos2| {
            let physical = position * pixels_per_point;
            egui::pos2((physical.x - 0.5).ceil(), (physical.y - 0.5).ceil())
        };
        let texel_rect = egui::Rect::from_min_max(snap(visible.min), snap(visible.max));
        if !texel_rect.is_positive() {
            return None;
        }
        let request = Request {
            source,
            filter,
            texel_rect,
            image_rect,
            pixels_per_point,
        };
        if let Some((pending, reply)) = &self.pending {
            match reply.try_recv() {
                Ok(buffer) => {
                    if *pending == request {
                        let image =
                            egui::ColorImage::from_rgba_unmultiplied(buffer.size, &buffer.rgba);
                        let texture = ctx.load_texture(
                            "filtered output viewport",
                            image,
                            egui::TextureOptions::NEAREST,
                        );
                        self.saved = self.pending.take().map(|(key, _)| (key, texture));
                    } else {
                        self.pending = None;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(16))
                }
            }
        }
        if let Some((key, texture)) = self.saved.as_ref().filter(|(key, _)| *key == request) {
            return Some((texture.clone(), key.draw_rect()));
        }

        if self.pending.is_none() {
            let work = Request {
                source: Arc::clone(&request.source),
                ..request
            };
            let ctx = ctx.clone();
            let (send, recv) = mpsc::channel();
            let worker = std::thread::Builder::new()
                .name("spektrafilm-display-interpolation".into())
                .spawn(move || {
                    let _ = send.send(work.render());
                    ctx.request_repaint();
                });
            match worker {
                Ok(_) => self.pending = Some((request, recv)),
                Err(error) => tracing::warn!(%error, "could not interpolate displayed pixels"),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn colorful_source() -> PreviewTextureData {
        let rgba = (0..4)
            .flat_map(|y| {
                (0..5).flat_map(move |x| {
                    [
                        ((x * 53 + y * 79) % 256) as u8,
                        ((x * 17 + y * 23 + 29) % 256) as u8,
                        ((x * 97 + y * 11) % 256) as u8,
                        255,
                    ]
                })
            })
            .collect();
        PreviewTextureData { size: [5, 4], rgba }
    }

    #[test]
    fn display_filters_match_vispy_kernel_and_shader_samples() {
        let source = colorful_source();
        for (mode, expected) in [
            ("cubic", [[4, 30, 6], [160, 93, 152], [184, 163, 163]]),
            ("spline16", [[0, 26, 0], [186, 95, 162], [193, 166, 167]]),
            ("spline36", [[0, 26, 0], [190, 95, 162], [193, 166, 169]]),
            ("lanczos", [[0, 26, 0], [190, 95, 159], [193, 165, 170]]),
            ("blackman", [[0, 26, 0], [189, 95, 162], [192, 165, 167]]),
        ] {
            let output = render_region(
                &source,
                Filter::for_name(mode).unwrap(),
                [17, 13],
                [0.06, -0.03],
                [0.063, 0.075],
            );
            for ((x, y), rgb) in [(0, 0), (8, 6), (16, 12)].into_iter().zip(expected) {
                let index = (y * output.size[0] + x) * 4;
                assert_eq!(&output.rgba[index..index + 3], rgb, "{mode} at ({x}, {y})");
            }
        }
    }

    #[test]
    fn higher_order_viewport_tracks_pan_and_the_selected_filter() {
        let ctx = egui::Context::default();
        let id = |mode: &str,
                  image: egui::Rect,
                  viewport: egui::Rect,
                  filtered: &mut FilteredViewport,
                  source: &Arc<PreviewTextureData>| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(result) =
                    filtered.texture(&ctx, Arc::clone(source), image, viewport, mode)
                {
                    break result;
                }
                assert!(
                    Instant::now() < deadline,
                    "high-order texture worker timed out"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut filtered = FilteredViewport::default();
        let source = Arc::new(colorful_source());
        let viewport = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(30.0, 22.0));
        let image = egui::Rect::from_min_max(egui::pos2(3.0, 6.0), egui::pos2(60.0, 90.0));
        assert!(
            filtered
                .texture(&ctx, Arc::clone(&source), image, viewport, "linear")
                .is_none()
        );
        let (texture, rect) = id("spline36", image, viewport, &mut filtered, &source);
        assert_eq!(rect.min, egui::pos2(3.0, 6.0));
        assert_eq!(rect.max, egui::pos2(30.0, 22.0));
        assert_eq!(texture.size(), [27, 16]);
        let (new, _) = id("cubic", image, viewport, &mut filtered, &source);
        assert_ne!(texture.id(), new.id());
        let image = image.translate(egui::vec2(-2.0, 5.0));
        let (_, rect) = id("cubic", image, viewport, &mut filtered, &source);
        assert_eq!(rect.min, egui::pos2(1.0, 11.0));
    }
}
