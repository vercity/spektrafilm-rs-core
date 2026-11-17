use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use spektrafilm_core::params::RuntimeParams;
use spektrafilm_io::raster::RasterColor;

use crate::{
    OutputDepths, SRGB_DISPLAY, export_output::StagedOutput, raw_workflow::RawDevelopSettings,
};

pub(super) const FACTORY_FILM: &str = "kodak_gold_200";
pub(super) const FACTORY_PAPER: &str = "kodak_supra_endura";
const DEFAULT_FILENAME: &str = "gui_default_state.json";
const DIALOGS_FILENAME: &str = "dialog_directories.json";
const RUST_STATE: &str = "spektrafilm_rs";

pub(super) struct GuiPreset {
    pub film: String,
    pub paper: String,
    /// The input color here is the user-selected interpretation, never a
    /// detected file tag from the currently open image.
    pub params: RuntimeParams,
    pub override_input_color: bool,
    pub saving_color: RasterColor,
    pub depths: OutputDepths,
    pub raw_develop: RawDevelopSettings,
    pub auto_preview: bool,
    pub display_transform: bool,
    pub gray_canvas: bool,
    pub white_padding: f32,
    pub interpolation: String,
    pub film_channels: [usize; 3],
    pub print_channels: [usize; 3],
}

impl Default for GuiPreset {
    fn default() -> Self {
        Self {
            film: FACTORY_FILM.into(),
            paper: FACTORY_PAPER.into(),
            params: RuntimeParams::default(),
            override_input_color: false,
            saving_color: SRGB_DISPLAY,
            depths: OutputDepths::default(),
            raw_develop: RawDevelopSettings::default(),
            auto_preview: true,
            display_transform: true,
            gray_canvas: true,
            white_padding: 0.03,
            interpolation: "spline36".into(),
            film_channels: [0, 1, 2],
            print_channels: [0, 1, 2],
        }
    }
}

const DIRECT_GROUPS: &[(&str, &str)] = &[
    ("camera", "/camera"),
    ("camera_diffusion", "/camera/diffusion_filter"),
    ("enlarger_diffusion", "/enlarger/diffusion_filter"),
    ("halation", "/film_render/halation"),
    ("couplers", "/film_render/dir_couplers"),
    ("chemistry", "/print_render/density_curves_morph"),
    ("glare", "/print_render/glare"),
    ("scanner", "/scanner"),
    ("input_gamut_compress", "/io/input_gamut_compress"),
    ("output_gamut_compress", "/io/output_gamut_compress"),
];

const INPUT_FIELDS: &[(&str, &str)] = &[
    ("input_color_space", "/io/input_color_space"),
    ("input_cctf_decoding", "/io/input_cctf_decoding"),
    ("upscale_factor", "/io/upscale_factor"),
    ("crop", "/io/crop"),
    ("crop_center", "/io/crop_center"),
    ("crop_size", "/io/crop_size"),
    ("rgb_to_raw_method", "/settings/rgb_to_raw_method"),
    (
        "apply_hanatos2025_adaptation_window",
        "/settings/apply_hanatos2025_adaptation_window",
    ),
    (
        "apply_hanatos2025_adaptation_surface",
        "/settings/apply_hanatos2025_adaptation_surface",
    ),
    ("spectral_gaussian_blur", "/settings/spectral_gaussian_blur"),
];

const SIMULATION_FIELDS: &[(&str, &str)] = &[
    ("print_illuminant", "/enlarger/illuminant"),
    (
        "print_exposure_compensation",
        "/enlarger/print_exposure_compensation",
    ),
    ("print_exposure", "/enlarger/print_exposure"),
    ("print_y_filter_shift", "/enlarger/y_filter_shift"),
    ("print_m_filter_shift", "/enlarger/m_filter_shift"),
    ("output_color_space", "/io/output_color_space"),
    ("scan_film", "/io/scan_film"),
];

