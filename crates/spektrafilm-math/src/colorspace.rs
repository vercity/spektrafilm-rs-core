/// Color space conversion matrices and utilities.
///
/// All matrices are row-major: output[i] = dot(matrix[i], input).
/// Source: IEC 61966-2-1 (sRGB), ICC profiles, CIE standards.
use crate::precision::{Scalar, from_f64, srgb_decode, srgb_encode};

/// Component transfer functions for the RGB spaces supported by the pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cctf {
    Srgb,
    ProPhoto,
    Bt2020,
    VLog,
    Linear,
}

impl Cctf {
    pub fn for_rgb_space(name: &str) -> Option<Self> {
        RgbSpace::from_name(name).map(RgbSpace::transfer)
    }

    pub fn encode(self, x: Scalar) -> Scalar {
        match self {
            Self::Srgb => srgb_encode(x),
            // ROMM RGB uses a 1/512 linear-to-power transition.
            Self::ProPhoto if x < from_f64(1.0 / 512.0) => x * from_f64(16.0),
            Self::ProPhoto => x.powf(from_f64(1.0 / 1.8)),
            Self::Bt2020 if x < from_f64(0.018) => x * from_f64(4.5),
            Self::Bt2020 => from_f64(1.099) * x.powf(from_f64(0.45)) - from_f64(0.099),
            Self::VLog if x < from_f64(0.01) => from_f64(5.6) * x + from_f64(0.125),
            Self::VLog => from_f64(0.241514) * (x + from_f64(0.00873)).log10() + from_f64(0.598206),
            Self::Linear => x,
        }
    }

    pub fn decode(self, x: Scalar) -> Scalar {
        match self {
            Self::Srgb => srgb_decode(x),
            Self::ProPhoto if x < from_f64(1.0 / 32.0) => x / from_f64(16.0),
            Self::ProPhoto => x.powf(from_f64(1.8)),
            Self::Bt2020 if x < from_f64(0.08124794403514046) => x / from_f64(4.5),
            Self::Bt2020 => ((x + from_f64(0.099)) / from_f64(1.099)).powf(from_f64(1.0 / 0.45)),
            Self::VLog if x < from_f64(0.181) => (x - from_f64(0.125)) / from_f64(5.6),
            Self::VLog => {
                from_f64(10.0).powf((x - from_f64(0.598206)) / from_f64(0.241514))
                    - from_f64(0.00873)
            }
            Self::Linear => x,
        }
    }
}

/// RGB primaries, reference white and their conventional transfer function.
/// Buffers in any space can also carry linear samples.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RgbSpace {
    Srgb,
    ProPhoto,
    Rec2020,
    Aces2065,
    VGamut,
}

