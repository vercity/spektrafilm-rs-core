// Minimal interactive preview for spektrafilm.
//
// Loads an image, exposes the most impactful runtime parameters as
// egui controls, and re-renders the full GPU-resident pipeline whenever
// a control changes. The output texture is uploaded to egui once per
// render; the panel scales it to fit.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;
use rayon::prelude::*;
use spektrafilm_core::params::RuntimeParams;
use spektrafilm_core::pipeline::Pipeline;
use spektrafilm_core::profile;
use spektrafilm_gpu::ComputeBackend;
use spektrafilm_io::{
    raster::{ColorTransform, RasterColor, RasterMetadata, RasterWriteOptions},
    raw, read_image, write_developed_raw,
};
use spektrafilm_math::precision::{from_f32, to_f32};
use spektrafilm_math::{
    colorspace::{Cctf, RgbSpace},
    image::ImageBuf,
};

mod development_animation;
#[cfg(target_os = "windows")]
mod display_profile_windows;
mod export_output;
mod paper_back;
mod presets;
mod preview_source;
mod raw_workflow;
mod source_snapshot;
mod spatial_filter;
mod viewer;

use export_output::{StagedOutput, temporary_export_directory, write_preview_output};
use presets::{FACTORY_FILM, FACTORY_PAPER, GuiPreset, Preferences};
use preview_source::{PreviewSource, interpret_input};
use raw_workflow::RawDevelopSettings;
use viewer::{InputLayerRequest, Layer, Viewer};

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(120);
const IN_FLIGHT_REPAINT: Duration = Duration::from_millis(16);
const PREVIEW_TEXTURE_MAX_DIM: usize = 8192;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    if let Err(err) = embedded_bundle::setup() {
        eprintln!("[spektrafilm] embedded bundle setup failed: {err:#}");
    }
    let backend = spektrafilm_gpu::select_backend();
    let backend: Arc<dyn ComputeBackend> = Arc::from(backend);
    let initial_image = std::env::args().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 900.0]),
        renderer: gui_renderer(),
        // egui's default device descriptor hardcodes
        // `max_texture_dimension_2d: 8192`, so uploading the preview of an
        // image wider or taller than 8192 px (a single texture) panics.
        // Raise the cap to the adapter's real limit (16384 on Apple
        // Silicon Metal, higher on discrete GPUs), floored at egui's 8192
        // so we never regress. This is eframe's OWN render device — the
        // compute pipeline's wgpu device (WgpuBackend::new) is separate
        // and already lifts its storage-buffer limits.
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            wgpu_setup: eframe::egui_wgpu::WgpuSetupCreateNew {
                device_descriptor: Arc::new(|adapter: &eframe::wgpu::Adapter| {
                    let base_limits = if adapter.get_info().backend == eframe::wgpu::Backend::Gl {
                        eframe::wgpu::Limits::downlevel_webgl2_defaults()
                    } else {
                        eframe::wgpu::Limits::default()
                    };
                    eframe::wgpu::DeviceDescriptor {
                        label: Some("spektrafilm egui wgpu device"),
                        required_features: eframe::wgpu::Features::default(),
                        required_limits: eframe::wgpu::Limits {
                            max_texture_dimension_2d: adapter
                                .limits()
                                .max_texture_dimension_2d
                                .max(8192),
                            ..base_limits
                        },
                        memory_hints: eframe::wgpu::MemoryHints::default(),
                    }
                }),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "spektrafilm",
        options,
        Box::new(|_| {
            Ok(Box::new(App::new_with_preferences(
                backend,
                initial_image,
                presets::config_dir(),
            )))
        }),
    )
}

#[cfg(embed_bundle)]
mod embedded_bundle {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    include!(env!("SPEKTRAFILM_EMBED_MANIFEST"));

    pub fn setup() -> Result<()> {
        let root = cache_root().join(BUNDLE_ID);
        let data_dir = root.join("data");
        let f64_cli = root.join(f64_cli_name());
        let marker = root.join(".ready");

        if !marker.is_file() || !data_dir.join("profiles").is_dir() || !f64_cli.is_file() {
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            for (rel, bytes) in DATA_FILES {
                write_file(&data_dir.join(rel), bytes)?;
            }
            write_file(&f64_cli, F64_EXE)?;
            write_file(&marker, BUNDLE_ID.as_bytes())?;
        }

        // This runs before any worker threads are spawned.
        unsafe {
            std::env::set_var("SPEKTRAFILM_DATA_DIR", &data_dir);
            std::env::set_var("SPEKTRAFILM_F64_CLI", &f64_cli);
        }
        Ok(())
    }

    fn cache_root() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("spektrafilm")
            .join("embedded")
    }

    fn f64_cli_name() -> &'static str {
        if cfg!(windows) {
            "spektrafilm-f64.exe"
        } else {
            "spektrafilm-f64"
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file =
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(not(embed_bundle))]
mod embedded_bundle {
    use anyhow::Result;

    pub fn setup() -> Result<()> {
        Ok(())
    }
}

fn gui_renderer() -> eframe::Renderer {
    match std::env::var("SPEKTRAFILM_GUI_RENDERER") {
        Ok(v) if v.eq_ignore_ascii_case("wgpu") => eframe::Renderer::Wgpu,
        Ok(v) if v.eq_ignore_ascii_case("glow") => eframe::Renderer::Glow,
        _ => {
            if cfg!(target_os = "macos") {
                // Metal-backed CAMetalLayer lets us tag the colorspace as sRGB.
                eframe::Renderer::Wgpu
            } else {
                // Keep the UI compositor away from wgpu/D3D12 by default on
                // Windows/Linux; CUDA/WGSL compute backends are selected separately.
                eframe::Renderer::Glow
            }
        }
    }
}

/// One entry in the film / paper combo box. `stock` is the filename
/// stem (the unique key the profile loader expects); `display` is the
/// human-readable label from the profile's `info.name`, falling back
/// to the stock id when missing.
#[derive(Debug, Clone)]
struct ProfileEntry {
    stock: String,
    display: String,
}

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct OutputDepths {
    tiff: u8,
    exr: u8,
    png: u8,
}

impl Default for OutputDepths {
    fn default() -> Self {
        Self {
            tiff: 16,
            exr: 16,
            png: 8,
        }
    }
}

impl OutputDepths {
    fn for_path(&self, path: &Path) -> Option<u8> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "tif" | "tiff" => Some(self.tiff),
            "exr" => Some(self.exr),
            "png" => Some(self.png),
            _ => None,
        }
    }
}

fn depth_control(ui: &mut egui::Ui, label: &str, current: &mut u8, choices: &[u8]) {
    egui::ComboBox::from_label(label)
        .selected_text(format!("{current} bit"))
        .show_ui(ui, |ui| {
            for &depth in choices {
                ui.selectable_value(current, depth, format!("{depth} bit"));
            }
        });
}

const IDENTITY_CHANNELS: [usize; 3] = [0, 1, 2];

fn channel_order_control(ui: &mut egui::Ui, label: &str, order: &mut [usize; 3]) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        order.iter_mut().fold(false, |changed, index| {
            ui.add(egui::DragValue::new(index).range(0..=2)).changed() || changed
        })
    })
    .inner
}

fn nonnegative_controls<T: egui::emath::Numeric, const N: usize>(
    ui: &mut egui::Ui,
    label: &str,
    fields: &mut [T; N],
) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        fields.iter_mut().fold(false, |changed, field| {
            ui.add(
                egui::DragValue::new(field)
                    .speed(0.05)
                    .range(0.0..=f64::INFINITY),
            )
            .changed()
                || changed
        })
    })
    .inner
}

const SRGB_DISPLAY: RasterColor = RasterColor {
    color_space: "sRGB",
    cctf_encoding: true,
};
const UNTAGGED_INPUT: RasterColor = RasterColor {
    color_space: "ProPhoto RGB",
    cctf_encoding: false,
};

fn simulation_color(params: &RuntimeParams) -> RasterColor {
    RasterColor::new(
        &params.io.output_color_space,
        params.io.output_cctf_encoding,
    )
    .unwrap_or(SRGB_DISPLAY)
}

fn color_control(ui: &mut egui::Ui, label: &str, color: &mut RasterColor) -> bool {
    let mut space = RgbSpace::from_name(color.color_space).expect("known RGB space");
    let mut changed = false;
    egui::ComboBox::from_label(label)
        .selected_text(space.name())
        .show_ui(ui, |ui| {
            for choice in RgbSpace::ALL {
                changed |= ui
                    .selectable_value(&mut space, choice, choice.name())
                    .changed();
            }
        });
    let encoded = &mut color.cctf_encoding;
    if space.transfer() == Cctf::Linear {
        *encoded = false;
    }
    changed |= ui
        .add_enabled(
            space.transfer() != Cctf::Linear,
            egui::Checkbox::new(encoded, "Apply transfer function"),
        )
        .changed();
    color.color_space = space.name();
    changed
}

fn has_hanatos_surface(film: &profile::Profile) -> bool {
    let params = &film.data.hanatos2025_adaptation_surface_params;
    params.len() == 3 && params.iter().all(|channel| channel.len() == 15)
}

struct App {
    backend: Arc<dyn ComputeBackend>,
    data_dir: PathBuf,
    preferences: Preferences,
    /// Profiles where `info.support == "film"`.
    films: Vec<ProfileEntry>,
    /// Profiles where `info.support == "paper"` (or other print-stage
    /// supports).
    papers: Vec<ProfileEntry>,
    film_name: String,
    print_name: String,
    /// Development-time family (minutes) of the selected film / paper —
    /// non-trivial only for B&W stocks. Drives the dev-time pickers.
    film_dev_times: Vec<f64>,
    print_dev_times: Vec<f64>,
    hanatos_surface_available: bool,
    params: RuntimeParams,
    source: Option<Arc<source_snapshot::SourceSnapshot>>,
    source_rotation: u8,
    raw_develop: RawDevelopSettings,
    image: Option<Arc<ImageBuf>>,
    source_metadata: RasterMetadata,
    detected_input_color: Option<RasterColor>,
    override_input_color: bool,
    manual_input_color: RasterColor,
    saving_color: RasterColor,
    display_transform: bool,
    #[cfg(target_os = "windows")]
    display_profile: display_profile_windows::DisplayProfile,
    output_depths: OutputDepths,
    /// Saveable output for the current image and settings.
    output_image: Option<ImageBuf>,
    output_tex: Option<egui::TextureHandle>,
    viewer: Viewer,
    pipeline_cache: Option<PipelineCache>,
    preview_source: Option<PreviewSource>,
    last_render_ms: f32,
    last_pipeline_build_ms: f32,
    last_input_clone_ms: f32,
    last_preview_ms: f32,
    last_worker_total_ms: f32,
    status: String,
    /// The generation of the current inputs. Edits invalidate in-flight results.
    render_generation: u64,
    auto_preview: bool,
    /// Only the newest render request is queued while a render runs.
    render_request: Option<RenderRequest>,
    /// Current preview zoom multiplier on top of the fit-to-panel
    /// scale (1.0 = fit). Mouse wheel adjusts it around the cursor,
    /// drag pans, double-click resets.
    zoom: f32,
    /// Pan offset (screen pixels) added to the preview centre.
    pan: egui::Vec2,
    gray_canvas: bool,
    white_border: f32,
    preview_interpolation: String,
    film_channel_swap: [usize; 3],
    print_channel_swap: [usize; 3],
    /// macOS-only: tag the wgpu CAMetalLayer's `colorspace` as sRGB on
    /// the first `update()` tick (it's not yet wired up at
    /// `App::new` time). Set to `true` once the call succeeds so we
    /// don't retry every frame.
    #[cfg(target_os = "macos")]
    metal_colorspace_tagged: bool,
    /// In-flight pipeline render. `Some` while the worker thread is
    /// running; main thread polls the receiver each frame and
    /// uploads the resulting texture once it lands. Decoupling the
    /// render from the UI thread is what keeps sliders responsive —
    /// the 250–500 ms pipeline used to block input handling.
    render_job: Option<RenderJob>,
    /// In-flight f64 export job. `Some` while the subprocess is
    /// running; the `update()` loop polls the receiver each frame and
    /// surfaces success/failure in `status` when the worker thread
    /// completes. Joined eagerly to release the thread.
    export_job: Option<ExportJob>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderMode {
    Preview,
    Scan,
}

impl RenderMode {
    fn label(self) -> &'static str {
        match self {
            Self::Preview => "Preview",
            Self::Scan => "Scan",
        }
    }
}

#[derive(Clone, Copy)]
struct RenderRequest {
    mode: RenderMode,
    automatic: bool,
    ready_at: Instant,
}

struct PipelineCache {
    key: String,
    pipeline: Pipeline,
}

/// An in-flight render owns the cache while the UI remains responsive.
struct RenderJob {
    generation: u64,
    mode: RenderMode,
    rx: mpsc::Receiver<RenderReply>,
    handle: Option<JoinHandle<()>>,
}

struct RenderReply {
    pipeline_cache: Option<PipelineCache>,
    preview_source: Option<PreviewSource>,
    result: Result<RenderResult, String>,
}

struct RenderResult {
    output: ImageBuf,
    preview: PreviewTextureData,
    display_transform: bool,
    input_clone_ms: f32,
    pipeline_build_ms: f32,
    render_ms: f32,
    preview_ms: f32,
    worker_total_ms: f32,
}

struct PreviewTextureData {
    size: [usize; 2],
    rgba: Vec<u8>,
}

/// One in-flight f64 export. The worker thread owns the child
/// process and polls `cancel` in its wait loop. On completion the
/// worker sends `Ok(elapsed_seconds, output_filename)` or `Err(msg)`;
/// `Err("cancelled")` is also produced when the cancel flag is set.
/// The join handle is held so we can `join()` after consuming the
/// message and on `on_exit` to drain the worker before the process dies.
struct ExportJob {
    rx: mpsc::Receiver<Result<(f32, String), String>>,
    handle: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    started_at: Instant,
}

impl App {
    #[cfg(test)]
    fn new(backend: Arc<dyn ComputeBackend>, initial_image: Option<PathBuf>) -> Self {
        Self::new_with_preferences(backend, initial_image, None)
    }