const PREFLASH_FIELDS: &[&str] = &[
    "preflash_exposure",
    "preflash_y_filter_shift",
    "preflash_m_filter_shift",
];

const GRAIN_ALIASES: &[(&str, &str)] = &[
    ("particle_area_um2", "agx_particle_area_um2"),
    ("particle_scale", "agx_particle_scale"),
    ("particle_scale_layers", "agx_particle_scale_layers"),
];

impl GuiPreset {
    pub fn read(path: &Path) -> Result<Self> {
        let data = read_json(path)?;
        Self::from_json(&data).with_context(|| format!("invalid preset {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        write_json(path, &self.to_json()?)
    }

    pub fn from_json(data: &Value) -> Result<Self> {
        ensure!(data.is_object(), "GUI preset must be a JSON object");
        let mut preset = Self::default();
        for name in DIRECT_GROUPS.iter().map(|(name, _)| *name).chain([
            "input_image",
            "grain",
            "preflashing",
            "special",
            "simulation",
            "display",
            "load_raw",
            "gui_only",
            RUST_STATE,
        ]) {
            if let Some(section) = data.get(name) {
                ensure!(
                    section.is_object(),
                    "preset section {name:?} must be a JSON object"
                );
            }
        }

        let mut runtime = serde_json::to_value(&preset.params)?;
        let rust_state = &data[RUST_STATE];
        if !rust_state.is_null() {
            ensure!(
                rust_state["version"].as_u64() == Some(1),
                "unsupported Rust preset version"
            );
            if let Some(params) = rust_state.get("params") {
                ensure!(params.is_object(), "Rust preset params must be an object");
                merge(&mut runtime, params);
            }
            preset.depths = read_or(rust_state.get("output_depths"), preset.depths)?;
        }
        for &(name, path) in DIRECT_GROUPS {
            if let Some(group) = data.get(name) {
                merge(
                    runtime.pointer_mut(path).expect("known runtime group"),
                    group,
                );
            }
        }
        if let Some(grain) = data.get("grain") {
            let mut grain = grain.clone();
            let object = grain.as_object_mut().expect("checked grain object");
            for &(python, rust) in GRAIN_ALIASES {
                if let Some(value) = object.remove(python) {
                    object.insert(rust.into(), value);
                }
            }
            merge(&mut runtime["film_render"]["grain"], &grain);
        }
        for &field in PREFLASH_FIELDS {
            if let Some(value) = data["preflashing"].get(field) {
                runtime["enlarger"][field] = value.clone();
            }
        }
        let input = &data["input_image"];
        copy_fields(input, &mut runtime, INPUT_FIELDS);
        for (legacy, current) in [
            ("apply_cctf_decoding", "input_cctf_decoding"),
            ("spectral_upsampling_method", "rgb_to_raw_method"),
        ] {
            if input.get(current).is_none() {
                if let Some(value) = input.get(legacy) {
                    let (_, path) = INPUT_FIELDS
                        .iter()
                        .find(|(name, _)| *name == current)
                        .unwrap();
                    *runtime.pointer_mut(path).expect("known runtime field") = value.clone();
                }
            }
        }
        if let Some(gamma) = data["special"]
            .get("film_gamma_factor")
            .or_else(|| data["special"].pointer("/film_render/density_curve_gamma"))
        {
            runtime["film_render"]["density_curve_gamma"] = gamma.clone();
        }
        let simulation = &data["simulation"];
        copy_fields(simulation, &mut runtime, SIMULATION_FIELDS);
        let display = data.get("display").unwrap_or(&data["gui_only"]["display"]);
        if let Some(size) = display
            .get("preview_max_size")
            .or_else(|| display.pointer("/settings/preview_max_size"))
        {
            runtime["settings"]["preview_max_size"] = size.clone();
        }

        preset.params = serde_json::from_value(runtime)?;
        let input_has_color = [
            "input_color_space",
            "input_cctf_decoding",
            "apply_cctf_decoding",
        ]
        .iter()
        .any(|&field| input.get(field).is_some())
            || ["/io/input_color_space", "/io/input_cctf_decoding"]
                .iter()
                .any(|path| input.pointer(path).is_some());
        preset.override_input_color =
            read_or(rust_state.get("override_input_color"), input_has_color)?;
        let input_color = RasterColor::new(
            &preset.params.io.input_color_space,
            preset.params.io.input_cctf_decoding,
        )
        .context("unsupported preset input color space")?;
        preset.params.io.input_color_space = input_color.color_space.into();
        preset.params.io.input_cctf_decoding = input_color.cctf_encoding;
        let sim_color = RasterColor::new(
            &preset.params.io.output_color_space,
            preset.params.io.output_cctf_encoding,
        )
        .context("unsupported preset simulation output color space")?;
        preset.params.io.output_color_space = sim_color.color_space.into();
        preset.params.io.output_cctf_encoding = sim_color.cctf_encoding;

        let simulation_field = |field: &str, group: &str| {
            simulation
                .get(field)
                .or_else(|| simulation[group].get(field))
        };
        preset.film = read_or(simulation_field("film_stock", "selection"), preset.film)?;
        preset.paper = read_or(simulation_field("print_paper", "selection"), preset.paper)?;
        preset.auto_preview = read_or(
            simulation_field("auto_preview", "workflow"),
            preset.auto_preview,
        )?;
        let saving_space: String = read_or(
            simulation_field("saving_color_space", "workflow"),
            preset.saving_color.color_space.into(),
        )?;
        let saving_encoded = read_or(
            simulation_field("saving_cctf_encoding", "workflow"),
            preset.saving_color.cctf_encoding,
        )?;
        preset.saving_color = RasterColor::new(&saving_space, saving_encoded)
            .context("unsupported preset saving color space")?;
        preset.display_transform = read_or(
            display.get("use_display_transform"),
            preset.display_transform,
        )?;
        preset.gray_canvas = read_or(display.get("gray_18_canvas"), preset.gray_canvas)?;
        preset.white_padding = read_or(display.get("white_padding"), preset.white_padding)?;
        preset.interpolation = read_or(display.get("output_interpolation"), preset.interpolation)?;
        preset.raw_develop = read_or(
            data.get("load_raw")
                .or_else(|| data.pointer("/gui_only/load_raw")),
            preset.raw_develop,
        )?;
        preset.film_channels = read_or(
            data["special"].get("film_channel_swap"),
            preset.film_channels,
        )?;
        preset.print_channels = read_or(
            data["special"].get("print_channel_swap"),
            preset.print_channels,
        )?;
        preset.validate()?;
        Ok(preset)
    }

    pub fn to_json(&self) -> Result<Value> {
        let runtime = serde_json::to_value(&self.params)?;
        let mut data = json!({
            "input_image": mapped_fields(&runtime, INPUT_FIELDS),
            "special": {
                "film_gamma_factor": runtime["film_render"]["density_curve_gamma"],
                "film_channel_swap": self.film_channels,
                "print_channel_swap": self.print_channels,
            },
            "simulation": mapped_fields(&runtime, SIMULATION_FIELDS),
            "display": {
                "use_display_transform": self.display_transform,
                "gray_18_canvas": self.gray_canvas,
                "white_padding": self.white_padding,
                "preview_max_size": self.params.settings.preview_max_size,
                "output_interpolation": self.interpolation,
            },
            "load_raw": self.raw_develop,
            "preflashing": runtime["enlarger"],
            "grain": runtime["film_render"]["grain"],
            RUST_STATE: {
                "version": 1,
                "params": runtime,
                "override_input_color": self.override_input_color,
                "output_depths": self.depths,
            },
        });
        for &(name, path) in DIRECT_GROUPS {
            data[name] = runtime.pointer(path).expect("known runtime group").clone();
        }
        let grain = data["grain"]
            .as_object_mut()
            .expect("serialized grain object");
        for &(python, rust) in GRAIN_ALIASES {
            if let Some(value) = grain.remove(rust) {
                grain.insert(python.into(), value);
            }
        }
        let simulation = &mut data["simulation"];
        simulation["film_stock"] = json!(self.film);
        simulation["print_paper"] = json!(self.paper);
        simulation["saving_color_space"] = json!(self.saving_color.color_space);
        simulation["saving_cctf_encoding"] = json!(self.saving_color.cctf_encoding);
        simulation["auto_preview"] = json!(self.auto_preview);
        Ok(data)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.params.settings.preview_max_size > 0,
            "preview size must be positive"
        );
        ensure!(
            (0.0..=1.0).contains(&self.white_padding),
            "white padding must be in [0, 1]"
        );
        ensure!(
            self.params.io.upscale_factor > 0.0,
            "scan scale must be positive"
        );
        ensure!(
            self.film_channels
                .iter()
                .chain(&self.print_channels)
                .all(|&index| index < 3),
            "channel indices must be in 0..=2"
        );
        ensure!(
            matches!(self.depths.tiff, 8 | 16 | 32)
                && matches!(self.depths.exr, 16 | 32)
                && matches!(self.depths.png, 8 | 16),
            "invalid preset output bit depth"
        );
        Ok(())
    }
}

fn copy_fields(source: &Value, target: &mut Value, fields: &[(&str, &str)]) {
    for &(name, path) in fields {
        if let Some(value) = source.get(name).or_else(|| source.pointer(path)) {
            *target.pointer_mut(path).expect("known runtime field") = value.clone();
        }
    }
}

fn mapped_fields(source: &Value, fields: &[(&str, &str)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|&(name, path)| {
                (
                    name.to_string(),
                    source.pointer(path).expect("known runtime field").clone(),
                )
            })
            .collect(),
    )
}

