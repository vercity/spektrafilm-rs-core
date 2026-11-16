use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use spektrafilm_core::params::RuntimeParams;
use spektrafilm_core::pipeline::Pipeline;
use spektrafilm_core::profile;
use spektrafilm_io::raw;
use spektrafilm_math::image::ImageBuf;

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
        /// Input image (TIFF, PNG, JPEG, or camera RAW).
        input: PathBuf,
        /// Output image path.
        #[arg(short, long)]
        output: PathBuf,
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
}

#[derive(Args)]
struct RawOptions {
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
}

impl LutInputEncoding {
    fn color_space(self) -> &'static str {
        match self {
            Self::LinearProphoto => "ProPhoto RGB",
            Self::Srgb => "sRGB",
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
            film,
            paper,
            scan_film,
            timings,
            params: params_file,
            raw,
            raw_out,
            iters,
            data_dir,
        } => {
            cmd_process(
                &input,
                &output,
                &film,
                paper.as_deref(),
                scan_film,
                timings,
                params_file.as_deref(),
                &raw,
                raw_out.as_deref(),
                iters,
                &data_dir,
            )?;
        }
        Commands::ListProfiles { data_dir } => {
            cmd_list_profiles(&data_dir);
        }
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
    film_name: &str,
    paper_name: Option<&str>,
    scan_film: bool,
    show_timings: bool,
    params_file: Option<&Path>,
    raw_options: &RawOptions,
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

    // Auto-detect input color space from file extension
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let is_raw = raw::is_raw(input);
    let white_balance = raw_options.white_balance(is_raw)?;
    if is_raw {
        params.io.input_color_space = raw::INPUT_COLOR_SPACE.into();
        params.io.input_cctf_decoding = false;
    } else if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
        params.io.input_color_space = "sRGB".into();
        params.io.input_cctf_decoding = false;
    }

    // Load image
    let t = Instant::now();
    let image = if is_raw {
        raw::decode_with_white_balance(input, white_balance)?
    } else {
        load_image(input)?
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
    save_image(&result, output)?;
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

    let params = lut_runtime_params(input_encoding);
    let pipeline = Pipeline::new_with_spectral(film, print, params, data_dir)
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!("initializing calibrated LUT pipeline for {film_name} / {print_stock}")
        })?;
    let backend = spektrafilm_gpu::cpu_backend::CpuBackend;

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
    let rendered = pipeline.process(grid, &backend);
    if rendered.data.iter().any(|value| !value.is_finite()) {
        bail!("calibrated LUT render returned a non-finite sample");
    }

    let transfer = match input_encoding {
        LutInputEncoding::LinearProphoto => "linear",
        LutInputEncoding::Srgb => "sRGB",
    };
    let mut cube_data = String::new();
    writeln!(&mut cube_data, "# Created by spektrafilm-rs")?;
    writeln!(
        &mut cube_data,
        "# Input {} ({transfer}) -> output sRGB SDR",
        input_encoding.color_space()
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

fn lut_runtime_params(input_encoding: LutInputEncoding) -> RuntimeParams {
    let mut params = RuntimeParams::default();
    params.io.input_color_space = input_encoding.color_space().into();
    params.io.input_cctf_decoding = matches!(input_encoding, LutInputEncoding::Srgb);

    // Match Python's per-pixel LUT mode. Optical, stochastic and image-wise
    // operations are disabled, while pointwise DIR stays enabled.
    params.camera.auto_exposure = false;
    params.camera.exposure_compensation_ev = 0.0;
    params.camera.lens_blur_um = 0.0;
    params.camera.diffusion_filter.active = false;
    params.enlarger.print_exposure = 1.0;
    params.enlarger.print_exposure_compensation = false;
    params.enlarger.lens_blur = 0.0;
    params.enlarger.diffusion_filter.active = false;
    params.film_render.halation.active = false;
    params.film_render.halation.boost_ev = 0.0;
    params.film_render.grain.active = false;
    params.film_render.glare.active = false;
    params.film_render.dir_couplers.diffusion_size_um = 0.0;
    params.film_render.dir_couplers.diffusion_tail_um = 0.0;
    params.print_render.glare.active = false;
    params.scanner.white_correction = false;
    params.scanner.black_correction = false;
    params.scanner.lens_blur = 0.0;
    params.scanner.unsharp_mask = [0.0, 0.0];
    params
}

/// Load raster RGB without changing the transfer function of a TIFF.
fn load_image(path: &Path) -> Result<ImageBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "tif" | "tiff" => load_tiff(path),
        "png" | "jpg" | "jpeg" => load_srgb_raster(path),
        _ => bail!("unsupported image format: .{ext} (supported: tiff, png, jpeg, camera RAW)"),
    }
}

