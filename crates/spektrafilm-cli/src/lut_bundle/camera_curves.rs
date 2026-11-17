// Camera transport curves matching the colour-science defaults used by spektrafilm
// 0.3.4. These use the published reflection-domain equations and code normalization.

#[derive(Clone, Copy)]
pub(super) enum CameraCurve {
    AcesCc,
    AcesCct,
    Arri3,
    Arri4,
    Apple,
    Blackmagic,
    Canon3,
    Davinci,
    Dji,
    Fuji,
    Fuji2,
    Nikon,
    Panasonic,
    Red,
    Sony,
}

const ACES_DARK: f64 = 1.0 / 65_536.0;
const ARRI3: Log10Toe = Log10Toe {
    cut: 0.010591,
    code_cut: 5.367655 * 0.010591 + 0.092809,
    a: 5.555556,
    b: 0.052272,
    c: 0.247190,
    d: 0.385537,
    e: 5.367655,
    f: 0.092809,
};
const FUJI: Log10Toe = Log10Toe {
    cut: 0.00089,
    code_cut: 0.100537775223865,
    a: 0.555556,
    b: 0.009468,
    c: 0.344676,
    d: 0.790453,
    e: 8.735631,
    f: 0.092864,
};
const FUJI2: Log10Toe = Log10Toe {
    cut: 0.000889,
    code_cut: 0.100686685370811,
    a: 5.555556,
    b: 0.064829,
    c: 0.245281,
    d: 0.384316,
    e: 8.799461,
    f: 0.092864,
};
const PANASONIC: Log10Toe = Log10Toe {
    cut: 0.01,
    code_cut: 0.181,
    a: 1.0,
    b: 0.00873,
    c: 0.241514,
    d: 0.598206,
    e: 5.6,
    f: 0.125,
};
const ARRI4_A: f64 = 2231.8263090676883;
const ARRI4_B: f64 = 0.9071358748778103;
const ARRI4_C: f64 = 1.0 - ARRI4_B;
const ARRI4_S: f64 = 0.1135972086105891;
const ARRI4_T: f64 = -0.01805699611991131;
const APPLE_R0: f64 = -0.05641088;
const APPLE_T: f64 = 0.01;
const APPLE_SIGMA: f64 = 47.28711236;
const APPLE_BETA: f64 = 0.00964052;
const APPLE_GAMMA: f64 = 0.08550479;
const APPLE_DELTA: f64 = 0.69336945;
const BMD_A: f64 = 0.08692876065491224;
const BMD_B: f64 = 0.005494072432257808;
const BMD_C: f64 = 0.5300133392291939;
const BMD_D: f64 = 8.283605932402494;
const BMD_E: f64 = 0.09246575342465753;
const NIKON_A: f64 = 0.635386119257087;
const NIKON_C: f64 = 0.1466275659824047;
const NIKON_D: f64 = 0.6050830889540567;
const SONY_TOE: f64 = 171.2102946929;
const CANON_LO: f64 = 0.097465473;
const CANON_HI: f64 = 0.15277891;

