//! Lensfun 0.3.4 correction order and coordinate conventions.
//!
//! The API layer in lensfun-rs uses a newer, focal-length-based frame.
//! Keeping the native 0.3.4 frame matches the Python RAW path, including
//! the auto-scale search and the calibration crop/aspect adjustments.

use std::f64::consts::PI;

use anyhow::{Result, ensure};
use lensfun::{
    DistortionModel, Lens, TcaModel, VignettingModel,
    mod_coord::dist_poly5,
    mod_subpix::{tca_linear, tca_poly3_forward},
};
use rayon::prelude::*;
use spektrafilm_math::{
    image::ImageBuf,
    precision::{from_f64, to_f64},
};

use super::{projection::Projection, sample};

pub(super) struct Correction {
    frame: Frame,
    projection: Option<Projection>,
    distortion: DistortionModel,
    tca: TcaModel,
    vignetting: VignettingModel,
    inverse_scale: f32,
}

struct Frame {
    dimensions: [f64; 2],
    norm_scale: f64,
    norm_unscale: f64,
    center: [f64; 2],
    vignette_scale: f64,
}

impl Frame {
    fn new(width: u32, height: u32, crop: f32, lens: &Lens) -> Self {
        let dimensions = [width, height].map(|n| f64::from(n.saturating_sub(1).max(1)));
        let short = dimensions[0].min(dimensions[1]);
        let aspect = (dimensions[0].max(dimensions[1]) / short) as f32;
        let calibration_diagonal = f64::from(lens.aspect_ratio * lens.aspect_ratio + 1.0).sqrt();
        let correction = (f64::from(lens.crop_factor) / f64::from(crop) * calibration_diagonal
            / f64::from(aspect * aspect + 1.0).sqrt()) as f32;
        let correction = f64::from(correction);

        Self {
            dimensions,
            norm_scale: 2.0 * correction / short,
            norm_unscale: short * 0.5 / correction,
            center: [
                (dimensions[0] / short + f64::from(lens.center_x)) * correction,
                (dimensions[1] / short + f64::from(lens.center_y)) * correction,
            ],
            vignette_scale: 1.0 / calibration_diagonal,
        }
    }

    fn axes(&self, width: u32, height: u32) -> [Vec<f32>; 2] {
        std::array::from_fn(|axis| {
            let mut coordinate = -self.center[axis] as f32;
            (0..[width, height][axis])
                .map(|_| {
                    let current = coordinate;
                    // Match Lensfun's f32 walk for each axis, including
                    // coordinates past a sign change within a row.
                    coordinate = (f64::from(coordinate) + self.norm_scale) as f32;
                    current
                })
                .collect()
        })
    }

    fn sensor_pixel(&self, point: [f32; 2]) -> [f32; 2] {
        std::array::from_fn(|i| ((f64::from(point[i]) + self.center[i]) * self.norm_unscale) as f32)
    }
}

impl Correction {
    pub(super) fn new(
        width: u32,
        height: u32,
        crop: f32,
        lens: &Lens,
        focal: f32,
        aperture: f32,
    ) -> Result<Self> {
        ensure!(width > 0 && height > 0, "empty image for lens correction");
        ensure!(
            crop.is_finite()
                && crop > 0.0
                && lens.crop_factor.is_finite()
                && lens.crop_factor > 0.0
                && lens.aspect_ratio.is_finite()
                && lens.aspect_ratio > 0.0,
            "invalid crop or aspect ratio for {}",
            lens.model
        );
        let frame = Frame::new(width, height, crop, lens);
        let millimeters =
            36.0_f64.hypot(24.0) * 0.5 * frame.vignette_scale / f64::from(lens.crop_factor);
        let calibrated = lens.interpolate_distortion(focal);
        let distortion = calibrated.map_or(DistortionModel::None, |cal| cal.model);
        let measured_focal = calibrated.and_then(|cal| cal.real_focal);
        let tca = lens
            .interpolate_tca(focal)
            .map_or(TcaModel::None, |cal| cal.model);
        // Lensfun defaults to the calibration at 1000 metres if no
        // focus distance is available to the RAW processing path.
        let vignetting = if aperture > 0.0 {
            lens.interpolate_vignetting(focal, aperture, 1000.0)
                .map_or(VignettingModel::None, |cal| cal.model)
        } else {
            VignettingModel::None
        };
        let projection = Projection::new(
            lens.lens_type,
            focal,
            measured_focal,
            millimeters,
            distortion,
        );

        let mut correction = Self {
            frame,
            projection,
            distortion,
            tca,
            vignetting,
            inverse_scale: 1.0,
        };
        correction.inverse_scale = correction.auto_scale().recip();
        Ok(correction)
    }