impl RgbSpace {
    pub const ALL: [Self; 5] = [
        Self::Srgb,
        Self::ProPhoto,
        Self::Rec2020,
        Self::Aces2065,
        Self::VGamut,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "sRGB" => Self::Srgb,
            "ProPhoto RGB" => Self::ProPhoto,
            "Rec. 2020" | "Rec2020" | "ITU-R BT.2020" => Self::Rec2020,
            "ACES2065-1" => Self::Aces2065,
            "V-Gamut" | "Panasonic V-Log" | "V-Log" | "vlog" => Self::VGamut,
            _ => return None,
        })
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Srgb => "sRGB",
            Self::ProPhoto => "ProPhoto RGB",
            Self::Rec2020 => "ITU-R BT.2020",
            Self::Aces2065 => "ACES2065-1",
            Self::VGamut => "V-Gamut",
        }
    }

    pub const fn transfer(self) -> Cctf {
        match self {
            Self::Srgb => Cctf::Srgb,
            Self::ProPhoto => Cctf::ProPhoto,
            Self::Rec2020 => Cctf::Bt2020,
            Self::Aces2065 => Cctf::Linear,
            Self::VGamut => Cctf::VLog,
        }
    }

    pub const fn primaries(self) -> [[f64; 2]; 3] {
        match self {
            Self::Srgb => [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]],
            Self::ProPhoto => [[0.7347, 0.2653], [0.1596, 0.8404], [0.0366, 0.0001]],
            Self::Rec2020 => [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]],
            Self::Aces2065 => [[0.7347, 0.2653], [0.0, 1.0], [0.0001, -0.077]],
            Self::VGamut => [[0.73, 0.28], [0.165, 0.84], [0.10, -0.03]],
        }
    }

    pub const fn white_xy(self) -> [f64; 2] {
        match self {
            Self::ProPhoto => [0.3457, 0.3585],
            Self::Aces2065 => [0.32168, 0.33767],
            _ => [0.3127, 0.329],
        }
    }

    pub fn white_xyz(self) -> [f64; 3] {
        let [x, y] = self.white_xy();
        xy_to_xyz_f64(x, y)
    }

    pub const fn to_xyz(self) -> [[f64; 3]; 3] {
        match self {
            Self::Srgb => SRGB_TO_XYZ_F64,
            Self::ProPhoto => PROPHOTO_TO_XYZ_F64,
            Self::Rec2020 => REC2020_TO_XYZ_F64,
            Self::Aces2065 => ACES_TO_XYZ_F64,
            // Panasonic V-Gamut reference coefficients used by colour-science.
            Self::VGamut => [
                [0.679644, 0.152211, 0.118600],
                [0.260686, 0.774894, -0.035580],
                [-0.009310, -0.004612, 1.102980],
            ],
        }
    }

    pub const fn from_xyz(self) -> [[f64; 3]; 3] {
        match self {
            Self::Srgb => XYZ_TO_SRGB_F64,
            Self::ProPhoto => XYZ_TO_PROPHOTO_F64,
            Self::Rec2020 => XYZ_TO_REC2020_F64,
            Self::Aces2065 => XYZ_TO_ACES_F64,
            Self::VGamut => [
                [1.589012, -0.313204, -0.180965],
                [-0.534053, 1.396011, 0.102458],
                [0.011179, 0.003194, 0.905535],
            ],
        }
    }
}

/// sRGB → CIE XYZ (D65), assuming linear input.
/// Matches Python colour-science 4-digit precision for exact parity.
pub const SRGB_TO_XYZ: [[f32; 3]; 3] = [
    [0.4124, 0.3576, 0.1805],
    [0.2126, 0.7152, 0.0722],
    [0.0193, 0.1192, 0.9505],
];

/// f64 variants for the calibration chain (Python parity).
pub const SRGB_TO_XYZ_F64: [[f64; 3]; 3] = [
    [0.4124, 0.3576, 0.1805],
    [0.2126, 0.7152, 0.0722],
    [0.0193, 0.1192, 0.9505],
];
/// ROMM RGB coefficients as provided by the Python reference.
pub const PROPHOTO_TO_XYZ_F64: [[f64; 3]; 3] = [
    [0.7977, 0.1352, 0.0313],
    [0.2880, 0.7119, 0.0001],
    [0.0, 0.0, 0.8249],
];
pub const REC2020_TO_XYZ_F64: [[f64; 3]; 3] = [
    [
        0.63695804830129099,
        0.14461690358620841,
        0.16888097516417205,
    ],
    [
        0.26270021201126692,
        0.67799807151887115,
        0.059301716469861938,
    ],
    [0.0, 0.028072693049087445, 1.0609850577107907],
];
// Bit-exact match with colour-science (Python `repr` decimals
// round-trip to the same f64 bits in Rust's correctly-rounded parser).
pub const ACES_TO_XYZ_F64: [[f64; 3]; 3] = [
    [0.9525523959, 0.0, 9.36786e-05],
    [0.3439664498, 0.7281660966, -0.0721325464],
    [0.0, 0.0, 1.0088251844],
];

