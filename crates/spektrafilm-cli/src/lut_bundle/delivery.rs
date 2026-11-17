//! Optional delivery artifacts for a completed LUT bundle.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use tempfile::NamedTempFile;

use super::qa::Cube;

mod ocio;

#[derive(Args, Debug, Default)]
pub(crate) struct DeliveryOptions {
    /// Also archive the complete bundle as a ZIP at this path.
    #[arg(long, value_name = "PATH")]
    zip: Option<PathBuf>,
    /// Include an OpenColorIO 2 configuration for the canonical LUT chains.
    #[arg(long)]
    ocio_config: bool,
    /// Also write per-LUT Autodesk 10-bit 3DL or Hald PNG files.
    #[arg(long, value_enum)]
    format: Option<ExtraFormat>,
    /// Write canonical CUBE files using the Lumix Real-Time LUT header.
    #[arg(long, value_enum)]
    target: Option<Target>,
    #[arg(skip)]
    python_layout: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Target {
    #[value(name = "lumix_realtime_vlog", alias = "lumix-realtime-vlog")]
    LumixRealtimeVlog,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExtraFormat {
    #[value(name = "3dl")]
    ThreeDl,
    HaldPng,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Self::LumixRealtimeVlog => "lumix_realtime_vlog",
        }
    }
}

pub(super) fn target_names() -> impl Iterator<Item = &'static str> {
    Target::value_variants().iter().map(|target| target.name())
}

impl DeliveryOptions {
    pub(super) fn from_recipe(
        target: Option<&str>,
        zip: Option<PathBuf>,
        ocio_config: bool,
    ) -> Result<Self> {
        let target = target
            .map(|value| {
                Target::from_str(value, false).map_err(|error| anyhow::anyhow!("target: {error}"))
            })
            .transpose()?;
        Ok(Self {
            zip,
            ocio_config,
            target,
            python_layout: true,
            ..Default::default()
        })
    }

    pub(super) fn target_name(&self) -> Option<&'static str> {
        self.target.map(Target::name)
    }

    pub(super) fn archive_path(&self) -> Option<&Path> {
        self.zip.as_deref()
    }
}

#[derive(Deserialize)]
struct BundleMeta {
    name: String,
    topology: String,
    stocks: Stocks,
    color_spaces: Endpoints,
    wires: BTreeMap<String, Option<serde_json::Value>>,
    luts: Vec<LutMeta>,
}

#[derive(Deserialize)]
struct Stocks {
    film: String,
    prints: Vec<String>,
}

#[derive(Deserialize)]
struct Endpoints {
    input: ColorSpace,
    output: ColorSpace,
}

#[derive(Deserialize)]
struct ColorSpace {
    name: String,
    cctf: bool,
}

#[derive(Deserialize)]
struct LutMeta {
    role: String,
    path: String,
    domain: String,
    range: String,
    print_profile: Option<String>,
}

impl LutMeta {
    fn path_in(&self, dir: &Path) -> Result<PathBuf> {
        let relative = Path::new(&self.path);
        ensure!(
            !self.path.is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
            "invalid LUT path {:?}",
            self.path
        );
        Ok(dir.join(relative))
    }
}

pub(super) struct StagedArchive {
    file: NamedTempFile,
    destination: PathBuf,
}

impl StagedArchive {
    pub(super) fn publish(self) -> Result<()> {
        self.file
            .persist_noclobber(&self.destination)
            .with_context(|| {
                format!(
                    "publishing ZIP {} (the bundle directory is complete)",
                    self.destination.display()
                )
            })?;
        Ok(())
    }
}

pub(super) fn validate_target(
    options: &DeliveryOptions,
    topology: &str,
    input: &str,
    output: &str,
) -> Result<()> {
    if matches!(options.target, Some(Target::LumixRealtimeVlog)) {
        ensure!(
            options.python_layout || topology == "1lut",
            "Lumix Real-Time LUT requires 1lut"
        );
        ensure!(
            input == "Panasonic V-Log" && matches!(output, "sRGB" | "Rec.709" | "Rec.2020"),
            "Lumix Real-Time LUT requires Panasonic V-Log input and encoded sRGB, Rec.709 or Rec.2020 output"
        );
    }
    Ok(())
}

pub(super) fn prepare_bundle(dir: &Path, options: &DeliveryOptions) -> Result<()> {
    if options.format.is_some() || options.ocio_config {
        let meta: BundleMeta = serde_json::from_reader(File::open(dir.join("bundle.json"))?)
            .context("reading bundle metadata for delivery")?;
        if let Some(format) = options.format {
            write_sidecars(dir, &meta, format)?;
        }
        if options.ocio_config {
            match ocio::validate_endpoints(&meta) {
                Ok(()) => fs::write(dir.join("config.ocio"), ocio::generate(&meta)?)
                    .context("writing OpenColorIO configuration")?,
                Err(error) => eprintln!("Skipping config.ocio: {error}"),
            }
        }
    }
    Ok(())
}

