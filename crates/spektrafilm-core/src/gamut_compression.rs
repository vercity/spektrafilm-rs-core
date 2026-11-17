//! Output gamut compression — port of upstream spektrafilm's
//! `utils/gamut_compression.compress_rgb`.
//!
//! The scan produces linear RGB in the chosen output color space. Colors
//! outside the output cube would otherwise be hard-clipped to `[0, 1]` at
//! encode. The `oklch` compressor instead reduces *perceptual chroma*
//! smoothly: linear RGB → XYZ → OkLab → OkLch `(L, C, h)`, look up the
//! max in-gamut chroma `C_max(L, h)` for the output cube, apply a Reinhard
//! knee to `C / C_max`, and reconstruct — preserving OkLab hue and
//! lightness. A one-sided lightness compression first rolls super-bright
//! highlights (`L > white`) back into range.
//!
//! Supports `oklch`, `oklrab`, `jzazbz`, `cam16ucs` (default), and `aces_rgc` via
//! `io.output_gamut_compress`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use spektrafilm_math::colorspace::RgbSpace;

use crate::params::OutputGamutCompressParams;

// OkLab (Ottosson) matrices, exactly as colour-science stores them.
const M1_XYZ_TO_LMS: [[f64; 3]; 3] = [
    [0.8189330101, 0.3618667424, -0.1288597137],
    [0.0329845436, 0.9293118715, 0.0361456387],
    [0.0482003018, 0.2643662691, 0.633851707],
];
const M1_LMS_TO_XYZ: [[f64; 3]; 3] = [
    [1.2270138511035211, -0.5577999806518222, 0.2812561489664678],
    [-0.0405801784232806, 1.11225686961683, -0.07167667866560119],
    [-0.0763812845057069, -0.4214819784180127, 1.5861632204407947],
];
const M2_LMS_TO_LAB: [[f64; 3]; 3] = [
    [0.2104542553, 0.793617785, -0.0040720468],
    [1.9779984951, -2.428592205, 0.4505937099],
    [0.0259040371, 0.7827717662, -0.808675766],
];
const M2_LAB_TO_LMS: [[f64; 3]; 3] = [
    [0.9999999984505196, 0.3963377921737678, 0.21580375806075877],
    [
        1.0000000088817607,
        -0.10556134232365633,
        -0.0638541747717059,
    ],
    [
        1.0000000546724108,
        -0.08948418209496574,
        -1.2914855378640917,
    ],
];

// C_max(L, h) table grid dims, matching `_OKLCH_CMAX_TABLE_N_*`. The L-axis
// bounds and the bisection's chroma ceiling are per-space (see `build`).
pub(crate) const N_L: usize = 64;
pub(crate) const N_H: usize = 720;
pub(crate) const N_BISECT: usize = 18;

#[inline]
fn mat_vec(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

#[inline]
pub(crate) fn xyz_to_oklab(xyz: [f64; 3]) -> [f64; 3] {
    let lms = mat_vec(&M1_XYZ_TO_LMS, xyz);
    let lms_ = [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()];
    mat_vec(&M2_LMS_TO_LAB, lms_)
}

#[inline]
pub(crate) fn oklab_to_xyz(lab: [f64; 3]) -> [f64; 3] {
    let lms_ = mat_vec(&M2_LAB_TO_LMS, lab);
    let lms = [lms_[0].powi(3), lms_[1].powi(3), lms_[2].powi(3)];
    mat_vec(&M1_LMS_TO_XYZ, lms)
}

// Ottosson 2023 "Lr": a 1D monotonic remap of OkLab L so the lightness scale
// tracks CIELAB L* more closely. Lr(0)=0, Lr(1)=1.
const OKLRAB_K1: f64 = 0.206;
const OKLRAB_K2: f64 = 0.03;
const OKLRAB_K3: f64 = (1.0 + OKLRAB_K1) / (1.0 + OKLRAB_K2);

#[inline]
fn oklab_l_to_lr(l: f64) -> f64 {
    let t = OKLRAB_K3 * l - OKLRAB_K1;
    0.5 * (t + (t * t + 4.0 * OKLRAB_K2 * OKLRAB_K3 * l).sqrt())
}

#[inline]
fn oklrab_lr_to_l(lr: f64) -> f64 {
    (lr * (lr + OKLRAB_K1)) / (OKLRAB_K3 * (lr + OKLRAB_K2))
}

/// Smooth Reinhard knee on normalized distance: identity below `threshold`,
/// asymptotic at `limit` above it. Matches the ACES RGC v1.3 reference.
#[inline]
pub(crate) fn reinhard_knee(d: f64, threshold: f64, limit: f64, power: f64) -> f64 {
    if d > threshold {
        let scale = limit - threshold;
        let x = (d - threshold) / scale;
        let y = x / (1.0 + x.powf(power)).powf(1.0 / power);
        threshold + scale * y
    } else {
        d
    }
}

#[inline]
pub(crate) fn h_grid(j: usize) -> f64 {
    // linspace(-pi, pi, N_H, endpoint=False)
    -std::f64::consts::PI + (2.0 * std::f64::consts::PI) * (j as f64) / (N_H as f64)
}

/// Bilinear `C_max(L, h)` lookup (L clamped, h wraps).
#[inline]
pub(crate) fn c_max_lookup(table: &[f64], l: f64, h: f64, l_min: f64, l_max: f64) -> f64 {
    let l = l.clamp(l_min, l_max);
    let h_step = h_grid(1) - h_grid(0);
    let h_idx = (h - h_grid(0)) / h_step;
    let h_floor = h_idx.floor();
    let h_lo = (h_floor as i64).rem_euclid(N_H as i64) as usize;
    let h_hi = (h_lo + 1) % N_H;
    let h_frac = h_idx - h_floor;

    let l_idx = (l - l_min) / (l_max - l_min) * ((N_L - 1) as f64);
    let l_lo = (l_idx.floor() as i64).clamp(0, (N_L - 2) as i64) as usize;
    let l_hi = l_lo + 1;
    let l_frac = l_idx - l_lo as f64;

    let v00 = table[l_lo * N_H + h_lo];
    let v01 = table[l_lo * N_H + h_hi];
    let v10 = table[l_hi * N_H + h_lo];
    let v11 = table[l_hi * N_H + h_hi];
    v00 * (1.0 - l_frac) * (1.0 - h_frac)
        + v01 * (1.0 - l_frac) * h_frac
        + v10 * l_frac * (1.0 - h_frac)
        + v11 * l_frac * h_frac
}

/// Perceptual space the chroma reduction runs in.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Space {
    Oklch,
    /// OkLab with the Ottosson Lr-rebased lightness on the L axis.
    Oklrab,
    Jzazbz,
    Cam16ucs,
}

