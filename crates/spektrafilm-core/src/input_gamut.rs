//! Input gamut compression — faithful port of upstream
//! `spektrafilm/utils/gamut_compression.py` (input side).
//!
//! The compression is baked into the tc_lut once at build time via
//! remap-resample: `new_lut[xy] = old_lut[compress(xy)]`. The per-pixel
//! runtime path (RGB → CIE xy → tc → bilinear LUT sample) therefore stays
//! compression-agnostic. The `"xy"` mode compresses radially from the film
//! white toward the visible spectral locus. `"oklch"` holds OkLab hue and
//! lightness fixed while compressing chroma to the same locus.

use std::sync::OnceLock;

use rayon::prelude::*;
use spektrafilm_math::spectral::{self, CMF_X_F64, CMF_Y_F64, CMF_Z_F64, TcLut};

use crate::gamut_compression::{
    N_BISECT, N_H, N_L, c_max_lookup, h_grid, oklab_to_xyz, reinhard_knee, xyz_to_oklab,
};

/// CIE 1931 2° visible spectral locus as a closed xy polygon, sampled at 5 nm
/// from 380 to 700 nm (65 vertices + the first repeated). Mirrors upstream
/// `spectral_locus_xy()`; the CMFs are the same colour-science values.
fn spectral_locus_xy() -> &'static [[f64; 2]] {
    // Wavelengths 380..=700 step 5 → indices 0..65 of the 380..780 grid.
    const VERTICES: usize = 65;
    static LOCUS: OnceLock<[[f64; 2]; VERTICES + 1]> = OnceLock::new();
    LOCUS.get_or_init(|| {
        std::array::from_fn(|vertex| {
            let i = vertex % VERTICES;
            xyz_to_locus_xy([CMF_X_F64[i], CMF_Y_F64[i], CMF_Z_F64[i]])
        })
    })
}

/// Distance from `origin` along unit `direction` to the first intersection
/// with the closed polygon, via parametric segment intersection. Returns
/// `f64::INFINITY` for rays that miss (should not happen for an interior
/// origin and the visible locus). Mirrors upstream `_ray_polygon_distance`.
fn ray_polygon_distance(origin: [f64; 2], direction: [f64; 2], polygon: &[[f64; 2]]) -> f64 {
    let mut t_min = f64::INFINITY;
    let (dx, dy) = (direction[0], direction[1]);
    for seg in polygon.windows(2) {
        let (a, b) = (seg[0], seg[1]);
        let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
        let denom = dx * ey - dy * ex;
        if denom.abs() <= 1e-12 {
            continue;
        }
        let (ox, oy) = (origin[0] - a[0], origin[1] - a[1]);
        // origin + t·direction = a + s·edge
        let t = (-ox * ey + oy * ex) / denom;
        let s = (-ox * dy + oy * dx) / denom;
        if t > 1e-9 && (0.0..=1.0).contains(&s) && t < t_min {
            t_min = t;
        }
    }
    t_min
}

/// ACES-RGC-style radial compression of a single CIE xy toward the spectral
/// locus, around `white_xy`. Hue (dominant wavelength) is preserved by
/// construction. Mirrors upstream `compress_xy_radial`.
fn compress_xy_radial(
    xy: [f64; 2],
    white_xy: [f64; 2],
    knee: (f64, f64, f64),
    locus: &[[f64; 2]],
) -> [f64; 2] {
    let delta = [xy[0] - white_xy[0], xy[1] - white_xy[1]];
    let dist = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
    if dist < 1e-9 {
        return xy;
    }
    let safe_dist = dist.max(1e-12);
    let direction = [delta[0] / safe_dist, delta[1] / safe_dist];
    let boundary = ray_polygon_distance(white_xy, direction, locus);
    let d_norm = dist / boundary.max(1e-12);
    let (t, l, p) = knee;
    let d_compressed = reinhard_knee(d_norm, t, l, p);
    let scaled = d_compressed * boundary;
    [
        white_xy[0] + direction[0] * scaled,
        white_xy[1] + direction[1] * scaled,
    ]
}