    fn new_with_preferences(
        backend: Arc<dyn ComputeBackend>,
        initial_image: Option<PathBuf>,
        config_dir: Option<PathBuf>,
    ) -> Self {
        let (preferences, directory_error) = Preferences::open(config_dir);
        let data_dir = pick_data_dir();
        let (films, papers) = scan_profiles(&data_dir);
        let factory = GuiPreset::default();
        let film_name = pick_default_stock(&films, FACTORY_FILM);
        let print_name = pick_default_stock(&papers, FACTORY_PAPER);
        let film_profile = profile::load_profile_by_name(&data_dir, &film_name).ok();
        let mut params = factory.params;
        params.io.scan_film = film_profile.as_ref().is_some_and(|f| f.is_positive());
        let film_dev_times = film_profile
            .as_ref()
            .map(|f| f.data.development_time.clone())
            .unwrap_or_default();
        let print_dev_times = profile_dev_times(&data_dir, &print_name);

        let mut app = Self {
            backend,
            data_dir,
            preferences,
            films,
            papers,
            film_name,
            print_name,
            film_dev_times,
            print_dev_times,
            hanatos_surface_available: film_profile.as_ref().is_some_and(has_hanatos_surface),
            params,
            source: None,
            source_rotation: 0,
            raw_develop: RawDevelopSettings::default(),
            image: None,
            source_metadata: RasterMetadata::default(),
            detected_input_color: None,
            override_input_color: false,
            manual_input_color: UNTAGGED_INPUT,
            saving_color: SRGB_DISPLAY,
            display_transform: true,
            #[cfg(target_os = "windows")]
            display_profile: display_profile_windows::DisplayProfile::default(),
            output_depths: OutputDepths::default(),
            output_image: None,
            output_tex: None,
            viewer: Viewer::default(),
            pipeline_cache: None,
            preview_source: None,
            last_render_ms: 0.0,
            last_pipeline_build_ms: 0.0,
            last_input_clone_ms: 0.0,
            last_preview_ms: 0.0,
            last_worker_total_ms: 0.0,
            render_generation: 0,
            auto_preview: true,
            render_request: None,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            gray_canvas: factory.gray_canvas,
            white_border: factory.white_padding,
            preview_interpolation: factory.interpolation,
            film_channel_swap: factory.film_channels,
            print_channel_swap: factory.print_channels,
            render_job: None,
            status: String::from("Load an image to start."),
            #[cfg(target_os = "macos")]
            metal_colorspace_tagged: false,
            export_job: None,
        };
        let startup = app
            .preferences
            .startup_preset()
            .and_then(|preset| match preset {
                Some(preset) => app.apply_preset(preset),
                None => Ok(()),
            });
        if let Some(p) = initial_image {
            app.load_image_from_path(&p);
        }
        if let Some(error) = startup.err().or(directory_error) {
            app.status = format!("{} GUI settings: {error:#}", app.status);
        }
        app
    }

    fn current_preset(&self) -> GuiPreset {
        let mut params = self.params.clone();
        params.io.input_color_space = self.manual_input_color.color_space.into();
        params.io.input_cctf_decoding = self.manual_input_color.cctf_encoding;
        GuiPreset {
            film: self.film_name.clone(),
            paper: self.print_name.clone(),
            params,
            override_input_color: self.override_input_color,
            saving_color: self.saving_color,
            depths: self.output_depths,
            raw_develop: self.raw_develop,
            auto_preview: self.auto_preview,
            display_transform: self.display_transform,
            gray_canvas: self.gray_canvas,
            white_padding: self.white_border,
            interpolation: self.preview_interpolation.clone(),
            film_channels: self.film_channel_swap,
            print_channels: self.print_channel_swap,
        }
    }

    fn apply_preset(&mut self, preset: GuiPreset) -> Result<()> {
        anyhow::ensure!(
            self.films.iter().any(|entry| entry.stock == preset.film),
            "unknown preset film: {}",
            preset.film
        );
        anyhow::ensure!(
            self.papers.iter().any(|entry| entry.stock == preset.paper),
            "unknown preset paper: {}",
            preset.paper
        );
        let film = profile::load_profile_by_name(&self.data_dir, &preset.film)?;
        let paper = profile::load_profile_by_name(&self.data_dir, &preset.paper)?;
        let input_color = RasterColor::new(
            &preset.params.io.input_color_space,
            preset.params.io.input_cctf_decoding,
        )
        .context("unsupported preset input color space")?;

        self.film_name = preset.film;
        self.print_name = preset.paper;
        self.film_dev_times.clone_from(&film.data.development_time);
        self.print_dev_times
            .clone_from(&paper.data.development_time);
        self.hanatos_surface_available = has_hanatos_surface(&film);
        self.params = preset.params;
        self.params.settings.apply_hanatos2025_adaptation_surface &= self.hanatos_surface_available;
        self.manual_input_color = input_color;
        self.override_input_color = preset.override_input_color;
        self.saving_color = preset.saving_color;
        self.output_depths = preset.depths;
        self.raw_develop = preset.raw_develop;
        self.auto_preview = preset.auto_preview;
        self.display_transform = preset.display_transform;
        self.gray_canvas = preset.gray_canvas;
        self.white_border = preset.white_padding;
        self.preview_interpolation = preset.interpolation;
        self.film_channel_swap = preset.film_channels;
        self.print_channel_swap = preset.print_channels;
        self.sync_input_color();
        self.viewer.clear_output();
        self.output_tex = None;
        self.request_render();
        Ok(())
    }

    fn refresh_film_selection(&mut self) {
        self.film_dev_times = Vec::new();
        self.hanatos_surface_available = false;
        self.params.film_render.development_time = None;
        if let Ok(film) = profile::load_profile_by_name(&self.data_dir, &self.film_name) {
            // The Python GUI keeps the selected paper on a film change.
            self.params.io.scan_film = film.is_positive();
            self.film_dev_times = film.data.development_time.clone();
            self.hanatos_surface_available = has_hanatos_surface(&film);
            self.params.film_render.halation.halation_strength = None;
            self.params.film_render.halation.halation_first_sigma_um = None;
            self.params.film_render.dir_couplers.gamma_samelayer_rgb = None;
            self.params
                .film_render
                .dir_couplers
                .gamma_interlayer_r_to_gb = None;
            self.params
                .film_render
                .dir_couplers
                .gamma_interlayer_g_to_rb = None;
            self.params
                .film_render
                .dir_couplers
                .gamma_interlayer_b_to_rg = None;
        }
        self.params.settings.apply_hanatos2025_adaptation_surface &= self.hanatos_surface_available;
        self.request_render();
    }

    fn load_preset(&mut self, path: &Path) -> Result<()> {
        self.apply_preset(GuiPreset::read(path)?)
    }

    fn restore_factory(&mut self) -> Result<()> {
        let mut factory = GuiPreset {
            film: pick_default_stock(&self.films, FACTORY_FILM),
            paper: pick_default_stock(&self.papers, FACTORY_PAPER),
            ..GuiPreset::default()
        };
        factory.params.io.scan_film =
            profile::load_profile_by_name(&self.data_dir, &factory.film)?.is_positive();
        self.preferences.clear_startup()?;
        self.apply_preset(factory)
    }

    fn remember_dialog(&mut self, key: &str, path: &Path) {
        if let Err(error) = self.preferences.remember_dialog(key, path) {
            tracing::warn!(%error, "could not remember the dialog directory");
        }
    }