impl Space {
    /// Linear output RGB → perceptual coordinates `[L, a, b]`.
    #[inline]
    fn rgb_to_lab(self, rgb: [f64; 3], target: RgbSpace, view: Cam16White) -> [f64; 3] {
        let xyz = mat_vec(&target.to_xyz(), rgb);
        match self {
            // oklrab shares OkLab coords; only the C_max lookup index differs.
            Space::Oklch | Space::Oklrab => xyz_to_oklab(xyz),
            Space::Jzazbz => xyz_to_jzazbz(xyz),
            Space::Cam16ucs => xyz_to_cam16ucs(xyz, view),
        }
    }
    /// Perceptual coordinates `[L, a, b]` → linear output RGB.
    #[inline]
    fn lab_to_rgb(self, lab: [f64; 3], target: RgbSpace, view: Cam16White) -> [f64; 3] {
        let xyz = match self {
            Space::Oklch | Space::Oklrab => oklab_to_xyz(lab),
            Space::Jzazbz => jzazbz_to_xyz(lab),
            Space::Cam16ucs => cam16ucs_to_xyz(lab, view),
        };
        mat_vec(&target.from_xyz(), xyz)
    }
    /// Reconstruction lightness `L` → C_max-table lookup index. Identity except
    /// oklrab, which indexes the table by Ottosson's rebased `Lr`.
    #[inline]
    fn lookup_lightness(self, l: f64) -> f64 {
        match self {
            Space::Oklrab => oklab_l_to_lr(l),
            _ => l,
        }
    }
    /// C_max-table lookup index → reconstruction lightness `L` (for the bake).
    #[inline]
    fn recon_lightness(self, lookup: f64) -> f64 {
        match self {
            Space::Oklrab => oklrab_lr_to_l(lookup),
            _ => lookup,
        }
    }
    /// `(l_min, l_max, chroma_upper)` — the C_max table's lightness-axis
    /// bounds and the bisection's chroma ceiling in this perceptual space.
    fn table_geometry(self) -> (f64, f64, f64) {
        match self {
            Space::Oklch | Space::Oklrab => (0.02, 1.0, 0.5),
            Space::Jzazbz => (0.002, 0.18, 0.3),
            Space::Cam16ucs => (1.0, 110.0, 150.0),
        }
    }
}

/// Which algorithm the compressor runs.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Off,
    /// ACES Reference Gamut Compression — per-channel knee in RGB, no table.
    AcesRgc,
    /// Perceptual chroma reduction against a `C_max(L, h)` table.
    Perceptual(Space),
}

/// Pre-computed output gamut compressor for a fixed output color space.
#[derive(Clone)]
pub struct OutputGamutCompress {
    kind: Kind,
    target: RgbSpace,
    cam16_white: Cam16White,
    knee: (f64, f64, f64),
    lightness: Option<(f64, f64, f64)>,
    /// Perceptual lightness of the output white.
    l_white: f64,
    /// Lightness-axis bounds of the `C_max` table grid.
    l_min: f64,
    l_max: f64,
    /// `C_max(L, h)` table, row-major `[N_L][N_H]`. Empty for off/aces_rgc.
    /// Shared via the process-wide cache — the table depends only on the
    /// perceptual space (the knee/lightness params don't enter the bake),
    /// and the ~105 ms bisection would otherwise re-run on every pipeline
    /// rebuild (the GUI rebuilds the pipeline per slider change).
    cmax: Arc<Vec<f64>>,
}

