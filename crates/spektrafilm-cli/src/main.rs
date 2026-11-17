use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use spektrafilm_core::params::RuntimeParams;
use spektrafilm_core::pipeline::{LutPipeline, Pipeline};
use spektrafilm_core::profile;
use spektrafilm_io::raw;
use spektrafilm_io::{
    raster::{
        ColorTransform, RasterColor, RasterImage, RasterWriteOptions, read_source_metadata,
        write_raster,
    },
    read_developed_raw, read_image,
};
use spektrafilm_math::image::ImageBuf;

mod lut_bundle;

#[derive(Parser)]
#[command(
    name = "spektrafilm",
    about = "Physically-based spectral film emulation"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Process an image through the film simulation pipeline.
    Process {
        /// Input image (TIFF, PNG, JPEG, EXR, or camera RAW).
        input: PathBuf,
        /// Output image path.
        #[arg(short, long)]
        output: PathBuf,
        /// Output sample depth. TIFF supports 8/16/32, EXR 16/32, PNG 8/16, JPEG 8.
        #[arg(long)]
        bit_depth: Option<u8>,
        /// Film stock name (e.g. kodak_portra_400).
        #[arg(long)]
        film: String,
        /// Paper stock name (e.g. fujifilm_crystal_archive_typeii).
        /// If omitted in print mode, uses the film's target_print.
        #[arg(long)]
        paper: Option<String>,
        /// Scan film directly (skip printing stage). If omitted, use io.scan_film from --params.
        #[arg(long)]
        scan_film: bool,
        /// Print per-stage timing information.
        #[arg(long)]
        timings: bool,
        /// Path to JSON params file for overrides.
        #[arg(long)]
        params: Option<PathBuf>,
        #[command(flatten)]
        raw: RawOptions,
        #[command(flatten)]
        image_options: ProcessImageOptions,
        /// Dump the final pipeline buffer as native-endian f64 (HxWx3, row-major,
        /// channel-interleaved) after pipeline color processing and before file quantization.
        #[arg(long)]
        raw_out: Option<PathBuf>,
        /// Run the pipeline N times in the same process. Each iteration is
        /// timed individually so you can see cold-start vs warm-cache speed.
        /// Default: 1 (no repetition).
        #[arg(long, default_value = "1")]
        iters: usize,
        /// Path to the data directory.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
    },
    /// List available film and paper profiles.
    ListProfiles {
        /// Path to the data directory.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
    },
    /// Export a 3D CUBE LUT for use in other software.
    ExportLut {
        /// Film stock name.
        #[arg(long)]
        film: String,
        /// Paper stock name.
        #[arg(long)]
        paper: Option<String>,
        /// LUT cube size, from 2 to 129 samples per axis.
        #[arg(
            long,
            default_value = "33",
            value_parser = clap::value_parser!(u32).range(2..=129)
        )]
        size: u32,
        /// Transfer function and primaries for the [0, 1] input domain.
        #[arg(long, value_enum, default_value = "linear-prophoto")]
        input_encoding: LutInputEncoding,
        /// Output .cube file path. Values are SDR sRGB.
        #[arg(short, long)]
        output: PathBuf,
        /// Path to the data directory.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
    },
    /// Export a film and one or more papers as a CUBE LUT bundle.
    ExportLutBundle(lut_bundle::BundleOptions),
}

#[derive(Args)]
struct ProcessImageOptions {
    /// Clockwise 90° rotations, applied after the file's orientation.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=3))]
    rotate_quarter_turns: u8,
    #[arg(long, hide = true, conflicts_with_all = ["raw_white_balance", "raw_lens_correction"])]
    developed_raw: Option<PathBuf>,
    /// Use the input space and transfer from --params. Develop RAW into that space.
    #[arg(long)]
    use_params_input_color: bool,
    /// Save in a different color space from the simulation output.
    #[arg(long)]
    save_color_space: Option<String>,
    /// Apply the saving color space's transfer. Specify false to save linear samples.
    #[arg(long, requires = "save_color_space")]
    save_cctf_encoding: Option<bool>,
}