    pub(super) fn apply(&self, image: &mut ImageBuf) {
        let [columns, rows] = self.frame.axes(image.width, image.height);
        let stride = 3 * image.width as usize;
        if let VignettingModel::Pa { k1, k2, k3 } = self.vignetting {
            let step = (self.frame.norm_scale * self.frame.vignette_scale) as f32;
            let norm = self.frame.vignette_scale as f32;
            image
                .data
                .par_chunks_exact_mut(stride)
                .zip(&rows)
                .for_each(|(row, &y)| {
                    let mut x = columns[0] * norm;
                    let y = y * norm;
                    let mut r2 = x * x + y * y;
                    for pixel in row.chunks_exact_mut(3) {
                        let r4 = r2 * r2;
                        let strength = 1.0 + k1 * r2 + k2 * r4 + k3 * (r4 * r2);
                        let gain = f64::from(strength.recip());
                        for component in pixel {
                            *component = from_f64((to_f64(*component) * gain).max(0.0));
                        }
                        r2 += 2.0 * step * x + step * step;
                        x += step;
                    }
                });
        }
        if self.projection.is_none()
            && matches!(self.distortion, DistortionModel::None)
            && matches!(self.tca, TcaModel::None)
        {
            return;
        }

        let mut corrected = vec![from_f64(0.0); image.data.len()];
        corrected
            .par_chunks_exact_mut(stride)
            .zip(rows)
            .for_each(|(row, y)| {
                for (pixel, &x) in row.chunks_exact_mut(3).zip(&columns) {
                    for (channel, [sx, sy]) in self.source_pixels([x, y]).into_iter().enumerate() {
                        pixel[channel] = sample(image, sx, sy, channel);
                    }
                }
            });
        image.data = corrected;
    }

    fn source_pixels(&self, point: [f32; 2]) -> [[f32; 2]; 3] {
        // Inverse sampling: scale the rectilinear destination, project
        // it into the source lens, then apply source distortion and TCA.
        let [x, y] = self.geometry(point.map(|v| v * self.inverse_scale));
        let (rx, ry, bx, by) = match self.tca {
            TcaModel::None => (x, y, x, y),
            TcaModel::Linear { kr, kb } => tca_linear(x, y, kr, kb),
            TcaModel::Poly3 { red, blue } => tca_poly3_forward(x, y, red, blue),
        };
        [[rx, ry], [x, y], [bx, by]].map(|point| self.frame.sensor_pixel(point))
    }

    fn geometry(&self, point: [f32; 2]) -> [f32; 2] {
        let [x, y] = self
            .projection
            .map_or(point, |projection| projection.source(point));
        let r2 = x * x + y * y;
        let scale = match self.distortion {
            DistortionModel::None => return [x, y],
            DistortionModel::Poly3 { k1 } => (1.0 - k1) + k1 * r2,
            DistortionModel::Poly5 { k1, k2 } => {
                let (x, y) = dist_poly5(x, y, k1, k2);
                return [x, y];
            }
            DistortionModel::Ptlens { a, b, c } => {
                let r = r2.sqrt();
                let constant = (1.0_f64 - f64::from(a) - f64::from(b) - f64::from(c)) as f32;
                a * r2 * r + b * r2 + c * r + constant
            }
        };
        [x * scale, y * scale]
    }

    fn auto_scale(&self) -> f32 {
        let tca_margin = if matches!(self.tca, TcaModel::None) {
            1.0
        } else {
            1.001
        };
        if self.projection.is_none() && matches!(self.distortion, DistortionModel::None) {
            return tca_margin;
        }
        let [width, height] = self.frame.dimensions;
        let [max_x, max_y] = self
            .frame
            .dimensions
            .map(|d| d * 0.5 * self.frame.norm_scale);
        let corner = (height as f32).atan2(width as f32);
        let angle = f64::from(corner);
        let diagonal = f64::from(((width * width + height * height) as f32).sqrt())
            * 0.5
            * self.frame.norm_scale;
        let edges = [
            (0.0, max_x),
            (corner, diagonal),
            ((PI * 0.5) as f32, max_y),
            ((PI - angle) as f32, diagonal),
            (PI as f32, max_x),
            ((PI + angle) as f32, diagonal),
            ((PI * 1.5) as f32, max_y),
            ((PI * 2.0 - angle) as f32, diagonal),
        ];
        let mut scale = 0.01_f32;
        for (angle, distance) in edges {
            let (sin, cos) = f64::from(angle).sin_cos();
            let residual = |radius: f32| {
                let radius = f64::from(radius);
                let [x, y] = self.geometry([(cos * radius) as f32, (sin * radius) as f32]);
                (f64::from(x.abs()) - max_x).max(f64::from(y.abs()) - max_y)
            };
            let mut r = distance as f32;
            let mut step = 0.0001_f32;
            // Solve for the output radius that maps to the sensor
            // boundary. The rectangle, rather than a circular radius,
            // determines the reachable edge for each ray.
            for _ in 0..=50 {
                let value = residual(r);
                if value.abs() < 0.001 {
                    if r > 0.0 {
                        scale = scale.max(distance as f32 / r);
                    }
                    break;
                }
                let delta = residual(r + step) - value;
                if delta.abs() < 0.00001 {
                    step *= 2.0;
                } else {
                    r = (f64::from(r) - value * f64::from(step) / delta) as f32;
                }
            }
        }
        ((f64::from(scale) * 1.001) as f32) * tca_margin
    }
}

#[cfg(test)]
impl Correction {
    pub(super) fn sample_at(&self, width: u32, height: u32, x: usize, y: usize) -> [[f32; 2]; 3] {
        let [columns, rows] = self.frame.axes(width, height);
        self.source_pixels([columns[x], rows[y]])
    }
}