impl OutputGamutCompress {
    /// No-op (identity) compressor.
    pub fn identity() -> Self {
        Self {
            kind: Kind::Off,
            target: RgbSpace::Srgb,
            cam16_white: Cam16White::D65,
            knee: (0.0, 1.0, 1.0),
            lightness: None,
            l_white: 1.0,
            l_min: 0.0,
            l_max: 1.0,
            cmax: Arc::new(Vec::new()),
        }
    }

    pub fn is_active(&self) -> bool {
        self.kind != Kind::Off
    }

    /// Build the perceptual boundary for the requested linear output RGB cube.
    pub fn build(params: &OutputGamutCompressParams, output_color_space: &str) -> Self {
        let knee = (
            params.knee[0] as f64,
            params.knee[1] as f64,
            params.knee[2] as f64,
        );
        let lightness = params
            .lightness_compression
            .map(|l| (l[0] as f64, l[1] as f64, l[2] as f64));
        // aces_rgc is per-channel in RGB (no perceptual space / table / lightness).
        if params.algorithm == "aces_rgc" {
            return Self {
                kind: Kind::AcesRgc,
                target: RgbSpace::Srgb,
                cam16_white: Cam16White::D65,
                knee,
                lightness: None,
                l_white: 1.0,
                l_min: 0.0,
                l_max: 1.0,
                cmax: Arc::new(Vec::new()),
            };
        }
        let space = match params.algorithm.as_str() {
            "off" => return Self::identity(),
            "oklch" => Space::Oklch,
            "oklrab" => Space::Oklrab,
            "jzazbz" => Space::Jzazbz,
            "cam16ucs" => Space::Cam16ucs,
            other => {
                tracing::error!(
                    algorithm = other,
                    "output gamut compression: unknown algorithm; disabling"
                );
                return Self::identity();
            }
        };
        let Some(target) = RgbSpace::from_name(output_color_space) else {
            tracing::error!(
                output = output_color_space,
                "unknown output space for gamut compression"
            );
            return Self::identity();
        };
        let cam16_white = Cam16White::new(target);
        let l_white = match space {
            Space::Oklch | Space::Oklrab => 1.0,
            Space::Jzazbz => xyz_to_jzazbz(target.white_xyz())[0],
            Space::Cam16ucs => xyz_to_cam16ucs(target.white_xyz(), cam16_white)[0],
        };
        let (l_min, l_max, _) = space.table_geometry();
        let cmax = cmax_table_cached(space, target, cam16_white);
        Self {
            kind: Kind::Perceptual(space),
            target,
            cam16_white,
            knee,
            lightness,
            l_white,
            l_min,
            l_max,
            cmax,
        }
    }

    /// GPU-pass inputs for the active compressor.
    pub fn gpu_params(&self) -> Option<spektrafilm_gpu::GamutGpuParams<'_>> {
        use spektrafilm_gpu::GamutMode;
        let mode = match self.kind {
            Kind::Off => return None,
            Kind::AcesRgc => GamutMode::AcesRgc,
            Kind::Perceptual(Space::Oklch) => GamutMode::Oklch,
            Kind::Perceptual(Space::Oklrab) => GamutMode::Oklrab,
            Kind::Perceptual(Space::Cam16ucs) => GamutMode::Cam16ucs,
            Kind::Perceptual(Space::Jzazbz) => GamutMode::Jzazbz,
        };
        let [r, g, b] = self.cam16_white.adaptation;
        Some(spektrafilm_gpu::GamutGpuParams {
            mode,
            target: self.target,
            cam16_white: [
                r as f32,
                g as f32,
                b as f32,
                self.cam16_white.response as f32,
            ],
            knee: [self.knee.0 as f32, self.knee.1 as f32, self.knee.2 as f32],
            lightness: self
                .lightness
                .map(|(t, l, p)| [t as f32, l as f32, p as f32]),
            l_white: self.l_white as f32,
            l_min: self.l_min as f32,
            l_max: self.l_max as f32,
            cmax: &self.cmax,
            n_l: N_L as u32,
            n_h: N_H as u32,
        })
    }

    /// Compress a single linear output-RGB pixel. Identity when inactive.
    #[inline]
    pub fn compress(&self, rgb: [f64; 3]) -> [f64; 3] {
        let space = match self.kind {
            Kind::Off => return rgb,
            Kind::AcesRgc => return self.compress_aces_rgc(rgb),
            Kind::Perceptual(s) => s,
        };
        let lab = space.rgb_to_lab(rgb, self.target, self.cam16_white);
        // One-sided lightness compression first (so C_max is looked up at the
        // corrected L), normalized by the perceptual white.
        let mut l = lab[0];
        if let Some((t, lim, p)) = self.lightness {
            l = reinhard_knee(l / self.l_white, t, lim, p) * self.l_white;
        }
        let a = lab[1];
        let b = lab[2];
        let c = a.hypot(b);
        let h = b.atan2(a);
        // C_max is looked up at the lookup-lightness (Lr for oklrab); the
        // reconstruction below keeps the reconstruction lightness `l`.
        let c_max = c_max_lookup(
            &self.cmax,
            space.lookup_lightness(l),
            h,
            self.l_min,
            self.l_max,
        );
        let safe = c_max.max(1e-9);
        let d_comp = reinhard_knee(c / safe, self.knee.0, self.knee.1, self.knee.2);
        let c_new = d_comp * safe;
        space.lab_to_rgb(
            [l, c_new * h.cos(), c_new * h.sin()],
            self.target,
            self.cam16_white,
        )
    }

    /// ACES Reference Gamut Compression v1.3: per-channel Reinhard knee on the
    /// achromatic distance `d = (max - c)/max`. Mirrors `compress_rgb_aces_rgc`.
    #[inline]
    fn compress_aces_rgc(&self, rgb: [f64; 3]) -> [f64; 3] {
        let ach = rgb[0].max(rgb[1]).max(rgb[2]);
        if ach <= 1e-12 {
            return rgb;
        }
        let mut out = [0.0f64; 3];
        for c in 0..3 {
            let d = (ach - rgb[c]) / ach;
            let dc = reinhard_knee(d, self.knee.0, self.knee.1, self.knee.2);
            out[c] = ach * (1.0 - dc);
        }
        out
    }
}

