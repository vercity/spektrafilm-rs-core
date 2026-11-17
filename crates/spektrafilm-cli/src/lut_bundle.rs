use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use clap::{Args, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use spektrafilm_core::{
    params::RuntimeParams,
    pipeline::{FilmCmy, FilmLogExposure, LutPipeline, PrintLogExposure},
    profile::load_profile_by_name,
};
use spektrafilm_math::{
    image::ImageBuf,
    precision::{from_f32, to_f64},
};

mod camera_curves;
pub(crate) mod compat;
mod delivery;
mod qa;
mod transport;

use transport::{Encoding, Exposure};

const UPSTREAM_VERSION: &str = "0.3.4";
const UPSTREAM_VERSION_TAG: &str = "v034";
const PROBE_SIZE: u32 = 9;
const PROJECT_URL: &str = "https://github.com/andreavolpato/spektrafilm";
const LICENSE_FILENAME: &str = "SPEKTRAFILM_LICENSE.txt";
const LICENSE: &str = include_str!("../../../SPEKTRAFILM_LICENSE.txt");

#[derive(Args)]
pub(crate) struct BundleOptions {
    /// Film profile ID.
    #[arg(long)]
    film: String,
    /// Printing stock ID. Repeat to share the front stages across multiple papers.
    #[arg(long, alias = "print", required = true)]
    paper: Vec<String>,
    /// How many pipeline stages to publish as separate CUBEs.
    #[arg(long, value_enum, default_value = "2lut")]
    topology: Topology,
    /// Samples along each dimension of each CUBE.
    #[arg(long, default_value = "33", value_parser = clap::value_parser!(u32).range(2..=129))]
    size: u32,
    /// Color space and transfer of the encoded input.
    #[arg(long, value_enum, default_value = "srgb")]
    input_encoding: Encoding,
    /// Color space and transfer of the output. Camera log encodings are input-only.
    #[arg(long, value_enum, default_value = "srgb")]
    output_encoding: Encoding,
    /// Headroom for input code 1.0. Accepts native, auto, or a numeric stop count.
    #[arg(long, default_value = "auto", allow_hyphen_values = true)]
    stops_above_midgray: Exposure,
    /// Gamut compression of the input before film exposure.
    #[arg(long, value_enum, default_value = "xy")]
    input_gamut: InputGamut,
    /// Gamut compression of the output RGB.
    #[arg(long, value_enum, default_value = "cam16ucs")]
    output_gamut: OutputGamut,
    /// Bake every contiguous multi-stage combination as an additional CUBE.
    #[arg(long)]
    combinations: bool,
    /// Evaluate the CUBEs as applied in sequence against the live pipeline.
    #[arg(long)]
    qa: bool,
    /// Run QA only for this zero-based printing stock index.
    #[arg(long, value_name = "I")]
    qa_print_index: Option<usize>,
    /// New output directory.
    #[arg(short, long)]
    output: PathBuf,
    /// Directory containing the stock profiles and spectral resources.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    #[command(flatten)]
    delivery: delivery::DeliveryOptions,
    #[arg(skip)]
    recipe: Option<compat::RecipeSettings>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Topology {
    #[value(name = "1lut")]
    One,
    #[value(name = "2lut")]
    Two,
    #[value(name = "3lut")]
    Three,
    #[value(name = "4lut")]
    Four,
}

impl Topology {
    fn name(self) -> &'static str {
        match self {
            Self::One => "1lut",
            Self::Two => "2lut",
            Self::Three => "3lut",
            Self::Four => "4lut",
        }
    }

    fn segments(self) -> &'static [Segment] {
        use Tap::*;
        const ONE: &[Segment] = &[Segment::new(RgbIn, RgbOut, "combined", "")];
        const TWO: &[Segment] = &[
            Segment::new(RgbIn, FilmCmy, "film", "film"),
            Segment::new(FilmCmy, RgbOut, "print", "print"),
        ];
        const THREE: &[Segment] = &[
            Segment::new(RgbIn, FilmLogE, "filming_expose", "l1"),
            Segment::new(FilmLogE, FilmCmy, "filming_develop", "l2"),
            Segment::new(FilmCmy, RgbOut, "printing_combined", "l3"),
        ];
        const FOUR: &[Segment] = &[
            Segment::new(RgbIn, FilmLogE, "filming_expose", "l1"),
            Segment::new(FilmLogE, FilmCmy, "filming_develop", "l2"),
            Segment::new(FilmCmy, PrintLogE, "printing_expose", "l3"),
            Segment::new(PrintLogE, RgbOut, "printing_develop_scan", "l4"),
        ];
        match self {
            Self::One => ONE,
            Self::Two => TWO,
            Self::Three => THREE,
            Self::Four => FOUR,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum InputGamut {
    Off,
    Xy,
    Oklch,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputGamut {
    Off,
    Oklch,
    Oklrab,
    Jzazbz,
    Cam16ucs,
    AcesRgc,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tap {
    RgbIn,
    FilmLogE,
    FilmCmy,
    PrintLogE,
    RgbOut,
}

impl Tap {
    fn name(self) -> &'static str {
        match self {
            Self::RgbIn => "input_rgb",
            Self::FilmLogE => "log_e_film",
            Self::FilmCmy => "cmy_film",
            Self::PrintLogE => "log_e_print",
            Self::RgbOut => "output_rgb",
        }
    }

    fn is_film(self) -> bool {
        matches!(self, Self::FilmLogE | Self::FilmCmy)
    }
}

#[derive(Clone, Copy)]
struct Segment {
    start: Tap,
    end: Tap,
    role: &'static str,
    suffix: &'static str,
}

impl Segment {
    const fn new(start: Tap, end: Tap, role: &'static str, suffix: &'static str) -> Self {
        Self {
            start,
            end,
            role,
            suffix,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct LogEWire {
    min: f64,
    max: f64,
}

impl LogEWire {
    fn from_probe(image: &ImageBuf) -> Self {
        let (min, max) = image
            .data
            .iter()
            .map(|&value| to_f64(value))
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
                (min.min(value), max.max(value))
            });
        Self {
            min: ((min - 0.1) * 10_000.0).floor() / 10_000.0,
            max: ((max + 0.1) * 10_000.0).ceil() / 10_000.0,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct DensityWire {
    d_min: [f64; 3],
    d_max: [f64; 3],
}

impl DensityWire {
    fn from_probe(image: &ImageBuf) -> Self {
        let mut peak = [f64::NEG_INFINITY; 3];
        for rgb in image.data.chunks_exact(3) {
            for channel in 0..3 {
                peak[channel] = peak[channel].max(to_f64(rgb[channel]));
            }
        }
        Self {
            d_min: [-0.2; 3],
            d_max: peak.map(|value| (value * 1.05 * 10_000.0).ceil() / 10_000.0),
        }
    }
}

#[derive(Default, Serialize)]
struct Wires {
    log_e_film: Option<LogEWire>,
    cmy_film: Option<DensityWire>,
    log_e_print: Option<LogEWire>,
    cmy_print: Option<DensityWire>,
}

impl Wires {
    fn bounds(&self, tap: Tap) -> Result<([f64; 3], [f64; 3])> {
        let as_bounds = |wire: LogEWire| ([wire.min; 3], [wire.max; 3]);
        match tap {
            Tap::FilmLogE => self.log_e_film.map(as_bounds),
            Tap::FilmCmy => self.cmy_film.map(|wire| (wire.d_min, wire.d_max)),
            Tap::PrintLogE => self.log_e_print.map(as_bounds),
            _ => None,
        }
        .with_context(|| format!("missing normalization range for {}", tap.name()))
    }
}

enum Samples {
    Input(ImageBuf),
    FilmExposure(FilmLogExposure),
    FilmDensity(FilmCmy),
    PrintExposure(PrintLogExposure),
    Output(ImageBuf),
}

impl Samples {
    fn tap(&self) -> Tap {
        match self {
            Self::Input(_) => Tap::RgbIn,
            Self::FilmExposure(_) => Tap::FilmLogE,
            Self::FilmDensity(_) => Tap::FilmCmy,
            Self::PrintExposure(_) => Tap::PrintLogE,
            Self::Output(_) => Tap::RgbOut,
        }
    }

    fn image(&self) -> &ImageBuf {
        match self {
            Self::Input(image) | Self::Output(image) => image,
            Self::FilmExposure(image) => image.as_image(),
            Self::FilmDensity(image) => image.as_image(),
            Self::PrintExposure(image) => image.as_image(),
        }
    }

    fn run(mut self, pipeline: &LutPipeline, end: Tap) -> Result<Self> {
        while self.tap() != end {
            self = match self {
                Self::Input(image) => {
                    Self::FilmExposure(pipeline.collect_film_log_exposure(&image)?)
                }
                Self::FilmExposure(exposure) => {
                    Self::FilmDensity(pipeline.develop_film_log_exposure(&exposure)?)
                }
                Self::FilmDensity(density) => {
                    Self::PrintExposure(pipeline.collect_print_log_exposure(&density)?)
                }
                Self::PrintExposure(exposure) => {
                    Self::Output(pipeline.inject_print_log_exposure(&exposure)?)
                }
                Self::Output(_) => anyhow::bail!("invalid LUT subchain ending at {}", end.name()),
            };
        }
        Ok(self)
    }
}

struct Baker {
    input: Encoding,
    output: Encoding,
    gain: f64,
    wires: Wires,
}

impl Baker {
    fn make_input(
        &self,
        tap: Tap,
        grid: impl Iterator<Item = [f64; 3]>,
        width: u32,
        height: u32,
    ) -> Result<Samples> {
        if tap == Tap::RgbIn {
            let data = grid
                .flat_map(|rgb| {
                    self.input
                        .decode(rgb)
                        .map(|value| from_f32((value * self.gain) as f32))
                })
                .collect();
            return Ok(Samples::Input(ImageBuf::from_data(width, height, data)));
        }
        let (min, max) = self.wires.bounds(tap)?;
        let data = grid
            .flat_map(|rgb| {
                std::array::from_fn::<_, 3, _>(|c| {
                    from_f32((rgb[c] * (max[c] - min[c]) + min[c]) as f32)
                })
            })
            .collect();
        let image = ImageBuf::from_data(width, height, data);
        Ok(match tap {
            Tap::FilmLogE => Samples::FilmExposure(FilmLogExposure::new(image)?),
            Tap::FilmCmy => Samples::FilmDensity(FilmCmy::new(image)?),
            Tap::PrintLogE => Samples::PrintExposure(PrintLogExposure::new(image)?),
            _ => anyhow::bail!("invalid LUT input tap"),
        })
    }

    fn encode(&self, samples: &Samples) -> Result<Vec<[f64; 3]>> {
        let tap = samples.tap();
        let values = samples.image().data.chunks_exact(3);
        Ok(if tap == Tap::RgbOut {
            let gain = self.output.output_gain();
            values
                .map(|pixel| {
                    self.output
                        .encode(std::array::from_fn(|c| to_f64(pixel[c]) * gain))
                        .map(|code| code.clamp(0.0, 1.0))
                })
                .collect()
        } else {
            let (min, max) = self.wires.bounds(tap)?;
            values
                .map(|pixel| {
                    std::array::from_fn(|c| {
                        ((to_f64(pixel[c]) - min[c]) / (max[c] - min[c])).clamp(0.0, 1.0)
                    })
                })
                .collect()
        })
    }
}

fn cube_grid(size: u32) -> impl Iterator<Item = [f64; 3]> {
    let max = f64::from(size - 1);
    let code = move |v| f64::from(v) / max;
    (0..size).flat_map(move |b| {
        (0..size).flat_map(move |g| (0..size).map(move |r| [code(r), code(g), code(b)]))
    })
}

fn measure_wires(baker: &Baker, pipeline: &LutPipeline, topology: Topology) -> Result<Wires> {
    if matches!(topology, Topology::One) {
        return Ok(Wires::default());
    }
    let input = baker.make_input(
        Tap::RgbIn,
        cube_grid(PROBE_SIZE),
        PROBE_SIZE,
        PROBE_SIZE.pow(2),
    )?;
    let film_log_e = pipeline.collect_film_log_exposure(input.image())?;
    let film_cmy = pipeline.develop_film_log_exposure(&film_log_e)?;
    Ok(Wires {
        log_e_film: matches!(topology, Topology::Three | Topology::Four)
            .then(|| LogEWire::from_probe(film_log_e.as_image())),
        cmy_film: Some(DensityWire::from_probe(film_cmy.as_image())),
        // The Python builder measures the shared print exposure range on the
        // first paper, even when other papers need more range.
        log_e_print: if matches!(topology, Topology::Four) {
            Some(LogEWire::from_probe(
                pipeline.collect_print_log_exposure(&film_cmy)?.as_image(),
            ))
        } else {
            None
        },
        cmy_print: None,
    })
}

#[derive(Serialize)]
struct LutFile {
    role: String,
    path: String,
    domain: &'static str,
    range: &'static str,
    print_profile: Option<String>,
}

fn stock_tag(stock: &str) -> String {
    const BRANDS: &[&str] = &[
        "kodak",
        "fujifilm",
        "fuji",
        "ilford",
        "agfa",
        "cinestill",
        "polaroid",
        "ferrania",
        "lomography",
    ];
    let lower = stock.to_ascii_lowercase();
    let parts = lower.split('_').collect::<Vec<_>>();
    let parts = if parts.first().is_some_and(|brand| BRANDS.contains(brand)) && parts.len() > 1 {
        &parts[1..]
    } else {
        &parts[..]
    };
    parts.iter().take(2).copied().collect()
}

struct BundleWriter<'a> {
    options: &'a BundleOptions,
    directory: &'a Path,
    baker: Baker,
    version_tag: String,
    luts: Vec<LutFile>,
}

impl BundleWriter<'_> {
    fn bake(
        &mut self,
        pipeline: &LutPipeline,
        segment: Segment,
        paper: Option<&str>,
        combination: Option<&str>,
    ) -> Result<()> {
        let size = self.options.size;
        let samples = self
            .baker
            .make_input(segment.start, cube_grid(size), size, size * size)?;
        let samples = samples.run(pipeline, segment.end)?;
        let code = self.baker.encode(&samples)?;

        let film_tag = stock_tag(&self.options.film);
        let title_stem = format!("{}_{film_tag}", self.version_tag);
        let paper_tag = paper.map_or(String::new(), |paper| format!("_{}", stock_tag(paper)));
        let suffix = if let Some(id) = combination {
            format!("_l{id}")
        } else if segment.suffix.is_empty() {
            String::new()
        } else {
            format!("_{}", segment.suffix)
        };
        let prefix = if combination.is_some() {
            "combinations/"
        } else {
            ""
        };
        let path = format!("{prefix}lut_{title_stem}{paper_tag}{suffix}.cube");
        let mut writer = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.directory.join(&path))
                .with_context(|| format!("creating LUT {path}"))?,
        );
        writeln!(writer, "# Andrea Volpato / spektrafilm, CC BY-SA 4.0")?;
        writeln!(writer, "# {PROJECT_URL}")?;
        let contract = |tap| match tap {
            Tap::RgbIn => self.baker.input.name(),
            Tap::RgbOut => self.baker.output.name(),
            tap => tap.name(),
        };
        writeln!(
            writer,
            "# {} -> {} (see bundle.json)",
            contract(segment.start),
            contract(segment.end)
        )?;
        let title_prefix = if self.options.recipe.is_some() { "" } else { "lut_" };
        writeln!(writer, "TITLE \"{title_prefix}{title_stem}{paper_tag}{suffix}\"")?;
        writeln!(writer, "DOMAIN_MIN 0.0 0.0 0.0")?;
        writeln!(writer, "DOMAIN_MAX 1.0 1.0 1.0")?;
        writeln!(writer, "LUT_3D_SIZE {size}")?;
        for [r, g, b] in code {
            writeln!(writer, "{r:.9} {g:.9} {b:.9}")?;
        }
        writer.flush()?;
        drop(writer);
        delivery::format_canonical_cube(&self.directory.join(&path), &self.options.delivery)?;
        self.luts.push(LutFile {
            role: combination.map_or_else(
                || segment.role.to_owned(),
                |name| format!("subchain_{name}"),
            ),
            path,
            domain: segment.start.name(),
            range: segment.end.name(),
            print_profile: paper.map(str::to_owned),
        });
        Ok(())
    }

    fn bake_group(&mut self, pipeline: &LutPipeline, paper: Option<&str>) -> Result<()> {
        let segments = self.options.topology.segments();
        for &segment in segments
            .iter()
            .filter(|segment| segment.end.is_film() == paper.is_none())
        {
            self.bake(pipeline, segment, paper, None)?;
        }
        if self.options.combinations {
            for length in 2..=segments.len() {
                for start in 0..=segments.len() - length {
                    let end = start + length - 1;
                    if segments[end].end.is_film() != paper.is_none() {
                        continue;
                    }
                    let id: String = (start + 1..=end + 1)
                        .map(|i| char::from(b'0' + i as u8))
                        .collect();
                    self.bake(
                        pipeline,
                        Segment::new(segments[start].start, segments[end].end, "", ""),
                        paper,
                        Some(&id),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn qa(&self, pipeline: &LutPipeline, paper: &str, samples: &[[f64; 3]]) -> Result<qa::Metrics> {
        let paths: Vec<_> = self
            .luts
            .iter()
            .filter(|file| {
                !file.role.starts_with("subchain_")
                    && file.print_profile.as_deref().is_none_or(|p| p == paper)
            })
            .map(|file| file.path.as_str())
            .collect();
        let samples_for_runtime =
            self.baker
                .make_input(Tap::RgbIn, samples.iter().copied(), samples.len() as u32, 1)?;
        let live = self
            .baker
            .encode(&samples_for_runtime.run(pipeline, Tap::RgbOut)?)?;
        let frame = qa::Frame {
            input: self.baker.input,
            output: self.baker.output,
            gain: self.baker.gain,
        };
        let mut metrics = qa::evaluate(self.directory, &paths, samples, &live, frame)?;
        metrics.gamut_edge_stress(frame, pipeline);
        Ok(metrics)
    }
}

fn runtime_params(options: &BundleOptions) -> RuntimeParams {
    let mut params = RuntimeParams::default();
    params.io.input_color_space = options.input_encoding.space().name().into();
    params.io.output_color_space = options.output_encoding.space().name().into();
    params.io.input_cctf_decoding = false;
    params.io.output_cctf_encoding = false;
    params.io.input_gamut_compress.active = !matches!(options.input_gamut, InputGamut::Off);
    params.io.input_gamut_compress.algorithm = match options.input_gamut {
        InputGamut::Off | InputGamut::Xy => "xy",
        InputGamut::Oklch => "oklch",
    }
    .into();
    params.io.output_gamut_compress.algorithm = match options.output_gamut {
        OutputGamut::Off => "off",
        OutputGamut::Oklch => "oklch",
        OutputGamut::Oklrab => "oklrab",
        OutputGamut::Jzazbz => "jzazbz",
        OutputGamut::Cam16ucs => "cam16ucs",
        OutputGamut::AcesRgc => "aces_rgc",
    }
    .into();
    if let Some(recipe) = &options.recipe {
        params.io.input_gamut_compress.clone_from(&recipe.input_gamut);
        params.io.output_gamut_compress.clone_from(&recipe.output_gamut);
    }
    params
}

fn stock_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub(crate) fn export(options: BundleOptions) -> Result<()> {
    ensure!(stock_id(&options.film), "invalid film profile ID");
    let mut seen = HashSet::new();
    for paper in &options.paper {
        ensure!(stock_id(paper), "invalid printing profile ID: {paper}");
        ensure!(
            seen.insert(stock_tag(paper)),
            "duplicated paper or conflicting output filename: {paper}"
        );
    }
    ensure!(
        options.output_encoding.supports_output(),
        "{} is an input-only bundle encoding",
        options.output_encoding.name()
    );
    delivery::validate_target(
        &options.delivery,
        options.topology.name(),
        options.input_encoding.name(),
        options.output_encoding.name(),
    )?;
    if let Some(index) = options.qa_print_index {
        ensure!(
            index < options.paper.len(),
            "QA print index {index} is out of range for {} printing stock(s)",
            options.paper.len()
        );
    }
    let destination = &options.output;
    let name = if let Some(recipe) = &options.recipe {
        &recipe.name
    } else {
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .context("expected a UTF-8 output directory name")?
    };
    ensure!(
        !destination.exists() && !destination.is_symlink(),
        "bundle destination already exists: {}",
        destination.display()
    );
    let parent = destination
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = tempfile::Builder::new()
        .prefix(".spektrafilm-bundle-")
        .tempdir_in(parent)
        .with_context(|| format!("creating staging directory in {}", parent.display()))?;
    if options.combinations && !matches!(options.topology, Topology::One) {
        fs::create_dir(staging.path().join("combinations"))?;
    }
    let (stops, gain) = options
        .stops_above_midgray
        .resolve(options.input_encoding)?;
    let mut writer = BundleWriter {
        options: &options,
        directory: staging.path(),
        baker: Baker {
            input: options.input_encoding,
            output: options.output_encoding,
            gain,
            wires: Wires::default(),
        },
        version_tag: if options.recipe.is_some() {
            UPSTREAM_VERSION_TAG.into()
        } else {
            format!("v{}", env!("CARGO_PKG_VERSION").replace('.', ""))
        },
        luts: Vec::new(),
    };
    let film = load_profile_by_name(&options.data_dir, &options.film)?;
    ensure!(
        film.info.stage == "filming",
        "{} is not a filming stock",
        options.film
    );
    let params = runtime_params(&options);
    let mut report = Vec::new();
    let samples = options.qa.then(qa::reference_samples);
    for (index, paper) in options.paper.iter().enumerate() {
        let stock = load_profile_by_name(&options.data_dir, paper)?;
        ensure!(
            stock.info.stage == "printing",
            "{paper} is not a printing stock"
        );
        let pipeline = LutPipeline::new(film.clone(), stock, params.clone(), &options.data_dir)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("initializing LUT pipeline for {} / {paper}", options.film))?;
        if index == 0 {
            writer.baker.wires = measure_wires(&writer.baker, &pipeline, options.topology)?;
            writer.bake_group(&pipeline, None)?;
        }
        writer.bake_group(&pipeline, Some(paper))?;
        if let Some(samples) = &samples
            && options
                .qa_print_index
                .is_none_or(|selected| selected == index)
        {
            report.push((paper.clone(), writer.qa(&pipeline, paper, samples)?));
        }
    }
    write_metadata(name, &writer, &params, stops, gain)?;
    write_readme(name, &writer, stops, gain)?;
    fs::write(staging.path().join(LICENSE_FILENAME), LICENSE)?;
    delivery::prepare_bundle(staging.path(), &options.delivery)?;
    if options.qa {
        qa::write_report(staging.path(), &options.film, report)?;
    }
    let archive = delivery::finish_bundle(staging.path(), destination, &options.delivery)?;
    ensure!(
        !destination.exists() && !destination.is_symlink(),
        "bundle destination already exists: {}",
        destination.display()
    );
    fs::rename(staging.path(), destination)
        .with_context(|| format!("publishing LUT bundle to {}", destination.display()))?;
    if let Some(archive) = archive {
        archive.publish()?;
    }
    eprintln!(
        "Saved {} CUBEs to {}",
        writer.luts.len(),
        destination.display()
    );
    Ok(())
}

fn write_metadata(
    name: &str,
    writer: &BundleWriter<'_>,
    params: &RuntimeParams,
    stops: Option<f64>,
    gain: f64,
) -> Result<()> {
    let options = writer.options;
    let input = options.input_encoding;
    let output = options.output_encoding;
    let topology = options.topology.name();
    let created = Utc::now();
    let params_snapshot: BTreeMap<_, Value> = options.paper.iter().map(|paper| (paper, json!({
        "film_profile": options.film, "print_profile": paper,
        "input_color_space": params.io.input_color_space, "output_color_space": params.io.output_color_space,
        "input_cctf": input.cctf(), "output_cctf": output.cctf(),
        "input_cctf_decoding": false, "output_cctf_encoding": false, "lut_mode": true,
        "input_gamut_compress": params.io.input_gamut_compress,
        "output_gamut_compress": params.io.output_gamut_compress,
        "stops_above_midgray": stops, "input_exposure_gain": gain,
        "resolution": options.size, "topology": topology,
    }))).collect();
    let metadata = json!({
        "schema_version": 1,
        "name": name,
        "topology": topology,
        "resolution": options.size,
        "target": options.delivery.target_name(),
        "provenance": {
            "spektrafilm_version": UPSTREAM_VERSION,
            "lut_creator_version": env!("CARGO_PKG_VERSION"),
            "created": created.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "copyright": "Andrea Volpato (spektrafilm stock profiles); Rust port by spektrafilm-rs contributors.",
            "license": "CC BY-SA 4.0, see SPEKTRAFILM_LICENSE.txt.",
            "citation": "Please cite Andrea Volpato and the spektrafilm project.",
            "project_url": PROJECT_URL,
            "notes": "Generated with spektrafilm-rs from the spektrafilm optical and spectral simulation.",
        },
        "stocks": { "film": options.film, "prints": options.paper },
        "color_spaces": {
            "input": { "name": input.name(), "cctf": input.cctf().is_some() },
            "output": { "name": output.name(), "cctf": output.cctf().is_some() },
        },
        "wires": writer.baker.wires,
        "luts": writer.luts,
        "input_exposure": stops.map(|stops| json!({ "stops_above_midgray": stops, "gain": gain })),
        "params_snapshot": params_snapshot,
    });
    let mut file = BufWriter::new(File::create(writer.directory.join("bundle.json"))?);
    serde_json::to_writer_pretty(&mut file, &metadata)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

fn write_readme(
    name: &str,
    writer: &BundleWriter<'_>,
    stops: Option<f64>,
    gain: f64,
) -> Result<()> {
    let mut readme = BufWriter::new(File::create(writer.directory.join("README.md"))?);
    writeln!(readme, "# {name}\n")?;
    writeln!(
        readme,
        "Convert {} to {} with a {} bundle.",
        writer.baker.input.name(),
        writer.baker.output.name(),
        writer.options.topology.name()
    )?;
    if let Some(stops) = stops {
        writeln!(
            readme,
            "\nInput code 1.0 maps to {stops:.4} stops above 18% gray in the film's frame. The post-decode linear gain is {gain:.8}."
        )?;
    } else {
        writeln!(
            readme,
            "\nThe input uses its native exposure range with unit gain."
        )?;
    }
    writeln!(
        readme,
        "\nFor one printing stock, apply the following CUBEs in order using trilinear or tetrahedral interpolation."
    )?;
    for paper in &writer.options.paper {
        writeln!(readme, "\n**{paper}**\n")?;
        for (i, lut) in writer
            .luts
            .iter()
            .filter(|lut| {
                !lut.role.starts_with("subchain_")
                    && lut.print_profile.as_ref().is_none_or(|p| p == paper)
            })
            .enumerate()
        {
            writeln!(
                readme,
                "{}. `{}` maps `{}` to `{}`.",
                i + 1,
                lut.path,
                lut.domain,
                lut.range
            )?;
        }
    }
    if writer.options.combinations {
        writeln!(
            readme,
            "\nOptional pre-collapsed subchains are listed in `bundle.json`."
        )?;
    }
    if !matches!(writer.options.topology, Topology::One) {
        writeln!(
            readme,
            "\nIntermediate codes use the affine mappings defined in `bundle.json`."
        )?;
    }
    if matches!(writer.options.topology, Topology::Four) {
        writeln!(
            readme,
            "The first listed paper supplies the shared print log-exposure range. Values outside this range clip at the wire endpoints."
        )?;
    }
    if writer.options.qa {
        writeln!(
            readme,
            "\n`qa.json` compares sequential interpolation and single-CUBE resampling against direct simulation. Open the per-paper `qa/*/report.html` for structure, noise and gamut edge diagnostics."
        )?;
    }
    writeln!(
        readme,
        "\nCredit Andrea Volpato and [spektrafilm]({PROJECT_URL}). See `{LICENSE_FILENAME}`."
    )?;
    readme.flush()?;
    Ok(())
}