/// f64 XYZ → RGB matrices (inverses of the *_TO_XYZ_F64 matrices),
/// matching colour-science exactly. colour-science stores the sRGB
/// matrix at the IEC 61966-2-1 standard 4-decimal precision — using
/// the high-precision inverse here produces ~1e-5 of drift in the
/// scan stage.
pub const XYZ_TO_SRGB_F64: [[f64; 3]; 3] = [
    [3.2406, -1.5372, -0.4986],
    [-0.9689, 1.8758, 0.0415],
    [0.0557, -0.204, 1.057],
];
pub const XYZ_TO_PROPHOTO_F64: [[f64; 3]; 3] = [
    [1.346, -0.2556, -0.0511],
    [-0.5446, 1.5082, 0.0205],
    [0.0, 0.0, 1.2123],
];
pub const XYZ_TO_REC2020_F64: [[f64; 3]; 3] = [
    [
        1.7166511879712687,
        -0.35567078377639255,
        -0.25336628137365996,
    ],
    [
        -0.66668435183248886,
        1.6164812366349386,
        0.015768545813911142,
    ],
    [
        0.017639857445310794,
        -0.042770613257808537,
        0.94210312123547413,
    ],
];
pub const XYZ_TO_ACES_F64: [[f64; 3]; 3] = [
    [1.0498110175, 0.0, -9.74845e-05],
    [-0.4959030231, 1.3733130458, 0.0982400361],
    [0.0, 0.0, 0.9912520182],
];

/// Multiply two row-major 3×3 matrices.
pub fn mat3_mul3_f64(left: &[[f64; 3]; 3], right: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut product = [[0.0; 3]; 3];
    for (out, row) in product.iter_mut().zip(left) {
        for (j, value) in out.iter_mut().enumerate() {
            *value = row[0] * right[0][j] + row[1] * right[1][j] + row[2] * right[2][j];
        }
    }
    product
}

/// CIE XYZ (D65) → sRGB linear.
/// Exact inverse of the 4-digit SRGB_TO_XYZ above.
pub const XYZ_TO_SRGB: [[f32; 3]; 3] = [
    [3.2406255, -1.5372080, -0.4986286],
    [-0.9689307, 1.8757561, 0.0415175],
    [0.0557101, -0.2040211, 1.0569959],
];

/// ProPhoto RGB → CIE XYZ (D50).
pub const PROPHOTO_TO_XYZ: [[f32; 3]; 3] = [
    [0.7977, 0.1352, 0.0313],
    [0.2880, 0.7119, 0.0001],
    [0.0, 0.0, 0.8249],
];

/// CIE XYZ (D50) → ProPhoto RGB.
pub const XYZ_TO_PROPHOTO: [[f32; 3]; 3] = [
    [1.3459433, -0.2556075, -0.0511118],
    [-0.5445989, 1.5081673, 0.0205351],
    [0.0000000, 0.0000000, 1.2118128],
];

/// Rec. 2020 → CIE XYZ (D65).
pub const REC2020_TO_XYZ: [[f32; 3]; 3] = [
    [0.6369580, 0.1446169, 0.1688810],
    [0.2627002, 0.6779981, 0.0593017],
    [0.0000000, 0.0280727, 1.0609851],
];

/// CIE XYZ (D65) → Rec. 2020.
pub const XYZ_TO_REC2020: [[f32; 3]; 3] = [
    [1.7166512, -0.3556708, -0.2533663],
    [-0.6666844, 1.6164812, 0.0157685],
    [0.0176399, -0.0427706, 0.9421031],
];

/// ACES AP0 (ACES2065-1) → CIE XYZ (D60-ish, ACES white).
pub const ACES_TO_XYZ: [[f32; 3]; 3] = [
    [0.9525524, 0.0000000, 0.0000937],
    [0.3439664, 0.7281661, -0.0721325],
    [0.0000000, 0.0000000, 1.0088252],
];

/// CIE XYZ → ACES AP0.
pub const XYZ_TO_ACES: [[f32; 3]; 3] = [
    [1.0498110, 0.0000000, -0.0000974],
    [-0.4959030, 1.3733131, 0.0982400],
    [0.0000000, 0.0000000, 0.9912520],
];