// Safdar 2017 JzAzBz with the same signed ST 2084 variant as colour-science.
// The transforms take relative XYZ, with diffuse white at 100 cd/m².
const JZAZBZ_WHITE_CD_M2: f64 = 100.0;
const JZAZBZ_PQ_PEAK_CD_M2: f64 = 10_000.0;
const JZAZBZ_B: f64 = 1.15;
const JZAZBZ_G: f64 = 0.66;
const JZAZBZ_D: f64 = -0.56;
const JZAZBZ_D0: f64 = 1.629_549_953_282_156_6e-11;
const JZAZBZ_PQ_C1: f64 = 3424.0 / 4096.0;
const JZAZBZ_PQ_C2: f64 = 2413.0 / 128.0;
const JZAZBZ_PQ_C3: f64 = 2392.0 / 128.0;
const JZAZBZ_PQ_M1: f64 = 2610.0 / 16384.0;
const JZAZBZ_PQ_M2: f64 = 1.7 * 2523.0 / 32.0;

const JZAZBZ_XYZ_TO_LMS: [[f64; 3]; 3] = [
    [0.41478972, 0.579999, 0.0146480],
    [-0.2015100, 1.120649, 0.0531008],
    [-0.0166008, 0.264800, 0.6684799],
];
const JZAZBZ_LMS_TO_XYZ: [[f64; 3]; 3] = [
    [
        1.9242264357876069,
        -1.0047923125953657,
        0.037651404030617994,
    ],
    [
        0.35031676209499907,
        0.7264811939316552,
        -0.06538442294808501,
    ],
    [
        -0.09098281098284752,
        -0.3127282905230739,
        1.5227665613052603,
    ],
];
const JZAZBZ_LMS_P_TO_IZAZBZ: [[f64; 3]; 3] = [
    [0.5, 0.5, 0.0],
    [3.524, -4.066708, 0.542708],
    [0.199076, 1.096799, -1.295875],
];
const JZAZBZ_IZAZBZ_TO_LMS_P: [[f64; 3]; 3] = [
    [0.9999999999999999, 0.1386050432715393, 0.05804731615611899],
    [
        0.9999999999999999,
        -0.13860504327153927,
        -0.05804731615611874,
    ],
    [
        0.9999999999999998,
        -0.09601924202631892,
        -0.8118918960560387,
    ],
];

#[inline]
fn signed_pow(value: f64, exponent: f64) -> f64 {
    value.abs().powf(exponent).copysign(value)
}

#[inline]
fn xyz_to_jzazbz(xyz: [f64; 3]) -> [f64; 3] {
    let [x, y, z] = xyz.map(|value| value * JZAZBZ_WHITE_CD_M2);
    let x_prime = JZAZBZ_B * x - (JZAZBZ_B - 1.0) * z;
    let y_prime = JZAZBZ_G * y - (JZAZBZ_G - 1.0) * x;
    let lms = mat_vec(&JZAZBZ_XYZ_TO_LMS, [x_prime, y_prime, z]);
    let lms_p = lms.map(|value| {
        let powered = signed_pow(value / JZAZBZ_PQ_PEAK_CD_M2, JZAZBZ_PQ_M1);
        signed_pow(
            (JZAZBZ_PQ_C1 + JZAZBZ_PQ_C2 * powered) / (1.0 + JZAZBZ_PQ_C3 * powered),
            JZAZBZ_PQ_M2,
        )
    });
    let [i, a, b] = mat_vec(&JZAZBZ_LMS_P_TO_IZAZBZ, lms_p);
    let j = ((1.0 + JZAZBZ_D) * i) / (1.0 + JZAZBZ_D * i) - JZAZBZ_D0;
    [j, a, b]
}