    fn preset_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Presets", |ui| {
            if ui.button("Load preset…").clicked() {
                ui.close_menu();
                if let Some(path) = self
                    .preferences
                    .dialog("load_preset")
                    .add_filter("JSON", &["json"])
                    .pick_file()
                {
                    self.remember_dialog("load_preset", &path);
                    self.status = match self.load_preset(&path) {
                        Ok(()) => format!("Loaded preset {}", path.display()),
                        Err(error) => format!("Load preset error: {error:#}"),
                    };
                }
            }
            if ui.button("Save preset…").clicked() {
                ui.close_menu();
                if let Some(mut path) = self
                    .preferences
                    .dialog("save_preset")
                    .add_filter("JSON", &["json"])
                    .set_file_name("gui_state.json")
                    .save_file()
                {
                    if path.extension().is_none() {
                        path.set_extension("json");
                    }
                    self.remember_dialog("save_preset", &path);
                    self.status = match self.current_preset().write(&path) {
                        Ok(()) => format!("Saved preset {}", path.display()),
                        Err(error) => format!("Save preset error: {error:#}"),
                    };
                }
            }
            ui.separator();
            if ui.button("Save as startup default").clicked() {
                ui.close_menu();
                self.status = match self.preferences.save_startup(&self.current_preset()) {
                    Ok(()) => "Saved GUI startup default.".into(),
                    Err(error) => format!("Startup preset error: {error:#}"),
                };
            }
            if ui.button("Restore factory defaults").clicked() {
                ui.close_menu();
                self.status = match self.restore_factory() {
                    Ok(()) => "Restored factory defaults.".into(),
                    Err(error) => format!("Restore defaults error: {error:#}"),
                };
            }
        });
    }

    fn load_image_from_path(&mut self, path: &Path) {
        let t = Instant::now();
        let captured = source_snapshot::SourceSnapshot::capture(path)
            .and_then(|source| {
                Ok((
                    read_image(source.path(), self.raw_develop.options())?,
                    source,
                ))
            })
            .with_context(|| format!("loading {}", path.display()));
        match captured {
            Ok((loaded, source)) => {
                self.detected_input_color = loaded.color;
                self.sync_input_color();
                self.source_metadata = loaded.metadata;
                let img = loaded.image;
                self.status = format!(
                    "Loaded {} × {} ({:.1} MP) in {:.0} ms",
                    img.width,
                    img.height,
                    (img.pixel_count() as f64 / 1e6),
                    t.elapsed().as_secs_f32() * 1000.0
                );
                self.image = Some(Arc::new(img));
                self.preview_source = None;
                self.viewer.source_changed();
                self.source = Some(Arc::new(source));
                self.source_rotation = 0;
                self.output_tex = None;
                self.request_render();
            }
            Err(e) => {
                self.status = format!("Load error: {e:#}");
            }
        }
    }

    fn source_is_raw(&self) -> bool {
        self.source
            .as_ref()
            .is_some_and(|source| raw::is_raw(source.path()))
    }

    fn redevelop_raw(&mut self) {
        let Some(source) = self
            .source
            .as_ref()
            .filter(|source| raw::is_raw(source.path()))
        else {
            return;
        };
        match read_image(source.path(), self.raw_develop.options()) {
            Ok(loaded) => {
                let image = if self.source_rotation == 0 {
                    loaded.image
                } else {
                    loaded.image.rotated(self.source_rotation)
                };
                self.image = Some(Arc::new(image));
                self.preview_source = None;
                self.viewer.source_changed();
                self.source_metadata = loaded.metadata;
                self.output_tex = None;
                self.request_render();
                self.status = "RAW developed.".into();
            }
            Err(error) => self.status = format!("RAW development error: {error:#}"),
        }
    }

    fn rotate_input(&mut self, clockwise_quarter_turns: u8) {
        let Some(image) = self.image.as_ref() else {
            return;
        };
        self.image = Some(Arc::new(image.rotated(clockwise_quarter_turns)));
        self.viewer.source_changed();
        self.preview_source = None;
        self.source_rotation = (self.source_rotation + clockwise_quarter_turns) % 4;
        self.output_tex = None;
        self.request_render();
    }

    fn sync_input_color(&mut self) {
        let color = if self.override_input_color {
            self.manual_input_color
        } else {
            self.detected_input_color.unwrap_or(UNTAGGED_INPUT)
        };
        self.params.io.input_color_space = color.color_space.into();
        self.params.io.input_cctf_decoding = color.cctf_encoding;
    }

    fn request_render(&mut self) {
        self.render_generation += 1;
        self.output_image = None;
        if self.auto_preview && self.image.is_some() {
            let ready_at = self
                .render_request
                .filter(|request| request.automatic)
                .map_or_else(
                    || Instant::now() + PREVIEW_DEBOUNCE,
                    |request| request.ready_at,
                );
            self.render_request = Some(RenderRequest {
                mode: RenderMode::Preview,
                automatic: true,
                ready_at,
            });
        } else {
            self.render_request = None;
        }
    }

    fn request_render_mode(&mut self, mode: RenderMode) {
        self.render_generation += 1;
        self.output_image = None;
        self.render_request = Some(RenderRequest {
            mode,
            automatic: false,
            ready_at: Instant::now(),
        });
    }

    fn set_auto_preview(&mut self, enabled: bool) {
        self.auto_preview = enabled;
        if enabled {
            self.request_render();
        } else if self.render_request.is_some_and(|request| request.automatic) {
            self.render_request = None;
        }
    }

    /// Run a queued render off the UI thread using a snapshot of the inputs.
    fn dispatch_render(&mut self, ctx: &egui::Context, mode: RenderMode) {
        let t_clone = Instant::now();
        let Some(image) = self.image.as_ref().cloned() else {
            return;
        };
        let input_clone_ms = t_clone.elapsed().as_secs_f32() * 1000.0;
        let film_name = self.film_name.clone();
        let print_name = self.print_name.clone();
        let data_dir = self.data_dir.clone();
        let params = self.params.clone();
        let display_transform = self.display_transform_active();
        let channel_orders = [self.film_channel_swap, self.print_channel_swap];
        let raw_input_destination =
            (self.override_input_color && self.source_is_raw()).then_some(self.manual_input_color);
        let mut pipeline_cache = self.pipeline_cache.take();
        let mut preview_source = if mode == RenderMode::Preview {
            self.preview_source.take()
        } else {
            None
        };
        let backend = self.backend.clone();
        let (tx, rx) = mpsc::channel();
        let ctx_for_worker = ctx.clone();
        self.status = format!("Rendering {}…", mode.label().to_lowercase());
        let handle = std::thread::Builder::new()
            .name("spektrafilm-render".into())
            .spawn(move || {
                // A panic inside the pipeline must still produce a channel
                // message — otherwise the receiver only sees a disconnect
                // and the user gets a uselessly vague status line.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let t_total = Instant::now();
                    let image = match mode {
                        RenderMode::Preview => PreviewSource::prepare(
                            &mut preview_source,
                            image,
                            params.settings.preview_max_size,
                            raw_input_destination,
                        ),
                        RenderMode::Scan => interpret_input(image, raw_input_destination),
                    };
                    let params = render_params(params, mode);
                    let pipeline = render_pipeline(
                        &mut pipeline_cache,
                        &data_dir,
                        &film_name,
                        &print_name,
                        channel_orders,
                        params,
                    )?;
                    let pipeline_build_ms = t_total.elapsed().as_secs_f32() * 1000.0;
                    let t = Instant::now();
                    let output = if pipeline.params.io.scan_film && pipeline.film.is_positive() {
                        pipeline.process((*image).clone(), backend.as_ref())
                    } else {
                        pipeline
                            .process_resident_borrowed(image.as_ref(), backend.as_ref())
                            .unwrap_or_else(|| pipeline.process((*image).clone(), backend.as_ref()))
                    };
                    let render_ms = t.elapsed().as_secs_f32() * 1000.0;
                    let t_preview = Instant::now();
                    let preview_color =
                        display_transform.then(|| simulation_color(&pipeline.params));
                    let preview = make_preview_texture_data(&output, preview_color);
                    let preview_ms = t_preview.elapsed().as_secs_f32() * 1000.0;
                    let worker_total_ms = t_total.elapsed().as_secs_f32() * 1000.0;
                    Ok(RenderResult {
                        output,
                        preview,
                        display_transform,
                        input_clone_ms,
                        pipeline_build_ms,
                        render_ms,
                        preview_ms,
                        worker_total_ms,
                    })
                }))
                .unwrap_or_else(|panic| Err(panic_message(&panic)));
                let _ = tx.send(RenderReply {
                    pipeline_cache,
                    preview_source,
                    result,
                });
                ctx_for_worker.request_repaint();
            })
            .expect("OS thread spawn");
        self.render_job = Some(RenderJob {
            generation: self.render_generation,
            mode,
            rx,
            handle: Some(handle),
        });
    }

    /// Central-panel preview with mouse-wheel zoom (centred on the
    /// cursor) and click-drag pan. Double-click resets to fit.
    /// `self.zoom = 1.0` means fit-to-panel; values >1 zoom in.
    fn draw_preview_with_zoom(&mut self, ui: &mut egui::Ui, tex: &egui::TextureHandle) {
        let avail = ui.available_size();
        let img_size = tex.size_vec2();
        let border = (img_size.x.max(img_size.y) * self.white_border).floor();
        let framed_size = img_size + egui::Vec2::splat(2.0 * border);
        let fit_scale = (avail.x / framed_size.x)
            .min(avail.y / framed_size.y)
            .min(1.0);

        let response = ui.allocate_response(avail, egui::Sense::click_and_drag());
        let rect = response.rect;

        // Two zoom sources, never combined:
        //   1. trackpad pinch (`zoom_delta` — already a multiplier
        //      around 1.0, smoothed by egui per frame)
        //   2. mouse wheel scroll (`smooth_scroll_delta.y` — egui's
        //      per-frame integrated scroll). Don't add `raw_scroll_delta`
        //      on top; `smooth` already accumulates the raw events.
        // Both are anchored on the cursor so the pixel under the
        // pointer stays put.
        let (pinch, scroll) = ui.input(|i| {
            if response.hovered() {
                (i.zoom_delta(), i.smooth_scroll_delta.y)
            } else {
                (1.0, 0.0)
            }
        });
        // Scroll → small log-zoom step per notch. 0.0015 keeps a single
        // trackpad swipe feeling like a smooth zoom rather than a leap.
        let scroll_factor = if scroll.abs() > 0.01 {
            (scroll * 0.0015).exp()
        } else {
            1.0
        };
        let factor = pinch * scroll_factor;
        if (factor - 1.0).abs() > 1e-4 {
            let cursor = ui.input(|i| i.pointer.hover_pos());
            let old_zoom = self.zoom;
            self.zoom = (self.zoom * factor).clamp(0.1, 32.0);
            if let Some(cur) = cursor {
                // Keep the image point under the cursor anchored.
                let centre = rect.center() + self.pan;
                let offset_from_centre = cur - centre;
                let scale_ratio = self.zoom / old_zoom;
                self.pan += offset_from_centre * (1.0 - scale_ratio);
            }
        }

        if response.dragged() {
            self.pan += response.drag_delta();
        }
        if response.double_clicked() {
            self.zoom = 1.0;
            self.pan = egui::Vec2::ZERO;
        }

        let centre = rect.center() + self.pan;
        let scale = fit_scale * self.zoom;
        let draw_rect = egui::Rect::from_center_size(centre, img_size * scale);
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        // Clip to the central panel so panned-off pixels don't leak.
        let painter = ui.painter_at(rect);
        if self.gray_canvas {
            // 18% scene-linear gray encoded as sRGB.
            painter.rect_filled(rect, 0.0, egui::Color32::from_gray(118));
        }
        if border > 0.0 {
            painter.rect_filled(draw_rect.expand(border * scale), 0.0, egui::Color32::WHITE);
        }
        if let Some((filtered, filtered_rect)) =
            self.viewer
                .output_filter(ui.ctx(), draw_rect, rect, &self.preview_interpolation)
        {
            painter.image(filtered.id(), filtered_rect, uv, egui::Color32::WHITE);
        } else {
            painter.image(tex.id(), draw_rect, uv, egui::Color32::WHITE);
        }

        // Zoom indicator + reset chip in the corner.
        let pct = (self.zoom * fit_scale * 100.0).round() as i32;
        let text = if (self.zoom - 1.0).abs() < 0.001 && self.pan.length_sq() < 1.0 {
            "fit".to_string()
        } else {
            format!("{pct} %  (double-click to reset)")
        };
        let pos = rect.left_top() + egui::vec2(8.0, 8.0);
        painter.text(
            pos,
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::monospace(12.0),
            egui::Color32::from_rgba_premultiplied(220, 220, 220, 200),
        );
    }

    fn display_transform_active(&self) -> bool {
        #[cfg(target_os = "windows")]
        return self.display_transform && self.display_profile.available();
        #[cfg(not(target_os = "windows"))]
        self.display_transform
    }

    fn monitor_rgb(&self, preview: PreviewTextureData) -> PreviewTextureData {
        #[cfg(target_os = "windows")]
        if self.display_transform_active() {
            if let Some(rgba) = self.display_profile.convert_rgba(&preview.rgba) {
                return PreviewTextureData {
                    size: preview.size,
                    rgba,
                };
            }
        }
        preview
    }

    fn refresh_output_display(&mut self, ctx: &egui::Context) {
        if let Some(output) = self.output_image.as_ref() {
            let color = self
                .display_transform_active()
                .then(|| simulation_color(&self.params));
            let preview = self.monitor_rgb(make_preview_texture_data(output, color));
            self.viewer.replace_output(
                ctx,
                &mut self.output_tex,
                preview,
                &self.preview_interpolation,
            );
        }
    }

    /// Accept only a completed render for the current input generation.
    fn poll_render_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.render_job.as_mut() else {
            return;
        };
        let reply = match job.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => RenderReply {
                pipeline_cache: None,
                preview_source: None,
                result: Err("worker thread vanished".into()),
            },
        };
        let generation = job.generation;
        let mode = job.mode;
        if let Some(h) = job.handle.take() {
            let _ = h.join();
        }
        self.render_job = None;
        self.pipeline_cache = reply.pipeline_cache;
        if let Some(preview_source) = reply.preview_source.filter(|cache| {
            self.image
                .as_ref()
                .is_some_and(|original| cache.belongs_to(original))
        }) {
            self.preview_source = Some(preview_source);
        }
        if generation != self.render_generation {
            return;
        }
        match reply.result {
            Ok(r) => {
                self.last_input_clone_ms = r.input_clone_ms;
                self.last_pipeline_build_ms = r.pipeline_build_ms;
                self.last_render_ms = r.render_ms;
                self.last_preview_ms = r.preview_ms;
                self.last_worker_total_ms = r.worker_total_ms;
                let display_transform = self.display_transform_active();
                let preview = if r.display_transform == display_transform {
                    r.preview
                } else {
                    make_preview_texture_data(
                        &r.output,
                        display_transform.then(|| simulation_color(&self.params)),
                    )
                };
                let preview = self.monitor_rgb(preview);
                self.viewer.show_output(
                    ctx,
                    &mut self.output_tex,
                    preview,
                    &self.preview_interpolation,
                );
                self.status = format!(
                    "{} {} × {} ({:.1} MP)",
                    mode.label(),
                    r.output.width,
                    r.output.height,
                    r.output.pixel_count() as f64 / 1e6
                );
                self.output_image = Some(r.output);
            }
            Err(msg) => {
                eprintln!("[spektrafilm] render error: {msg}");
                self.status = format!("Render error: {msg}");
            }
        }
    }

    /// Save the rendered output for the current input and film selection.
    fn save_dialog(&mut self) {
        if self.output_image.is_none() {
            self.status = "Run Preview or Scan before saving.".into();
            return;
        }
        let default_name = match self.source.as_ref().map(|source| source.original_path()) {
            Some(p) => {
                let stem = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("spektrafilm");
                format!("{stem}_{}_spektra.png", self.film_name)
            }
            None => "spektrafilm.png".into(),
        };
        let Some(mut path) = self
            .preferences
            .dialog("save_output")
            .add_filter("Image", &["png", "tif", "tiff", "exr", "jpg", "jpeg"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return;
        };
        let t = Instant::now();
        if path.extension().is_none() {
            path.set_extension("png");
        }
        self.remember_dialog("save_output", &path);
        match self.save_to_path(&path) {
            Ok(()) => {
                self.status = format!(
                    "Saved {} in {:.0} ms",
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("(file)"),
                    t.elapsed().as_secs_f32() * 1000.0
                );
            }
            Err(e) => {
                self.status = format!("Save error: {e:#}");
            }
        }
    }

    fn save_to_path(&self, path: &Path) -> Result<()> {
        let rendered = self
            .output_image
            .as_ref()
            .context("run Preview or Scan before saving")?;
        let source = simulation_color(&self.params);
        let converted = (source != self.saving_color).then(|| {
            let mut image = rendered.clone();
            ColorTransform::new(source, self.saving_color).apply_image(&mut image);
            image
        });
        write_preview_output(
            path,
            converted.as_ref().unwrap_or(rendered),
            &RasterWriteOptions {
                bit_depth: self.bit_depth_for(path),
                color_space: Some(self.saving_color.color_space),
                cctf_encoding: self.saving_color.cctf_encoding,
                source_metadata: Some(&self.source_metadata),
            },
        )
    }

    fn bit_depth_for(&self, path: &Path) -> Option<u8> {
        self.output_depths.for_path(path)
    }

    /// Export the current frame by subprocessing the f64-built
    /// `spektrafilm` CLI. The GUI runs the pipeline at f32 on the GPU
    /// for fast iteration; export re-runs the pipeline at f64 (CPU)
    /// using the same params and writes the selected image format.
    ///
    /// The f64 CLI is located via, in order: `$SPEKTRAFILM_F64_CLI`,
    /// then `spektrafilm-f64` on `PATH`, then `target/release/spektrafilm-f64`
    /// relative to `CARGO_MANIFEST_DIR`. Build it with
    /// `cargo build --release --features precision-f64 -p spektrafilm-cli`
    /// and either rename / symlink it to `spektrafilm-f64` or set the env var.
    fn export_dialog(&mut self, ctx: &egui::Context) {
        let Some(source) = self.source.clone() else {
            self.status = "Load an image before exporting.".into();
            return;
        };
        let cli_path = match locate_f64_cli() {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Export: {e}");
                return;
            }
        };
        let stem = source
            .original_path()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("spektrafilm");
        let default_name = format!("{stem}_{}_spektra_f64.png", self.film_name);
        let Some(mut out_path) = self
            .preferences
            .dialog("export_output")
            .add_filter("Image", &["png", "tif", "tiff", "exr", "jpg", "jpeg"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return;
        };
        if out_path.extension().is_none() {
            out_path.set_extension("png");
        }
        self.remember_dialog("export_output", &out_path);
        self.start_f64_export(ctx, source, cli_path, out_path);
    }

    fn start_f64_export(
        &mut self,
        ctx: &egui::Context,
        source: Arc<source_snapshot::SourceSnapshot>,
        cli_path: PathBuf,
        out_path: PathBuf,
    ) {
        // Spawn the export on a worker thread so the egui event loop
        // keeps drawing. The cancel flag is shared with the worker so
        // the Cancel button (and `on_exit`) can kill the child cleanly
        // instead of letting it orphan after the GUI window closes.
        //
        // f64 CPU export uses the same spectral and grain settings as Scan.
        let film = self.film_name.clone();
        let paper = self.print_name.clone();
        let mut params = self.params.clone();
        params.settings.preview_mode = false;
        let bit_depth = self.bit_depth_for(&out_path);
        let developed_raw = if raw::is_raw(source.path()) {
            self.image.clone()
        } else {
            None
        };
        let rotation = self.source_rotation;
        let channel_orders = [self.film_channel_swap, self.print_channel_swap];
        let use_params_input_color = self.override_input_color;
        let saving_color = self.saving_color;
        let data_dir = self.data_dir.clone();
        let (tx, rx) = mpsc::channel();
        let ctx_for_worker = ctx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_worker = Arc::clone(&cancel);
        let started_at = Instant::now();
        let handle = std::thread::Builder::new()
            .name("spektrafilm-export".into())
            .spawn(move || {
                let res = run_f64_export(
                    &cli_path,
                    source.path(),
                    &out_path,
                    bit_depth,
                    &film,
                    &paper,
                    &params,
                    &data_dir,
                    ExportImageOptions {
                        use_params_input_color,
                        saving_color: Some(saving_color),
                        rotation: if developed_raw.is_some() { 0 } else { rotation },
                        developed_raw: developed_raw.as_deref(),
                        channel_orders: Some(channel_orders),
                    },
                    &cancel_for_worker,
                );
                let name = out_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("(file)")
                    .to_string();
                let msg = match res {
                    Ok(()) => Ok((started_at.elapsed().as_secs_f32(), name)),
                    Err(e) => Err(format!("{e:#}")),
                };
                let _ = tx.send(msg);
                ctx_for_worker.request_repaint();
            })
            .expect("OS thread spawn");
        self.status = "Exporting at f64 (CPU)…".into();
        self.export_job = Some(ExportJob {
            rx,
            handle: Some(handle),
            cancel,
            started_at,
        });
    }

    /// Request cancellation of the in-flight export. The worker thread
    /// observes the flag in its `try_wait` poll loop and SIGKILLs the
    /// child; `poll_export_job` then drains the resulting error message
    /// on the next `update()` tick.
    fn cancel_export(&mut self) {
        let Some(job) = self.export_job.as_ref() else {
            return;
        };
        if !job.cancel.swap(true, Ordering::SeqCst) {
            self.status = "Cancelling export…".into();
        }
    }

    /// Called once per `update()`. If the export is still in-flight,
    /// refreshes the status with elapsed time and requests a repaint a
    /// second from now (so the timer ticks without us busy-looping).
    /// If the job finished, drains the result into the status bar and
    /// joins the worker thread.
    fn poll_export_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.export_job.as_mut() else {
            return;
        };
        let msg = match job.rx.try_recv() {
            Ok(m) => m,
            Err(mpsc::TryRecvError::Empty) => {
                let secs = job.started_at.elapsed().as_secs();
                self.status = if job.cancel.load(Ordering::SeqCst) {
                    format!("Cancelling export… ({secs}s)")
                } else {
                    format!("Exporting at f64 (CPU)… ({secs}s)")
                };
                ctx.request_repaint_after(Duration::from_secs(1));
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status = "Export error: worker thread vanished".into();
                self.export_job = None;
                return;
            }
        };
        if let Some(h) = job.handle.take() {
            let _ = h.join();
        }
        self.export_job = None;
        self.status = match msg {
            Ok((secs, name)) => format!("Exported (f64 CPU) {name} in {secs:.1} s"),
            Err(e) if e.contains("cancelled") => "Export cancelled.".into(),
            Err(e) => format!("Export error: {e}"),
        };
    }

    fn controls_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("spektrafilm");
        ui.add_space(6.0);

        // ── File ────────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            if ui.button("Open…").clicked() {
                if let Some(path) = self.preferences.dialog("open_input")
                    .add_filter(
                        "Image",
                        &[
                            "png", "tif", "tiff", "exr", "jpg", "jpeg", // standard
                            "dng", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf",
                            "rw2", "pef", "srw", "x3f", "iiq", "3fr", "crw", "rwl", "mrw", "mef",
                            "kdc",
                        ],
                    )
                    .pick_file()
                {
                    self.remember_dialog("open_input", &path);
                    self.load_image_from_path(&path);
                }
            }
            let save_enabled = self.output_image.is_some();
            if ui
                .add_enabled(save_enabled, egui::Button::new("Save…"))
                .on_disabled_hover_text("Run Preview or Scan before saving")
                .clicked()
            {
                self.save_dialog();
            }
            let export_busy = self.export_job.is_some();
            if export_busy {
                if ui
                    .button("Cancel")
                    .on_hover_text("Stop the in-flight f64 CPU export and kill the child process.")
                    .clicked()
                {
                    self.cancel_export();
                }
            } else {
                let export_enabled = self.source.is_some();
                if ui
                    .add_enabled(export_enabled, egui::Button::new("Export…"))
                    .on_hover_text(
                        "Render the opened image at the selected scan scale with the f64 CPU pipeline.",
                    )
                    .on_disabled_hover_text("Load an image first")
                    .clicked()
                {
                    self.export_dialog(ctx);
                }
            }
        });
        self.preset_menu(ui);
        ui.horizontal(|ui| {
            let loaded = self.image.is_some();
            if ui
                .add_enabled(loaded, egui::Button::new("Preview"))
                .on_hover_text("Render within the preview size limit.")
                .clicked()
            {
                self.request_render_mode(RenderMode::Preview);
            }
            if ui
                .add_enabled(loaded, egui::Button::new("Scan"))
                .on_hover_text("Render with full effects at the selected scan scale.")
                .clicked()
            {
                self.request_render_mode(RenderMode::Scan);
            }
            let mut auto_preview = self.auto_preview;
            if ui.checkbox(&mut auto_preview, "Auto preview").changed() {
                self.set_auto_preview(auto_preview);
            }
        });
        if let Some(source) = &self.source {
            ui.label(
                egui::RichText::new(
                    source
                        .original_path()
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(""),
                )
                .small(),
            );
        }
        ui.horizontal(|ui| {
            let loaded = self.image.is_some();
            if ui
                .add_enabled(loaded, egui::Button::new("Rotate left"))
                .clicked()
            {
                self.rotate_input(3);
            }
            if ui
                .add_enabled(loaded, egui::Button::new("Rotate right"))
                .clicked()
            {
                self.rotate_input(1);
            }
        });
        let loaded_raw = self.source_is_raw();
        let redevelop = egui::CollapsingHeader::new("RAW development")
            .default_open(loaded_raw)
            .show(ui, |ui| self.raw_develop.controls(ui, loaded_raw))
            .body_returned
            .unwrap_or(false);
        if redevelop {
            self.redevelop_raw();
        }
        ui.add_space(4.0);

        // ── Profiles ────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Profiles")
            .default_open(true)
            .show(ui, |ui| {
                let film_changed =
                    profile_combo(ui, "film", "Film stock", &self.films, &mut self.film_name);
                if film_changed {
                    self.refresh_film_selection();
                }
                // B&W stocks are profiled at several development times —
                // pick one (longer = more contrast). Hidden for the
                // single-time / colour case.
                if self.film_dev_times.len() > 1
                    && dev_time_combo(
                        ui,
                        "film_dev_time",
                        "Development time",
                        &self.film_dev_times.clone(),
                        &mut self.params.film_render.development_time,
                    )
                {
                    self.request_render();
                }
                // Slide films are scanned directly, so the print paper is
                // unused — disable the picker and say why rather than
                // letting it silently affect nothing.
                if self.params.io.scan_film {
                    ui.label(
                        egui::RichText::new("Slide film — scanned directly (no print paper).")
                            .italics()
                            .small(),
                    );
                }
                let paper_changed = ui
                    .add_enabled_ui(!self.params.io.scan_film, |ui| {
                        profile_combo(
                            ui,
                            "paper",
                            "Print paper",
                            &self.papers,
                            &mut self.print_name,
                        )
                    })
                    .inner;
                if paper_changed {
                    self.print_dev_times = profile_dev_times(&self.data_dir, &self.print_name);
                    self.params.print_render.development_time = None;
                    self.request_render();
                }
                if !self.params.io.scan_film
                    && self.print_dev_times.len() > 1
                    && dev_time_combo(
                        ui,
                        "print_dev_time",
                        "Print development time",
                        &self.print_dev_times.clone(),
                        &mut self.params.print_render.development_time,
                    )
                {
                    self.request_render();
                }
            });

        // ── Exposure ────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Exposure")
            .default_open(true)
            .show(ui, |ui| {
                let mut changed = false;
                changed |= ui
                    .checkbox(&mut self.params.camera.auto_exposure, "Auto exposure")
                    .changed();
                if self.params.camera.auto_exposure {
                    let method = &mut self.params.camera.auto_exposure_method;
                    egui::ComboBox::from_label("Metering")
                        .selected_text(method.clone())
                        .show_ui(ui, |ui| {
                            for opt in [
                                "average",
                                "median",
                                "center_weighted",
                                "partial",
                                "matrix",
                                "multi_zone",
                                "highlight_weighted",
                            ] {
                                changed |=
                                    ui.selectable_value(method, opt.to_string(), opt).changed();
                            }
                        });
                }
                changed |= ui
                    .add(
                        egui::Slider::new(
                            &mut self.params.camera.exposure_compensation_ev,
                            -5.0..=5.0,
                        )
                        .text("EV compensation")
                        .step_by(0.1),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.params.camera.film_format_mm, 4.0..=120.0)
                            .text("Film format (mm)")
                            .logarithmic(true),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.params.camera.lens_blur_um, 0.0..=100.0)
                            .text("Lens blur (µm)"),
                    )
                    .changed();
                if changed {
                    self.request_render();
                }
            });

        // ── Halation ────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Halation")
            .default_open(true)
            .show(ui, |ui| {
                let h = &mut self.params.film_render.halation;
                let mut changed = false;
                changed |= ui.checkbox(&mut h.active, "Active").changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut h.halation_amount, 0.0..=3.0)
                            .text("Halation amount"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut h.halation_spatial_scale, 0.1..=5.0)
                            .text("Halation scale"),
                    )
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut h.scatter_amount, 0.0..=3.0).text("Scatter amount"))
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut h.scatter_spatial_scale, 0.1..=5.0)
                            .text("Scatter scale"),
                    )
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut h.halation_n_bounces, 1..=5).text("Bounces"))
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut h.halation_bounce_decay, 0.0..=1.0)
                            .text("Bounce decay"),
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut h.halation_renormalize, "Renormalize")
                    .changed();
                if changed {
                    self.request_render();
                }
            });

        // ── DIR couplers ────────────────────────────────────────────────
        egui::CollapsingHeader::new("DIR couplers")
            .default_open(true)
            .show(ui, |ui| {
                let d = &mut self.params.film_render.dir_couplers;
                let mut changed = false;
                changed |= ui.checkbox(&mut d.active, "Active").changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.amount, 0.0..=2.0).text("Amount"))
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut d.diffusion_size_um, 0.0..=100.0)
                            .text("Diffusion size (µm)"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut d.diffusion_tail_um, 0.0..=400.0)
                            .text("Diffusion tail (µm)"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut d.diffusion_tail_weight, 0.0..=1.0)
                            .text("Tail weight"),
                    )
                    .changed();
                if changed {
                    self.request_render();
                }
            });

        // ── Diffusion filter (lens) ─────────────────────────────────────
        egui::CollapsingHeader::new("Diffusion filter (lens)")
            .default_open(true)
            .show(ui, |ui| {
                let df = &mut self.params.camera.diffusion_filter;
                let mut changed = false;
                changed |= ui.checkbox(&mut df.active, "Active").changed();
                egui::ComboBox::from_label("Family")
                    .selected_text(df.filter_family.clone())
                    .show_ui(ui, |ui| {
                        for fam in ["black_pro_mist", "glimmerglass", "pro_mist", "cinebloom"] {
                            changed |= ui
                                .selectable_value(&mut df.filter_family, fam.to_string(), fam)
                                .changed();
                        }
                    });
                changed |= ui
                    .add(egui::Slider::new(&mut df.strength, 0.0..=2.0).text("Strength"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut df.spatial_scale, 0.1..=3.0).text("Spatial scale"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut df.halo_warmth, -1.5..=1.5).text("Halo warmth"))
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut df.core_intensity, 0.0..=2.0).text("Core intensity"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut df.halo_intensity, 0.0..=2.0).text("Halo intensity"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut df.bloom_intensity, 0.0..=2.0)
                            .text("Bloom intensity"),
                    )
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut df.halo_size, 0.1..=3.0).text("Halo size"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut df.bloom_size, 0.1..=3.0).text("Bloom size"))
                    .changed();
                if changed {
                    self.request_render();
                }
            });

        // ── Grain ───────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Grain")
            .default_open(true)
            .show(ui, |ui| {
                let g = &mut self.params.film_render.grain;
                let mut changed = false;
                changed |= ui.checkbox(&mut g.active, "Active").changed();
                changed |= ui
                    .checkbox(&mut g.sublayers_active, "Model emulsion sublayers")
                    .on_hover_text("Used by the CPU grain model in full-resolution exports.")
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut g.agx_particle_area_um2, 0.05..=1.0)
                            .text("Particle area (µm²)"),
                    )
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut g.blur, 0.0..=3.0).text("Post-blur σ"))
                    .changed();
                changed |=
                    nonnegative_controls(ui, "RGB particle scale", &mut g.agx_particle_scale);
                changed |= nonnegative_controls(
                    ui,
                    "Sublayer particle scale",
                    &mut g.agx_particle_scale_layers,
                );
                changed |= nonnegative_controls(ui, "Minimum grain density", &mut g.density_min);
                changed |= nonnegative_controls(ui, "Grain uniformity", &mut g.uniformity);
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut g.blur_dye_clouds_um)
                            .speed(0.1)
                            .range(0.0..=f32::INFINITY)
                            .suffix(" µm")
                            .prefix("Dye cloud blur "),
                    )
                    .changed();
                changed |= nonnegative_controls(
                    ui,
                    "Microstructure blur / clump size",
                    &mut g.micro_structure,
                );
                if !g.sublayers_active {
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut g.n_sub_layers, 1..=4)
                                .text("Aggregate grain sublayers"),
                        )
                        .changed();
                }
                if changed {
                    self.request_render();
                }
            });

        // ── Glare ───────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Glare")
            .default_open(false)
            .show(ui, |ui| {
                let g = &mut self.params.print_render.glare;
                let mut changed = false;
                changed |= ui.checkbox(&mut g.active, "Active").changed();
                changed |= ui
                    .add(egui::Slider::new(&mut g.percent, 0.0..=0.2).text("Percent"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut g.roughness, 0.0..=2.0).text("Roughness"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut g.blur, 0.0..=5.0).text("Blur σ (px)"))
                    .changed();
                if changed {
                    self.request_render();
                }
            });

        // ── Print curves (s023 morph) ───────────────────────────────────
        egui::CollapsingHeader::new("Print curves")
            .default_open(false)
            .show(ui, |ui| {
                let m = &mut self.params.print_render.density_curves_morph;
                let mut changed = false;
                changed |= ui
                    .checkbox(&mut m.active, "Morph density curves")
                    .on_hover_text(
                        "Rebuild the print density curves from the profile's parametric \
                         model with coupled-gamma morphing. Off = use the stored curves.",
                    )
                    .changed();
                if m.active {
                    changed |= ui
                        .add(egui::Slider::new(&mut m.gamma_factor, 0.5..=2.0).text("Gamma"))
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut m.gamma_factor_fast, 0.5..=2.0)
                                .text("Gamma fast"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut m.gamma_factor_slow, 0.5..=2.0)
                                .text("Gamma slow"),
                        )
                        .changed();
                    changed |= ui
                        .add(egui::Slider::new(&mut m.gamma_factor_red, 0.5..=2.0).text("Gamma R"))
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut m.gamma_factor_green, 0.5..=2.0).text("Gamma G"),
                        )
                        .changed();
                    changed |= ui
                        .add(egui::Slider::new(&mut m.gamma_factor_blue, 0.5..=2.0).text("Gamma B"))
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut m.developer_exhaustion, 0.0..=1.0)
                                .text("Developer exhaustion"),
                        )
                        .changed();
                }
                if changed {
                    self.request_render();
                }
            });

        // ── Scanner ─────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Scanner")
            .default_open(false)
            .show(ui, |ui| {
                let s = &mut self.params.scanner;
                let mut changed = false;
                changed |= ui
                    .add(egui::Slider::new(&mut s.lens_blur, 0.0..=5.0).text("Lens blur σ (px)"))
                    .changed();
                let [mut sigma, mut amount] = s.unsharp_mask;
                changed |= ui
                    .add(egui::Slider::new(&mut sigma, 0.0..=3.0).text("Unsharp σ (px)"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut amount, 0.0..=2.0).text("Unsharp amount"))
                    .changed();
                if changed {
                    s.unsharp_mask = [sigma, amount];
                }
                changed |= ui
                    .checkbox(&mut s.white_correction, "White correction")
                    .changed();
                if s.white_correction {
                    changed |= ui
                        .add(egui::Slider::new(&mut s.white_level, 0.5..=1.0).text("White level"))
                        .changed();
                }
                changed |= ui
                    .checkbox(&mut s.black_correction, "Black correction")
                    .changed();
                if s.black_correction {
                    changed |= ui
                        .add(egui::Slider::new(&mut s.black_level, 0.0..=0.5).text("Black level"))
                        .changed();
                }
                if changed {
                    self.request_render();
                }
            });

        egui::CollapsingHeader::new("Dye-density channels").show(ui, |ui| {
            let film_changed =
                channel_order_control(ui, "Film C/M/Y order", &mut self.film_channel_swap);
            let print_changed = ui
                .add_enabled_ui(!self.params.io.scan_film, |ui| {
                    channel_order_control(ui, "Print C/M/Y order", &mut self.print_channel_swap)
                })
                .inner;
            if film_changed || print_changed {
                self.request_render();
            }
        });

        // ── Color management ────────────────────────────────────────────
        egui::CollapsingHeader::new("Color management")
            .default_open(false)
            .show(ui, |ui| {
                let tag = self.detected_input_color;
                ui.label(match tag {
                    Some(color) if color.cctf_encoding => {
                        format!("Detected input: {} (encoded)", color.color_space)
                    }
                    Some(color) => format!("Detected input: {} (linear)", color.color_space),
                    None => "Untagged input: assume linear ProPhoto RGB".to_string(),
                });
                let changed_input_mode = ui
                    .checkbox(&mut self.override_input_color, "Override input color space")
                    .on_hover_text("Interpret raster samples in this space, or convert developed RAW to this space.")
                    .changed();
                let changed_input_color = self.override_input_color
                    && color_control(ui, "Input color space", &mut self.manual_input_color);
                if changed_input_mode || changed_input_color {
                    self.sync_input_color();
                    self.request_render();
                }
                ui.separator();
                let mut simulation = simulation_color(&self.params);
                let changed_simulation =
                    color_control(ui, "Simulation output color space", &mut simulation);
                if changed_simulation {
                    self.params.io.output_color_space = simulation.color_space.into();
                    self.params.io.output_cctf_encoding = simulation.cctf_encoding;
                }
                let algo = &mut self.params.io.output_gamut_compress.algorithm;
                let mut changed = changed_simulation;
                egui::ComboBox::from_label("Output gamut compression")
                    .selected_text(algo.clone())
                    .show_ui(ui, |ui| {
                        for opt in ["off", "oklch", "oklrab", "cam16ucs", "jzazbz", "aces_rgc"] {
                            changed |= ui.selectable_value(algo, opt.to_string(), opt).changed();
                        }
                    });
                // Input gamut compression — baked into the tc_lut at build time,
                // so changing it rebuilds the LUT on the next pass. "xy" is the
                // ACES-RGC-style radial compression toward the spectral locus.
                let in_algo = &mut self.params.io.input_gamut_compress.algorithm;
                egui::ComboBox::from_label("Input gamut compression")
                    .selected_text(in_algo.clone())
                    .show_ui(ui, |ui| {
                        for opt in ["off", "xy", "oklch"] {
                            changed |= ui.selectable_value(in_algo, opt.to_string(), opt).changed();
                        }
                    });
                changed |= ui.checkbox(&mut self.params.io.input_gamut_compress.active, "Enable input gamut compression").changed();
                changed |= nonnegative_controls(ui, "Input gamut knee", &mut self.params.io.input_gamut_compress.knee);
                changed |= nonnegative_controls(ui, "Output gamut knee", &mut self.params.io.output_gamut_compress.knee);
                // CAT16 (vs CAT02) for the input chromatic adaptation feeding
                // Hanatos — better blue/violet behavior; matches upstream >=0.3.3.
                changed |= ui
                    .checkbox(
                        &mut self.params.settings.use_cat16,
                        "CAT16 input adaptation",
                    )
                    .changed();
                // RGB → film raw spectral upsampler. hanatos2025 is the default
                // spectral LUT; arctic2026alpha02 is the new memory-color
                // reflectance LUT; mallett2019 is a faster matrix basis.
                let method = &mut self.params.settings.rgb_to_raw_method;
                egui::ComboBox::from_label("RGB→raw upsampling")
                    .selected_text(method.clone())
                    .show_ui(ui, |ui| {
                        for opt in ["hanatos2025", "arctic2026alpha02", "mallett2019"] {
                            changed |= ui.selectable_value(method, opt.to_string(), opt).changed();
                        }
                    });
                if method == "hanatos2025" {
                    let settings = &mut self.params.settings;
                    changed |= ui
                        .checkbox(
                            &mut settings.apply_hanatos2025_adaptation_window,
                            "Hanatos adaptation window",
                        )
                        .changed();
                    changed |= ui
                        .add_enabled(
                            self.hanatos_surface_available,
                            egui::Checkbox::new(
                                &mut settings.apply_hanatos2025_adaptation_surface,
                                "Hanatos adaptation surface",
                            ),
                        )
                        .on_disabled_hover_text("This film profile has no adaptation surface.")
                        .changed();
                    ui.horizontal(|ui| {
                        ui.label("Spectral blur");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut settings.spectral_gaussian_blur)
                                    .speed(0.1)
                                    .range(0.0..=spektrafilm_math::spectral::N_WAVELENGTHS as f64),
                            )
                            .on_hover_text("Gaussian sigma in wavelength sample intervals.")
                            .changed();
                    });
                }
                if changed {
                    self.request_render();
                }
            });

        // ── Enlarger ────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Enlarger")
            .default_open(false)
            .show(ui, |ui| {
                let e = &mut self.params.enlarger;
                let mut changed = false;
                egui::ComboBox::from_label("Print illuminant")
                    .selected_text(&e.illuminant)
                    .show_ui(ui, |ui| {
                        for name in ["TH-KG3", "TH-KG3-L", "D50", "D55", "D65"] {
                            changed |= ui
                                .selectable_value(&mut e.illuminant, name.into(), name)
                                .changed();
                        }
                    });
                changed |= ui
                    .add(
                        egui::Slider::new(&mut e.print_exposure, 0.1..=5.0)
                            .text("Print exposure")
                            .logarithmic(true),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut e.m_filter_shift, -50.0..=50.0)
                            .text("Magenta filter shift"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut e.y_filter_shift, -50.0..=50.0)
                            .text("Yellow filter shift"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut e.preflash_exposure, 0.0..=0.5)
                            .text("Preflash exposure"),
                    )
                    .on_hover_text(
                        "Uniform low pre-exposure of the print through the film base. \
                         Lifts shadow density and lowers print contrast. 0 = off.",
                    )
                    .changed();
                if e.preflash_exposure > 0.0 {
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut e.preflash_m_filter_shift, -50.0..=50.0)
                                .text("Preflash magenta shift"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut e.preflash_y_filter_shift, -50.0..=50.0)
                                .text("Preflash yellow shift"),
                        )
                        .changed();
                }
                if changed {
                    self.request_render();
                }
            });

        // ── Output ──────────────────────────────────────────────────────
        egui::CollapsingHeader::new("Output")
            .default_open(false)
            .show(ui, |ui| {
                depth_control(
                    ui,
                    "TIFF bit depth",
                    &mut self.output_depths.tiff,
                    &[8, 16, 32],
                );
                depth_control(ui, "EXR bit depth", &mut self.output_depths.exr, &[16, 32]);
                depth_control(ui, "PNG bit depth", &mut self.output_depths.png, &[8, 16]);
                let io = &mut self.params.io;
                let mut changed = false;
                let mut scan_film = io.scan_film;
                changed |= ui
                    .checkbox(&mut scan_film, "Scan film (skip printing)")
                    .on_hover_text(
                        "Scan the developed film directly instead of printing onto paper. \
                         Auto-enabled for positive/slide stocks (no print paper); toggle \
                         manually to scan a negative as-is.",
                    )
                    .changed();
                if changed {
                    io.scan_film = scan_film;
                }
                changed |= ui.checkbox(&mut io.crop, "Crop").changed();
                if io.crop {
                    for (index, axis) in ["X", "Y"].into_iter().enumerate() {
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut io.crop_center[index], 0.0..=1.0)
                                    .text(format!("Crop center {axis}")),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut io.crop_size[index], 0.01..=1.0)
                                    .text(format!("Crop size {axis}")),
                            )
                            .on_hover_text("Fraction of the original image's long edge")
                            .changed();
                    }
                }
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.params.settings.preview_max_size, 128..=4096)
                            .logarithmic(true)
                            .text("Preview max size"),
                    )
                    .on_hover_text("Preview long-edge limit in pixels.")
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut io.upscale_factor, 0.1..=1.0)
                            .text("Scan / export scale"),
                    )
                    .on_hover_text("1.0 retains the full resolution of the cropped image.")
                    .changed();
                color_control(ui, "Saving color space", &mut self.saving_color);
                if changed {
                    self.request_render();
                }
            });

        egui::CollapsingHeader::new("Preview display").show(ui, |ui| {
            if ui
                .checkbox(&mut self.display_transform, "Convert colors for display")
                .changed()
            {
                self.refresh_output_display(ctx);
            }
            #[cfg(target_os = "windows")]
            if self.display_transform && !self.display_profile.available() {
                ui.label("This monitor has no usable ICC profile.");
            }
            let mut interpolation_changed = false;
            egui::ComboBox::from_label("Output interpolation")
                .selected_text(&self.preview_interpolation)
                .show_ui(ui, |ui| {
                    for interpolation in spatial_filter::OUTPUT_INTERPOLATIONS {
                        interpolation_changed |= ui
                            .selectable_value(
                                &mut self.preview_interpolation,
                                interpolation.into(),
                                interpolation,
                            )
                            .changed();
                    }
                });
            if interpolation_changed {
                self.refresh_output_display(ctx);
            }
            ui.checkbox(&mut self.gray_canvas, "18% gray canvas");
            ui.add(egui::Slider::new(&mut self.white_border, 0.0..=1.0).text("White border"))
                .on_hover_text("Displayed border width as a fraction of the image long edge.");
        });

        // ── Metrics ─────────────────────────────────────────────────────
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.monospace(format!(
            "render:        {:>6.1} ms   {}",
            self.last_render_ms,
            fps_label(self.last_render_ms)
        ));
        ui.monospace(format!(
            "pipeline build:{:>6.1} ms",
            self.last_pipeline_build_ms
        ));
        ui.monospace(format!(
            "input handle:  {:>6.1} ms",
            self.last_input_clone_ms
        ));
        ui.monospace(format!("preview pack:  {:>6.1} ms", self.last_preview_ms));
        ui.monospace(format!(
            "worker total:  {:>6.1} ms",
            self.last_worker_total_ms
        ));
        ui.monospace(format!("backend: {}", self.backend.name()));
        ui.label(egui::RichText::new(&self.status).small());
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // First-paint hook: tag the wgpu Metal layer's colorspace as sRGB.
        // Has to happen here (not in `App::new`) because the CAMetalLayer
        // isn't wired up at construction time.
        #[cfg(target_os = "macos")]
        if !self.metal_colorspace_tagged {
            match tag_metal_layer_srgb(frame) {
                Ok(()) => {
                    eprintln!("[spektrafilm] CAMetalLayer.colorspace = sRGB — tagged OK");
                    self.metal_colorspace_tagged = true;
                }
                Err(e) => {
                    eprintln!("[spektrafilm] colorspace tag attempt: {e}");
                }
            }
        }
        #[cfg(target_os = "windows")]
        if self.display_profile.refresh(ctx, frame) && self.display_transform {
            self.refresh_output_display(ctx);
        }
        let _ = frame;

        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|file| file.path.clone())
        });
        if let Some(path) = dropped {
            self.load_image_from_path(&path);
        }
        self.poll_render_job(ctx);
        self.poll_export_job(ctx);
        egui::SidePanel::right("controls")
            .resizable(false)
            .exact_width(340.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls_panel(ui, ctx));
            });
        if self.render_job.is_some() {
            ctx.request_repaint_after(IN_FLIGHT_REPAINT);
        } else if let Some(request) = self.render_request {
            let delay = request.ready_at.saturating_duration_since(Instant::now());
            if delay.is_zero() {
                self.render_request = None;
                self.dispatch_render(ctx, request.mode);
            } else {
                ctx.request_repaint_after(delay);
            }
        }

        self.viewer
            .advance_output(ctx, &mut self.output_tex, &self.preview_interpolation);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.viewer
                .layers(ui, self.image.is_some(), self.output_tex.is_some());
            let tex = match self.viewer.layer {
                Layer::PaperBack => Some(self.viewer.paper_texture(ctx, self.image.as_deref())),
                Layer::Input => self.image.as_ref().cloned().and_then(|image| {
                    let color = RasterColor::new(
                        &self.params.io.input_color_space,
                        self.params.io.input_cctf_decoding,
                    )
                    .unwrap_or(UNTAGGED_INPUT);
                    let convert_raw = (self.override_input_color && self.source_is_raw())
                        .then_some(self.manual_input_color);
                    self.viewer.input_texture(
                        ctx,
                        InputLayerRequest {
                            image,
                            color,
                            convert_raw,
                            cap: self.params.settings.preview_max_size,
                        },
                    )
                }),
                Layer::Output => self.output_tex.clone(),
            };
            if let Some(tex) = tex {
                self.draw_preview_with_zoom(ui, &tex);
            } else {
                ui.centered_and_justified(|ui| {
                    let message = if self.image.is_some() {
                        &self.status
                    } else {
                        "Drop or open an image to start."
                    };
                    ui.label(egui::RichText::new(message).size(18.0));
                });
            }
        });
    }

    /// Called once when the window is closing. If an export is still
    /// in-flight, set the cancel flag and join the worker thread so
    /// the child process is SIGKILLed before the GUI exits. Without
    /// this the f64 CLI orphans into its own process group and keeps
    /// hammering the CPU long after the window is gone.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let Some(job) = self.export_job.take() else {
            return;
        };
        job.cancel.store(true, Ordering::SeqCst);
        if let Some(h) = job.handle {
            let _ = h.join();
        }
    }
}

