use std::str::FromStr;

use anyhow::{Result, ensure};
use clap::ValueEnum;
use spektrafilm_math::colorspace::RgbSpace;

use super::camera_curves::CameraCurve;

const FILM_MIDGRAY: f64 = 0.18;
const PQ_PEAK: f64 = 10_000.0;
const PQ_MIDGRAY: f64 = 100.0;
const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 32.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 128.0;
const PQ_C3: f64 = 2392.0 / 128.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum Encoding {
    #[value(name = "acescc", aliases = ["ACEScc"])]
    AcesCc,
    #[value(name = "acescct", aliases = ["ACEScct"])]
    AcesCct,
    #[value(name = "logc3", aliases = ["ARRI LogC3 (EI800)"])]
    ArriLogC3,
    #[value(name = "logc4", aliases = ["ARRI LogC4"])]
    ArriLogC4,
    #[value(name = "adobergb", aliases = ["Adobe RGB"])]
    AdobeRgb,
    #[value(name = "applelog", aliases = ["Apple Log"])]
    AppleLog,
    #[value(name = "bmfilmgen5", aliases = ["Blackmagic Film Gen 5"])]
    BlackmagicFilmGen5,
    #[value(name = "canonlog3", aliases = ["Canon Log 3"])]
    CanonLog3,
    #[value(name = "dcip3", aliases = ["DCI-P3"])]
    DciP3,
    #[value(name = "dlog", aliases = ["DJI D-Log"])]
    DjiDLog,
    #[value(name = "davinciintermediate", aliases = ["DaVinci Intermediate"])]
    DavinciIntermediate,
    #[value(name = "displayp3", aliases = ["Display P3"])]
    DisplayP3,
    #[value(name = "flog", aliases = ["Fujifilm F-Log"])]
    FujiFLog,
    #[value(name = "flog2", aliases = ["Fujifilm F-Log2"])]
    FujiFLog2,
    #[value(name = "nlog", aliases = ["Nikon N-Log"])]
    NikonNLog,
    #[value(name = "p3d65pq", aliases = ["P3-D65 PQ"])]
    P3D65Pq,
    #[value(name = "vlog", aliases = ["Panasonic V-Log", "v-log"])]
    VLog,
    #[value(name = "prophoto", aliases = ["ProPhoto RGB"])]
    Prophoto,
    #[value(name = "redlog3g10", aliases = ["RED Log3G10"])]
    RedLog3G10,
    #[value(name = "rec2020", aliases = ["Rec.2020"])]
    Rec2020,
    #[value(name = "rec2100-hlg", aliases = ["Rec.2100 HLG", "rec2100hlg"])]
    Rec2100Hlg,
    #[value(name = "rec2100-pq", aliases = ["Rec.2100 PQ", "rec2100pq"])]
    Rec2100Pq,
    #[value(name = "rec709", aliases = ["Rec.709"])]
    Rec709,
    #[value(name = "slog3", aliases = ["Sony S-Log3"])]
    SonySLog3,
    #[value(name = "slog3cine", aliases = ["Sony S-Log3 (S-Gamut3.Cine)"])]
    SonySLog3Cine,
    #[value(name = "srgb", aliases = ["sRGB"])]
    Srgb,
    #[value(name = "linear-srgb", aliases = ["sRGB Linear", "srgblin"])]
    LinearSrgb,
    #[value(name = "linear-prophoto", aliases = ["ProPhoto Linear", "prophotolin"])]
    LinearProphoto,
    #[value(name = "linear-rec2020", aliases = ["Rec.2020 Linear", "rec2020lin"])]
    LinearRec2020,
}

#[derive(Clone, Copy)]
enum Transfer {
    Camera(CameraCurve),
    Bt1886,
    Gamma22,
    Gamma26,
    Hlg,
    Linear,
    Pq,
    Prophoto,
    Srgb,
}