#[inline]
fn jzazbz_to_xyz([j, a, b]: [f64; 3]) -> [f64; 3] {
    let j = j + JZAZBZ_D0;
    let i = j / (1.0 + JZAZBZ_D - JZAZBZ_D * j);
    let lms_p = mat_vec(&JZAZBZ_IZAZBZ_TO_LMS_P, [i, a, b]);
    let lms = lms_p.map(|value| {
        let powered = signed_pow(value, 1.0 / JZAZBZ_PQ_M2);
        let delta = powered - JZAZBZ_PQ_C1;
        let numerator = if delta < 0.0 { 0.0 } else { delta };
        JZAZBZ_PQ_PEAK_CD_M2
            * signed_pow(
                numerator / (JZAZBZ_PQ_C2 - JZAZBZ_PQ_C3 * powered),
                1.0 / JZAZBZ_PQ_M1,
            )
    });
    let [x_prime, y_prime, z] = mat_vec(&JZAZBZ_LMS_TO_XYZ, lms);
    let x = (x_prime + (JZAZBZ_B - 1.0) * z) / JZAZBZ_B;
    let y = (y_prime + (JZAZBZ_G - 1.0) * x) / JZAZBZ_G;
    [x, y, z].map(|value| value / JZAZBZ_WHITE_CD_M2)
}

// ─────────────────────────────────────────────────────────────────────────
// CAM16-UCS (upstream's default `cam16ucs` method).
//
// Viewing conditions: output white, L_A = 64 cd/m², Y_b = 20, average
// surround. The white-dependent terms are computed once. The
// per-pixel path is the CIECAM16 forward/inverse + the Luo 2006 UCS transform.
// XYZ here is at Y=1; CIECAM16 works in the Y=100 domain, hence the ×/÷100.
// ─────────────────────────────────────────────────────────────────────────
const CAM16_M16: [[f64; 3]; 3] = [
    [0.401288, 0.650173, -0.051461],
    [-0.250268, 1.204414, 0.045854],
    [-0.002079, 0.048952, 0.953127],
];
const CAM16_M16I: [[f64; 3]; 3] = [
    [1.8620678550872327, -1.0112546305316843, 0.14918677544445175],
    [
        0.3875265432361372,
        0.6214474419314753,
        -0.008973985167612516,
    ],
    [
        -0.015841498849333863,
        -0.03412293802851557,
        1.0499644368778496,
    ],
];
const CAM16_F_L: f64 = 0.6839903845696502;
const CAM16_N: f64 = 0.2;
const CAM16_N_BB: f64 = 1.0003040045593807;
const CAM16_N_CB: f64 = 1.0003040045593807;
const CAM16_Z: f64 = 1.9272135954999579;
const CAM16_C: f64 = 0.69;
const CAM16_N_C: f64 = 1.0;
// Luo 2006 CAM16-UCS coefficients (K_L=1.0, c1, c2).
const UCS_C1: f64 = 0.007;
const UCS_C2: f64 = 0.0228;

#[derive(Clone, Copy)]
struct Cam16White {
    adaptation: [f64; 3],
    response: f64,
}

impl Cam16White {
    const D65: Self = Self {
        adaptation: [1.0228770275436545, 0.9852074782801457, 0.9285450586783286],
        response: 37.16907530221132,
    };

    fn new(target: RgbSpace) -> Self {
        if target.white_xy() == RgbSpace::Srgb.white_xy() {
            return Self::D65;
        }
        let rgb = mat_vec(&CAM16_M16, target.white_xyz().map(|v| v * 100.0));
        let degree = 1.0 - (-106.0_f64 / 92.0).exp() / 3.6;
        let adaptation = rgb.map(|v| degree * (100.0 / v) + 1.0 - degree);
        let [r, g, b] = cam16_padc_forward(std::array::from_fn(|i| rgb[i] * adaptation[i]));
        let response = (2.0 * r + g + b / 20.0 - 0.305) * CAM16_N_BB;
        Self {
            adaptation,
            response,
        }
    }
}

#[inline]
fn atan2_deg(y: f64, x: f64) -> f64 {
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

#[inline]
fn cam16_padc_forward(rgb: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0f64; 3];
    for i in 0..3 {
        let flr = (CAM16_F_L * rgb[i].abs() / 100.0).powf(0.42);
        out[i] = 400.0 * rgb[i].signum() * flr / (27.13 + flr) + 0.1;
    }
    out
}

#[inline]
fn cam16_padc_inverse(rgb: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0f64; 3];
    for i in 0..3 {
        let d = rgb[i] - 0.1;
        let base = (27.13 * d.abs()) / (400.0 - d.abs());
        out[i] = d.signum() * 100.0 / CAM16_F_L * signed_pow(base, 1.0 / 0.42);
    }
    out
}