/// Extract the human-readable payload from a caught panic. Panics carry
/// either a `&str` (literal messages) or a `String` (formatted ones);
/// anything else gets a generic label.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        format!("panic: {s}")
    } else if let Some(s) = panic.downcast_ref::<String>() {
        format!("panic: {s}")
    } else {
        "panic (no message)".to_string()
    }
}

/// Find the data directory. Probes, in order:
///   1. `$SPEKTRAFILM_DATA_DIR` — explicit override.
///   2. `<exe>/../Resources/data` — macOS `.app` bundle layout
///      (`Foo.app/Contents/MacOS/<exe>` → `Contents/Resources/data`).
///   3. `<exe>/data` — data shipped next to the binary.
///   4. `./data` — current working directory.
///   5. `<CARGO_MANIFEST_DIR>/../../data` — workspace root, for
///      `cargo run -p spektrafilm-gui`.
///
/// Finder launches apps with `cwd=/`, so the exe-relative probes (2–3)
/// matter for a double-clicked `.app`. Falls back to `./data` so the
/// downstream loader surfaces a clear "profiles not found" error.
fn pick_data_dir() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("SPEKTRAFILM_DATA_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Some(dir) = spektrafilm_core::data::bundled_data_dir() {
        candidates.push(dir);
    }
    candidates.push(PathBuf::from("data"));
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("..").join("..").join("data"));
    }
    candidates
        .into_iter()
        .find(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("data"))
}