const OKLCH_L_MIN: f64 = 0.05;
const OKLCH_L_MAX: f64 = 1.0;

fn xyz_to_locus_xy([x, y, z]: [f64; 3]) -> [f64; 2] {
    let total = (x + y + z).max(1e-12);
    [x / total, y / total]
}

/// Ray crossing against the closed CIE xy polygon. The horizontal ray
/// counts each vertex once by treating the endpoints as a half-open range.
fn inside_locus([x, y]: [f64; 2], locus: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for edge in locus.windows(2) {
        let [x0, y0] = edge[0];
        let [x1, y1] = edge[1];
        if (y0 > y) != (y1 > y) && x < x0 + (x1 - x0) * (y - y0) / (y1 - y0) {
            inside = !inside;
        }
    }
    inside
}

/// The input C_max table is defined by the spectral locus alone.
fn oklch_c_max_table() -> &'static [f64] {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    if let Some(table) = TABLE.get() {
        return table;
    }
    // Bake before initializing the cache so concurrent Rayon callers can
    // keep working while the first bake is in progress.
    let table = build_oklch_c_max_table(spectral_locus_xy());
    TABLE.get_or_init(|| table)
}

fn build_oklch_c_max_table(locus: &[[f64; 2]]) -> Vec<f64> {
    let mut table = vec![0.0; N_L * N_H];
    table
        .par_chunks_exact_mut(N_H)
        .enumerate()
        .for_each(|(row, bins)| {
            let l = OKLCH_L_MIN + row as f64 * ((OKLCH_L_MAX - OKLCH_L_MIN) / (N_L - 1) as f64);
            for (bin, out) in bins.iter_mut().enumerate() {
                let (sin_h, cos_h) = h_grid(bin).sin_cos();
                let mut lo = 0.0;
                let mut hi = 0.5;
                for _ in 0..N_BISECT {
                    let mid = (lo + hi) * 0.5;
                    let xy = xyz_to_locus_xy(oklab_to_xyz([l, mid * cos_h, mid * sin_h]));
                    if inside_locus(xy, locus) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                *out = lo;
            }
        });
    table
}

fn compress_oklch_chroma([x, y]: [f64; 2], knee: (f64, f64, f64)) -> [f64; 2] {
    // The LUT reaches y=0 at its edges. Python floors y before converting
    // xy to XYZ at Y=1, so these cells keep their limiting chromaticity.
    let y_denominator = y.max(1e-12);
    let [l, a, b] = xyz_to_oklab([x / y_denominator, 1.0, (1.0 - x - y) / y_denominator]);
    let h = b.atan2(a);
    let c_max = c_max_lookup(oklch_c_max_table(), l, h, OKLCH_L_MIN, OKLCH_L_MAX).max(1e-9);
    let (t, limit, p) = knee;
    let c = reinhard_knee(a.hypot(b) / c_max, t, limit, p) * c_max;
    let (sin_h, cos_h) = h.sin_cos();
    xyz_to_locus_xy(oklab_to_xyz([l, c * cos_h, c * sin_h]))
}

enum Algorithm {
    Off,
    Xy,
    Oklch,
}

/// Configured input gamut compressor. `build` resolves the algorithm string;
/// `is_active` reports whether a non-identity remap will be applied.
pub struct InputGamutCompress {
    algorithm: Algorithm,
    knee: (f64, f64, f64),
}

impl InputGamutCompress {
    /// Resolve `"off"`, `"xy"`, or `"oklch"` and validate the knee following
    /// upstream `InputGamutCompressSpec.__post_init__`.
    pub fn build(algorithm: &str, knee: [f32; 3]) -> Result<Self, String> {
        let algorithm = match algorithm {
            "off" => Algorithm::Off,
            "xy" => Algorithm::Xy,
            "oklch" => Algorithm::Oklch,
            other => return Err(format!("unknown input gamut algorithm: {other}")),
        };
        let [t, l, p] = knee;
        if !(0.0..1.0).contains(&t) {
            return Err(format!("knee threshold must be in [0, 1), got {t}"));
        }
        if l <= 0.0 {
            return Err(format!("knee limit must be > 0, got {l}"));
        }
        if p <= 0.0 {
            return Err(format!("knee power must be > 0, got {p}"));
        }
        Ok(Self {
            algorithm,
            knee: (knee[0] as f64, knee[1] as f64, knee[2] as f64),
        })
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.algorithm, Algorithm::Off)
    }

    /// Bake the compression into a freshly resampled copy of `lut`. For each
    /// LUT cell we decode its tc index to CIE xy, compress, re-encode to tc,
    /// and bilinearly sample the original LUT there (clamp-to-edge boundary,
    /// matching `scipy.ndimage.map_coordinates(order=1, mode="nearest")`).
    /// The `"xy"` algorithm uses `ref_xy` as its achromatic axis.
    /// Returns `lut` unchanged when inactive.
    pub fn remap(&self, lut: &TcLut, ref_xy: [f64; 2]) -> TcLut {
        if !self.is_active() {
            return lut.clone();
        }
        let size = lut.size;
        let ch = lut.channels;
        let inv = 1.0 / (size as f64 - 1.0);
        let mut data = vec![0.0f64; size * size * ch];
        for i in 0..size {
            for j in 0..size {
                // Step 1: tc cell → CIE xy.
                let (x, y) = spectral::tc_to_xy(i as f64 * inv, j as f64 * inv);
                // Step 2: compress.
                let cxy = match self.algorithm {
                    Algorithm::Off => [x, y],
                    Algorithm::Xy => {
                        compress_xy_radial([x, y], ref_xy, self.knee, spectral_locus_xy())
                    }
                    Algorithm::Oklch => compress_oklch_chroma([x, y], self.knee),
                };
                // Step 3: compressed xy → tc.
                let (tx, ty) = spectral::xy_to_tc(cxy[0], cxy[1]);
                // Step 4: bilinear sample original LUT at (tx, ty) grid coords.
                let sample =
                    bilinear_sample(lut, tx * (size as f64 - 1.0), ty * (size as f64 - 1.0));
                let base = (i * size + j) * ch;
                data[base..base + ch].copy_from_slice(&sample[..ch]);
            }
        }
        TcLut {
            size,
            channels: ch,
            data,
        }
    }
}