/// CAT02 forward matrix (XYZ → LMS).
pub const CAT02_FORWARD: [[f32; 3]; 3] = [
    [0.7328, 0.4296, -0.1624],
    [-0.7036, 1.6975, 0.0061],
    [0.0030, 0.0136, 0.9834],
];

/// CAT02 inverse matrix (LMS → XYZ).
pub const CAT02_INVERSE: [[f32; 3]; 3] = [
    [1.096124, -0.278869, 0.182745],
    [0.454369, 0.473533, 0.072098],
    [-0.009628, -0.005698, 1.015326],
];

/// CIE D50 white point (XYZ, Y=1).
pub const D50_XYZ: [f32; 3] = [0.96422, 1.0, 0.82521];
/// CIE D55 white point (XYZ, Y=1).
pub const D55_XYZ: [f32; 3] = [0.95682, 1.0, 0.92149];
/// CIE D65 white point (XYZ, Y=1).
/// Derived from xy=(0.3127, 0.3290): X=0.3127/0.3290, Z=(1-0.3127-0.3290)/0.3290
pub const D65_XYZ: [f32; 3] = [0.95047, 1.0, 1.08883];

/// sRGB whitepoint as xy chromaticity (matches Python colour-science).
pub const SRGB_WHITE_XY: (f32, f32) = (0.3127, 0.329);
/// Convert xy chromaticity to XYZ (Y=1).
pub fn xy_to_xyz(x: f32, y: f32) -> [f32; 3] {
    if y <= 0.0 {
        return [0.0, 1.0, 0.0];
    }
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Convert xy chromaticity to XYZ (Y=1) in f64.
pub fn xy_to_xyz_f64(x: f64, y: f64) -> [f64; 3] {
    if y <= 0.0 {
        return [0.0, 1.0, 0.0];
    }
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Apply 3x3 matrix to RGB triple.
#[inline]
pub fn mat3_mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Chromatic adaptation from source white to destination white using CAT02.
/// Computes in f64 internally for precision, returns f32.
pub fn chromatic_adaptation_matrix(src_white: [f32; 3], dst_white: [f32; 3]) -> [[f32; 3]; 3] {
    // High-precision CAT02 matrices (f64)
    const FWD: [[f64; 3]; 3] = [
        [0.7328, 0.4296, -0.1624],
        [-0.7036, 1.6975, 0.0061],
        [0.003, 0.0136, 0.9834],
    ];
    const INV: [[f64; 3]; 3] = [
        [1.096123820835514, -0.278869000218287, 0.182745179382773],
        [0.454369041975359, 0.473533154307412, 0.072097803717229],
        [-0.009627608738429, -0.005698031216113, 1.015325639954543],
    ];

    let sw = [
        src_white[0] as f64,
        src_white[1] as f64,
        src_white[2] as f64,
    ];
    let dw = [
        dst_white[0] as f64,
        dst_white[1] as f64,
        dst_white[2] as f64,
    ];

    // src/dst → LMS
    let mut src_lms = [0.0f64; 3];
    let mut dst_lms = [0.0f64; 3];
    for i in 0..3 {
        src_lms[i] = FWD[i][0] * sw[0] + FWD[i][1] * sw[1] + FWD[i][2] * sw[2];
        dst_lms[i] = FWD[i][0] * dw[0] + FWD[i][1] * dw[1] + FWD[i][2] * dw[2];
    }

    let gain = [
        dst_lms[0] / src_lms[0],
        dst_lms[1] / src_lms[1],
        dst_lms[2] / src_lms[2],
    ];

    // M_adapt = INV * diag(gain) * FWD
    let mut result = [[0.0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            let sum = INV[i][0] * gain[0] * FWD[0][j]
                + INV[i][1] * gain[1] * FWD[1][j]
                + INV[i][2] * gain[2] * FWD[2][j];
            result[i][j] = sum as f32;
        }
    }
    result
}

/// Chromatic adaptation in full f64 precision, returning f64 matrix.
pub fn chromatic_adaptation_matrix_f64(src_white: [f64; 3], dst_white: [f64; 3]) -> [[f64; 3]; 3] {
    const FWD: [[f64; 3]; 3] = [
        [0.7328, 0.4296, -0.1624],
        [-0.7036, 1.6975, 0.0061],
        [0.003, 0.0136, 0.9834],
    ];
    const INV: [[f64; 3]; 3] = [
        [1.096123820835514, -0.278869000218287, 0.182745179382773],
        [0.454369041975359, 0.473533154307412, 0.072097803717229],
        [-0.009627608738429, -0.005698031216113, 1.015325639954543],
    ];
    von_kries(src_white, dst_white, &FWD, &INV)
}

/// CAT16 (Li et al. 2017) von Kries chromatic adaptation. Same form as
/// `chromatic_adaptation_matrix_f64` (CAT02) but with the CIECAM16 cone
/// matrix `M16`, which avoids CAT02's cone-primary instabilities around
/// blue/violet. Matches colour-science's
/// `matrix_chromatic_adaptation_VonKries(..., transform="CAT16")`.
pub fn chromatic_adaptation_matrix_cat16_f64(
    src_white: [f64; 3],
    dst_white: [f64; 3],
) -> [[f64; 3]; 3] {
    const FWD: [[f64; 3]; 3] = [
        [0.401288, 0.650173, -0.051461],
        [-0.250268, 1.204414, 0.045854],
        [-0.002079, 0.048952, 0.953127],
    ];
    const INV: [[f64; 3]; 3] = [
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
    von_kries(src_white, dst_white, &FWD, &INV)
}

/// Shared von Kries adaptation: `inv · diag(dst_lms/src_lms) · fwd`, where
/// `fwd`/`inv` are the cone matrix and its inverse for the chosen transform.
fn von_kries(
    src_white: [f64; 3],
    dst_white: [f64; 3],
    fwd: &[[f64; 3]; 3],
    inv: &[[f64; 3]; 3],
) -> [[f64; 3]; 3] {
    let mut src_lms = [0.0f64; 3];
    let mut dst_lms = [0.0f64; 3];
    for i in 0..3 {
        src_lms[i] = fwd[i][0] * src_white[0] + fwd[i][1] * src_white[1] + fwd[i][2] * src_white[2];
        dst_lms[i] = fwd[i][0] * dst_white[0] + fwd[i][1] * dst_white[1] + fwd[i][2] * dst_white[2];
    }
    let gain = [
        dst_lms[0] / src_lms[0],
        dst_lms[1] / src_lms[1],
        dst_lms[2] / src_lms[2],
    ];

    let mut result = [[0.0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            result[i][j] = inv[i][0] * gain[0] * fwd[0][j]
                + inv[i][1] * gain[1] * fwd[1][j]
                + inv[i][2] * gain[2] * fwd[2][j];
        }
    }
    result
}

/// Build a full RGB→RGB conversion matrix:
/// src_RGB → src_XYZ → adapted_XYZ → dst_XYZ → dst_RGB
pub fn rgb_to_rgb_matrix(
    src_to_xyz: &[[f32; 3]; 3],
    src_white: [f32; 3],
    xyz_to_dst: &[[f32; 3]; 3],
    dst_white: [f32; 3],
) -> [[f32; 3]; 3] {
    let adapt = chromatic_adaptation_matrix(src_white, dst_white);

    // Combined = xyz_to_dst * adapt * src_to_xyz
    let mut tmp = [[0.0f32; 3]; 3];
    mat3_mul3(&adapt, src_to_xyz, &mut tmp);

    let mut result = [[0.0f32; 3]; 3];
    mat3_mul3(xyz_to_dst, &tmp, &mut result);
    result
}

/// Multiply two 3x3 matrices: out = a * b
fn mat3_mul3(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3], out: &mut [[f32; 3]; 3]) {
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precision::{from_f64, srgb_decode, srgb_encode};

    #[test]
    fn supported_transfer_functions_keep_extended_samples() {
        // Reference values from colour-science for -0.1, 0.18 and 1.5.
        let input = [-0.1, 0.18, 1.5];
        for (cctf, expected) in [
            (Cctf::Srgb, [-1.292, 0.46135612950044164, 1.194176534680845]),
            (
                Cctf::ProPhoto,
                [-1.6, 0.3857114247511376, 1.2526463499187717],
            ),
            (
                Cctf::Bt2020,
                [-0.45, 0.4090077288641504, 1.2199816664692549],
            ),
            (Cctf::Linear, input),
        ] {
            for (linear, encoded) in input.into_iter().zip(expected) {
                assert!((cctf.encode(from_f64(linear)) as f64 - encoded).abs() < 3e-7);
                assert!((cctf.decode(from_f64(encoded)) as f64 - linear).abs() < 3e-7);
            }
        }
    }

    /// CAT16 adaptation matches colour-science's
    /// `matrix_chromatic_adaptation_VonKries(transform="CAT16")` for the
    /// ProPhoto-white → D55 case used by the input projection.
    #[test]
    fn cat16_matches_colour() {
        let src = [0.9642956764295677, 1.0, 0.8251046025104605]; // ProPhoto white
        let dst = [0.9567982961086806, 1.0, 0.9213965001151279]; // D55
        let got = chromatic_adaptation_matrix_cat16_f64(src, dst);
        let want = [
            [
                0.9955740273098072,
                -0.016544957254590615,
                0.01613798204777648,
            ],
            [
                -0.0022206862456965637,
                1.0026392099167525,
                -0.0006033317106774956,
            ],
            [
                -0.00013585839424725028,
                0.0054854466982645,
                1.1102132484680078,
            ],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (got[i][j] - want[i][j]).abs() < 1e-12,
                    "[{i}][{j}] got {} want {}",
                    got[i][j],
                    want[i][j]
                );
            }
        }
    }

    #[test]
    fn test_srgb_roundtrip() {
        for &v in &[0.0_f64, 0.001, 0.01, 0.1, 0.18, 0.5, 0.9, 1.0] {
            let s = from_f64(v);
            let encoded = srgb_encode(s);
            let decoded = srgb_decode(encoded);
            assert!(
                (s - decoded).abs() < from_f64(1e-5),
                "roundtrip failed for {v}: encoded={encoded}, decoded={decoded}"
            );
        }
    }

    #[test]
    fn vlog_matches_panasonic_code_values() {
        let transfer = RgbSpace::VGamut.transfer();
        for (linear, code) in [
            (-0.01, 0.069),
            (0.0, 0.125),
            (0.18, 0.423_311_448_760_136),
            (1.0, 0.599_117_700_158_145_9),
        ] {
            assert!((transfer.encode(from_f64(linear)) - from_f64(code)).abs() < from_f64(1e-6));
            assert!((transfer.decode(from_f64(code)) - from_f64(linear)).abs() < from_f64(1e-6));
        }
        assert!(transfer.decode(from_f64(1.0)) > from_f64(46.0));
    }

    #[test]
    fn test_identity_adaptation() {
        let m = chromatic_adaptation_matrix(D65_XYZ, D65_XYZ);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (m[i][j] - expected).abs() < 1e-4,
                    "identity adaptation [{i}][{j}]: expected {expected}, got {}",
                    m[i][j]
                );
            }
        }
    }

    #[test]
    fn test_srgb_xyz_roundtrip() {
        let rgb = [0.5, 0.3, 0.8];
        let xyz = mat3_mul(&SRGB_TO_XYZ, rgb);
        let rgb2 = mat3_mul(&XYZ_TO_SRGB, xyz);
        for i in 0..3 {
            assert!(
                (rgb[i] - rgb2[i]).abs() < 1e-5,
                "RGB roundtrip failed channel {i}: {:.6} vs {:.6}",
                rgb[i],
                rgb2[i]
            );
        }
    }
}