/// Scan `<data_dir>/profiles/*.json`, parse each profile's `info`, and
/// bucket the results by `info.support`. Films go into the first vec,
/// papers (and any other print-stage supports) into the second.
/// Each entry carries the filename stem (the unique loader key) plus a
/// human-readable display label.
fn scan_profiles(data_dir: &Path) -> (Vec<ProfileEntry>, Vec<ProfileEntry>) {
    let mut films = Vec::new();
    let mut papers = Vec::new();
    let dir = data_dir.join("profiles");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (films, papers);
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_lossy = name.to_string_lossy().to_string();
        let Some(stem) = name_lossy.strip_suffix(".json") else {
            continue;
        };
        let stock = stem.to_string();
        // Cheap probe: just deserialize the file's `info` field. We
        // could skip the rest of the profile but `Profile` already does
        // the right thing — and we pay this once at startup.
        let (display, is_paper) = match profile::load_profile_by_name(data_dir, &stock) {
            Ok(p) => {
                let display = p.info.name.clone().unwrap_or_else(|| stock.clone());
                let is_paper = p.info.support == "paper" || p.info.stage == "printing";
                (display, is_paper)
            }
            Err(_) => (stock.clone(), false),
        };
        let entry = ProfileEntry { stock, display };
        if is_paper {
            papers.push(entry);
        } else {
            films.push(entry);
        }
    }
    films.sort_by(|a, b| a.display.cmp(&b.display));
    papers.sort_by(|a, b| a.display.cmp(&b.display));
    (films, papers)
}

