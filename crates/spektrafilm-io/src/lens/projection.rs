//! Inverse projection for resampling a RAW into the rectilinear output grid.

use lensfun::{DistortionModel, LensType, mod_coord::geometry};

/// Geometry is measured in focal-length units. The calibration uses the
/// Hugin coordinate convention, where one unit corresponds to half the
/// narrow dimension of the calibration frame.
#[derive(Clone, Copy)]
pub(super) struct Projection {
    lens_type: LensType,
    focal_units: f32,
    inverse_focal: f32,
}

impl Projection {
    pub(super) fn new(
        lens_type: LensType,
        focal: f32,
        measured_focal: Option<f32>,
        millimeters: f64,
        distortion: DistortionModel,
    ) -> Option<Self> {
        if matches!(lens_type, LensType::Rectilinear | LensType::Unknown) {
            return None;
        }

        // Poly3 and PTLens absorb an overall radial scale in their
        // constant term. Explicit measurements refer to the optical
        // focal length. Lensfun's nominal fallback already uses Hugin's
        // convention, so the two adjustments there cancel each other.
        let correction = match distortion {
            DistortionModel::Poly3 { k1 } => 1.0 - k1,
            DistortionModel::Ptlens { a, b, c } => {
                (1.0_f64 - f64::from(a) - f64::from(b) - f64::from(c)) as f32
            }
            _ => 1.0,
        };
        let focal = measured_focal.map_or(focal, |value| value / correction);
        let focal_units = (f64::from(focal) / millimeters) as f32;
        Some(Self {
            lens_type,
            focal_units,
            inverse_focal: 1.0 / focal_units,
        })
    }

    /// Map an output-plane point into the original lens projection.
    pub(super) fn source(self, [x, y]: [f32; 2]) -> [f32; 2] {
        let focal = self.focal_units;
        let inverse = self.inverse_focal;
        let as_f64 = f64::from(focal);
        match self.lens_type {
            LensType::FisheyeEquidistant => {
                let radius = (x * x + y * y).sqrt() * inverse;
                let scale = if radius == 0.0 {
                    1.0
                } else {
                    radius.atan() / radius
                };
                [scale * x, scale * y]
            }
            LensType::Panoramic => {
                let longitude = focal * (x * inverse).atan();
                [longitude, y * (longitude * inverse).cos()]
            }
            LensType::Equirectangular => to_equirectangular(x, y, focal),
            _ => {
                // Lensfun routes the four other fisheye models through
                // its equirectangular callbacks.
                let [lon, lat] = to_equirectangular(x, y, focal);
                let (px, py) = match self.lens_type {
                    LensType::FisheyeOrthographic => {
                        geometry::erect_orthographic(lon * inverse, lat * inverse)
                    }
                    LensType::FisheyeStereographic => {
                        geometry::erect_stereographic(lon * inverse, lat * inverse)
                    }
                    LensType::FisheyeEquisolid => {
                        geometry::erect_equisolid(lon / focal, lat / focal)
                    }
                    LensType::FisheyeThoby => geometry::erect_thoby(lon * inverse, lat * inverse),
                    LensType::Rectilinear
                    | LensType::Unknown
                    | LensType::FisheyeEquidistant
                    | LensType::Panoramic
                    | LensType::Equirectangular => {
                        unreachable!("projection selected in Projection::new and above")
                    }
                };
                [
                    (f64::from(px) * as_f64) as f32,
                    (f64::from(py) * as_f64) as f32,
                ]
            }
        }
    }
}

fn to_equirectangular(x: f32, y: f32, focal: f32) -> [f32; 2] {
    [
        focal * x.atan2(focal),
        focal * y.atan2((focal * focal + x * x).sqrt()),
    ]
}