fn merge(base: &mut Value, supplied: &Value) {
    if let (Some(base), Some(supplied)) = (base.as_object_mut(), supplied.as_object()) {
        for (field, value) in supplied {
            merge(base.entry(field).or_insert(Value::Null), value);
        }
    } else {
        *base = supplied.clone();
    }
}

fn read_or<T: DeserializeOwned>(value: Option<&Value>, fallback: T) -> Result<T> {
    match value {
        Some(value) => Ok(serde_json::from_value(value.clone())?),
        None => Ok(fallback),
    }
}

pub(super) fn config_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SPEKTRAFILM_CONFIG_DIR") {
        return Some(path.into());
    }
    #[cfg(target_os = "windows")]
    let directory = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let directory = std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
    });
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    directory.map(|path| path.join("spektrafilm"))
}

#[derive(Default)]
pub(super) struct Preferences {
    directory: Option<PathBuf>,
    dialog_dirs: BTreeMap<String, PathBuf>,
}

impl Preferences {
    pub fn open(directory: Option<PathBuf>) -> (Self, Option<anyhow::Error>) {
        let mut options = Self {
            directory,
            ..Self::default()
        };
        let result = options.directory.as_ref().map(|dir| {
            read_optional_json(&dir.join(DIALOGS_FILENAME)).and_then(|data| {
                data.map(serde_json::from_value)
                    .transpose()
                    .map_err(Into::into)
            })
        });
        match result {
            Some(Ok(Some(dialogs))) => options.dialog_dirs = dialogs,
            Some(Err(error)) => return (options, Some(error)),
            Some(Ok(None)) | None => {}
        }
        (options, None)
    }