/// Combo box that picks one of `entries` by its `stock` id (the
/// underlying file stem) while showing `display` (the human-readable
/// name) as the label. Falls back to showing the raw stock id if no
/// entry with the current `selected_stock` exists.
/// Development-time family of a profile (empty for colour stocks or on
/// load failure — the picker hides itself for ≤1 entries either way).
fn profile_dev_times(data_dir: &Path, stock: &str) -> Vec<f64> {
    profile::load_profile_by_name(data_dir, stock)
        .map(|p| p.data.development_time)
        .unwrap_or_default()
}

/// Development-time picker for a B&W development-time family. `selection`
/// of `None` means the profile's default (the floor-middle entry, matching
/// upstream's `select_development_time`). Returns true when changed.
fn dev_time_combo(
    ui: &mut egui::Ui,
    salt: &str,
    label: &str,
    times: &[f64],
    selection: &mut Option<f64>,
) -> bool {
    // Same resolution as the render path, so the combo always highlights
    // exactly the entry the pipeline will use.
    let current_idx = profile::development_time_index(times, *selection);
    let mut changed = false;
    ui.label(label);
    egui::ComboBox::from_id_salt(salt)
        .selected_text(format!("{} min", times[current_idx]))
        .width(ui.available_width().min(280.0))
        .show_ui(ui, |ui| {
            for (i, t) in times.iter().enumerate() {
                if ui
                    .selectable_label(i == current_idx, format!("{t} min"))
                    .clicked()
                    && i != current_idx
                {
                    *selection = Some(*t);
                    changed = true;
                }
            }
        });
    changed
}

fn profile_combo(
    ui: &mut egui::Ui,
    salt: &str,
    label: &str,
    entries: &[ProfileEntry],
    selected_stock: &mut String,
) -> bool {
    ui.label(label);
    let display = entries
        .iter()
        .find(|e| &e.stock == selected_stock)
        .map(|e| e.display.clone())
        .unwrap_or_else(|| selected_stock.clone());
    let prev = selected_stock.clone();
    egui::ComboBox::from_id_salt(salt)
        .selected_text(&display)
        .width(ui.available_width().min(280.0))
        .show_ui(ui, |ui| {
            for entry in entries {
                ui.selectable_value(selected_stock, entry.stock.clone(), &entry.display);
            }
        });
    prev != *selected_stock
}

fn fps_label(ms: f32) -> &'static str {
    if ms < 16.0 {
        "60 fps"
    } else if ms < 33.0 {
        "30 fps"
    } else if ms < 67.0 {
        "15 fps"
    } else if ms < 200.0 {
        "5 fps"
    } else if ms < 400.0 {
        "2 fps"
    } else {
        ""
    }
}

fn render_params(mut params: RuntimeParams, mode: RenderMode) -> RuntimeParams {
    params.settings.preview_mode = mode == RenderMode::Preview;
    if mode == RenderMode::Preview {
        // Preview source has already been resized before metering and cropping.
        // A scan scale change must only affect Scan and Export.
        params.io.upscale_factor = 1.0;

        // Match the Python GUI's preview-mode effect policy.
        params.camera.lens_blur_um = 0.0;
        params.enlarger.lens_blur = 0.0;
        params.film_render.dir_couplers.diffusion_size_um = 0.0;
        params.film_render.dir_couplers.diffusion_tail_um = 0.0;
        params.film_render.grain.active = false;
        params.print_render.glare.blur = 0.0;
        params.scanner.lens_blur = 0.0;
        params.scanner.unsharp_mask = [0.0, 0.0];
    }
    params
}

fn render_pipeline(
    cache: &mut Option<PipelineCache>,
    data_dir: &Path,
    film_name: &str,
    print_name: &str,
    channel_orders: [[usize; 3]; 2],
    params: RuntimeParams,
) -> Result<Pipeline, String> {
    let key = render_pipeline_cache_key(film_name, print_name, channel_orders, &params);
    if let Some(cached) = cache.as_ref().filter(|cached| cached.key == key) {
        return Ok(cached.pipeline.clone().with_params(params));
    }

    let mut film = profile::load_profile_by_name(data_dir, film_name)
        .map_err(|e| format!("film profile '{film_name}': {e}"))?;
    profile::reorder_channel_density(&mut film, channel_orders[0])
        .map_err(|e| format!("film profile '{film_name}': {e}"))?;
    let effective_print_name = if params.io.scan_film {
        film_name
    } else {
        print_name
    };
    let mut print = profile::load_profile_by_name(data_dir, effective_print_name)
        .map_err(|e| format!("print profile '{effective_print_name}': {e}"))?;
    if !params.io.scan_film {
        profile::reorder_channel_density(&mut print, channel_orders[1])
            .map_err(|e| format!("print profile '{effective_print_name}': {e}"))?;
    }
    let pipeline = Pipeline::new_with_spectral(film, print, params, data_dir)
        .map_err(|e| format!("pipeline build: {e}"))?;
    *cache = Some(PipelineCache {
        key,
        pipeline: pipeline.clone(),
    });
    Ok(pipeline)
}

fn render_pipeline_cache_key(
    film_name: &str,
    print_name: &str,
    channel_orders: [[usize; 3]; 2],
    params: &RuntimeParams,
) -> String {
    serde_json::json!({
        "channel_orders": channel_orders,
        "film": film_name,
        "print": print_name,
        "camera_filter_uv": params.camera.filter_uv,
        "camera_filter_ir": params.camera.filter_ir,
        "film_dev": params.film_render.development_time,
        "print_dev": params.print_render.development_time,
        "scan_film": params.io.scan_film,
        "input_color_space": params.io.input_color_space,
        "input_cctf_decoding": params.io.input_cctf_decoding,
        "input_gamut": params.io.input_gamut_compress,
        "output_color_space": params.io.output_color_space,
        "output_gamut": params.io.output_gamut_compress,
        "settings": {
            "rgb_to_raw_method": params.settings.rgb_to_raw_method,
            "apply_hanatos2025_adaptation_window": params.settings.apply_hanatos2025_adaptation_window,
            "apply_hanatos2025_adaptation_surface": params.settings.apply_hanatos2025_adaptation_surface,
            "spectral_gaussian_blur": params.settings.spectral_gaussian_blur,
            "lut_resolution": params.settings.lut_resolution,
            "neutral_print_filters_from_database": params.settings.neutral_print_filters_from_database,
            "use_cat16": params.settings.use_cat16,
        },
        "enlarger": {
            "illuminant": params.enlarger.illuminant,
            "c_filter_neutral": params.enlarger.c_filter_neutral,
            "m_filter_neutral": params.enlarger.m_filter_neutral,
            "y_filter_neutral": params.enlarger.y_filter_neutral,
            "m_filter_shift": params.enlarger.m_filter_shift,
            "y_filter_shift": params.enlarger.y_filter_shift,
            "preflash_exposure": params.enlarger.preflash_exposure,
            "preflash_m_filter_shift": params.enlarger.preflash_m_filter_shift,
            "preflash_y_filter_shift": params.enlarger.preflash_y_filter_shift,
            "normalize_print_exposure": params.enlarger.normalize_print_exposure,
            "print_exposure_compensation": params.enlarger.print_exposure_compensation,
        },
        "exposure_compensation_ev": if params.enlarger.print_exposure_compensation {
            params.camera.exposure_compensation_ev
        } else {
            0.0
        },
    })
    .to_string()
}

fn pick_default_stock(entries: &[ProfileEntry], preferred: &str) -> String {
    if entries.iter().any(|e| e.stock == preferred) {
        preferred.to_string()
    } else {
        entries
            .first()
            .map(|e| e.stock.clone())
            .unwrap_or_else(|| preferred.to_string())
    }
}

/// Convert the pipeline's RGB output into a display-encoded RGBA preview.
/// The full output remains available for saving, but the interactive texture is
/// capped so a large RAW does not force a full-size upload into egui every pass.
fn make_preview_texture_data(
    out: &ImageBuf,
    source_color: Option<RasterColor>,
) -> PreviewTextureData {
    let transform = source_color
        .filter(|&color| color != SRGB_DISPLAY)
        .map(|color| ColorTransform::new(color, SRGB_DISPLAY));
    let src_w = out.width as usize;
    let src_h = out.height as usize;
    let max_src_dim = src_w.max(src_h).max(1);
    let scale_num = PREVIEW_TEXTURE_MAX_DIM.min(max_src_dim);
    let w = ((src_w * scale_num + max_src_dim / 2) / max_src_dim).max(1);
    let h = ((src_h * scale_num + max_src_dim / 2) / max_src_dim).max(1);
    let mut rgba = vec![0u8; w * h * 4];
    rgba.par_chunks_exact_mut(w * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let gy = if h == src_h {
                y as f32
            } else {
                (y as f32 + 0.5) * (src_h as f32 / h as f32) - 0.5
            };
            let y0 = gy.floor().clamp(0.0, (src_h - 1) as f32) as usize;
            let y1 = (y0 + 1).min(src_h - 1);
            let ty = (gy - y0 as f32).clamp(0.0, 1.0);

            for x in 0..w {
                let gx = if w == src_w {
                    x as f32
                } else {
                    (x as f32 + 0.5) * (src_w as f32 / w as f32) - 0.5
                };
                let x0 = gx.floor().clamp(0.0, (src_w - 1) as f32) as usize;
                let x1 = (x0 + 1).min(src_w - 1);
                let tx = (gx - x0 as f32).clamp(0.0, 1.0);
                let p00 = (y0 * src_w + x0) * 3;
                let p10 = (y0 * src_w + x1) * 3;
                let p01 = (y1 * src_w + x0) * 3;
                let p11 = (y1 * src_w + x1) * 3;
                let dst = x * 4;

                let sample = std::array::from_fn(|c| {
                    let v00 = to_f32(out.data[p00 + c]);
                    let v10 = to_f32(out.data[p10 + c]);
                    let v01 = to_f32(out.data[p01 + c]);
                    let v11 = to_f32(out.data[p11 + c]);
                    let top = v00 + (v10 - v00) * tx;
                    let bot = v01 + (v11 - v01) * tx;
                    from_f32(top + (bot - top) * ty)
                });
                let rgb = transform
                    .as_ref()
                    .map_or(sample, |transform| transform.apply(sample));
                for (c, sample) in rgb.into_iter().map(to_f32).enumerate() {
                    row[dst + c] = (sample.clamp(0.0, 1.0) * 255.0) as u8;
                }
                row[dst + 3] = 255;
            }
        });
    PreviewTextureData { size: [w, h], rgba }
}