fn decode_no_limits(path: &Path) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::open(path)
        .with_context(|| format!("opening image: {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting image format: {}", path.display()))?;
    reader.no_limits();
    reader
        .decode()
        .with_context(|| format!("decoding image: {}", path.display()))
}

fn load_tiff(path: &Path) -> Result<ImageBuf> {
    let img = decode_no_limits(path)?;
    let rgb = img.to_rgb32f();
    let (w, h) = (rgb.width(), rgb.height());
    let data: Vec<f32> = rgb.into_raw();
    let scalars: Vec<spektrafilm_math::precision::Scalar> = data
        .into_iter()
        .map(spektrafilm_math::precision::from_f32)
        .collect();
    Ok(ImageBuf::from_data(w, h, scalars))
}

fn load_srgb_raster(path: &Path) -> Result<ImageBuf> {
    let img = decode_no_limits(path)?;
    let rgb = img.to_rgb32f();
    let (w, h) = (rgb.width(), rgb.height());
    let data: Vec<f32> = rgb.into_raw();
    // PNG and JPEG use the standard sRGB transfer curve by default.
    let scalars: Vec<spektrafilm_math::precision::Scalar> = data
        .into_iter()
        .map(|v| spektrafilm_math::precision::srgb_decode(spektrafilm_math::precision::from_f32(v)))
        .collect();
    Ok(ImageBuf::from_data(w, h, scalars))
}

/// Save an ImageBuf to TIFF (16-bit) or PNG (8-bit).
fn save_image(img: &ImageBuf, path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "tif" | "tiff" => save_tiff(img, path),
        "png" => save_png(img, path),
        "jpg" | "jpeg" => save_jpeg(img, path),
        _ => bail!("unsupported output format: .{ext} (supported: tiff, png, jpg)"),
    }
}

fn save_tiff(img: &ImageBuf, path: &Path) -> Result<()> {
    // Convert to 16-bit
    let data_u16: Vec<u16> = img
        .data
        .iter()
        .map(|&v| ((v.clamp(0.0, 1.0) * 65535.0).round_ties_even()) as u16)
        .collect();

    let buf: image::ImageBuffer<image::Rgb<u16>, Vec<u16>> =
        image::ImageBuffer::from_raw(img.width, img.height, data_u16)
            .context("creating image buffer")?;

    buf.save(path)
        .with_context(|| format!("saving TIFF: {}", path.display()))?;
    Ok(())
}

fn save_jpeg(img: &ImageBuf, path: &Path) -> Result<()> {
    // Same 8-bit quantize as PNG; round_ties_even keeps the pre-encode
    // pixel values numpy-identical. Quality 95 — defending the f64
    // export path against silent quality loss; `image`'s default of 75
    // would defeat the point of running the pipeline at f64.
    use image::ImageEncoder;
    let data_u8: Vec<u8> = img
        .data
        .iter()
        .map(|&v| ((v.clamp(0.0, 1.0) * 255.0).round_ties_even()) as u8)
        .collect();
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(path)
            .with_context(|| format!("creating JPEG: {}", path.display()))?,
    );
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 95)
        .write_image(
            &data_u8,
            img.width,
            img.height,
            image::ExtendedColorType::Rgb8,
        )
        .with_context(|| format!("saving JPEG: {}", path.display()))?;
    Ok(())
}

fn save_png(img: &ImageBuf, path: &Path) -> Result<()> {
    // Python: `np.round(x * 255).astype(np.uint8)`. Numpy uses
    // round-half-to-even (banker's rounding), *not* round-half-up.
    // `f64::round()` in Rust is round-half-away-from-zero so it
    // disagrees with numpy at exact x.5 inputs — a rare but real
    // diff that prevented the bare-chain output from being
    // bit-identical with Python. `round_ties_even` (stable since
    // Rust 1.77) matches numpy literally.
    let data_u8: Vec<u8> = img
        .data
        .iter()
        .map(|&v| ((v.clamp(0.0, 1.0) * 255.0).round_ties_even()) as u8)
        .collect();

    let buf: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
        image::ImageBuffer::from_raw(img.width, img.height, data_u8)
            .context("creating image buffer")?;

    buf.save(path)
        .with_context(|| format!("saving PNG: {}", path.display()))?;
    Ok(())
}