    pub fn startup_preset(&self) -> Result<Option<GuiPreset>> {
        let Some(directory) = &self.directory else {
            return Ok(None);
        };
        read_optional_json(&directory.join(DEFAULT_FILENAME))?
            .map(|data| GuiPreset::from_json(&data).context("invalid startup preset"))
            .transpose()
    }

    pub fn save_startup(&self, preset: &GuiPreset) -> Result<()> {
        let directory = self
            .directory
            .as_ref()
            .context("no GUI configuration directory")?;
        preset.write(&directory.join(DEFAULT_FILENAME))
    }

    pub fn clear_startup(&self) -> Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        match fs::remove_file(directory.join(DEFAULT_FILENAME)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result.context("clearing saved GUI startup preset"),
        }
    }

    pub fn dialog(&self, key: &str) -> rfd::FileDialog {
        let dialog = rfd::FileDialog::new();
        match self.dialog_dirs.get(key).filter(|path| path.is_dir()) {
            Some(path) => dialog.set_directory(path),
            None => dialog,
        }
    }

    pub fn remember_dialog(&mut self, key: &str, file: &Path) -> Result<()> {
        let Some(directory) = file.parent().filter(|path| !path.as_os_str().is_empty()) else {
            return Ok(());
        };
        if self
            .dialog_dirs
            .get(key)
            .is_some_and(|prior| prior == directory)
        {
            return Ok(());
        }
        self.dialog_dirs
            .insert(key.to_string(), directory.to_path_buf());
        if let Some(config) = &self.directory {
            write_json(
                &config.join(DIALOGS_FILENAME),
                &serde_json::to_value(&self.dialog_dirs)?,
            )?;
        }
        Ok(())
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("reading {}", path.display()))
}