/// Locate the f64-built `spektrafilm` CLI binary. Search order:
///   1. `$SPEKTRAFILM_F64_CLI` — explicit override, full path.
///   2. `spektrafilm-f64` next to the running GUI executable (release
///      builds: both binaries live in `target/release/`).
///   3. `spektrafilm-f64` on `$PATH`.
///   4. `target/release/spektrafilm-f64` relative to `CARGO_MANIFEST_DIR`
///      (handy when running via `cargo run`).
///
/// Returns a path that exists and is executable, or an explanatory
/// error pointing to the build command.
fn locate_f64_cli() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("SPEKTRAFILM_F64_CLI") {
        let path = PathBuf::from(&p);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "SPEKTRAFILM_F64_CLI points at {p} but no such file"
        ));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for name in f64_cli_names() {
            let p = dir.join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    for name in f64_cli_names() {
        if let Some(p) = which_on_path(name) {
            return Ok(p);
        }
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        for name in f64_cli_names() {
            let p = PathBuf::from(&manifest)
                .join("..")
                .join("..")
                .join("target")
                .join("release")
                .join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err("no f64 CLI found. Build it with \
         `cargo build --release --features precision-f64 -p spektrafilm-cli`, \
         rename `target/release/spektrafilm` to `spektrafilm-f64`, \
         and either put it on PATH or set $SPEKTRAFILM_F64_CLI."
        .into())
}

fn f64_cli_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["spektrafilm-f64.exe", "spektrafilm-f64"]
    } else {
        &["spektrafilm-f64"]
    }
}

/// Minimal PATH lookup so we don't pull in the `which` crate for one call.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
struct TempPath(PathBuf);

#[cfg(test)]
impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Default)]
struct ExportImageOptions<'a> {
    use_params_input_color: bool,
    saving_color: Option<RasterColor>,
    rotation: u8,
    developed_raw: Option<&'a ImageBuf>,
    channel_orders: Option<[[usize; 3]; 2]>,
}

/// Run the f64 CLI against a staged output and publish after it exits
/// successfully. Capture stderr and reap the child on cancellation.
fn run_f64_export(
    cli_path: &Path,
    input: &Path,
    output: &Path,
    bit_depth: Option<u8>,
    film: &str,
    paper: &str,
    params: &spektrafilm_core::params::RuntimeParams,
    data_dir: &Path,
    image: ExportImageOptions<'_>,
    cancel: &AtomicBool,
) -> Result<()> {
    anyhow::ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
    let staged = StagedOutput::new(output)?;
    let job_files = temporary_export_directory(&std::env::temp_dir(), "spektrafilm-export-")
        .context("creating temporary export job directory")?;
    let params_path = job_files.path().join("params.json");
    {
        let file =
            std::fs::File::create(&params_path).context("creating temporary export params")?;
        serde_json::to_writer(file, params).context("serializing export params")?;
    }

    // Redirect diagnostics to disk so the child can make progress while the
    // GUI polls for completion or cancellation.
    let stderr_path = job_files.path().join("stderr");
    let stderr_file = std::fs::File::create_new(&stderr_path)
        .with_context(|| format!("creating export log {}", stderr_path.display()))?;

    let mut cmd = std::process::Command::new(cli_path);
    // Force the CPU backend: the wgpu shaders run f32 even in a
    // precision-f64 build (WGSL has no f64). Letting the child default
    // to GPU would silently demote the export back to f32 math.
    cmd.env("SPEKTRAFILM_BACKEND", "cpu")
        .arg("process")
        .arg(input)
        .arg("-o")
        .arg(staged.path())
        .arg("--film")
        .arg(film)
        .arg("--paper")
        .arg(paper)
        .arg("--params")
        .arg(&params_path)
        .arg("--data-dir")
        .arg(data_dir)
        .stdout(std::process::Stdio::null())
        .stderr(stderr_file);
    if params.io.scan_film {
        cmd.arg("--scan-film");
    }

    if let Some(bit_depth) = bit_depth {
        cmd.arg("--bit-depth").arg(bit_depth.to_string());
    }
    if let Some(developed) = image.developed_raw {
        let developed_path = job_files.path().join("developed-aces.bin");
        write_developed_raw(&developed_path, developed).context("staging developed RAW pixels")?;
        cmd.arg("--developed-raw").arg(developed_path);
    }
    if image.rotation != 0 {
        cmd.arg("--rotate-quarter-turns")
            .arg(image.rotation.to_string());
    }
    if let Some([film, print]) = image.channel_orders {
        for (flag, order) in [
            ("--film-channel-order", film),
            ("--print-channel-order", print),
        ] {
            if order != IDENTITY_CHANNELS {
                cmd.arg(flag)
                    .arg(format!("{},{},{}", order[0], order[1], order[2]));
            }
        }
    }
    if image.use_params_input_color {
        cmd.arg("--use-params-input-color");
    }
    if let Some(saving_color) = image.saving_color {
        cmd.arg("--save-color-space")
            .arg(saving_color.color_space)
            .arg("--save-cctf-encoding")
            .arg(saving_color.cctf_encoding.to_string());
    }

    anyhow::ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", cli_path.display()))?;

    // Poll loop. 100 ms is responsive to a Cancel click without
    // burning CPU while the export grinds through CPU f64 math.
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("cancelled");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("waiting for f64 CLI");
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    if !status.success() {
        let stderr_bytes = std::fs::read(&stderr_path)
            .with_context(|| format!("reading export log after CLI exited {status}"))?;
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let trimmed = stderr.trim();
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "(signal)".into());
        anyhow::bail!(
            "f64 CLI exited {code} — {}",
            if trimmed.is_empty() {
                "(no stderr)"
            } else {
                trimmed
            }
        );
    }
    // A cancel may arrive between the last poll and the child's exit.
    anyhow::ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
    staged.publish()
}

#[cfg(test)]
mod f64_export_tests {
    use super::*;
    use std::{fs, sync::OnceLock};

    fn run_child(input: &Path, target: &Path, cancel: &AtomicBool) -> Result<()> {
        static FIXTURE: OnceLock<tempfile::TempDir> = OnceLock::new();
        let fixture_name = format!("export-child{}", std::env::consts::EXE_SUFFIX);
        let directory = FIXTURE.get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let source =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/f64_export_child.rs");
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let result = std::process::Command::new(rustc)
                .arg("--edition=2024")
                .arg(source)
                .arg("-o")
                .arg(directory.path().join(&fixture_name))
                .output()
                .expect("start rustc for f64 export fixture");
            assert!(
                result.status.success(),
                "compile f64 export fixture: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            directory
        });
        run_f64_export(
            &directory.path().join(fixture_name),
            input,
            target,
            Some(32),
            "film",
            "paper",
            &RuntimeParams::default(),
            Path::new("."),
            ExportImageOptions::default(),
            cancel,
        )
    }

    #[test]
    fn successful_child_creates_or_replaces_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("rendered frame.tiff");
        let cancel = AtomicBool::new(false);
        run_child(Path::new("complete"), &target, &cancel).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"complete");

        fs::write(&target, b"previous contents").unwrap();
        run_child(Path::new("complete"), &target, &cancel).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"complete");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_child_leaves_existing_and_absent_targets_intact() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("rendered.tiff");
        let cancel = AtomicBool::new(false);
        for existing in [false, true] {
            if existing {
                fs::write(&target, b"previous contents").unwrap();
            }
            let error = run_child(Path::new("fail"), &target, &cancel).unwrap_err();
            assert!(error.to_string().contains("exited 23 — encoder failed"));
            if existing {
                assert_eq!(fs::read(&target).unwrap(), b"previous contents");
            } else {
                assert!(!target.exists());
            }
            assert_eq!(
                fs::read_dir(directory.path()).unwrap().count(),
                usize::from(existing)
            );
        }
    }

    #[test]
    fn cancelling_child_after_write_leaves_targets_intact() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("staged-output-written");
        let target = directory.path().join("rendered.tiff");

        for existing in [false, true] {
            if existing {
                fs::write(&target, b"previous contents").unwrap();
            }
            let cancel = AtomicBool::new(false);
            let (written, while_writing, result) = std::thread::scope(|scope| {
                let export = scope.spawn(|| run_child(&marker, &target, &cancel));
                let deadline = Instant::now() + Duration::from_secs(5);
                while !marker.is_file() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let written = marker.is_file();
                let while_writing = fs::read(&target);
                cancel.store(true, Ordering::SeqCst);
                (written, while_writing, export.join().unwrap())
            });
            assert!(written, "child did not write staged output");
            assert_eq!(result.unwrap_err().to_string(), "cancelled");
            if existing {
                assert_eq!(while_writing.unwrap(), b"previous contents");
                assert_eq!(fs::read(&target).unwrap(), b"previous contents");
            } else {
                assert_eq!(
                    while_writing.unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
                assert!(!target.exists());
            }
            fs::remove_file(&marker).unwrap();
            assert_eq!(
                fs::read_dir(directory.path()).unwrap().count(),
                usize::from(existing)
            );
        }
    }

    #[test]
    fn failed_publish_leaves_destination_directory_intact() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("rendered.tiff");
        fs::create_dir(&target).unwrap();
        let marker = target.join("keep");
        fs::write(&marker, b"keep").unwrap();

        let error = run_child(Path::new("complete"), &target, &AtomicBool::new(false)).unwrap_err();
        assert!(error.to_string().starts_with("publishing output to "));
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn locked_destination_retains_previous_contents() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("locked.tiff");
        fs::write(&target, b"previous contents").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();

        let result = run_child(Path::new("complete"), &target, &AtomicBool::new(false));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("publishing output to ")
        );
        drop(lock);
        assert_eq!(fs::read(&target).unwrap(), b"previous contents");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