struct EncodingSpec {
    name: &'static str,
    short_tag: &'static str,
    space: RgbSpace,
    cctf: Option<&'static str>,
    transfer: Transfer,
    output: bool,
}

impl Encoding {
    fn spec(self) -> EncodingSpec {
        use Transfer::*;
        match self {
            Self::AcesCc => EncodingSpec {
                name: "ACEScc",
                short_tag: "acescc",
                space: RgbSpace::AcesCg,
                cctf: Some("ACEScc"),
                transfer: Camera(CameraCurve::AcesCc),
                output: false,
            },
            Self::AcesCct => EncodingSpec {
                name: "ACEScct",
                short_tag: "acescct",
                space: RgbSpace::AcesCg,
                cctf: Some("ACEScct"),
                transfer: Camera(CameraCurve::AcesCct),
                output: false,
            },
            Self::ArriLogC3 => EncodingSpec {
                name: "ARRI LogC3 (EI800)",
                short_tag: "logc3",
                space: RgbSpace::ArriWideGamut3,
                cctf: Some("ARRI LogC3"),
                transfer: Camera(CameraCurve::Arri3),
                output: false,
            },
            Self::ArriLogC4 => EncodingSpec {
                name: "ARRI LogC4",
                short_tag: "logc4",
                space: RgbSpace::ArriWideGamut4,
                cctf: Some("ARRI LogC4"),
                transfer: Camera(CameraCurve::Arri4),
                output: false,
            },
            Self::AdobeRgb => EncodingSpec {
                name: "Adobe RGB",
                short_tag: "adobergb",
                space: RgbSpace::AdobeRgb,
                cctf: Some("Gamma 2.2"),
                transfer: Gamma22,
                output: true,
            },
            Self::AppleLog => EncodingSpec {
                name: "Apple Log",
                short_tag: "applelog",
                space: RgbSpace::Rec2020,
                cctf: Some("Apple Log Profile"),
                transfer: Camera(CameraCurve::Apple),
                output: false,
            },
            Self::BlackmagicFilmGen5 => EncodingSpec {
                name: "Blackmagic Film Gen 5",
                short_tag: "bmfilmgen5",
                space: RgbSpace::BlackmagicWideGamut,
                cctf: Some("Blackmagic Film Generation 5"),
                transfer: Camera(CameraCurve::Blackmagic),
                output: false,
            },
            Self::CanonLog3 => EncodingSpec {
                name: "Canon Log 3",
                short_tag: "canonlog3",
                space: RgbSpace::CanonCinemaGamut,
                cctf: Some("Canon Log 3"),
                transfer: Camera(CameraCurve::Canon3),
                output: false,
            },
            Self::DciP3 => EncodingSpec {
                name: "DCI-P3",
                short_tag: "dcip3",
                space: RgbSpace::DciP3,
                cctf: Some("Gamma 2.6"),
                transfer: Gamma26,
                output: true,
            },
            Self::DjiDLog => EncodingSpec {
                name: "DJI D-Log",
                short_tag: "dlog",
                space: RgbSpace::DjiDGamut,
                cctf: Some("D-Log"),
                transfer: Camera(CameraCurve::Dji),
                output: false,
            },
            Self::DavinciIntermediate => EncodingSpec {
                name: "DaVinci Intermediate",
                short_tag: "davinciintermediate",
                space: RgbSpace::DavinciWideGamut,
                cctf: Some("DaVinci Intermediate"),
                transfer: Camera(CameraCurve::Davinci),
                output: false,
            },
            Self::DisplayP3 => EncodingSpec {
                name: "Display P3",
                short_tag: "displayp3",
                space: RgbSpace::DisplayP3,
                cctf: Some("sRGB"),
                transfer: Srgb,
                output: true,
            },
            Self::FujiFLog => EncodingSpec {
                name: "Fujifilm F-Log",
                short_tag: "flog",
                space: RgbSpace::FujiFGamut,
                cctf: Some("F-Log"),
                transfer: Camera(CameraCurve::Fuji),
                output: false,
            },
            Self::FujiFLog2 => EncodingSpec {
                name: "Fujifilm F-Log2",
                short_tag: "flog2",
                space: RgbSpace::FujiFGamut,
                cctf: Some("F-Log2"),
                transfer: Camera(CameraCurve::Fuji2),
                output: false,
            },
            Self::NikonNLog => EncodingSpec {
                name: "Nikon N-Log",
                short_tag: "nlog",
                space: RgbSpace::Rec2020,
                cctf: Some("N-Log"),
                transfer: Camera(CameraCurve::Nikon),
                output: false,
            },
            Self::P3D65Pq => EncodingSpec {
                name: "P3-D65 PQ",
                short_tag: "p3d65pq",
                space: RgbSpace::P3D65,
                cctf: Some("ITU-R BT.2100 PQ"),
                transfer: Pq,
                output: true,
            },
            Self::VLog => EncodingSpec {
                name: "Panasonic V-Log",
                short_tag: "vlog",
                space: RgbSpace::VGamut,
                cctf: Some("V-Log"),
                transfer: Camera(CameraCurve::Panasonic),
                output: false,
            },
            Self::Prophoto => EncodingSpec {
                name: "ProPhoto RGB",
                short_tag: "prophoto",
                space: RgbSpace::ProPhoto,
                cctf: Some("ProPhoto RGB"),
                transfer: Prophoto,
                output: true,
            },
            Self::RedLog3G10 => EncodingSpec {
                name: "RED Log3G10",
                short_tag: "redlog3g10",
                space: RgbSpace::RedWideGamut,
                cctf: Some("Log3G10"),
                transfer: Camera(CameraCurve::Red),
                output: false,
            },
            Self::Rec2020 => EncodingSpec {
                name: "Rec.2020",
                short_tag: "rec2020",
                space: RgbSpace::Rec2020,
                cctf: Some("ITU-R BT.1886"),
                transfer: Bt1886,
                output: true,
            },
            Self::Rec2100Hlg => EncodingSpec {
                name: "Rec.2100 HLG",
                short_tag: "rec2100hlg",
                space: RgbSpace::Rec2020,
                cctf: Some("ITU-R BT.2100 HLG"),
                transfer: Hlg,
                output: true,
            },
            Self::Rec2100Pq => EncodingSpec {
                name: "Rec.2100 PQ",
                short_tag: "rec2100pq",
                space: RgbSpace::Rec2020,
                cctf: Some("ITU-R BT.2100 PQ"),
                transfer: Pq,
                output: true,
            },
            Self::Rec709 => EncodingSpec {
                name: "Rec.709",
                short_tag: "rec709",
                space: RgbSpace::Rec709,
                cctf: Some("ITU-R BT.1886"),
                transfer: Bt1886,
                output: true,
            },
            Self::SonySLog3 => EncodingSpec {
                name: "Sony S-Log3",
                short_tag: "slog3",
                space: RgbSpace::SonySGamut3,
                cctf: Some("S-Log3"),
                transfer: Camera(CameraCurve::Sony),
                output: false,
            },
            Self::SonySLog3Cine => EncodingSpec {
                name: "Sony S-Log3 (S-Gamut3.Cine)",
                short_tag: "slog3cine",
                space: RgbSpace::SonySGamut3Cine,
                cctf: Some("S-Log3"),
                transfer: Camera(CameraCurve::Sony),
                output: false,
            },
            Self::Srgb => EncodingSpec {
                name: "sRGB",
                short_tag: "srgb",
                space: RgbSpace::Srgb,
                cctf: Some("sRGB"),
                transfer: Srgb,
                output: true,
            },
            Self::LinearSrgb => EncodingSpec {
                name: "sRGB Linear",
                short_tag: "srgblin",
                space: RgbSpace::Srgb,
                cctf: None,
                transfer: Linear,
                output: true,
            },
            Self::LinearProphoto => EncodingSpec {
                name: "ProPhoto Linear",
                short_tag: "prophotolin",
                space: RgbSpace::ProPhoto,
                cctf: None,
                transfer: Linear,
                output: true,
            },
            Self::LinearRec2020 => EncodingSpec {
                name: "Rec.2020 Linear",
                short_tag: "rec2020lin",
                space: RgbSpace::Rec2020,
                cctf: None,
                transfer: Linear,
                output: true,
            },
        }
    }