#[derive(Args)]
struct RawOptions {
    /// Apply the matching Lensfun calibration to camera RAW inputs.
    #[arg(long)]
    raw_lens_correction: bool,
    /// White balance for camera RAW inputs (defaults to as-shot).
    #[arg(long, value_enum)]
    raw_white_balance: Option<RawWhiteBalance>,
    /// Temperature in kelvin for custom RAW white balance.
    #[arg(long, requires = "raw_white_balance")]
    raw_temperature: Option<f64>,
    /// Green multiplier for custom RAW white balance (defaults to 1).
    #[arg(long, requires = "raw_temperature")]
    raw_tint: Option<f64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum RawWhiteBalance {
    AsShot,
    Daylight,
    Tungsten,
    Custom,
}

impl RawOptions {
    fn decode_options(&self, is_raw: bool) -> Result<raw::DevelopOptions> {
        ensure!(
            is_raw || !self.raw_lens_correction,
            "--raw-lens-correction requires a camera RAW input"
        );
        Ok(raw::DevelopOptions {
            white_balance: self.white_balance(is_raw)?,
            lens_correction: self.raw_lens_correction,
        })
    }

    fn white_balance(&self, is_raw: bool) -> Result<raw::WhiteBalance> {
        let Some(balance) = self.raw_white_balance else {
            return Ok(raw::WhiteBalance::AsShot);
        };
        ensure!(is_raw, "--raw-white-balance requires a camera RAW input");
        ensure!(
            matches!(balance, RawWhiteBalance::Custom) || self.raw_temperature.is_none(),
            "--raw-temperature requires --raw-white-balance custom"
        );
        Ok(match balance {
            RawWhiteBalance::AsShot => raw::WhiteBalance::AsShot,
            RawWhiteBalance::Daylight => raw::WhiteBalance::Daylight,
            RawWhiteBalance::Tungsten => raw::WhiteBalance::Tungsten,
            RawWhiteBalance::Custom => raw::WhiteBalance::Custom {
                kelvin: self
                    .raw_temperature
                    .context("custom RAW white balance requires --raw-temperature")?,
                tint: self.raw_tint.unwrap_or(1.0),
            },
        })
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LutInputEncoding {
    LinearProphoto,
    Srgb,
    #[value(name = "vlog", alias = "v-log")]
    VLog,
}

impl LutInputEncoding {
    fn color_space(self) -> spektrafilm_math::colorspace::RgbSpace {
        use spektrafilm_math::colorspace::RgbSpace;
        match self {
            Self::LinearProphoto => RgbSpace::ProPhoto,
            Self::Srgb => RgbSpace::Srgb,
            Self::VLog => RgbSpace::VGamut,
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Process {
            input,
            output,
            bit_depth,
            film,
            paper,
            scan_film,
            timings,
            params: params_file,
            raw,
            image_options,
            raw_out,
            iters,
            data_dir,
        } => {
            cmd_process(
                &input,
                &output,
                bit_depth,
                &film,
                paper.as_deref(),
                scan_film,
                timings,
                params_file.as_deref(),
                &raw,
                &image_options,
                raw_out.as_deref(),
                iters,
                &data_dir,
            )?;
        }
        Commands::ListProfiles { data_dir } => {
            cmd_list_profiles(&data_dir);
        }
        Commands::ExportLutBundle(options) => lut_bundle::export(options)?,
        Commands::ExportLut {
            film,
            paper,
            size,
            input_encoding,
            output,
            data_dir,
        } => {
            cmd_export_lut(
                &film,
                paper.as_deref(),
                size,
                input_encoding,
                &output,
                &data_dir,
            )?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_process(
    input: &Path,
    output: &Path,
    bit_depth: Option<u8>,
    film_name: &str,
    paper_name: Option<&str>,
    scan_film: bool,
    show_timings: bool,
    params_file: Option<&Path>,
    raw_options: &RawOptions,
    image_options: &ProcessImageOptions,
    raw_out: Option<&Path>,
    iters: usize,
    data_dir: &Path,
) -> Result<()> {
    let total_start = Instant::now();

    let mut params = if let Some(pf) = params_file {
        let f = std::fs::File::open(pf)
            .with_context(|| format!("opening params file: {}", pf.display()))?;
        serde_json::from_reader(std::io::BufReader::new(f))
            .with_context(|| format!("parsing params file: {}", pf.display()))?
    } else {
        RuntimeParams::default()
    };
    params.io.scan_film = scan_film || params.io.scan_film;

    // Load profiles
    let t = Instant::now();
    let film = profile::load_profile_by_name(data_dir, film_name)
        .with_context(|| format!("loading film profile: {film_name}"))?;

    let print_stock = if params.io.scan_film {
        film_name.to_string()
    } else if let Some(p) = paper_name {
        p.to_string()
    } else if let Some(ref target) = film.info.target_print {
        target.clone()
    } else {
        bail!(
            "no paper specified and film has no target_print. Use --paper, --scan-film, or io.scan_film in --params"
        );
    };

    let print = profile::load_profile_by_name(data_dir, &print_stock)
        .with_context(|| format!("loading print profile: {print_stock}"))?;
    eprintln!("Profiles loaded: {} ms", t.elapsed().as_millis());

    let t = Instant::now();
    let is_raw = raw::is_raw(input);
    let develop_options = raw_options.decode_options(is_raw)?;
    let mut loaded = if let Some(developed) = image_options.developed_raw.as_deref() {
        ensure!(
            is_raw,
            "--developed-raw requires the original camera RAW input"
        );
        RasterImage {
            image: read_developed_raw(developed)?,
            metadata: read_source_metadata(input),
            color: RasterColor::new(raw::INPUT_COLOR_SPACE, false),
        }
    } else {
        read_image(input, develop_options)?
    };
    if let Some(color) = loaded.color {
        if !image_options.use_params_input_color {
            params.io.input_color_space = color.color_space.into();
            params.io.input_cctf_decoding = color.cctf_encoding;
        } else if is_raw {
            let destination =
                RasterColor::new(&params.io.input_color_space, params.io.input_cctf_decoding)
                    .context("unknown RAW input color space")?;
            if color != destination {
                ColorTransform::new(color, destination).apply_image(&mut loaded.image);
            }
        }
    }
    let image = if image_options.rotate_quarter_turns == 0 {
        loaded.image
    } else {
        loaded.image.rotated(image_options.rotate_quarter_turns)
    };
    eprintln!(
        "Image loaded: {}x{} ({} MP), {} ms",
        image.width,
        image.height,
        (image.pixel_count() as f64 / 1e6 * 10.0).round() / 10.0,
        t.elapsed().as_millis()
    );

    // Select backend
    let backend = spektrafilm_gpu::select_backend();
    eprintln!("Backend: {}", backend.name());

    // Build the calibrated pipeline with the selected spectral upsampler.
    let t = Instant::now();
    let pipeline = Pipeline::new_with_spectral(film, print, params, data_dir)
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!("initializing spectral pipeline for {film_name} / {print_stock}")
        })?;
    let result = if iters > 1 {
        // Warm-cache bench: process the image `iters` times in the same backend
        // instance. The first iteration pays shader-compile cost; subsequent ones
        // hit the cache and reflect "GUI live preview" performance.
        let mut last = pipeline.process(image.clone(), backend.as_ref());
        let iter1_ms = t.elapsed().as_millis();
        eprintln!("Pipeline (iter 1): {} ms (cold)", iter1_ms);
        for i in 2..=iters {
            let ti = Instant::now();
            last = pipeline.process(image.clone(), backend.as_ref());
            eprintln!(
                "Pipeline (iter {}): {} ms (warm)",
                i,
                ti.elapsed().as_millis()
            );
        }
        last
    } else {
        let r = pipeline.process(image, backend.as_ref());
        eprintln!("Pipeline: {} ms", t.elapsed().as_millis());
        r
    };

    // Save output
    let t = Instant::now();
    let mut write_options = RasterWriteOptions {
        bit_depth,
        color_space: Some(&pipeline.params.io.output_color_space),
        cctf_encoding: pipeline.params.io.output_cctf_encoding,
        source_metadata: Some(&loaded.metadata),
    };
    let converted = if let Some(name) = &image_options.save_color_space {
        let source = RasterColor::new(
            &pipeline.params.io.output_color_space,
            pipeline.params.io.output_cctf_encoding,
        )
        .context("unknown simulation output color space")?;
        let destination = RasterColor::new(name, image_options.save_cctf_encoding.unwrap_or(true))
            .with_context(|| format!("unknown saving color space: {name}"))?;
        write_options.color_space = Some(destination.color_space);
        write_options.cctf_encoding = destination.cctf_encoding;
        (source != destination).then(|| {
            let mut converted = result.clone();
            ColorTransform::new(source, destination).apply_image(&mut converted);
            converted
        })
    } else {
        None
    };
    write_raster(
        output,
        converted.as_ref().unwrap_or(&result),
        &write_options,
    )?;
    eprintln!(
        "Saved: {} ({}x{}), {} ms",
        output.display(),
        result.width,
        result.height,
        t.elapsed().as_millis()
    );

    // Optional dump of the final pipeline buffer without file quantization.
    if let Some(p) = raw_out {
        use std::io::Write;
        let buf: Vec<f64> = result.data.iter().map(|&v| v as f64).collect();
        let bytes = bytemuck_cast_f64_to_bytes(&buf);
        let mut f = std::fs::File::create(p)
            .with_context(|| format!("creating raw_out file: {}", p.display()))?;
        f.write_all(bytes)?;
        eprintln!("raw f64 buffer: {} ({} values)", p.display(), buf.len());
    }

    if show_timings {
        eprintln!("Total: {} ms", total_start.elapsed().as_millis());
    }

    Ok(())
}

fn bytemuck_cast_f64_to_bytes(v: &[f64]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn cmd_list_profiles(data_dir: &Path) {
    let profiles_dir = data_dir.join("profiles");
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if name.ends_with(".json") {
                names.push(name.trim_end_matches(".json").to_string());
            }
        }
    } else {
        eprintln!("Profile directory not found: {}", profiles_dir.display());
        return;
    }
    names.sort();
    for name in &names {
        // Try to load and show the human-readable name
        if let Ok(p) = profile::load_profile_by_name(data_dir, name) {
            let display_name = p.info.name.as_deref().unwrap_or(name);
            let film_type = &p.info.film_type;
            let support = &p.info.support;
            println!("{name:<40} {display_name:<30} ({film_type}, {support})");
        } else {
            println!("{name}");
        }
    }
}

fn cmd_export_lut(
    film_name: &str,
    paper_name: Option<&str>,
    size: u32,
    input_encoding: LutInputEncoding,
    output: &Path,
    data_dir: &Path,
) -> Result<()> {
    let film = profile::load_profile_by_name(data_dir, film_name)
        .with_context(|| format!("loading film profile: {film_name}"))?;

    let print_stock = if let Some(p) = paper_name {
        p.to_string()
    } else if let Some(ref target) = film.info.target_print {
        target.clone()
    } else {
        bail!("no paper specified and film has no target_print");
    };
    let print = profile::load_profile_by_name(data_dir, &print_stock)
        .with_context(|| format!("loading print profile: {print_stock}"))?;

    let mut params = RuntimeParams::default();
    params.io.input_color_space = input_encoding.color_space().name().into();
    // Native V-Log code 1.0 has eight stops of headroom above 18% gray.
    params.io.input_cctf_decoding = !matches!(input_encoding, LutInputEncoding::LinearProphoto);
    let pipeline = LutPipeline::new(film, print, params, data_dir)
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!("initializing calibrated LUT pipeline for {film_name} / {print_stock}")
        })?;

    // Flatten the cube as a 2D image with red as the fastest-varying axis.
    // The LUT mode disables spatial and image-dependent effects.
    use spektrafilm_math::precision::from_f64;
    let sample = |index| from_f64(f64::from(index) / f64::from(size - 1));
    let mut grid = Vec::with_capacity(3 * (size as usize).pow(3));
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                grid.extend([sample(r), sample(g), sample(b)]);
            }
        }
    }
    eprintln!("Generating {size}x{size}x{size} CUBE LUT...");
    let grid = ImageBuf::from_data(size, size * size, grid);
    let rendered = pipeline.process(&grid).context("sampling calibrated LUT")?;

    let transfer = match input_encoding {
        LutInputEncoding::LinearProphoto => "linear",
        LutInputEncoding::Srgb => "sRGB",
        LutInputEncoding::VLog => "V-Log",
    };
    let mut cube_data = String::new();
    writeln!(&mut cube_data, "# Created by spektrafilm-rs")?;
    writeln!(
        &mut cube_data,
        "# Input {} ({transfer}) -> output sRGB SDR",
        input_encoding.color_space().name()
    )?;
    writeln!(
        &mut cube_data,
        "TITLE \"spektrafilm {film_name} -> {print_stock}\""
    )?;
    writeln!(&mut cube_data, "LUT_3D_SIZE {size}")?;
    writeln!(&mut cube_data, "DOMAIN_MIN 0.0 0.0 0.0")?;
    writeln!(&mut cube_data, "DOMAIN_MAX 1.0 1.0 1.0")?;
    for px in rendered.data.chunks_exact(3) {
        let [r, g, b] = [px[0], px[1], px[2]].map(|value| value.clamp(0.0, 1.0));
        writeln!(&mut cube_data, "{r:.9} {g:.9} {b:.9}")?;
    }

    std::fs::write(output, &cube_data)
        .with_context(|| format!("writing CUBE file: {}", output.display()))?;
    eprintln!("LUT saved: {}", output.display());
    Ok(())
}