fn read_optional_json(path: &Path) -> Result<Option<Value>> {
    match File::open(path) {
        Ok(file) => Ok(Some(
            serde_json::from_reader(BufReader::new(file))
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("opening {}", path.display())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let staged = StagedOutput::new(path)?;
    {
        let mut output = BufWriter::new(File::create_new(staged.path())?);
        serde_json::to_writer_pretty(&mut output, value)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    staged.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::App;
    use spektrafilm_gpu::cpu_backend::CpuBackend;
    use std::sync::Arc;

    #[test]
    fn reads_partial_flat_and_nested_python_gui_states() {
        let flat = GuiPreset::from_json(&json!({
            "input_image": {"input_color_space": "V-Gamut", "apply_cctf_decoding": true},
            "grain": {"particle_area_um2": 0.83},
            "preflashing": {"preflash_exposure": 0.1, "illuminant": "ignored passthrough"},
            "simulation": {"print_illuminant": "D55", "output_color_space": "ProPhoto RGB", "auto_preview": false},
            "display": {"white_padding": 0.05, "preview_max_size": 800},
            "special": {"film_gamma_factor": 0.78, "print_channel_swap": [2, 0, 1]},
        })).unwrap();
        assert!(flat.params.io.input_cctf_decoding);
        assert!(flat.override_input_color);
        assert_eq!(flat.params.film_render.grain.agx_particle_area_um2, 0.83);
        assert_eq!(flat.params.enlarger.preflash_exposure, 0.1);
        assert_eq!(flat.params.enlarger.illuminant, "D55");
        assert_eq!(flat.paper, FACTORY_PAPER);
        assert_eq!(flat.params.film_render.density_curve_gamma, 0.78);
        assert_eq!(flat.params.settings.preview_max_size, 800);
        assert_eq!(flat.white_padding, 0.05);
        assert_eq!(flat.print_channels, [2, 0, 1]);
        assert!(!flat.auto_preview);

        let nested = GuiPreset::from_json(&json!({
            "input_image": {"io": {"input_color_space": "ITU-R BT.2020"}, "settings": {"spectral_gaussian_blur": 2.3}},
            "simulation": {"selection": {"film_stock": "kodak_portra_400", "print_paper": "kodak_2383"}, "enlarger": {"print_exposure": 2.0}},
            "gui_only": {"display": {"settings": {"preview_max_size": 1000}}},
        })).unwrap();
        assert_eq!(nested.film, "kodak_portra_400");
        assert_eq!(nested.paper, "kodak_2383");
        assert_eq!(nested.params.settings.spectral_gaussian_blur, 2.3);
        assert_eq!(nested.params.settings.preview_max_size, 1000);
        assert_eq!(nested.params.enlarger.print_exposure, 2.0);
        assert_eq!(nested.params.io.input_color_space, "ITU-R BT.2020");
    }

    #[test]
    fn roundtrips_python_keys_and_rust_only_preferences() {
        let mut original = GuiPreset::default();
        original.depths.exr = 32;
        original.params.film_render.grain.agx_particle_scale = [1.2, 0.8, 1.1];
        original.params.film_render.development_time = Some(7.0);
        original.params.settings.use_cat16 = false;
        original.params.io.output_cctf_encoding = false;
        let serialized = original.to_json().unwrap();
        assert_eq!(
            serialized["grain"]["particle_scale"],
            json!([1.2, 0.8, 1.1])
        );
        assert!(serialized["grain"].get("agx_particle_scale").is_none());
        assert_eq!(serialized["simulation"]["film_stock"], FACTORY_FILM);
        assert_eq!(serialized[RUST_STATE]["version"], 1);
        let decoded = GuiPreset::from_json(&serialized).unwrap();
        assert_eq!(decoded.depths.exr, 32);
        assert_eq!(decoded.params.film_render.development_time, Some(7.0));
        assert!(!decoded.params.settings.use_cat16);
        assert!(!decoded.params.io.output_cctf_encoding);
        assert!(!decoded.override_input_color);
        assert_eq!(decoded.to_json().unwrap(), serialized);
    }

    #[test]
    fn startup_defaults_and_dialog_locations_persist_independently() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let profile_dir = root.join("config");
        let mut app =
            App::new_with_preferences(Arc::new(CpuBackend), None, Some(profile_dir.clone()));
        assert_eq!(app.film_name, FACTORY_FILM);
        assert_eq!(app.print_name, FACTORY_PAPER);
        app.film_name = "kodak_portra_400".into();
        app.params.camera.exposure_compensation_ev = 2.0;
        app.preferences.save_startup(&app.current_preset()).unwrap();
        app.preferences
            .remember_dialog("open_input", &root.join("image.tiff"))
            .unwrap();

        let mut reopened =
            App::new_with_preferences(Arc::new(CpuBackend), None, Some(profile_dir.clone()));
        assert_eq!(reopened.film_name, "kodak_portra_400");
        assert_eq!(reopened.params.camera.exposure_compensation_ev, 2.0);
        assert_eq!(reopened.preferences.dialog_dirs["open_input"], root);
        reopened.restore_factory().unwrap();
        assert_eq!(reopened.film_name, FACTORY_FILM);
        assert_eq!(reopened.print_name, FACTORY_PAPER);
        assert_eq!(reopened.params.camera.exposure_compensation_ev, 0.0);
        assert!(!profile_dir.join(DEFAULT_FILENAME).exists());
        assert!(profile_dir.join(DIALOGS_FILENAME).is_file());
    }

    #[test]
    fn rejects_bad_presets_without_mutating_app_and_preserves_paper_on_film_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.json");
        let mut app = App::new(Arc::new(CpuBackend), None);
        let prior = app.current_preset().to_json().unwrap();
        for bad in [
            "{ bad json",
            r#"{"simulation":{"film_stock":"no-such-stock"}}"#,
            r#"{"display":{"white_padding":"huge"}}"#,
        ] {
            fs::write(&path, bad).unwrap();
            assert!(app.load_preset(&path).is_err());
            assert_eq!(app.current_preset().to_json().unwrap(), prior);
        }
        app.film_name = "fujifilm_provia_100f".into();
        app.refresh_film_selection();
        assert_eq!(app.print_name, FACTORY_PAPER);
        assert!(app.params.io.scan_film);
        app.film_name = "kodak_gold_200".into();
        app.refresh_film_selection();
        assert_eq!(app.print_name, FACTORY_PAPER);
        assert!(!app.params.io.scan_film);
    }
}
