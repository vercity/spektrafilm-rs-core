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
    Bt1886,
    Gamma22,
    Gamma26,
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
            Self::Bt1886 => x.abs().powf(from_f64(1.0 / 2.4)).copysign(x),
            Self::Gamma22 => x.abs().powf(from_f64(1.0 / 2.2)).copysign(x),
            Self::Gamma26 => x.abs().powf(from_f64(1.0 / 2.6)).copysign(x),
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
            Self::Bt1886 => x.max(from_f64(0.0)).powf(from_f64(2.4)),
            Self::Gamma22 => x.abs().powf(from_f64(2.2)).copysign(x),
            Self::Gamma26 => x.abs().powf(from_f64(2.6)).copysign(x),
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
    Rec709,
    DisplayP3,
    DciP3,
    AdobeRgb,
    P3D65,
    AcesCg,
    ArriWideGamut3,
    ArriWideGamut4,
    SonySGamut3,
    SonySGamut3Cine,
    FujiFGamut,
    CanonCinemaGamut,
    RedWideGamut,
    DavinciWideGamut,
    BlackmagicWideGamut,
    DjiDGamut,
}

impl RgbSpace {
    /// RGB spaces supported by the raster reader and writer.
    pub const ALL: [Self; 9] = [
        Self::Srgb,
        Self::ProPhoto,
        Self::Rec2020,
        Self::Aces2065,
        Self::VGamut,
        Self::Rec709,
        Self::DisplayP3,
        Self::DciP3,
        Self::AdobeRgb,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "sRGB" => Self::Srgb,
            "ProPhoto RGB" => Self::ProPhoto,
            "Rec. 2020" | "Rec2020" | "ITU-R BT.2020" => Self::Rec2020,
            "ACES2065-1" => Self::Aces2065,
            "V-Gamut" | "Panasonic V-Log" | "V-Log" | "vlog" => Self::VGamut,
            "ITU-R BT.709" | "Rec.709" | "Rec709" => Self::Rec709,
            "Display P3" => Self::DisplayP3,
            "DCI-P3" => Self::DciP3,
            "Adobe RGB (1998)" | "Adobe RGB" => Self::AdobeRgb,
            "P3-D65" => Self::P3D65,
            "ACEScg" | "ACEScct" | "ACEScc" => Self::AcesCg,
            "ARRI Wide Gamut 3" => Self::ArriWideGamut3,
            "ARRI Wide Gamut 4" => Self::ArriWideGamut4,
            "S-Gamut3" => Self::SonySGamut3,
            "S-Gamut3.Cine" => Self::SonySGamut3Cine,
            "F-Gamut" => Self::FujiFGamut,
            "Cinema Gamut" => Self::CanonCinemaGamut,
            "REDWideGamutRGB" => Self::RedWideGamut,
            "DaVinci Wide Gamut" => Self::DavinciWideGamut,
            "Blackmagic Wide Gamut" => Self::BlackmagicWideGamut,
            "DJI D-Gamut" => Self::DjiDGamut,
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
            Self::Rec709 => "ITU-R BT.709",
            Self::DisplayP3 => "Display P3",
            Self::DciP3 => "DCI-P3",
            Self::AdobeRgb => "Adobe RGB (1998)",
            Self::P3D65 => "P3-D65",
            Self::AcesCg => "ACEScg",
            Self::ArriWideGamut3 => "ARRI Wide Gamut 3",
            Self::ArriWideGamut4 => "ARRI Wide Gamut 4",
            Self::SonySGamut3 => "S-Gamut3",
            Self::SonySGamut3Cine => "S-Gamut3.Cine",
            Self::FujiFGamut => "F-Gamut",
            Self::CanonCinemaGamut => "Cinema Gamut",
            Self::RedWideGamut => "REDWideGamutRGB",
            Self::DavinciWideGamut => "DaVinci Wide Gamut",
            Self::BlackmagicWideGamut => "Blackmagic Wide Gamut",
            Self::DjiDGamut => "DJI D-Gamut",
        }
    }

    pub const fn transfer(self) -> Cctf {
        match self {
            Self::Srgb => Cctf::Srgb,
            Self::ProPhoto => Cctf::ProPhoto,
            Self::Rec2020 => Cctf::Bt2020,
            Self::Aces2065 => Cctf::Linear,
            Self::VGamut => Cctf::VLog,
            Self::Rec709 => Cctf::Bt1886,
            Self::DisplayP3 => Cctf::Srgb,
            Self::DciP3 => Cctf::Gamma26,
            Self::AdobeRgb => Cctf::Gamma22,
            Self::P3D65 => Cctf::Linear,
            Self::AcesCg => Cctf::Linear,
            Self::ArriWideGamut3 => Cctf::Linear,
            Self::ArriWideGamut4 => Cctf::Linear,
            Self::SonySGamut3 => Cctf::Linear,
            Self::SonySGamut3Cine => Cctf::Linear,
            Self::FujiFGamut => Cctf::Linear,
            Self::CanonCinemaGamut => Cctf::Linear,
            Self::RedWideGamut => Cctf::Linear,
            Self::DavinciWideGamut => Cctf::Linear,
            Self::BlackmagicWideGamut => Cctf::Linear,
            Self::DjiDGamut => Cctf::Linear,
        }
    }

    pub const fn primaries(self) -> [[f64; 2]; 3] {
        match self {
            Self::Srgb => [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]],
            Self::ProPhoto => [[0.7347, 0.2653], [0.1596, 0.8404], [0.0366, 0.0001]],
            Self::Rec2020 => [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]],
            Self::Aces2065 => [[0.7347, 0.2653], [0.0, 1.0], [0.0001, -0.077]],
            Self::VGamut => [[0.73, 0.28], [0.165, 0.84], [0.10, -0.03]],
            Self::Rec709 => [[0.64, 0.33], [0.3, 0.6], [0.15, 0.06]],
            Self::DisplayP3 => [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06]],
            Self::DciP3 => [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06]],
            Self::AdobeRgb => [[0.64, 0.33], [0.21, 0.71], [0.15, 0.06]],
            Self::P3D65 => [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06]],
            Self::AcesCg => [[0.713, 0.293], [0.165, 0.83], [0.128, 0.044]],
            Self::ArriWideGamut3 => [[0.684, 0.313], [0.221, 0.848], [0.0861, -0.102]],
            Self::ArriWideGamut4 => [[0.7347, 0.2653], [0.1424, 0.8576], [0.0991, -0.0308]],
            Self::SonySGamut3 => [[0.73, 0.28], [0.14, 0.855], [0.1, -0.05]],
            Self::SonySGamut3Cine => [[0.766, 0.275], [0.225, 0.8], [0.089, -0.087]],
            Self::FujiFGamut => [[0.708, 0.292], [0.17, 0.797], [0.131, 0.046]],
            Self::CanonCinemaGamut => [[0.74, 0.27], [0.17, 1.14], [0.08, -0.1]],
            Self::RedWideGamut => [
                [0.780308, 0.304253],
                [0.121595, 1.493994],
                [0.095612, -0.084589],
            ],
            Self::DavinciWideGamut => [[0.8, 0.313], [0.1682, 0.9877], [0.079, -0.1155]],
            Self::BlackmagicWideGamut => [
                [0.7177215, 0.3171181],
                [0.228041, 0.861569],
                [0.1005841, -0.0820452],
            ],
            Self::DjiDGamut => [[0.71, 0.31], [0.21, 0.88], [0.09, -0.08]],
        }
    }

    pub const fn white_xy(self) -> [f64; 2] {
        match self {
            Self::ProPhoto => [0.3457, 0.3585],
            Self::Aces2065 => [0.32168, 0.33767],
            Self::Rec709 => [0.3127, 0.329],
            Self::DisplayP3 => [0.3127, 0.329],
            Self::DciP3 => [0.314, 0.351],
            Self::AdobeRgb => [0.3127, 0.329],
            Self::P3D65 => [0.3127, 0.329],
            Self::AcesCg => [0.32168, 0.33767],
            Self::ArriWideGamut3 => [0.3127, 0.329],
            Self::ArriWideGamut4 => [0.3127, 0.329],
            Self::SonySGamut3 => [0.3127, 0.329],
            Self::SonySGamut3Cine => [0.3127, 0.329],
            Self::FujiFGamut => [0.3127, 0.329],
            Self::CanonCinemaGamut => [0.3127, 0.329],
            Self::RedWideGamut => [0.3127, 0.329],
            Self::DavinciWideGamut => [0.3127, 0.329],
            Self::BlackmagicWideGamut => [0.312717, 0.3290312],
            Self::DjiDGamut => [0.3127, 0.329],
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
            Self::Rec709 => [
                [
                    0.41239079926595934,
                    0.35758433938387796,
                    0.18048078840183432,
                ],
                [0.2126390058715103, 0.7151686787677559, 0.07219231536073373],
                [
                    0.019330818715591825,
                    0.11919477979462595,
                    0.9505321522496608,
                ],
            ],
            Self::DisplayP3 => [
                [0.486570948648216, 0.2656676931690931, 0.19821728523436247],
                [0.2289745640697487, 0.6917385218365064, 0.07928691409374498],
                [
                    -3.9720755169334855e-17,
                    0.04511338185890264,
                    1.0439443689009757,
                ],
            ],
            Self::DciP3 => [
                [0.4451698155645524, 0.2771344092067776, 0.1722826698155645],
                [0.20949167791273052, 0.7215952541610436, 0.0689130679262258],
                [
                    -3.634101316969856e-17,
                    0.04706056005398115,
                    0.9073553943619731,
                ],
            ],
            Self::AdobeRgb => [
                [0.57667, 0.18556, 0.18823],
                [0.29734, 0.62736, 0.07529],
                [0.02703, 0.07069, 0.99134],
            ],
            Self::P3D65 => [
                [0.486570948648216, 0.2656676931690931, 0.19821728523436247],
                [0.2289745640697487, 0.6917385218365064, 0.07928691409374498],
                [
                    -3.9720755169334855e-17,
                    0.04511338185890264,
                    1.0439443689009757,
                ],
            ],
            Self::AcesCg => [
                [0.6624541811085054, 0.13400420645643313, 0.1561876870049077],
                [0.27222871678091454, 0.6740817658111483, 0.05368951740793702],
                [
                    -0.005574649490394108,
                    0.004060733528982825,
                    1.0103391003129967,
                ],
            ],
            Self::ArriWideGamut3 => [
                [0.638008, 0.214704, 0.097744],
                [0.291954, 0.823841, -0.115795],
                [0.002798, -0.067034, 1.153294],
            ],
            Self::ArriWideGamut4 => [
                [0.7048583204072321, 0.129760295170463, 0.11583731147397654],
                [0.254524176404027, 0.781477732712002, -0.03600190911602904],
                [0.0, 0.0, 1.0890577507598784],
            ],
            Self::SonySGamut3 => [
                [0.7064827132, 0.1288010498, 0.1151721641],
                [0.2709796708, 0.7866064112, -0.057586082],
                [-0.0096778454, 0.0046000375, 1.0941355587],
            ],
            Self::SonySGamut3Cine => [
                [0.5990839208, 0.2489255161, 0.1024464902],
                [0.2150758201, 0.8850685017, -0.1001443219],
                [-0.0320658495, -0.0276583907, 1.148781991],
            ],
            Self::FujiFGamut => [
                [0.636958048301291, 0.1446169035862084, 0.16888097516417205],
                [0.2627002120112669, 0.6779980715188711, 0.05930171646986194],
                [
                    4.9941065744660724e-17,
                    0.028072693049087445,
                    1.0609850577107907,
                ],
            ],
            Self::CanonCinemaGamut => [
                [0.7160496465515205, 0.12968347787573964, 0.10472280262441158],
                [0.2612613575255548, 0.8696421457549598, -0.13090350328051448],
                [
                    -0.009676346575020556,
                    -0.2364816361263487,
                    1.3352157334612478,
                ],
            ],
            Self::RedWideGamut => [
                [0.735275, 0.068609, 0.146571],
                [0.286694, 0.842979, -0.129673],
                [-0.079681, -0.347343, 1.516082],
            ],
            Self::DavinciWideGamut => [
                [0.7006223920936714, 0.14877481512319676, 0.10105871983480325],
                [0.2741185109066489, 0.8736318959404367, -0.14775040684708576],
                [
                    -0.09896291288323113,
                    -0.1378953250755433,
                    1.3259159887186527,
                ],
            ],
            Self::BlackmagicWideGamut => [
                [0.6065303722131459, 0.2204080953375982, 0.12347900045836878],
                [0.26798940707297414, 0.8327308786223493, -0.1007202856953232],
                [
                    -0.029442166015309742,
                    -0.08661060696630066,
                    1.2048607644426022,
                ],
            ],
            Self::DjiDGamut => [
                [0.6482, 0.194, 0.1082],
                [0.283, 0.8132, -0.0962],
                [-0.0183, -0.0832, 1.1903],
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
            Self::Rec709 => [
                [3.2409699419045226, -1.537383177570094, -0.49861076029300344],
                [-0.9692436362808798, 1.8759675015077206, 0.04155505740717561],
                [
                    0.055630079696993635,
                    -0.2039769588889765,
                    1.0569715142428784,
                ],
            ],
            Self::DisplayP3 => [
                [
                    2.4934969119414263,
                    -0.9313836179191245,
                    -0.40271078445071706,
                ],
                [-0.8294889695615748, 1.7626640603183463, 0.0236246858419436],
                [
                    0.035845830243784474,
                    -0.07617238926804183,
                    0.9568845240076874,
                ],
            ],
            Self::DciP3 => [
                [2.725394030491733, -1.018003006227185, -0.4401631951900365],
                [-0.7951680258087642, 1.689732054843624, 0.022647190608477447],
                [
                    0.041241891395700045,
                    -0.08763901921586241,
                    1.1009293786463223,
                ],
            ],
            Self::AdobeRgb => [
                [2.04159, -0.56501, -0.34473],
                [-0.96924, 1.87597, 0.04156],
                [0.01344, -0.11836, 1.01517],
            ],
            Self::P3D65 => [
                [
                    2.4934969119414263,
                    -0.9313836179191245,
                    -0.40271078445071706,
                ],
                [-0.8294889695615748, 1.7626640603183463, 0.0236246858419436],
                [
                    0.035845830243784474,
                    -0.07617238926804183,
                    0.9568845240076874,
                ],
            ],
            Self::AcesCg => [
                [1.6410233796943257, -0.32480329418479, -0.2364246952376122],
                [-0.6636628587229829, 1.615331591657338, 0.01675634768553013],
                [
                    0.011721894328375377,
                    -0.008284441996237412,
                    0.988394858539022,
                ],
            ],
            Self::ArriWideGamut3 => [
                [1.789066, -0.482534, -0.200076],
                [-0.639849, 1.3964, 0.194432],
                [-0.041532, 0.082335, 0.878868],
            ],
            Self::ArriWideGamut4 => [
                [
                    1.5092154722421087,
                    -0.25059734520437993,
                    -0.16881147529407306,
                ],
                [
                    -0.49154545166061886,
                    1.3612455459293507,
                    0.09728294201372903,
                ],
                [0.0, 0.0, 0.9182249511582473],
            ],
            Self::SonySGamut3 => [
                [1.5073998991, -0.2458221374, -0.1716116808],
                [-0.5181517271, 1.3553912409, 0.1258786682],
                [0.0155116982, -0.0078727714, 0.9119163656],
            ],
            Self::SonySGamut3Cine => [
                [1.8467789693, -0.525986123, -0.2105452114],
                [-0.4441532629, 1.2594429028, 0.1493999729],
                [0.0408554212, 0.0156408893, 0.8682072487],
            ],
            Self::FujiFGamut => [
                [
                    1.7166511879712687,
                    -0.35567078377639255,
                    -0.25336628137365996,
                ],
                [
                    -0.6666843518324889,
                    1.6164812366349386,
                    0.015768545813911142,
                ],
                [
                    0.017639857445310794,
                    -0.04277061325780854,
                    0.9421031212354741,
                ],
            ],
            Self::CanonCinemaGamut => [
                [1.4898182749321833, -0.2608959021837416, -0.1424265217774008],
                [-0.4581665744692729, 1.2616277830502278, 0.15962363162996532],
                [-0.0703496677225017, 0.2215576672256381, 0.7761816036271039],
            ],
            Self::RedWideGamut => [
                [
                    1.412806612336158,
                    -0.17752236616704917,
                    -0.15177037638116067,
                ],
                [-0.48620318583506894, 1.290696210836872, 0.15740028369104153],
                [-0.0371387758049716, 0.2863757595579626, 0.6876796053100493],
            ],
            Self::DavinciWideGamut => [
                [1.516672042024044, -0.2814780478789693, -0.14696363323677802],
                [
                    -0.46491710123327584,
                    1.2514237756817124,
                    0.17488460886509075,
                ],
                [0.06484904706715999, 0.10913934371056985, 0.7614146215498789],
            ],
            Self::BlackmagicWideGamut => [
                [
                    1.8663823403522926,
                    -0.5183973428703209,
                    -0.23460980943156387,
                ],
                [-0.6003424924985552, 1.3781489624976893, 0.17673183028613085],
                [
                    0.002451993738130899,
                    0.08639967419752269,
                    0.8369427073175155,
                ],
            ],
            Self::DjiDGamut => [
                [1.7257, -0.4314, -0.1917],
                [-0.6025, 1.3906, 0.1671],
                [-0.0156, 0.0905, 0.8489],
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