/// Bilinear sample of all channels at fractional grid coordinate `(ci, cj)`,
/// clamping fetch indices to the edge (scipy `mode="nearest"`).
fn bilinear_sample(lut: &TcLut, ci: f64, cj: f64) -> [f64; 3] {
    let size = lut.size;
    let ch = lut.channels;
    let max_idx = size as isize - 1;
    let clamp = |v: isize| v.clamp(0, max_idx) as usize;

    let bi = ci.floor();
    let bj = cj.floor();
    let fi = ci - bi;
    let fj = cj - bj;
    let (i0, i1) = (clamp(bi as isize), clamp(bi as isize + 1));
    let (j0, j1) = (clamp(bj as isize), clamp(bj as isize + 1));

    let cell = |i: usize, j: usize, c: usize| lut.data[(i * size + j) * ch + c];
    let mut out = [0.0f64; 3];
    for (c, slot) in out.iter_mut().enumerate().take(ch) {
        let v00 = cell(i0, j0, c);
        let v01 = cell(i0, j1, c);
        let v10 = cell(i1, j0, c);
        let v11 = cell(i1, j1, c);
        *slot = v00 * (1.0 - fi) * (1.0 - fj)
            + v01 * (1.0 - fi) * fj
            + v10 * fi * (1.0 - fj)
            + v11 * fi * fj;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic synthetic LUT, mirrored bit-for-bit in the Python
    /// reference (`tmp/igc_ref.py`): cell (i, j, c) = `((i² + 3j + 7c) % 97) / 97`.
    fn synthetic_lut(size: usize) -> TcLut {
        let mut data = vec![0.0f64; size * size * 3];
        for i in 0..size {
            for j in 0..size {
                for c in 0..3 {
                    let v = ((i * i + j * 3 + c * 7) % 97) as f64 / 97.0;
                    data[(i * size + j) * 3 + c] = v;
                }
            }
        }
        TcLut {
            size,
            channels: 3,
            data,
        }
    }

    /// The baked tc_lut remap matches the upstream remap-resample
    /// (`remap_tc_lut_for_compression` with the `"xy"` algorithm) against the
    /// CIE 1931 2° locus, sampled with scipy `map_coordinates(order=1,
    /// mode="nearest")`.
    #[test]
    fn remap_matches_python_reference() {
        let spec = InputGamutCompress::build("xy", [0.0, 1.0, 6.0]).unwrap();
        let lut = synthetic_lut(64);
        let out = spec.remap(&lut, [0.3127, 0.3290]);

        // (i, j) → [r, g, b] from the Python reference.
        let cells = [
            (0usize, 0usize, [0.4137658005, 0.1877218965, 0.2598868450]),
            (10, 20, [0.7521338372, 0.8242987857, 0.8964637341]),
            (32, 32, [0.5463917966, 0.6185567450, 0.6907215587]),
            (50, 5, [0.5807479510, 0.0691153288, 0.1412802772]),
            (63, 63, [0.6891891640, 0.7613541125, 0.8335190609]),
            (5, 60, [0.0995187055, 0.1716836540, 0.2438486024]),
        ];
        for (i, j, expected) in cells {
            let base = (i * 64 + j) * 3;
            for c in 0..3 {
                assert!(
                    (out.data[base + c] - expected[c]).abs() < 1e-7,
                    "cell ({i},{j}) ch {c}: got {}, expected {}",
                    out.data[base + c],
                    expected[c],
                );
            }
        }

        let checksum: f64 = out.data.iter().sum();
        assert!(
            (checksum - 5846.5426164588).abs() < 1e-6,
            "checksum {checksum}",
        );
    }

    /// An inactive ("off") compressor returns the LUT unchanged.
    #[test]
    fn off_is_identity() {
        let spec = InputGamutCompress::build("off", [0.0, 1.0, 6.0]).unwrap();
        assert!(!spec.is_active());
        let lut = synthetic_lut(16);
        let out = spec.remap(&lut, [0.3127, 0.3290]);
        assert_eq!(out.data, lut.data);
    }

    /// Python's input OkLCh remap, including LUT edges where y=0 and
    /// saturated cells outside the spectral locus. The film white only
    /// affects the radial algorithm.
    #[test]
    fn oklch_remap_matches_python_reference() {
        let spec = InputGamutCompress::build("oklch", [0.0, 1.0, 6.0]).unwrap();
        let lut = synthetic_lut(64);
        let out = spec.remap(&lut, [0.3127, 0.3290]);
        let cells = [
            (0, 0, [0.3097350191, 0.3818999675, 0.4540649160]),
            (10, 20, [0.3902313195, 0.4623962679, 0.5345612164]),
            (32, 32, [0.5463948705, 0.6185598189, 0.6907152120]),
            (63, 0, [0.3013510998, 0.3735160483, 0.4456809967]),
            (63, 63, [0.4193442459, 0.4915091943, 0.5636741428]),
        ];
        for (i, j, expected) in cells {
            let base = (i * 64 + j) * 3;
            for (actual, expected) in out.data[base..base + 3].iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-7, "cell ({i},{j}): {actual}");
            }
        }
        let checksum: f64 = out.data.iter().sum();
        assert!((checksum - 6060.7266009485).abs() < 1e-6, "sum {checksum}");
        assert_eq!(out.data, spec.remap(&lut, [0.3457, 0.3585]).data);
    }
}
