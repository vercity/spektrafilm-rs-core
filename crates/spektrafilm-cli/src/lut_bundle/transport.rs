use std::str::FromStr;

use anyhow::{Result, ensure};
use clap::ValueEnum;
use spektrafilm_math::colorspace::RgbSpace;

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
    Srgb,
    Rec709,
    Rec2020,
    Rec2100Pq,
    #[value(name = "vlog", alias = "v-log")]
    VLog,
    Prophoto,
    LinearSrgb,
    LinearProphoto,
    LinearRec2020,
}

impl Encoding {
    pub(super) fn space(self) -> RgbSpace {
        match self {
            Self::Srgb | Self::Rec709 | Self::LinearSrgb => RgbSpace::Srgb,
            Self::Rec2020 | Self::Rec2100Pq | Self::LinearRec2020 => RgbSpace::Rec2020,
            Self::VLog => RgbSpace::VGamut,
            Self::Prophoto | Self::LinearProphoto => RgbSpace::ProPhoto,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Srgb => "sRGB",
            Self::Rec709 => "Rec.709",
            Self::Rec2020 => "Rec.2020",
            Self::Rec2100Pq => "Rec.2100 PQ",
            Self::VLog => "Panasonic V-Log",
            Self::Prophoto => "ProPhoto RGB",
            Self::LinearSrgb => "sRGB Linear",
            Self::LinearProphoto => "ProPhoto Linear",
            Self::LinearRec2020 => "Rec.2020 Linear",
        }
    }

    pub(super) fn cctf(self) -> Option<&'static str> {
        match self {
            Self::Srgb => Some("sRGB"),
            Self::Rec709 | Self::Rec2020 => Some("ITU-R BT.1886"),
            Self::Rec2100Pq => Some("ITU-R BT.2100 PQ"),
            Self::VLog => Some("V-Log"),
            Self::Prophoto => Some("ProPhoto RGB"),
            Self::LinearSrgb | Self::LinearProphoto | Self::LinearRec2020 => None,
        }
    }

    pub(super) fn decode(self, code: f64) -> f64 {
        match self {
            Self::Srgb if code <= 0.04045 => code / 12.92,
            Self::Srgb => ((code + 0.055) / 1.055).powf(2.4),
            Self::Rec709 | Self::Rec2020 => code.abs().powf(2.4).copysign(code),
            Self::Rec2100Pq => {
                let value = code.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
                PQ_PEAK * ((value - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * value)).powf(1.0 / PQ_M1)
            }
            Self::VLog if code < 0.181 => (code - 0.125) / 5.6,
            Self::VLog => 10_f64.powf((code - 0.598_206) / 0.241_514) - 0.008_73,
            Self::Prophoto if code < 1.0 / 32.0 => code / 16.0,
            Self::Prophoto => code.powf(1.8),
            Self::LinearSrgb | Self::LinearProphoto | Self::LinearRec2020 => code,
        }
    }

    pub(super) fn encode(self, value: f64) -> f64 {
        match self {
            Self::Srgb if value <= 0.0031308 => value * 12.92,
            Self::Srgb => 1.055 * value.powf(1.0 / 2.4) - 0.055,
            Self::Rec709 | Self::Rec2020 => value.abs().powf(1.0 / 2.4).copysign(value),
            Self::Rec2100Pq => {
                let power = (value.max(0.0) / PQ_PEAK).powf(PQ_M1);
                ((PQ_C1 + PQ_C2 * power) / (1.0 + PQ_C3 * power)).powf(PQ_M2)
            }
            Self::VLog if value < 0.01 => 5.6 * value + 0.125,
            Self::VLog => 0.241_514 * (value + 0.008_73).log10() + 0.598_206,
            Self::Prophoto if value < 1.0 / 512.0 => value * 16.0,
            Self::Prophoto => value.powf(1.0 / 1.8),
            Self::LinearSrgb | Self::LinearProphoto | Self::LinearRec2020 => value,
        }
    }

    pub(super) fn output_gain(self) -> f64 {
        if matches!(self, Self::Rec2100Pq) {
            PQ_MIDGRAY / FILM_MIDGRAY
        } else {
            1.0
        }
    }

    fn auto_stops(self) -> f64 {
        match self {
            Self::Srgb | Self::Rec709 | Self::Rec2020 | Self::Prophoto => 4.0,
            Self::LinearSrgb | Self::LinearProphoto | Self::LinearRec2020 => 6.0,
            Self::VLog => (self.decode(1.0) / FILM_MIDGRAY).log2(),
            Self::Rec2100Pq => (self.decode(1.0) / PQ_MIDGRAY).log2(),
        }
    }
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
        let gain = target_white / input.decode(1.0);
        // The Python builder packages the decoded inputs as float32.
        ensure!(
            (target_white as f32).is_finite() && target_white as f32 > 0.0,
            "headroom cannot be represented by the LUT input samples"
        );
        Ok((Some(stops), gain))
    }
}