    pub(super) fn space(self) -> RgbSpace {
        self.spec().space
    }

    pub(super) fn name(self) -> &'static str {
        self.spec().name
    }

    pub(super) fn short_tag(self) -> &'static str {
        self.spec().short_tag
    }

    pub(super) fn cctf(self) -> Option<&'static str> {
        self.spec().cctf
    }

    pub(super) fn supports_output(self) -> bool {
        self.spec().output
    }

    pub(super) fn decode(self, code: [f64; 3]) -> [f64; 3] {
        use Transfer::*;
        match self.spec().transfer {
            Hlg => hlg_decode(code),
            transfer => code.map(|code| match transfer {
                Srgb if code <= 0.0031308 * 12.92 => code / 12.92,
                Srgb => ((code + 0.055) / 1.055).powf(2.4),
                Bt1886 => code.max(0.0).powf(2.4),
                Gamma22 => signed_pow(code, 2.2),
                Gamma26 => signed_pow(code, 2.6),
                Pq => {
                    let value = code.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
                    PQ_PEAK * ((value - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * value)).powf(1.0 / PQ_M1)
                }
                Camera(curve) => curve.decode(code),
                Prophoto if code < 1.0 / 32.0 => code / 16.0,
                Prophoto => code.powf(1.8),
                Linear => code,
                Hlg => unreachable!("HLG transfer samples the full RGB triple"),
            }),
        }
    }

    pub(super) fn encode(self, rgb: [f64; 3]) -> [f64; 3] {
        use Transfer::*;
        match self.spec().transfer {
            Hlg => hlg_encode(rgb),
            transfer => rgb.map(|value| match transfer {
                Srgb if value <= 0.0031308 => value * 12.92,
                Srgb => 1.055 * value.powf(1.0 / 2.4) - 0.055,
                Bt1886 => signed_pow(value, 1.0 / 2.4),
                Gamma22 => signed_pow(value, 1.0 / 2.2),
                Gamma26 => signed_pow(value, 1.0 / 2.6),
                Pq => {
                    let power = (value.max(0.0) / PQ_PEAK).powf(PQ_M1);
                    ((PQ_C1 + PQ_C2 * power) / (1.0 + PQ_C3 * power)).powf(PQ_M2)
                }
                Camera(curve) => curve.encode(value),
                Prophoto if value < 1.0 / 512.0 => value * 16.0,
                Prophoto => value.powf(1.0 / 1.8),
                Linear => value,
                Hlg => unreachable!("HLG transfer samples the full RGB triple"),
            }),
        }
    }

    fn midgray(self) -> f64 {
        if matches!(self.spec().transfer, Transfer::Pq) {
            PQ_MIDGRAY
        } else {
            FILM_MIDGRAY
        }
    }

    pub(super) fn output_gain(self) -> f64 {
        self.midgray() / FILM_MIDGRAY
    }

    fn auto_stops(self) -> f64 {
        use Transfer::*;
        match self.spec().transfer {
            Srgb | Bt1886 | Gamma22 | Gamma26 | Prophoto => 4.0,
            Linear => 6.0,
            Camera(_) | Pq | Hlg => (self.decode([1.0; 3])[0] / self.midgray()).log2(),
        }
    }
}

