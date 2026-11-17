//! Command and TOML recipe compatibility with Python spektrafilm-lut.

use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use serde::Deserialize;
use spektrafilm_core::params::{InputGamutCompressParams, OutputGamutCompressParams};

use super::{
    BundleOptions, Encoding, Exposure, InputGamut, OutputGamut, Topology, UPSTREAM_VERSION_TAG,
    delivery, export, stock_tag,
};

#[derive(Parser)]
#[command(about = "Bake spektrafilm LUT bundles for external grading tools")]
struct LutCli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build one LUT bundle.
    Build(BuildArgs),
    /// List registry entries.
    List {
        #[arg(value_enum)]
        kind: ListKind,
        /// Directory containing the stock profiles.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ListKind {
    Film,
    Print,
    Input,
    Output,
    Target,
}

#[derive(Args)]
struct BuildArgs {
    /// Load BundleSpec fields from TOML. Explicit CLI flags take precedence.
    #[arg(long = "from", value_name = "FILE")]
    from_toml: Option<PathBuf>,
    /// Bundle name, computed from the stocks and encoding when omitted.
    #[arg(long)]
    name: Option<String>,
    /// Film profile slug.
    #[arg(long)]
    film: Option<String>,
    /// Printing stock slug. Repeat for multiple stocks.
    #[arg(long = "print", value_name = "PRINT")]
    prints: Vec<String>,
    /// Input encoding name or short tag.
    #[arg(long)]
    input: Option<String>,
    /// Output encoding name or short tag.
    #[arg(long)]
    output: Option<String>,
    /// Number of stages in the bundle (default 1lut).
    #[arg(long, value_enum)]
    topology: Option<Topology>,
    /// Samples along each CUBE axis (default 33).
    #[arg(long)]
    resolution: Option<u32>,
    /// Delivery target.
    #[arg(long)]
    target: Option<String>,
    /// Write a directory or a directory and sibling ZIP.
    #[arg(long, value_enum)]
    container: Option<Container>,
    /// Linear headroom above 0.18, or auto to derive it from the encoding.
    #[arg(long, allow_negative_numbers = true)]
    stops_above_gray: Option<Exposure>,
    /// Evaluate the CUBEs against the live pipeline.
    #[arg(long)]
    qa: bool,
    /// QA only this zero-based printing stock index.
    #[arg(long, value_name = "I")]
    qa_print_index: Option<usize>,
    /// Include an OpenColorIO 2 configuration.
    #[arg(long)]
    ocio_config: bool,
    /// Bake all contiguous multi-stage combinations.
    #[arg(long)]
    combinations: bool,
    /// Create the bundle inside this directory.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Directory containing the profiles and spectral resources.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum Container {
    #[default]
    Directory,
    Zip,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Recipe {
    name: Option<String>,
    film_profile: Option<String>,
    print_profiles: Vec<String>,
    input_color_space: Option<String>,
    output_color_space: Option<String>,
    topology: Option<String>,
    resolution: Option<u32>,
    target: Option<String>,
    container: Option<String>,
    stops_above_midgray: Option<RecipeExposure>,
    qa: bool,
    qa_print_index: Option<usize>,
    ocio_config: bool,
    include_combinations: bool,
    input_gamut_compress: Option<InputGamutRecipe>,
    output_gamut_compress: Option<OutputGamutRecipe>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RecipeExposure {
    Number(f64),
    Mode(String),
}

impl RecipeExposure {
    fn resolve(self) -> Result<Exposure> {
        match self {
            Self::Number(stops) => {
                ensure!(stops.is_finite(), "headroom must be finite");
                Ok(Exposure::Stops(stops))
            }
            Self::Mode(mode) => mode.parse(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct InputGamutRecipe {
    active: Option<bool>,
    algorithm: Option<String>,
    knee: Option<[f32; 3]>,
}

impl InputGamutRecipe {
    fn apply(self) -> Result<InputGamutCompressParams> {
        let mut params = InputGamutCompressParams::default();
        if let Some(active) = self.active {
            params.active = active;
        }
        if let Some(algorithm) = self.algorithm {
            ensure!(
                matches!(algorithm.as_str(), "xy" | "oklch"),
                "input_gamut_compress algorithm must be xy or oklch"
            );
            params.algorithm = algorithm;
        }
        if let Some(knee) = self.knee {
            validate_knee(knee, "input_gamut_compress.knee")?;
            params.knee = knee;
        }
        Ok(params)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OutputGamutRecipe {
    Algorithm(String),
    Table(OutputGamutTable),
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OutputGamutTable {
    algorithm: Option<String>,
    knee: Option<[f32; 3]>,
    lightness_compression: Option<[f32; 3]>,
}

impl OutputGamutTable {
    fn apply(self) -> Result<OutputGamutCompressParams> {
        let mut params = OutputGamutCompressParams::default();
        if let Some(algorithm) = self.algorithm {
            ensure!(
                matches!(
                    algorithm.as_str(),
                    "off" | "aces_rgc" | "oklch" | "oklrab" | "jzazbz" | "cam16ucs"
                ),
                "output_gamut_compress algorithm must be off, aces_rgc, oklch, oklrab, jzazbz or cam16ucs"
            );
            params.algorithm = algorithm;
        }
        if let Some(knee) = self.knee {
            validate_knee(knee, "output_gamut_compress.knee")?;
            params.knee = knee;
        }
        if let Some(lightness) = self.lightness_compression {
            validate_knee(lightness, "output_gamut_compress.lightness_compression")?;
            params.lightness_compression = Some(lightness);
        }
        Ok(params)
    }
}

fn validate_knee([threshold, limit, power]: [f32; 3], field: &str) -> Result<()> {
    ensure!(
        (0.0..1.0).contains(&threshold)
            && limit.is_finite()
            && limit > 0.0
            && power.is_finite()
            && power > 0.0,
        "{field} requires a threshold in [0, 1), a positive limit and a positive power"
    );
    Ok(())
}

pub(super) struct RecipeSettings {
    pub(super) name: String,
    pub(super) input_gamut: InputGamutCompressParams,
    pub(super) output_gamut: OutputGamutCompressParams,
}

impl BuildArgs {
    fn resolve(self, recipe: Recipe) -> Result<BundleOptions> {
        let film = self
            .film
            .or(recipe.film_profile)
            .context("missing --film (or film_profile in TOML)")?;
        let paper = if self.prints.is_empty() {
            recipe.print_profiles
        } else {
            self.prints
        };
        ensure!(
            !paper.is_empty(),
            "missing --print (or print_profiles in TOML)"
        );
        let input = self.input.or(recipe.input_color_space);
        let output = self.output.or(recipe.output_color_space);
        let input_encoding = parse_encoding(input.as_deref(), ListKind::Input)?;
        let output_encoding = parse_encoding(output.as_deref(), ListKind::Output)?;

        let topology = resolve_enum(self.topology, recipe.topology.as_deref(), "topology")?
            .unwrap_or(Topology::One);
        let size = self.resolution.or(recipe.resolution).unwrap_or(33);
        ensure!(
            (2..=129).contains(&size),
            "resolution must be within 2..=129"
        );
        let stops_above_midgray = if let Some(stops) = self.stops_above_gray {
            stops
        } else {
            recipe
                .stops_above_midgray
                .map(RecipeExposure::resolve)
                .transpose()?
                .unwrap_or(Exposure::Auto)
        };
        let qa_print_index = self.qa_print_index.or(recipe.qa_print_index);
        if let Some(index) = qa_print_index {
            ensure!(
                index < paper.len(),
                "QA print index {index} is out of range for {} printing stock(s)",
                paper.len()
            );
        }

        let name = self
            .name
            .or(recipe.name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                default_name(&film, &paper, topology, input_encoding, output_encoding)
            });
        ensure!(
            !name.contains(['/', '\\', '\0'])
                && matches!(
                    Path::new(&name).components().next(),
                    Some(Component::Normal(_))
                ),
            "bundle name must be a single path component"
        );
        let mut output = self.out.join(&name);
        let container = resolve_enum(self.container, recipe.container.as_deref(), "container")?
            .unwrap_or_default();
        let zip = match container {
            Container::Directory => None,
            Container::Zip
                if output
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zip")) =>
            {
                let archive = output.with_extension("");
                Some(std::mem::replace(&mut output, archive))
            }
            Container::Zip => Some(output.with_extension("zip")),
        };
        let delivery = delivery::DeliveryOptions::from_recipe(
            self.target.or(recipe.target).as_deref(),
            zip,
            self.ocio_config || recipe.ocio_config,
        )?;
        delivery::validate_target(
            &delivery,
            topology.name(),
            input_encoding.name(),
            output_encoding.name(),
        )?;
        let input_gamut = recipe.input_gamut_compress.unwrap_or_default().apply()?;
        let output_gamut = match recipe.output_gamut_compress {
            Some(OutputGamutRecipe::Table(table)) => table,
            Some(OutputGamutRecipe::Algorithm(algorithm)) => OutputGamutTable {
                algorithm: Some(algorithm),
                ..Default::default()
            },
            None => OutputGamutTable::default(),
        }
        .apply()?;

        Ok(BundleOptions {
            film,
            paper,
            topology,
            size,
            input_encoding,
            output_encoding,
            stops_above_midgray,
            input_gamut: InputGamut::Xy,
            output_gamut: OutputGamut::Cam16ucs,
            combinations: self.combinations || recipe.include_combinations,
            qa: self.qa || recipe.qa,
            qa_print_index,
            output,
            data_dir: data_dir(self.data_dir),
            delivery,
            recipe: Some(RecipeSettings {
                name,
                input_gamut,
                output_gamut,
            }),
        })
    }
}

fn resolve_enum<T: ValueEnum>(
    cli: Option<T>,
    recipe: Option<&str>,
    field: &str,
) -> Result<Option<T>> {
    if cli.is_some() {
        return Ok(cli);
    }
    recipe
        .map(|value| T::from_str(value, false).map_err(|error| anyhow::anyhow!("{field}: {error}")))
        .transpose()
}

fn public_encodings(kind: ListKind) -> impl Iterator<Item = Encoding> {
    Encoding::value_variants()
        .iter()
        .copied()
        .filter(move |entry| {
            entry.cctf().is_some() && (!matches!(kind, ListKind::Output) || entry.supports_output())
        })
}

fn parse_encoding(value: Option<&str>, kind: ListKind) -> Result<Encoding> {
    let role = if matches!(kind, ListKind::Input) {
        "input"
    } else {
        "output"
    };
    let value =
        value.with_context(|| format!("missing --{role} (or {role}_color_space in TOML)"))?;
    public_encodings(kind)
        .find(|entry| value == entry.name() || value == entry.short_tag())
        .with_context(|| {
            format!("unknown {role} color space {value:?}. Try `spektrafilm-lut list {role}`.")
        })
}

fn default_name(
    film: &str,
    paper: &[String],
    topology: Topology,
    input: Encoding,
    output: Encoding,
) -> String {
    let film = stock_tag(film);
    let paper = match paper {
        [one] => stock_tag(one),
        _ => format!("{}printpack", paper.len()),
    };
    format!(
        "spektrafilm_{UPSTREAM_VERSION_TAG}_{film}_{paper}_{}_{}_{}",
        topology.name(),
        input.short_tag(),
        output.short_tag()
    )
}

fn data_dir(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(explicit) = explicit {
        return explicit;
    }
    let local = crate::default_data_dir();
    if local.is_dir() {
        return local;
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    if repository.is_dir() {
        repository
    } else {
        local
    }
}

#[derive(Deserialize)]
struct ProfileEntry {
    info: ProfileInfo,
}

#[derive(Deserialize)]
struct ProfileInfo {
    stage: String,
    stock: Option<String>,
}

fn list_profiles(data_dir: &Path, stage: &str) -> Result<()> {
    let directory = data_dir.join("profiles");
    let entries = fs::read_dir(&directory)
        .with_context(|| format!("opening stock profiles in {}", directory.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let profile: ProfileEntry = serde_json::from_reader(File::open(&path)?)
            .with_context(|| format!("reading {}", path.display()))?;
        if profile.info.stage == stage {
            let name = profile.info.stock.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            names.push(name);
        }
    }
    names.sort_unstable();
    for name in names {
        println!("{name}");
    }
    Ok(())
}

fn list(kind: ListKind, explicit_data_dir: Option<PathBuf>) -> Result<()> {
    match kind {
        ListKind::Film => list_profiles(&data_dir(explicit_data_dir), "filming")?,
        ListKind::Print => list_profiles(&data_dir(explicit_data_dir), "printing")?,
        ListKind::Target => {
            for name in delivery::target_names() {
                println!("{name}");
            }
        }
        ListKind::Input | ListKind::Output => {
            let mut entries: Vec<_> = public_encodings(kind).collect();
            entries.sort_unstable_by_key(|entry| entry.name());
            let width = entries
                .iter()
                .map(|entry| entry.name().len())
                .max()
                .unwrap_or(0);
            for entry in entries {
                println!("{:<width$}  {}", entry.name(), entry.short_tag());
            }
        }
    }
    Ok(())
}

pub(crate) fn run() -> Result<()> {
    match LutCli::parse().command {
        Command::Build(args) => {
            let recipe = match &args.from_toml {
                Some(path) => {
                    let contents = fs::read_to_string(path)
                        .with_context(|| format!("reading TOML recipe {}", path.display()))?;
                    toml::from_str(&contents)
                        .with_context(|| format!("parsing TOML recipe {}", path.display()))?
                }
                None => Recipe::default(),
            };
            let options = match args.resolve(recipe) {
                Ok(options) => options,
                Err(error) => {
                    clap::Error::raw(ErrorKind::ValueValidation, error.to_string()).exit()
                }
            };
            if let Some(parent) = options.output.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating output directory {}", parent.display()))?;
            }
            let result = options
                .delivery
                .archive_path()
                .unwrap_or(&options.output)
                .to_owned();
            export(options)?;
            println!("[done] {}", result.display());
            Ok(())
        }
        Command::List { kind, data_dir } => list(kind, data_dir),
    }
}