/// XYZ (Y=1) → CAM16-UCS `(Jp, ap, bp)`.
fn xyz_to_cam16ucs(xyz: [f64; 3], white: Cam16White) -> [f64; 3] {
    let xyz100 = [xyz[0] * 100.0, xyz[1] * 100.0, xyz[2] * 100.0];
    let rgb = mat_vec(&CAM16_M16, xyz100);
    let rgb_c = [
        rgb[0] * white.adaptation[0],
        rgb[1] * white.adaptation[1],
        rgb[2] * white.adaptation[2],
    ];
    let [ra, ga, ba] = cam16_padc_forward(rgb_c);
    let a = ra - 12.0 * ga / 11.0 + ba / 11.0;
    let b = (ra + ga - 2.0 * ba) / 9.0;
    let h = atan2_deg(b, a);
    let e_t = 0.25 * ((2.0 + h * std::f64::consts::PI / 180.0).cos() + 3.8);
    let a_resp = (2.0 * ra + ga + ba / 20.0 - 0.305) * CAM16_N_BB;
    let jj = 100.0 * signed_pow(a_resp / white.response, CAM16_C * CAM16_Z);
    let denom = ra + ga + 21.0 * ba / 20.0;
    let t = if denom != 0.0 {
        (50000.0 / 13.0) * CAM16_N_C * CAM16_N_CB * (e_t * (a * a + b * b).sqrt()) / denom
    } else {
        0.0
    };
    let cc = signed_pow(t, 0.9)
        * signed_pow(jj / 100.0, 0.5)
        * (1.64 - 0.29f64.powf(CAM16_N)).powf(0.73);
    let m = cc * CAM16_F_L.powf(0.25);
    let jp = (1.0 + 100.0 * UCS_C1) * jj / (1.0 + UCS_C1 * jj);
    // Colour's inverse keeps only the achromatic response when the
    // logarithmic chroma of an extended-range pixel is undefined.
    let chroma_log = UCS_C2 * m;
    let mp = if chroma_log > -1.0 {
        chroma_log.ln_1p() / UCS_C2
    } else {
        0.0
    };
    let hr = h * std::f64::consts::PI / 180.0;
    [jp, mp * hr.cos(), mp * hr.sin()]
}

/// CAM16-UCS `(Jp, ap, bp)` → XYZ (Y=1).
fn cam16ucs_to_xyz(jab: [f64; 3], white: Cam16White) -> [f64; 3] {
    let [jp, ap, bp] = jab;
    let mp = (ap * ap + bp * bp).sqrt();
    let h = atan2_deg(bp, ap);
    let jj = jp / ((1.0 + 100.0 * UCS_C1) - UCS_C1 * jp);
    let m = (UCS_C2 * mp).exp_m1() / UCS_C2;
    let cc = m / CAM16_F_L.powf(0.25);
    let j_prime = jj.max(f64::EPSILON);
    let t = (cc / ((j_prime / 100.0).sqrt() * (1.64 - 0.29f64.powf(CAM16_N)).powf(0.73)))
        .powf(1.0 / 0.9);
    let e_t = 0.25 * ((2.0 + h * std::f64::consts::PI / 180.0).cos() + 3.8);
    let a_resp = white.response * signed_pow(jj / 100.0, 1.0 / (CAM16_C * CAM16_Z));
    let p1 = if t != 0.0 {
        (50000.0 / 13.0) * CAM16_N_C * CAM16_N_CB * e_t / t
    } else {
        0.0
    };
    let p2 = a_resp / CAM16_N_BB + 0.305;
    let p3 = 21.0 / 20.0;
    let (mut a, mut b) = cam16_opponent_inverse(p1, p2, p3, h);
    // Achromatic guard, matching colour's `ab * np.where(t == 0, 0, 1)`:
    // when t == 0 the hue is undefined and the opponent inverse must be zeroed.
    if t == 0.0 {
        a = 0.0;
        b = 0.0;
    }
    let ra = (460.0 * p2 + 451.0 * a + 288.0 * b) / 1403.0;
    let ga = (460.0 * p2 - 891.0 * a - 261.0 * b) / 1403.0;
    let ba = (460.0 * p2 - 220.0 * a - 6300.0 * b) / 1403.0;
    let rgb_c = cam16_padc_inverse([ra, ga, ba]);
    let rgb = [
        rgb_c[0] / white.adaptation[0],
        rgb_c[1] / white.adaptation[1],
        rgb_c[2] / white.adaptation[2],
    ];
    let xyz100 = mat_vec(&CAM16_M16I, rgb);
    [xyz100[0] / 100.0, xyz100[1] / 100.0, xyz100[2] / 100.0]
}

/// Inverse opponent dimensions (CIECAM02/16 `opponent_colour_dimensions_inverse`).
fn cam16_opponent_inverse(p1: f64, p2: f64, p3: f64, h: f64) -> (f64, f64) {
    let hr = h * std::f64::consts::PI / 180.0;
    let s = hr.sin();
    let c = hr.cos();
    let nn = p2 * (2.0 + p3) * (460.0 / 1403.0);
    if s.abs() >= c.abs() {
        let p4 = if s != 0.0 { p1 / s } else { 0.0 };
        let b = nn
            / (p4 + (2.0 + p3) * (220.0 / 1403.0) * (c / s) - (27.0 / 1403.0)
                + p3 * (6300.0 / 1403.0));
        (b * (c / s), b)
    } else {
        let p5 = if c != 0.0 { p1 / c } else { 0.0 };
        let a = nn
            / (p5 + (2.0 + p3) * (220.0 / 1403.0)
                - ((27.0 / 1403.0) - p3 * (6300.0 / 1403.0)) * (s / c));
        (a, a * (s / c))
    }
}