fn signed_pow(x: f64, power: f64) -> f64 {
    x.abs().powf(power).copysign(x)
}

const HLG_A: f64 = 0.17883277;
const HLG_B: f64 = 0.28466892;
const HLG_C: f64 = 0.559910729529562;
const HLG_PEAK: f64 = 1000.0;
const HLG_GAMMA: f64 = 1.2;

fn hlg_luminance(rgb: [f64; 3]) -> f64 {
    0.2627 * rgb[0] + 0.6780 * rgb[1] + 0.0593 * rgb[2]
}

// BT.2100-2 display-referred transfer, with zero black and a 1000-nit
// display, like the upstream's colour-science call. OOTF gain couples RGB.
fn hlg_decode(code: [f64; 3]) -> [f64; 3] {
    let scene = code.map(|v| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.5 {
            v * v / 3.0
        } else {
            (((v - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
        }
    });
    let gain = HLG_PEAK * hlg_luminance(scene).abs().powf(HLG_GAMMA - 1.0);
    scene.map(|v| v * gain)
}

fn hlg_encode(display: [f64; 3]) -> [f64; 3] {
    let display = display.map(|v| v.max(0.0) / HLG_PEAK);
    let luminance = hlg_luminance(display);
    if luminance == 0.0 {
        return [0.0; 3];
    }
    let factor = 12.0 * luminance.powf((1.0 - HLG_GAMMA) / HLG_GAMMA);
    display.map(|v| {
        let scene = v * factor;
        if scene <= 1.0 {
            0.5 * scene.sqrt()
        } else {
            HLG_A * (scene - HLG_B).ln() + HLG_C
        }
    })
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Exposure {
    Native,
    Auto,
    Stops(f64),
}

impl FromStr for Exposure {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        Ok(match text {
            "native" => Self::Native,
            "auto" => Self::Auto,
            _ => {
                let value = text.parse::<f64>()?;
                ensure!(value.is_finite(), "headroom must be finite");
                Self::Stops(value)
            }
        })
    }
}

impl Exposure {
    pub(super) fn resolve(self, input: Encoding) -> Result<(Option<f64>, f64)> {
        let stops = match self {
            Self::Native => return Ok((None, 1.0)),
            Self::Auto => input.auto_stops(),
            Self::Stops(value) => value,
        };
        let target_white = FILM_MIDGRAY * stops.exp2();
        let gain = target_white / input.decode([1.0; 3])[0];
        // The Python builder packages the decoded inputs as float32.
        ensure!(
            (target_white as f32).is_finite() && target_white as f32 > 0.0,
            "headroom cannot be represented by the LUT input samples"
        );
        Ok((Some(stops), gain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_registry_transfers_match_colour_science_047() {
        // Python 0.3.4's decode_cctf / encode_cctf using colour-science 0.4.7.
        // Mixed HLG triples detect accidental application of a scalar OOTF.
        let cases = [
            (
                "ACEScc",
                [0.03, 0.46, 0.89],
                [0.0017069067973222482, 0.3162637260116271, 58.5988318446389],
                [-0.02, 0.001, 0.18],
                [
                    -0.35844748858447484,
                    -0.01402878337112365,
                    0.4135884024924423,
                ],
            ),
            (
                "ACEScct",
                [0.03, 0.46, 0.89],
                [-0.0040706419767245795, 0.3162637260116271, 58.5988318446389],
                [-0.02, 0.001, 0.18],
                [
                    -0.13789922063725452,
                    0.08344577193748999,
                    0.4135884024924423,
                ],
            ),
            (
                "ARRI LogC3 (EI800)",
                [0.03, 0.46, 0.89],
                [
                    -0.011701385428087312,
                    0.35076340320835436,
                    19.763153468593753,
                ],
                [-0.02, 0.001, 0.18],
                [-0.014544100000000004, 0.098176655, 0.39100683203408376],
            ),
            (
                "ARRI LogC4",
                [0.03, 0.46, 0.89],
                [-0.01403872884191558, 1.4273826627897652, 144.81371609115462],
                [-0.02, 0.001, 0.18],
                [
                    -0.017104327684224208,
                    0.09606843585490345,
                    0.2783958365482653,
                ],
            ),
            (
                "Adobe RGB",
                [0.03, 0.46, 0.89],
                [
                    0.0004463409776771545,
                    0.18116205135809363,
                    0.7738521875689158,
                ],
                [0.02, 0.001, 0.18],
                [
                    0.16894327434034292,
                    0.043287612810830586,
                    0.4586564468643811,
                ],
            ),
            (
                "Apple Log",
                [0.03, 0.46, 0.89],
                [-0.03122313833963706, 0.14115710938324433, 4.913721003192893],
                [-0.02, 0.001, 0.18],
                [0.06269099240945344, 0.15585875465504548, 0.4882724585268676],
            ),
            (
                "Blackmagic Film Gen 5",
                [0.03, 0.46, 0.89],
                [
                    -0.0075408890686499115,
                    0.4414103485956766,
                    62.87076650643388,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.07320636522339237,
                    0.10074935935706003,
                    0.38356164383561653,
                ],
            ),
            (
                "Canon Log 3",
                [0.03, 0.46, 0.89],
                [-0.05085881717887345, 0.4386342110818632, 7.3299629446545005],
                [-0.02, 0.001, 0.18],
                [0.0819974879036659, 0.12731716755555555, 0.3433893703739355],
            ),
            (
                "DCI-P3",
                [0.03, 0.46, 0.89],
                [
                    0.00010977798685202408,
                    0.13279152620175508,
                    0.7386080170281603,
                ],
                [0.02, 0.001, 0.18],
                [0.22210073631264332, 0.07017038286703829, 0.5170902489415321],
            ),
            (
                "DJI D-Log",
                [0.03, 0.46, 0.89],
                [
                    -0.010439834024896264,
                    0.3197785056869118,
                    15.648596551767376,
                ],
                [-0.02, 0.001, 0.18],
                [-0.027600000000000013, 0.098925, 0.3987645561893306],
            ),
            (
                "DaVinci Intermediate",
                [0.03, 0.46, 0.89],
                [0.0028754587515795148, 0.5980104232964981, 35.3300095501091],
                [-0.02, 0.001, 0.18],
                [-0.20888537100000001, 0.01044426855, 0.33604327238485526],
            ),
            (
                "Display P3",
                [0.03, 0.46, 0.89],
                [
                    0.0023219814241486067,
                    0.1788675039555289,
                    0.7677687605549981,
                ],
                [-0.02, 0.001, 0.18],
                [-0.2584, 0.012920000000000001, 0.46135612950044164],
            ),
            (
                "Fujifilm F-Log",
                [0.03, 0.46, 0.89],
                [
                    -0.007196274659495119,
                    0.1808991777471792,
                    3.4830976474497826,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.08184861999999998,
                    0.10145319719315149,
                    0.45931845866162124,
                ],
            ),
            (
                "Fujifilm F-Log2",
                [0.03, 0.46, 0.89],
                [
                    -0.007144073938165076,
                    0.3546273545194485,
                    20.733845210866665,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.08312522000000003,
                    0.1016240981003585,
                    0.39100724189123004,
                ],
            ),
            (
                "Nikon N-Log",
                [0.03, 0.46, 0.89],
                [-0.00739474309509695, 0.37177491811311636, 6.980589252453166],
                [-0.02, 0.001, 0.18],
                [-0.14746005580881263, 0.1296713497676577, 0.3636677701171387],
            ),
            (
                "P3-D65 PQ",
                [0.03, 0.46, 0.89],
                [0.01976557471536851, 61.41170153600915, 3559.9365520030533],
                [0.02, 100.0, 500.0],
                [0.030169785283485264, 0.508078421517399, 0.6765848107833876],
            ),
            (
                "Panasonic V-Log",
                [0.03, 0.46, 0.89],
                [
                    -0.016964285714285716,
                    0.2590336382108272,
                    16.141796658608367,
                ],
                [-0.02, 0.001, 0.18],
                [0.013000000000000012, 0.1306, 0.42331144876013616],
            ),
            (
                "ProPhoto RGB",
                [0.03, 0.46, 0.89],
                [0.001875, 0.24715198168901525, 0.8107781047580545],
                [-0.02, 0.001, 0.18],
                [-0.32, 0.016, 0.3857114247511376],
            ),
            (
                "RED Log3G10",
                [0.03, 0.46, 0.89],
                [-0.007687499110112423, 0.7045857577734002, 59.57211945663491],
                [-0.02, 0.001, 0.18],
                [-0.151927, 0.09731290182539942, 0.33333291202599186],
            ),
            (
                "Rec.2020",
                [0.03, 0.46, 0.89],
                [
                    0.00022135585372644274,
                    0.1551024993018552,
                    0.7560247547092495,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.19592769603435264,
                    0.056234132519034905,
                    0.4894370895738783,
                ],
            ),
            (
                "Rec.2100 HLG",
                [0.03, 0.46, 0.89],
                [0.18129758749149621, 42.62507723688955, 333.2765417402916],
                [0.02, 4.0, 89.0],
                [0.0115835308883134, 0.16381586482420474, 0.6929084891396396],
            ),
            (
                "Rec.2100 PQ",
                [0.03, 0.46, 0.89],
                [0.01976557471536851, 61.41170153600915, 3559.9365520030533],
                [0.02, 100.0, 500.0],
                [0.030169785283485264, 0.508078421517399, 0.6765848107833876],
            ),
            (
                "Rec.709",
                [0.03, 0.46, 0.89],
                [
                    0.00022135585372644274,
                    0.1551024993018552,
                    0.7560247547092495,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.19592769603435264,
                    0.056234132519034905,
                    0.4894370895738783,
                ],
            ),
            (
                "Sony S-Log3",
                [0.03, 0.46, 0.89],
                [
                    -0.009493304059712585,
                    0.28660342595514215,
                    14.257763285561687,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.0395747491133268,
                    0.09948606883396546,
                    0.41055718475073316,
                ],
            ),
            (
                "Sony S-Log3 (S-Gamut3.Cine)",
                [0.03, 0.46, 0.89],
                [
                    -0.009493304059712585,
                    0.28660342595514215,
                    14.257763285561687,
                ],
                [-0.02, 0.001, 0.18],
                [
                    -0.0395747491133268,
                    0.09948606883396546,
                    0.41055718475073316,
                ],
            ),
            (
                "sRGB",
                [0.03, 0.46, 0.89],
                [
                    0.0023219814241486067,
                    0.1788675039555289,
                    0.7677687605549981,
                ],
                [-0.02, 0.001, 0.18],
                [-0.2584, 0.012920000000000001, 0.46135612950044164],
            ),
        ];
        for (name, code, decoded, linear, encoded) in cases {
            let encoding = Encoding::from_str(name, false).unwrap();
            assert_eq!(
                Encoding::from_str(encoding.short_tag(), false).unwrap(),
                encoding
            );
            for (actual, expected) in encoding
                .decode(code)
                .into_iter()
                .zip(decoded)
                .chain(encoding.encode(linear).into_iter().zip(encoded))
            {
                assert!(
                    (actual - expected).abs() < 2e-12 * expected.abs().max(1.0),
                    "{name}: {actual:.15e} != {expected:.15e}"
                );
            }
        }
    }

    #[test]
    fn hlg_round_trip_uses_the_1000_nit_display_referred_convention() {
        let hlg = Encoding::Rec2100Hlg;
        for code in [[0.0; 3], [1.0; 3], [0.01, 0.55, 0.89], [0.87, 0.52, 0.37]] {
            let round_trip = hlg.encode(hlg.decode(code));
            for (a, b) in code.into_iter().zip(round_trip) {
                assert!((a - b).abs() < 3e-15);
            }
        }
        let (stops, gain) = Exposure::Auto.resolve(hlg).unwrap();
        assert!((stops.unwrap() - 12.43971546).abs() < 1e-6);
        assert!((gain - 1.0).abs() < 1e-12);
    }
}