struct Log10Toe {
    cut: f64,
    code_cut: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Log10Toe {
    fn decode(&self, code: f64) -> f64 {
        if code < self.code_cut {
            (code - self.f) / self.e
        } else {
            (10_f64.powf((code - self.d) / self.c) - self.b) / self.a
        }
    }

    fn encode(&self, value: f64) -> f64 {
        if value < self.cut {
            self.e * value + self.f
        } else {
            self.c * (self.a * value + self.b).log10() + self.d
        }
    }
}

impl CameraCurve {
    pub(super) fn decode(self, x: f64) -> f64 {
        match self {
            Self::AcesCc if x < (9.72 - 15.0) / 17.52 => {
                ((x * 17.52 - 9.72).exp2() - ACES_DARK) * 2.0
            }
            Self::AcesCc => (x * 17.52 - 9.72).exp2().min(65504.0),
            Self::AcesCct if x <= 0.155251141552511 => (x - 0.0729055341958355) / 10.5402377416545,
            Self::AcesCct => (x * 17.52 - 9.72).exp2(),
            Self::Arri3 => ARRI3.decode(x),
            Self::Arri4 if x < 0.0 => x * ARRI4_S + ARRI4_T,
            Self::Arri4 => ((14.0 * ((x - ARRI4_C) / ARRI4_B) + 6.0).exp2() - 64.0) / ARRI4_A,
            Self::Apple if x < APPLE_SIGMA * (APPLE_T - APPLE_R0) * (APPLE_T - APPLE_R0) => {
                (x.max(0.0) / APPLE_SIGMA).sqrt() + APPLE_R0
            }
            Self::Apple => ((x - APPLE_DELTA) / APPLE_GAMMA).exp2() - APPLE_BETA,
            Self::Blackmagic if x < BMD_D * 0.005 + BMD_E => (x - BMD_E) / BMD_D,
            Self::Blackmagic => ((x - BMD_C) / BMD_A).exp() - BMD_B,
            Self::Canon3 if x < CANON_LO => {
                -0.9 * (10_f64.powf((0.12783901 - x) / 0.36726845) - 1.0) / 14.98325
            }
            Self::Canon3 if x <= CANON_HI => 0.9 * (x - 0.12512219) / 1.9754798,
            Self::Canon3 => 0.9 * (10_f64.powf((x - 0.12240537) / 0.36726845) - 1.0) / 14.98325,
            Self::Davinci if x <= 0.02740668 => x / 10.44426855,
            Self::Davinci => (x / 0.07329248 - 7.0).exp2() - 0.0075,
            Self::Dji if x <= 0.14 => (x - 0.0929) / 6.025,
            Self::Dji => (10_f64.powf(3.89616 * x - 2.27752) - 0.0108) / 0.9892,
            Self::Fuji => FUJI.decode(x),
            Self::Fuji2 => FUJI2.decode(x),
            Self::Nikon if x < 0.4418377321603128 => (x / NIKON_A).powi(3) - 0.0075,
            Self::Nikon => ((x - NIKON_D) / NIKON_C).exp(),
            Self::Panasonic => PANASONIC.decode(x),
            Self::Red if x < 0.0 => x / 15.1927 - 0.01,
            Self::Red => (10_f64.powf(x / 0.224282) - 1.0) / 155.975327 - 0.01,
            Self::Sony if x < SONY_TOE / 1023.0 => {
                (x * 1023.0 - 95.0) * 0.01125 / (SONY_TOE - 95.0)
            }
            Self::Sony => 10_f64.powf((x * 1023.0 - 420.0) / 261.5) * 0.19 - 0.01,
        }
    }

    pub(super) fn encode(self, x: f64) -> f64 {
        match self {
            Self::AcesCc if x < 2.0 * ACES_DARK => {
                ((ACES_DARK + x.max(0.0) * 0.5).log2() + 9.72) / 17.52
            }
            Self::AcesCc => (x.log2() + 9.72) / 17.52,
            Self::AcesCct if x <= 0.0078125 => 10.5402377416545 * x + 0.0729055341958355,
            Self::AcesCct => (x.log2() + 9.72) / 17.52,
            Self::Arri3 => ARRI3.encode(x),
            Self::Arri4 if x < ARRI4_T => (x - ARRI4_T) / ARRI4_S,
            Self::Arri4 => ((ARRI4_A * x + 64.0).log2() - 6.0) / 14.0 * ARRI4_B + ARRI4_C,
            Self::Apple if x < APPLE_T => APPLE_SIGMA * (x.max(APPLE_R0) - APPLE_R0).powi(2),
            Self::Apple => APPLE_GAMMA * (x + APPLE_BETA).log2() + APPLE_DELTA,
            Self::Blackmagic if x < 0.005 => BMD_D * x + BMD_E,
            Self::Blackmagic => BMD_A * (x + BMD_B).ln() + BMD_C,
            Self::Canon3 if x < Self::Canon3.decode(CANON_LO) => {
                -0.36726845 * (1.0 - (x / 0.9) * 14.98325).log10() + 0.12783901
            }
            Self::Canon3 if x <= Self::Canon3.decode(CANON_HI) => {
                1.9754798 * (x / 0.9) + 0.12512219
            }
            Self::Canon3 => 0.36726845 * (1.0 + (x / 0.9) * 14.98325).log10() + 0.12240537,
            Self::Davinci if x <= 0.00262409 => x * 10.44426855,
            Self::Davinci => 0.07329248 * ((x + 0.0075).log2() + 7.0),
            Self::Dji if x <= 0.0078 => 6.025 * x + 0.0929,
            Self::Dji => (x * 0.9892 + 0.0108).log10() * 0.256663 + 0.584555,
            Self::Fuji => FUJI.encode(x),
            Self::Fuji2 => FUJI2.encode(x),
            Self::Nikon if x < 0.328 => NIKON_A * (x + 0.0075).cbrt(),
            Self::Nikon => NIKON_C * x.ln() + NIKON_D,
            Self::Panasonic => PANASONIC.encode(x),
            Self::Red if x < -0.01 => (x + 0.01) * 15.1927,
            Self::Red => 0.224282 * ((x + 0.01) * 155.975327 + 1.0).log10(),
            Self::Sony if x < 0.01125 => (x * (SONY_TOE - 95.0) / 0.01125 + 95.0) / 1023.0,
            Self::Sony => (420.0 + ((x + 0.01) / 0.19).log10() * 261.5) / 1023.0,
        }
    }
}