/// Cache baked boundaries by perceptual space and output RGB primaries.
fn cmax_table_cached(space: Space, target: RgbSpace, view: Cam16White) -> Arc<Vec<f64>> {
    type Tables = HashMap<(Space, RgbSpace), Arc<Vec<f64>>>;
    static CACHE: OnceLock<Mutex<Tables>> = OnceLock::new();
    let key = (space, target);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(table) = cache.lock().unwrap().get(&key) {
        return table.clone();
    }
    let table = Arc::new(build_cmax_table(space, target, view));
    cache.lock().unwrap().insert(key, table.clone());
    table
}

/// Bisect the max in-gamut chroma at each `(L, h)` grid node for the given
/// perceptual space, mirroring `_build_polar_perceptual_c_max_table`. Built
/// once per space (see `cmax_table_cached`).
fn build_cmax_table(space: Space, target: RgbSpace, view: Cam16White) -> Vec<f64> {
    use rayon::prelude::*;
    let (l_min, l_max, chroma_upper) = space.table_geometry();
    let mut table = vec![0.0f64; N_L * N_H];
    table
        .par_chunks_exact_mut(N_H)
        .enumerate()
        .for_each(|(i, row)| {
            // Grid value is the lookup-lightness; map to reconstruction
            // lightness (Lr→L for oklrab) before the gamut check.
            let lookup_l = l_min + (l_max - l_min) * (i as f64) / ((N_L - 1) as f64);
            let l = space.recon_lightness(lookup_l);
            for (j, out) in row.iter_mut().enumerate() {
                let h = h_grid(j);
                let (cos_h, sin_h) = (h.cos(), h.sin());
                let mut lo = 0.0f64;
                let mut hi = chroma_upper;
                for _ in 0..N_BISECT {
                    let mid = (lo + hi) * 0.5;
                    let rgb = space.lab_to_rgb([l, mid * cos_h, mid * sin_h], target, view);
                    let in_gamut = rgb.iter().all(|&v| v >= -1e-6 && v <= 1.0 + 1e-6);
                    if in_gamut {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn build_for(algorithm: &str) -> OutputGamutCompress {
        let params = OutputGamutCompressParams {
            algorithm: algorithm.into(),
            knee: [0.0, 1.0, 6.0],
            lightness_compression: Some([0.7, 1.0, 2.2]),
        };
        OutputGamutCompress::build(&params, "sRGB")
    }

    // Full cross-checks against upstream (tmp/gen_oklch_ref.py,
    // tmp/gen_cam16_ref.py) confirmed bit-identical C_max tables and a
    // per-pixel compress matching Python to ~1.6e-8 over 100+ in/out-of-gamut
    // rows for both spaces, and the CAM16-UCS conversion matching colour to
    // ~3e-15 (forward) with a ~4e-15 roundtrip. The reference rows below pin
    // that in CI without depending on the generated dumps.

    fn check_reference(algo: &str, cases: &[([f64; 3], [f64; 3])]) {
        let comp = build_for(algo);
        for (rgb, want) in cases {
            let got = comp.compress(*rgb);
            for c in 0..3 {
                // 1e-6 bound: bit-faithful tables; the per-pixel chain agrees
                // with Python to ~1.6e-8 at extreme out-of-gamut points
                // (release-build FMA reordering), far below the threshold.
                assert!(
                    (got[c] - want[c]).abs() < 1e-6,
                    "{algo} rgb {rgb:?} ch {c}: got {} want {}",
                    got[c],
                    want[c]
                );
            }
        }
    }

    /// Regression guard: oklch reference rows from `compress_rgb_oklch_chroma`
    /// (knee (0,1,6), lightness (0.7,1,2.2)) — in-gamut, super-red, super-cyan.
    #[test]
    fn oklch_matches_python_reference() {
        check_reference(
            "oklch",
            &[
                (
                    [0.5, 0.4, 0.3],
                    [0.49934573392726778, 0.3994604511652195, 0.29954075929970825],
                ),
                (
                    [2.0, 0.0, 0.0],
                    [0.9999165297352135, 0.32648626944154618, 0.2608847299103626],
                ),
                (
                    [0.0, 2.0, 2.0],
                    [0.50013028382005009, 1.0000925606064377, 0.98946014302066698],
                ),
            ],
        );
    }

    /// Regression guard: cam16ucs reference rows from
    /// `compress_rgb_cam16ucs_chroma` (same knee/lightness).
    #[test]
    fn cam16ucs_matches_python_reference() {
        check_reference(
            "cam16ucs",
            &[
                (
                    [0.5, 0.4, 0.3],
                    [
                        0.49974636928661886,
                        0.39981273339722234,
                        0.29983897633446865,
                    ],
                ),
                (
                    [2.0, 0.0, 0.0],
                    [0.9987900431158222, 0.32032525739267781, 0.25154805990152163],
                ),
                (
                    [0.0, 2.0, 2.0],
                    [
                        0.46476658163581974,
                        0.99783501836142674,
                        0.99071804859295698,
                    ],
                ),
            ],
        );
    }

    #[test]
    fn cam16ucs_uses_the_destination_primaries_and_white() {
        let params = OutputGamutCompressParams::default();
        for (target, expected) in [
            (
                "ITU-R BT.2020",
                [0.999_107_053, 0.486_623_383, 0.528_226_888],
            ),
            (
                "ProPhoto RGB",
                [0.998_866_418, 0.503_254_590, 0.555_874_980],
            ),
        ] {
            let comp = OutputGamutCompress::build(&params, target);
            let rgb = comp.compress([2.0, 0.1, 0.3]);
            for (actual, expected) in rgb.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-6, "{target}: {rgb:?}");
            }
            let gpu = comp.gpu_params().unwrap();
            assert_eq!(gpu.mode, spektrafilm_gpu::GamutMode::Cam16ucs);
            assert_eq!(gpu.target.name(), target);
        }
    }

    #[test]
    fn cam16ucs_preserves_signed_lightness_below_black() {
        let params = OutputGamutCompressParams::default();
        for (target, expected) in [
            (
                "DCI-P3",
                [-0.008_287_031_476, -0.008_042_001_813, -0.008_137_953_822],
            ),
            (
                "Display P3",
                [-0.007_540_716_123, -0.007_409_805_518, -0.007_378_748_681],
            ),
            (
                "ProPhoto RGB",
                [-0.078_304_226_014, -0.077_950_152_323, -0.079_298_662_671],
            ),
        ] {
            let comp = OutputGamutCompress::build(&params, target);
            let rgb = comp.compress([-0.25, 0.0, 0.5]);
            for (actual, expected) in rgb.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-6, "{target}: {rgb:?}");
            }
        }
    }

    /// Safdar 2017 reference rows from Python's `compress_rgb_jzazbz_chroma`
    /// with the default chroma and lightness knees.
    #[test]
    fn jzazbz_matches_python_reference() {
        let params = OutputGamutCompressParams {
            algorithm: "jzazbz".into(),
            ..Default::default()
        };
        let srgb = OutputGamutCompress::build(&params, "sRGB");
        assert!(srgb.is_active());
        assert_eq!(
            srgb.gpu_params().unwrap().mode,
            spektrafilm_gpu::GamutMode::Jzazbz
        );
        assert_eq!(srgb.compress([0.0; 3]), [0.0; 3]);
        let cases = [
            (
                "sRGB",
                [0.5, 0.4, 0.3],
                [0.500_001_736, 0.400_024_820, 0.300_007_383],
            ),
            (
                "sRGB",
                [2.0, 0.0, 0.0],
                [1.000_143_220, 0.414_960_635, 0.327_763_363],
            ),
            (
                "sRGB",
                [-0.25, 0.0, 0.5],
                [-0.082_010_818, -0.081_771_448, -0.081_736_008],
            ),
            (
                "ITU-R BT.2020",
                [1.0, -0.2, 0.7],
                [0.794_302_238, 0.005_812_849, 0.592_695_809],
            ),
            (
                "ProPhoto RGB",
                [0.0, 2.0, 2.0],
                [0.776_402_056, 0.811_315_640, 1.056_883_781],
            ),
        ];
        for (target, rgb, expected) in cases {
            let comp = OutputGamutCompress::build(&params, target);
            let actual = comp.compress(rgb);
            for (got, want) in actual.into_iter().zip(expected) {
                assert!((got - want).abs() < 1e-6, "{target} {rgb:?}: {actual:?}");
            }
        }
    }

    /// Regression guard: oklrab reference rows from `compress_rgb_oklrab_chroma`.
    #[test]
    fn oklrab_matches_python_reference() {
        check_reference(
            "oklrab",
            &[
                (
                    [0.5, 0.4, 0.3],
                    [0.4993457339279313, 0.39946045116508211, 0.29954075929887197],
                ),
                (
                    [2.0, 0.0, 0.0],
                    [
                        0.99981431200564952,
                        0.32651963747538049,
                        0.26092249322038208,
                    ],
                ),
                (
                    [0.0, 2.0, 2.0],
                    [0.49984508410893458, 1.0001915827542378, 0.98955691468555185],
                ),
            ],
        );
    }

    /// Regression guard: aces_rgc reference rows from `compress_rgb_aces_rgc`
    /// (per-channel knee, no lightness/table).
    #[test]
    fn aces_rgc_matches_python_reference() {
        check_reference(
            "aces_rgc",
            &[
                (
                    [0.5, 0.4, 0.3],
                    [0.5, 0.40000106662684631, 0.30013620807161223],
                ),
                (
                    [2.0, 0.0, 0.0],
                    [2.0, 0.21820256371932145, 0.21820256371932145],
                ),
                (
                    [1.2, -0.1, 0.4],
                    [1.2, 0.092551484339614021, 0.41114161919196679],
                ),
            ],
        );
    }

    /// `off` (and unported algorithms) must be a true identity no-op.
    #[test]
    fn off_is_identity() {
        let params = OutputGamutCompressParams {
            algorithm: "off".into(),
            ..Default::default()
        };
        let comp = OutputGamutCompress::build(&params, "sRGB");
        assert!(!comp.is_active());
        assert_eq!(comp.compress([1.7, 0.0, 0.3]), [1.7, 0.0, 0.3]);
    }
}