pub(super) fn finish_bundle(
    dir: &Path,
    destination: &Path,
    options: &DeliveryOptions,
) -> Result<Option<StagedArchive>> {
    options
        .archive_path()
        .map(|path| {
            let root = if options.python_layout {
                Some(
                    destination
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("ZIP bundle root needs a UTF-8 directory name")?,
                )
            } else {
                None
            };
            stage_archive(dir, archive_destination(path, destination)?, root)
        })
        .transpose()
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn archive_destination(zip: &Path, bundle: &Path) -> Result<PathBuf> {
    let zip = parent(zip)
        .canonicalize()
        .with_context(|| format!("opening ZIP parent directory {}", parent(zip).display()))?
        .join(
            zip.file_name()
                .context("ZIP destination needs a file name")?,
        );
    let bundle = parent(bundle)
        .canonicalize()
        .context("opening bundle parent directory")?
        .join(
            bundle
                .file_name()
                .context("bundle destination needs a directory name")?,
        );
    ensure!(
        !zip.starts_with(bundle),
        "ZIP destination must be outside the bundle directory"
    );
    ensure!(
        !zip.try_exists()? && !zip.is_symlink(),
        "ZIP destination already exists: {}",
        zip.display()
    );
    Ok(zip)
}

fn stage_archive(dir: &Path, destination: PathBuf, root: Option<&str>) -> Result<StagedArchive> {
    let mut file =
        NamedTempFile::new_in(parent(&destination)).context("creating temporary ZIP archive")?;
    let mut archive = zip::ZipWriter::new(file.as_file_mut());
    if let Some(root) = root {
        archive.add_directory(format!("{root}/"), zip::write::SimpleFileOptions::default())?;
    }
    add_archive_files(&mut archive, dir, dir, root)?;
    archive.finish().context("finishing ZIP archive")?.flush()?;
    Ok(StagedArchive { file, destination })
}

fn add_archive_files(
    archive: &mut zip::ZipWriter<&mut File>,
    root: &Path,
    dir: &Path,
    archive_root: Option<&str>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type()?;
        let relative = path.strip_prefix(root)?;
        let name = archive_root
            .into_iter()
            .map(Ok)
            .chain(
                relative
                    .iter()
                    .map(|part| part.to_str().context("non-UTF-8 archive entry")),
            )
            .collect::<Result<Vec<_>>>()?
            .join("/");
        if kind.is_dir() {
            if archive_root.is_some() {
                archive
                    .add_directory(format!("{name}/"), zip::write::SimpleFileOptions::default())?;
            }
            add_archive_files(archive, root, &path, archive_root)?;
        } else {
            ensure!(
                kind.is_file(),
                "unsupported bundle entry {}",
                path.display()
            );
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .large_file(entry.metadata()?.len() >= u64::from(u32::MAX));
            archive.start_file(name, options)?;
            io::copy(&mut File::open(&path)?, archive)?;
        }
    }
    Ok(())
}

pub(super) fn format_canonical_cube(path: &Path, options: &DeliveryOptions) -> Result<()> {
    if matches!(options.target, Some(Target::LumixRealtimeVlog)) {
        let cube = Cube::read(path)?;
        let mut lines = BufReader::new(File::open(path)?).lines();
        let title = loop {
            let line = lines.next().context("Lumix CUBE requires a TITLE")??;
            if line.trim_start().starts_with("TITLE ") {
                break line;
            }
        };
        let mut file = BufWriter::new(File::create(path)?);
        writeln!(file, "{}", title.trim())?;
        writeln!(file, "#LUMIXPHOTOSTYLE VLOG")?;
        writeln!(file, "LUT_3D_SIZE {}", cube.size)?;
        writeln!(file, "DOMAIN_MIN 0.000000 0.000000 0.000000")?;
        writeln!(file, "DOMAIN_MAX 1.000000 1.000000 1.000000")?;
        writeln!(file)?;
        for [r, g, b] in cube.data {
            writeln!(file, "{r:.6} {g:.6} {b:.6}")?;
        }
        file.flush()?;
    }
    Ok(())
}

fn write_sidecars(dir: &Path, meta: &BundleMeta, format: ExtraFormat) -> Result<()> {
    for lut in &meta.luts {
        let path = lut.path_in(dir)?;
        let cube =
            Cube::read(&path).with_context(|| format!("reading source LUT {}", path.display()))?;
        match format {
            ExtraFormat::ThreeDl => write_three_dl(&path.with_extension("3dl"), cube)?,
            ExtraFormat::HaldPng => write_hald(&path.with_extension("png"), cube)?,
        }
    }
    Ok(())
}

fn write_three_dl(path: &Path, cube: Cube) -> Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    let scale = 1023.0 / (cube.size - 1) as f64;
    for index in 0..cube.size {
        if index > 0 {
            write!(file, " ")?;
        }
        write!(file, "{}", (index as f64 * scale).round_ties_even() as u16)?;
    }
    writeln!(file)?;
    for pixel in cube.data {
        let [r, g, b] = pixel.map(|sample| (sample * 1023.0).round_ties_even() as u16);
        writeln!(file, "{r} {g} {b}")?;
    }
    file.flush()?;
    Ok(())
}

fn write_hald(path: &Path, cube: Cube) -> Result<()> {
    let level = cube.size.isqrt();
    ensure!(
        level * level == cube.size,
        "Hald PNG requires a perfect-square LUT size"
    );
    let width = (cube.size * level) as u32;
    let pixels: Vec<_> = cube
        .data
        .into_iter()
        .flatten()
        .map(|sample| (sample * 255.0).round_ties_even() as u8)
        .collect();
    image::save_buffer_with_format(
        path,
        &pixels,
        width,
        width,
        image::ColorType::Rgb8,
        image::ImageFormat::Png,
    )
    .with_context(|| format!("writing Hald PNG {}", path.display()))
}