/// macOS only: walk from the eframe `RawWindowHandle` down to the
/// `CAMetalLayer` and tag its colorspace as sRGB. Without this the
/// metal layer's `colorspace` is null and macOS treats the framebuffer
/// pixels as raw display primaries — sRGB content rendered into a
/// wide-gamut panel comes out oversaturated. Setting the layer's
/// colorspace makes the OS gamut-map exactly like it does for a
/// regular sRGB-tagged PNG opened in Preview, so what you see in the
/// GUI matches what you export.
#[cfg(target_os = "macos")]
fn tag_metal_layer_srgb<H: raw_window_handle::HasWindowHandle>(h: &H) -> Result<(), String> {
    use core_graphics::color_space::{CGColorSpace, kCGColorSpaceSRGB};
    use foreign_types::ForeignType;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::RawWindowHandle;

    let handle = h
        .window_handle()
        .map_err(|e| format!("no window handle: {e}"))?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return Err("not an AppKit window".into());
    };
    let ns_view: *mut AnyObject = appkit.ns_view.as_ptr().cast();
    if ns_view.is_null() {
        return Err("ns_view is null".into());
    }

    // `kCGColorSpaceSRGB` is an extern static CFStringRef, accessing it
    // is unsafe in the 2024 edition's stricter model.
    let srgb_cs = unsafe { CGColorSpace::create_with_name(kCGColorSpaceSRGB) }
        .ok_or_else(|| "CGColorSpaceCreateWithName(sRGB) returned null".to_string())?;
    // `foreign_types::ForeignType::as_ptr` returns the opaque
    // `CGColorSpaceRef` (i.e. `*mut sys::CGColorSpace`). `setColorspace:`
    // is declared as taking a `CGColorSpaceRef`, whose ObjC type encoding
    // is `^{CGColorSpace=}`. A bare `*mut c_void` encodes as `^v`, which
    // objc2's debug-build argument-encoding verification rejects (the
    // release build skips that check, which is why only debug crashed).
    // Cast through a zero-field opaque struct whose `Encode` matches the
    // expected `^{CGColorSpace=}` so both builds pass the same pointer.
    #[repr(C)]
    struct CGColorSpaceOpaque {
        _private: [u8; 0],
    }
    // A pointer to this type encodes as `^{CGColorSpace=}` — what
    // `setColorspace:` expects. `RefEncode` (not `Encode`) is the trait
    // objc2 consults for a `*const T` argument.
    unsafe impl objc2::RefEncode for CGColorSpaceOpaque {
        const ENCODING_REF: objc2::Encoding =
            objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGColorSpace", &[]));
    }
    let cs_ref = srgb_cs.as_ptr() as *const CGColorSpaceOpaque;

    // Only `CAMetalLayer` has a `colorspace` property; calling
    // `setColorspace:` on a plain `CALayer` is an unrecognized selector
    // (crash), so we always gate on `respondsToSelector:`.
    let selector = objc2::sel!(setColorspace:);
    unsafe {
        let root_layer: *mut AnyObject = msg_send![ns_view, layer];
        if root_layer.is_null() {
            return Err("ns_view.layer is null".into());
        }
        // Find the CAMetalLayer. wgpu-hal 24 (the raw-window-metal
        // approach) does NOT replace the view's layer: when the root
        // layer isn't already a CAMetalLayer — the default for an
        // eframe/winit NSView — it installs the metal layer as a
        // *sublayer*. So the root layer is a plain CALayer with no
        // `colorspace`; the layer we must tag is the metal sublayer.
        // (Older eframe set the view's own layer to the CAMetalLayer, so
        // check the root first and fall back to scanning sublayers.)
        let metal_layer = if msg_send![root_layer, respondsToSelector: selector] {
            root_layer
        } else {
            let sublayers: *mut AnyObject = msg_send![root_layer, sublayers];
            let mut found: *mut AnyObject = std::ptr::null_mut();
            if !sublayers.is_null() {
                let count: usize = msg_send![sublayers, count];
                for i in 0..count {
                    let sub: *mut AnyObject = msg_send![sublayers, objectAtIndex: i];
                    if !sub.is_null() && msg_send![sub, respondsToSelector: selector] {
                        found = sub;
                        break;
                    }
                }
            }
            found
        };
        if metal_layer.is_null() {
            // The metal sublayer may not exist yet on the first frame;
            // the caller retries until this succeeds.
            return Err("no CAMetalLayer on the view or its sublayers yet".into());
        }
        // CAMetalLayer.colorspace is `retain`-strong, so ObjC takes its
        // own reference; we can let `srgb_cs` drop after the call.
        let _: () = msg_send![metal_layer, setColorspace: cs_ref];
        tracing::info!("tagged CAMetalLayer.colorspace = sRGB");
    }
    Ok(())
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use spektrafilm_gpu::cpu_backend::CpuBackend;

    #[test]
    fn output_depths_are_independent_for_each_format() {
        let depths = OutputDepths {
            tiff: 8,
            exr: 32,
            png: 16,
        };
        for (filename, expected) in [
            ("scan.TIFF", Some(8)),
            ("scan.exr", Some(32)),
            ("scan.png", Some(16)),
            ("scan.jpg", None),
        ] {
            assert_eq!(depths.for_path(Path::new(filename)), expected);
        }
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("preview.png");
        write_preview_output(
            &png,
            &ImageBuf::new(1, 1),
            &RasterWriteOptions {
                bit_depth: depths.for_path(&png),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(png).unwrap()[24], 16);
    }

    #[test]
    fn manual_color_overrides_tags_and_auto_mode_resolves_each_new_input() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("tagged.png");
        let tiff = directory.path().join("untagged.tiff");
        let image = ImageBuf::from_data(1, 1, vec![0.25; 3]);
        for (path, color) in [(&png, Some("sRGB")), (&tiff, None)] {
            spektrafilm_io::raster::write_raster(
                path,
                &image,
                &RasterWriteOptions {
                    color_space: color,
                    cctf_encoding: true,
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let mut app = App::new(Arc::new(CpuBackend), Some(png.clone()));
        assert_eq!(app.params.io.input_color_space, "sRGB");
        assert!(app.params.io.input_cctf_decoding);
        app.load_image_from_path(&tiff);
        assert_eq!(app.params.io.input_color_space, "ProPhoto RGB");
        assert!(!app.params.io.input_cctf_decoding);

        app.manual_input_color = RasterColor::new("V-Gamut", true).unwrap();
        app.override_input_color = true;
        app.sync_input_color();
        app.load_image_from_path(&png);
        assert_eq!(app.params.io.input_color_space, "V-Gamut");
        assert!(app.params.io.input_cctf_decoding);

        app.override_input_color = false;
        app.sync_input_color();
        assert_eq!(app.params.io.input_color_space, "sRGB");
    }

    #[test]
    fn display_conversion_does_not_quantize_or_relabel_the_saved_output() {
        let directory = tempfile::tempdir().unwrap();
        let tiff = directory.path().join("converted.tiff");
        let exr = directory.path().join("converted.exr");
        let image = ImageBuf::from_data(1, 1, vec![0.71, 0.12, -0.03]);
        let mut app = App::new(Arc::new(CpuBackend), None);
        app.params.io.output_color_space = "ProPhoto RGB".into();
        app.params.io.output_cctf_encoding = false;
        app.saving_color = SRGB_DISPLAY;
        app.output_image = Some(image.clone());
        app.output_depths.tiff = 32;
        app.output_depths.exr = 32;

        let managed = make_preview_texture_data(&image, Some(simulation_color(&app.params)));
        let unmanaged = make_preview_texture_data(&image, None);
        assert_eq!(managed.rgba, [255, 0, 0, 255]);
        assert_eq!(unmanaged.rgba, [181, 30, 0, 255]);

        app.save_to_path(&tiff).unwrap();
        app.display_transform = false;
        app.save_to_path(&exr).unwrap();
        for path in [&tiff, &exr] {
            let read = read_image(path, raw::DevelopOptions::default()).unwrap();
            assert_eq!(read.color, Some(SRGB_DISPLAY));
            for (actual, expected) in read.image.data.iter().map(|&value| to_f32(value)).zip([
                1.146545,
                -0.17519957,
                -0.74973375,
            ]) {
                assert!((actual - expected).abs() < 5e-6);
            }
        }
        assert_eq!(app.output_image.as_ref().unwrap().data, image.data);
    }

    #[test]
    fn applying_channel_order_rebuilds_cached_print_and_film_profiles() {
        let app = App::new(Arc::new(CpuBackend), None);
        let mut cache = None;
        let mut params = app.params.clone();
        params.settings.rgb_to_raw_method = "mallett2019".into();
        let baseline = render_pipeline(
            &mut cache,
            &app.data_dir,
            &app.film_name,
            &app.print_name,
            [IDENTITY_CHANNELS; 2],
            params.clone(),
        )
        .unwrap();
        let orders = [[2, 1, 0], [1, 0, 2]];
        let changed = render_pipeline(
            &mut cache,
            &app.data_dir,
            &app.film_name,
            &app.print_name,
            orders,
            params,
        )
        .unwrap();
        for ((original, changed), order) in [
            (&baseline.film, &changed.film),
            (&baseline.print, &changed.print),
        ]
        .into_iter()
        .zip(orders)
        {
            assert!(
                original
                    .data
                    .log_sensitivity
                    .iter()
                    .flatten()
                    .zip(changed.data.log_sensitivity.iter().flatten())
                    .all(|(before, after)| before.to_bits() == after.to_bits())
            );
            for (prior, reordered) in original
                .data
                .channel_density
                .iter()
                .zip(&changed.data.channel_density)
            {
                assert!(
                    reordered
                        .iter()
                        .zip(order)
                        .all(|(actual, index)| actual.to_bits() == prior[index].to_bits())
                );
            }
        }
    }

    fn fixture_path(name: &str) -> TempPath {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempPath(std::env::temp_dir().join(format!(
            "spektrafilm-test-{}-{stamp}-{name}",
            std::process::id(),
        )))
    }

    fn start_preview(app: &mut App) -> impl FnOnce(Result<RenderResult, String>) + use<> {
        let (tx, rx) = mpsc::channel();
        app.render_request = None;
        app.render_job = Some(RenderJob {
            generation: app.render_generation,
            mode: RenderMode::Preview,
            rx,
            handle: None,
        });
        move |result| {
            tx.send(RenderReply {
                result,
                pipeline_cache: None,
                preview_source: None,
            })
            .ok()
            .unwrap();
        }
    }

    fn rendered_pixel(red: f32) -> RenderResult {
        let output = ImageBuf::from_data(1, 1, vec![from_f32(red), from_f32(0.0), from_f32(0.0)]);
        let preview = make_preview_texture_data(&output, Some(SRGB_DISPLAY));
        RenderResult {
            output,
            preview,
            display_transform: true,
            input_clone_ms: 0.0,
            pipeline_build_ms: 0.0,
            render_ms: 0.0,
            preview_ms: 0.0,
            worker_total_ms: 0.0,
        }
    }

    #[test]
    fn stale_preview_cannot_be_saved_after_multiple_edits() {
        let ctx = egui::Context::default();
        let mut app = App::new(Arc::new(CpuBackend), None);
        app.image = Some(Arc::new(ImageBuf::new(1, 1)));
        app.request_render();
        start_preview(&mut app)(Ok(rendered_pixel(0.1)));
        app.poll_render_job(&ctx);
        assert!(app.output_image.is_some());

        app.params.camera.exposure_compensation_ev = 1.0;
        app.request_render();
        assert!(app.output_image.is_none());
        let stale = start_preview(&mut app);
        app.film_name = "another-stock".into();
        app.request_render();
        app.params.camera.exposure_compensation_ev = 2.0;
        app.request_render();
        let queued_at = app.render_request.unwrap().ready_at;

        stale(Ok(rendered_pixel(0.2)));
        app.poll_render_job(&ctx);
        assert!(app.output_image.is_none());
        assert_eq!(app.render_request.unwrap().ready_at, queued_at);

        start_preview(&mut app)(Ok(rendered_pixel(0.3)));
        app.poll_render_job(&ctx);
        assert_eq!(app.output_image.unwrap().data[0], from_f32(0.3));
    }

    #[test]
    fn reloading_same_path_discards_old_preview_and_accepts_current_errors() {
        let ctx = egui::Context::default();
        let mut app = App::new(Arc::new(CpuBackend), None);
        let path = fixture_path("reload.png");
        image::RgbImage::from_pixel(1, 1, image::Rgb([128, 0, 0]))
            .save(&path.0)
            .unwrap();

        app.load_image_from_path(&path.0);
        assert_eq!(app.source.as_ref().unwrap().original_path(), path.0);
        start_preview(&mut app)(Ok(rendered_pixel(0.1)));
        app.poll_render_job(&ctx);
        assert!(app.output_tex.is_some());
        let stale = start_preview(&mut app);

        app.load_image_from_path(&path.0);
        let loaded_status = app.status.clone();
        assert!(app.output_image.is_none());
        assert!(app.output_tex.is_none());
        drop(stale);
        app.poll_render_job(&ctx);
        assert_eq!(app.status, loaded_status);
        assert!(app.render_request.is_some());

        start_preview(&mut app)(Err("test failure".into()));
        app.poll_render_job(&ctx);
        assert_eq!(app.status, "Render error: test failure");
        assert!(app.output_image.is_none());
        assert!(app.output_tex.is_none());
    }

    #[test]
    fn editing_with_auto_preview_off_only_invalidates_the_previous_result() {
        let mut app = App::new(Arc::new(CpuBackend), None);
        app.image = Some(Arc::new(ImageBuf::new(1, 1)));
        app.request_render();
        assert!(app.render_request.unwrap().automatic);
        app.set_auto_preview(false);
        assert!(app.render_request.is_none());

        app.output_image = Some(ImageBuf::new(1, 1));
        app.request_render();
        assert!(app.output_image.is_none());
        assert!(app.render_request.is_none());

        app.request_render_mode(RenderMode::Scan);
        assert_eq!(app.render_request.unwrap().mode, RenderMode::Scan);
        assert!(!app.render_request.unwrap().automatic);
        app.request_render_mode(RenderMode::Preview);
        assert_eq!(app.render_request.unwrap().mode, RenderMode::Preview);
        assert!(!app.render_request.unwrap().automatic);

        app.set_auto_preview(true);
        assert!(app.render_request.unwrap().automatic);
    }

    #[test]
    fn preview_limits_its_render_before_processing_and_leaves_scan_scale_independent() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("opened.png");
        let image = image::RgbImage::from_fn(1024, 768, |x, y| {
            let rgb = if (256..768).contains(&x) && (256..512).contains(&y) {
                [128, 96, 64]
            } else {
                [220, 208, 196]
            };
            image::Rgb(rgb)
        });
        image.save(&source).unwrap();
        let mut app = App::new(Arc::new(CpuBackend), Some(source));
        app.film_name = "kodak_ektar_100".into();
        app.print_name = "kodak_ultra_endura".into();
        app.params.io.crop = true;
        app.params.io.crop_center = [0.5, 0.5];
        app.params.io.crop_size = [0.5, 0.25];
        app.params.settings.preview_max_size = 128;
        app.params.film_render.grain.active = false;
        app.params.film_render.halation.active = false;
        app.params.film_render.halation.boost_ev = 0.0;
        app.params.film_render.dir_couplers.active = false;
        app.params.print_render.glare.active = false;
        app.params.scanner.unsharp_mask = [0.0, 0.0];
        app.params.io.output_cctf_encoding = false;
        assert!(app.params.camera.auto_exposure);

        let ctx = egui::Context::default();
        let finish = |app: &mut App, mode| {
            app.request_render_mode(mode);
            let request = app.render_request.take().unwrap();
            app.dispatch_render(&ctx, request.mode);
            assert!(app.pipeline_cache.is_none());
            let deadline = Instant::now() + Duration::from_secs(10);
            while app.render_job.is_some() && Instant::now() < deadline {
                app.poll_render_job(&ctx);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(app.render_job.is_none());
            assert!(app.pipeline_cache.is_some());
            app.output_image.take().expect(&app.status)
        };

        let preview = finish(&mut app, RenderMode::Preview);
        assert_eq!((preview.width, preview.height), (64, 32));
        assert_eq!(app.params.io.upscale_factor, 1.0);
        let scan = finish(&mut app, RenderMode::Scan);
        assert_eq!((scan.width, scan.height), (512, 256));
        // Gaussian prefiltering before cropping mixes some of the bright
        // surround into the preview. Scan still meters and crops the original.
        assert!(
            preview
                .data
                .iter()
                .zip(&scan.data)
                .take(3)
                .any(
                    |(preview_value, scan_value)| (preview_value - scan_value).abs()
                        > from_f32(1e-5)
                )
        );

        app.params.io.upscale_factor = 0.5;
        let preview_at_half_scan_scale = finish(&mut app, RenderMode::Preview);
        assert_eq!(preview_at_half_scan_scale.data, preview.data);
        let half_scan = finish(&mut app, RenderMode::Scan);
        assert_eq!((half_scan.width, half_scan.height), (256, 128));
        let saved = directory.path().join("scan.tiff");
        write_preview_output(
            &saved,
            &half_scan,
            &RasterWriteOptions {
                bit_depth: Some(32),
                ..Default::default()
            },
        )
        .unwrap();
        let saved = read_image(&saved, raw::DevelopOptions::default()).unwrap();
        assert_eq!((saved.image.width, saved.image.height), (256, 128));
    }

    #[cfg(unix)]
    #[test]
    fn verbose_export_child_exits_and_preserves_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let script = fixture_path("verbose-export.sh");
        std::fs::write(
            &script.0,
            r#"#!/bin/sh
case " $* " in
    *" --bit-depth 32 "*) ;;
    *) printf 'missing export bit depth\n' >&2; exit 24 ;;
esac
head -c 1048576 /dev/zero
head -c 1048576 /dev/zero | tr '\000' x >&2
printf '\nexpected export error\n' >&2
exit 23
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script.0, std::fs::Permissions::from_mode(0o700)).unwrap();

        let cancel = AtomicBool::new(false);
        let output = fixture_path("unused-output.tiff");
        let err = std::thread::scope(|scope| {
            let (done, waiting) = mpsc::channel::<()>();
            let cancel = &cancel;
            scope.spawn(move || {
                if waiting.recv_timeout(Duration::from_secs(5)).is_err() {
                    cancel.store(true, Ordering::SeqCst);
                }
            });
            let result = run_f64_export(
                &script.0,
                Path::new("unused-input"),
                &output.0,
                Some(32),
                "film",
                "paper",
                &RuntimeParams::default(),
                Path::new("."),
                ExportImageOptions::default(),
                cancel,
            );
            done.send(()).ok();
            result.unwrap_err().to_string()
        });
        assert!(err.contains("exited 23"));
        assert!(err.ends_with("expected export error"));
    }
}
