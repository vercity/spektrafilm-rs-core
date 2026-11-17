/// Film and paper profiles stored as JSON.
///
/// Mirrors Python `profiles/io.py`. Profiles contain spectral sensitivity data,
/// density curves, and metadata for each film stock and paper type.
use serde::{Deserialize, Serialize};
use spektrafilm_math::spectral::N_WAVELENGTHS;
use std::{io::Write, path::Path};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    pub metadata: ProfileMetadata,
    pub info: ProfileInfo,
    pub data: ProfileData,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileMetadata {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub copyright: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub citation: String,
    #[serde(default)]
    pub datasource: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileInfo {
    pub stock: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "type", default = "default_negative")]
    pub film_type: String,
    #[serde(default = "default_film")]
    pub support: String,
    #[serde(default = "default_filming")]
    pub stage: String,
    #[serde(rename = "use", default = "default_still")]
    pub usage: String,
    #[serde(default = "default_weak")]
    pub antihalation: String,
    pub target_print: Option<String>,
    #[serde(default = "default_color")]
    pub channel_model: String,
    #[serde(default = "default_status_m")]
    pub densitometer: String,
    #[serde(default = "default_log_sens")]
    pub log_sensitivity_density_over_min: f64,
    #[serde(default = "default_d55")]
    pub reference_illuminant: String,
    #[serde(default = "default_d50")]
    pub viewing_illuminant: String,
}

fn default_negative() -> String {
    "negative".into()
}
fn default_film() -> String {
    "film".into()
}
fn default_filming() -> String {
    "filming".into()
}
fn default_still() -> String {
    "still".into()
}
fn default_weak() -> String {
    "weak".into()
}
fn default_color() -> String {
    "color".into()
}
fn default_status_m() -> String {
    "status_M".into()
}
fn default_log_sens() -> f64 {
    0.2
}
fn default_d55() -> String {
    "D55".into()
}
fn default_d50() -> String {
    "D50".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileData {
    #[serde(default)]
    pub wavelengths: Vec<f64>,
    /// Null samples have zero linear sensitivity after exponentiation.
    #[serde(default, deserialize_with = "deser_nullable_matrix")]
    pub log_sensitivity: Vec<Vec<f64>>,
    #[serde(default, deserialize_with = "deser_zero_vec")]
    pub hanatos2025_adaptation_window_params: Vec<f64>,
    #[serde(default, deserialize_with = "deser_zero_matrix")]
    pub hanatos2025_adaptation_surface_params: Vec<Vec<f64>>,
    /// NaN-preserving: null values mean "no data at this wavelength"
    #[serde(default, deserialize_with = "deser_nullable_matrix")]
    pub channel_density: Vec<Vec<f64>>,
    /// NaN-preserving base+fog as stored in the JSON: one row per wavelength,
    /// with 1 column for colour profiles or N columns (one per development
    /// time) for B&W families. `base_density` below holds the resolved column.
    #[serde(
        rename = "base_density",
        default,
        deserialize_with = "deser_base_density_rows",
        serialize_with = "serialize_base_density_rows"
    )]
    pub base_density_rows: Vec<Vec<f64>>,
    /// Resolved base+fog spectrum (the selected development-time column of
    /// `base_density_rows`). `load_profile` selects the default column;
    /// `resolve_for_render` re-selects it from the chosen development time.
    #[serde(skip)]
    pub base_density: Vec<f64>,
    /// NaN-preserving
    #[serde(default, deserialize_with = "deser_nullable_vec")]
    pub midscale_neutral_density: Vec<f64>,
    #[serde(default)]
    pub log_exposure: Vec<f64>,
    #[serde(default, deserialize_with = "deser_zero_matrix")]
    pub density_curves: Vec<Vec<f64>>,
    #[serde(default, deserialize_with = "deser_zero_tensor")]
    pub density_curves_layers: Vec<Vec<Vec<f64>>>,
    /// Parametric fit of the density curves (sum-of-CDFs per channel), used by
    /// the print-curve morph. Absent on older profiles. On B&W profiles the
    /// first axis indexes development time instead of channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density_curves_model: Option<DensityCurvesModel>,
    /// Development-time family axis (minutes), one entry per density-curves
    /// column on B&W profiles. Empty for colour profiles.
    #[serde(
        default,
        deserialize_with = "deser_zero_vec",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub development_time: Vec<f64>,
}

/// Sum-of-CDFs parametric model of a profile's density curves.
///
/// `centers`, `amplitudes`, `sigmas` are each `[n_channels][n_layers]`. Each
/// channel's density is `sum_i amplitudes[i] * Phi((x - centers[i]) / sigmas[i])`,
/// where `Phi` is the (sign-flipped for positive stocks) standard-normal CDF.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DensityCurvesModel {
    #[serde(default)]
    pub model_type: String,
    #[serde(default)]
    pub centers: Vec<Vec<f64>>,
    #[serde(default)]
    pub amplitudes: Vec<Vec<f64>>,
    #[serde(default)]
    pub sigmas: Vec<Vec<f64>>,
}

impl DensityCurvesModel {
    pub fn n_channels(&self) -> usize {
        self.centers.len()
    }
    pub fn n_layers(&self) -> usize {
        self.centers.first().map_or(0, Vec::len)
    }
}

/// Deserialize with null → 0.0 for finite curves and adaptation parameters.
/// Whole-field `null` (B&W profiles null out unused fields) reads as empty.
fn deser_zero_vec<'de, D>(deserializer: D) -> Result<Vec<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Vec<Option<f64>>> = Deserialize::deserialize(deserializer)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .map(|x| x.unwrap_or(0.0))
        .collect())
}
fn deser_zero_matrix<'de, D>(deserializer: D) -> Result<Vec<Vec<f64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Vec<Vec<Option<f64>>>> = Deserialize::deserialize(deserializer)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .map(|row| row.into_iter().map(|x| x.unwrap_or(0.0)).collect())
        .collect())
}
fn deser_zero_tensor<'de, D>(deserializer: D) -> Result<Vec<Vec<Vec<f64>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Vec<Vec<Vec<Option<f64>>>>> = Deserialize::deserialize(deserializer)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .map(|m| {
            m.into_iter()
                .map(|r| r.into_iter().map(|x| x.unwrap_or(0.0)).collect())
                .collect()
        })
        .collect())
}

/// Deserialize with null → NaN (for spectral data where null means "no measurement").
/// Whole-field `null` reads as empty.
fn deser_nullable_vec<'de, D>(deserializer: D) -> Result<Vec<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Vec<Option<f64>>> = Deserialize::deserialize(deserializer)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .map(|x| x.unwrap_or(f64::NAN))
        .collect())
}

/// Deserialize a Vec<Vec<f64>> where elements may be null.
/// Preserve missing measurements as NaN, matching Python's spectral arrays.
fn deser_nullable_matrix<'de, D>(deserializer: D) -> Result<Vec<Vec<f64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Vec<Vec<Option<f64>>>> = Deserialize::deserialize(deserializer)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .map(|row| row.into_iter().map(|x| x.unwrap_or(f64::NAN)).collect())
        .collect())
}

/// Base+fog density: accepts both the colour shape (1-D, one value per
/// wavelength) and the B&W development-time family shape (n_wl × N rows).
/// Normalized to one row per wavelength; nulls → NaN.
fn deser_base_density_rows<'de, D>(deserializer: D) -> Result<Vec<Vec<f64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Row {
        One(Option<f64>),
        Many(Vec<Option<f64>>),
    }
    let v: Option<Vec<Row>> = Deserialize::deserialize(deserializer)?;
    Ok(v.unwrap_or_default()
        .into_iter()
        .map(|r| match r {
            Row::One(x) => vec![x.unwrap_or(f64::NAN)],
            Row::Many(xs) => xs.into_iter().map(|x| x.unwrap_or(f64::NAN)).collect(),
        })
        .collect())
}

fn serialize_base_density_rows<S>(rows: &[Vec<f64>], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if rows.iter().all(|row| row.len() == 1) {
        // Python colour profiles store base+fog as one scalar per wavelength.
        serializer.collect_seq(rows.iter().map(|row| row[0]))
    } else {
        rows.serialize(serializer)
    }
}

impl Profile {
    pub fn is_negative(&self) -> bool {
        self.info.film_type == "negative"
    }
    pub fn is_positive(&self) -> bool {
        self.info.film_type == "positive"
    }
    pub fn is_film(&self) -> bool {
        self.info.support == "film"
    }
    pub fn is_paper(&self) -> bool {
        self.info.support == "paper"
    }
    pub fn is_color(&self) -> bool {
        self.info.channel_model == "color"
    }
    pub fn is_bw(&self) -> bool {
        self.info.channel_model == "bw"
    }
    pub fn is_filming(&self) -> bool {
        self.info.stage == "filming"
    }
    pub fn is_printing(&self) -> bool {
        self.info.stage == "printing"
    }

    /// Get density curves as [N][3] f64 array for calibration precision.
    pub fn density_curves_f64(&self) -> Vec<[f64; 3]> {
        self.data
            .density_curves
            .iter()
            .map(|row| {
                [
                    row.get(0).copied().unwrap_or(0.0),
                    row.get(1).copied().unwrap_or(0.0),
                    row.get(2).copied().unwrap_or(0.0),
                ]
            })
            .collect()
    }

    /// Get log_exposure as f64 slice.
    pub fn log_exposure_f64(&self) -> Vec<f64> {
        self.data.log_exposure.clone()
    }

    /// Get density curves as [N][3] f32 array for fast interpolation.
    pub fn density_curves_f32(&self) -> Vec<[f32; 3]> {
        self.data
            .density_curves
            .iter()
            .map(|row| {
                [
                    row.get(0).copied().unwrap_or(0.0) as f32,
                    row.get(1).copied().unwrap_or(0.0) as f32,
                    row.get(2).copied().unwrap_or(0.0) as f32,
                ]
            })
            .collect()
    }

    /// Get log_exposure as f32 slice.
    pub fn log_exposure_f32(&self) -> Vec<f32> {
        self.data.log_exposure.iter().map(|&v| v as f32).collect()
    }

    /// Get log_sensitivity as [81][3] f32 array.
    pub fn log_sensitivity_f32(&self) -> Vec<[f32; 3]> {
        self.data
            .log_sensitivity
            .iter()
            .map(|row| {
                [
                    row.get(0).copied().unwrap_or(0.0) as f32,
                    row.get(1).copied().unwrap_or(0.0) as f32,
                    row.get(2).copied().unwrap_or(0.0) as f32,
                ]
            })
            .collect()
    }

    /// Get log_sensitivity as [81][3] f64 array (precision-preserving).
    pub fn log_sensitivity_f64(&self) -> Vec<[f64; 3]> {
        self.data
            .log_sensitivity
            .iter()
            .map(|row| {
                [
                    row.get(0).copied().unwrap_or(0.0),
                    row.get(1).copied().unwrap_or(0.0),
                    row.get(2).copied().unwrap_or(0.0),
                ]
            })
            .collect()
    }
}

/// Load a profile from a JSON file on disk.
pub fn load_profile(path: &Path) -> Result<Profile, ProfileError> {
    let file =
        std::fs::File::open(path).map_err(|e| ProfileError::Io(path.display().to_string(), e))?;
    let reader = std::io::BufReader::new(file);
    let mut profile: Profile = serde_json::from_reader(reader)
        .map_err(|e| ProfileError::Parse(path.display().to_string(), e))?;
    validate_profile(&profile)?;
    // Resolve the default base+fog column so the field is usable straight
    // after load. `resolve_for_render` re-selects it for B&W development-time
    // families when a specific time is requested.
    let idx = development_time_index(&profile.data.development_time, None);
    profile.data.base_density = base_density_column(&profile.data.base_density_rows, idx);
    Ok(profile)
}

/// Write a film or paper profile in the stock JSON schema.
///
/// Save the original profile before resolving a selected development time for
/// rendering. Missing spectral measurements are written as JSON `null`.
pub fn save_profile(path: &Path, profile: &Profile) -> Result<(), ProfileError> {
    validate_profile(profile)?;
    validate_profile_for_save(profile)?;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let io_error = |error| ProfileError::Write(path.display().to_string(), error);
    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(io_error)?;
    {
        let mut writer = std::io::BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, profile)
            .map_err(|error| ProfileError::Encode(path.display().to_string(), error))?;
        writer.write_all(b"\n").map_err(io_error)?;
        writer.flush().map_err(io_error)?;
    }
    temporary
        .persist(path)
        .map_err(|error| io_error(error.error))?;
    Ok(())
}

/// Load a profile by stock name from a data directory.
pub fn load_profile_by_name(data_dir: &Path, stock: &str) -> Result<Profile, ProfileError> {
    let path = data_dir.join("profiles").join(format!("{stock}.json"));
    load_profile(&path)
}

/// Index into a development-time family: nearest entry to `requested` by
/// absolute difference, or the floor-middle entry when unset. Mirrors
/// Python `select_development_time` (`None` → `(N-1)//2`, else nearest).
/// Public so the GUI's picker resolves the selection identically.
pub fn development_time_index(times: &[f64], requested: Option<f64>) -> usize {
    if times.len() <= 1 {
        return 0;
    }
    match requested {
        Some(t) => times
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                (*a - t)
                    .abs()
                    .partial_cmp(&(*b - t).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0),
        None => (times.len() - 1) / 2,
    }
}

/// Extract one column of the per-wavelength base-density rows (clamping the
/// index for single-column colour profiles).
fn base_density_column(rows: &[Vec<f64>], idx: usize) -> Vec<f64> {
    rows.iter()
        .map(|row| {
            let i = idx.min(row.len().saturating_sub(1));
            row.get(i).copied().unwrap_or(f64::NAN)
        })
        .collect()
}

/// Resolve a profile for rendering: collapse a B&W development-time family
/// to the requested time and broadcast the single channel onto the engine's
/// 3-channel layout. Colour profiles pass through untouched.
///
/// Mirrors upstream's `select_development_time` (nearest entry; default the
/// floor-middle of the family) followed by its `n_channels == 1` semantics:
/// the engine runs the same per-channel math on identical replicated
/// channels, while `channel_density` becomes `[dye, 0, 0]` so every spectral
/// integration computes exactly the upstream single-channel
/// `density · dye_spectrum` (the G/B lanes carry no spectral weight).
pub fn resolve_for_render(mut profile: Profile, development_time: Option<f64>) -> Profile {
    if !profile.is_bw() {
        return profile;
    }
    let idx = development_time_index(&profile.data.development_time, development_time);
    let d = &mut profile.data;

    // 1. Collapse the development-time family (columns of density_curves,
    //    base_density, and rows of the curves model) to the chosen entry.
    for row in &mut d.density_curves {
        let i = idx.min(row.len().saturating_sub(1));
        *row = vec![row.get(i).copied().unwrap_or(0.0)];
    }
    d.base_density = base_density_column(&d.base_density_rows, idx);
    d.base_density_rows = d.base_density.iter().map(|&v| vec![v]).collect();
    if let Some(model) = &mut d.density_curves_model {
        let pick = |m: &[Vec<f64>]| -> Vec<Vec<f64>> {
            m.get(idx.min(m.len().saturating_sub(1)))
                .cloned()
                .map(|row| vec![row])
                .unwrap_or_default()
        };
        model.centers = pick(&model.centers);
        model.amplitudes = pick(&model.amplitudes);
        model.sigmas = pick(&model.sigmas);
    }
    // Keep the selected development-time column for the monochrome grain
    // model, matching upstream's `[:, :, idx]`.
    for layer_row in &mut d.density_curves_layers {
        for layer in layer_row.iter_mut() {
            let i = idx.min(layer.len().saturating_sub(1));
            *layer = vec![layer.get(i).copied().unwrap_or(0.0)];
        }
    }
    if !d.development_time.is_empty() {
        let i = idx.min(d.development_time.len() - 1);
        d.development_time = vec![d.development_time[i]];
    }

    // 2. Broadcast the single channel to the 3-channel engine layout.
    for row in &mut d.log_sensitivity {
        let s = row.first().copied().unwrap_or(0.0);
        *row = vec![s, s, s];
    }
    for row in &mut d.density_curves {
        let v = row.first().copied().unwrap_or(0.0);
        *row = vec![v, v, v];
    }
    for row in &mut d.channel_density {
        let c = row.first().copied().unwrap_or(f64::NAN);
        *row = vec![c, 0.0, 0.0];
    }
    if let Some(model) = &mut d.density_curves_model {
        let bcast = |m: &mut Vec<Vec<f64>>| {
            if let Some(row) = m.first().cloned() {
                *m = vec![row.clone(), row.clone(), row];
            }
        };
        bcast(&mut model.centers);
        bcast(&mut model.amplitudes);
        bcast(&mut model.sigmas);
    }
    profile
}

fn validate_profile(profile: &Profile) -> Result<(), ProfileError> {
    let data = &profile.data;
    if data.log_exposure.is_empty() {
        return Err(ProfileError::Validation("log_exposure is empty".into()));
    }
    if data.density_curves.len() != data.log_exposure.len() {
        return Err(ProfileError::Validation(
            "density_curves length must match log_exposure length".into(),
        ));
    }
    // Rendering indexes the shared 81-band illuminants and observer tables.
    // Missing wavelength labels, base+fog and neutral metadata are allowed.
    for (field, rows) in [
        ("log_sensitivity", data.log_sensitivity.len()),
        ("channel_density", data.channel_density.len()),
    ] {
        if rows != N_WAVELENGTHS {
            return Err(ProfileError::Validation(format!(
                "{field} must have {N_WAVELENGTHS} wavelength rows, found {rows}"
            )));
        }
    }
    for (field, rows) in [
        ("wavelengths", data.wavelengths.len()),
        ("base_density", data.base_density_rows.len()),
        (
            "midscale_neutral_density",
            data.midscale_neutral_density.len(),
        ),
    ] {
        if rows != 0 && rows != N_WAVELENGTHS {
            return Err(ProfileError::Validation(format!(
                "{field} must have {N_WAVELENGTHS} entries when provided, found {rows}"
            )));
        }
    }
    let n_channels = if profile.is_bw() { 1 } else { 3 };
    // Density curves for B&W index the development-time family.
    let curve_columns = if profile.is_bw() {
        data.development_time.len().max(1)
    } else {
        n_channels
    };
    for (field, rows, columns) in [
        ("log_sensitivity", &data.log_sensitivity, n_channels),
        ("channel_density", &data.channel_density, n_channels),
        ("density_curves", &data.density_curves, curve_columns),
    ] {
        if let Some((row, values)) = rows.iter().enumerate().find(|(_, v)| v.len() != columns) {
            return Err(ProfileError::Validation(format!(
                "{field} row {} must have {columns} columns, found {}",
                row + 1,
                values.len()
            )));
        }
    }

    let base_columns = if profile.is_bw() { curve_columns } else { 1 };
    for (row, values) in data.base_density_rows.iter().enumerate() {
        if values.len() != 1 && values.len() != base_columns {
            let expected = if base_columns == 1 {
                "1 column".to_string()
            } else {
                format!("1 or {base_columns} columns")
            };
            return Err(ProfileError::Validation(format!(
                "base_density row {} must have {expected}, found {}",
                row + 1,
                values.len()
            )));
        }
    }
    Ok(())
}

fn validate_profile_for_save(profile: &Profile) -> Result<(), ProfileError> {
    let data = &profile.data;
    let invalid = |field| ProfileError::Validation(format!("{field} contains non-finite values"));
    for (field, values) in [
        ("wavelengths", data.wavelengths.as_slice()),
        ("log_exposure", &data.log_exposure),
        ("development_time", &data.development_time),
        (
            "hanatos2025_adaptation_window_params",
            &data.hanatos2025_adaptation_window_params,
        ),
        (
            "log_sensitivity_density_over_min",
            std::slice::from_ref(&profile.info.log_sensitivity_density_over_min),
        ),
    ] {
        if values.iter().any(|value| !value.is_finite()) {
            return Err(invalid(field));
        }
    }
    for (field, rows) in [
        ("density_curves", &data.density_curves),
        (
            "hanatos2025_adaptation_surface_params",
            &data.hanatos2025_adaptation_surface_params,
        ),
    ] {
        if rows.iter().flatten().any(|value| !value.is_finite()) {
            return Err(invalid(field));
        }
    }
    if data
        .density_curves_layers
        .iter()
        .flatten()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err(invalid("density_curves_layers"));
    }
    if let Some(model) = &data.density_curves_model {
        for (field, rows) in [
            ("centers", &model.centers),
            ("amplitudes", &model.amplitudes),
            ("sigmas", &model.sigmas),
        ] {
            if rows.iter().flatten().any(|value| !value.is_finite()) {
                return Err(invalid(field));
            }
        }
    }
    // Non-finite spectral values may only be the NaN sentinel for an
    // unmeasured band. Serializing infinity as null would discard real data.
    for (field, rows) in [
        ("log_sensitivity", &data.log_sensitivity),
        ("channel_density", &data.channel_density),
        ("base_density", &data.base_density_rows),
    ] {
        if rows.iter().flatten().any(|value| value.is_infinite()) {
            return Err(invalid(field));
        }
    }
    if data
        .midscale_neutral_density
        .iter()
        .any(|value| value.is_infinite())
    {
        return Err(invalid("midscale_neutral_density"));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("loading profile {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parsing profile {0}: {1}")]
    Parse(String, serde_json::Error),
    #[error("writing profile {0}: {1}")]
    Write(String, std::io::Error),
    #[error("encoding profile {0}: {1}")]
    Encode(String, serde_json::Error),
    #[error("invalid profile: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sensitivity_samples_are_distinct_from_log_zero() {
        let data: ProfileData =
            serde_json::from_str(r#"{"log_sensitivity":[[null,0.0,-1.0]]}"#).unwrap();

        assert!(data.log_sensitivity[0][0].is_nan());
        assert_eq!(data.log_sensitivity[0][1..], [0.0, -1.0]);
    }

    fn data_profile(name: &str) -> Option<Profile> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("data/profiles/{name}.json"));
        if !path.exists() {
            eprintln!("Skipping test — profile not found at {}", path.display());
            return None;
        }
        Some(load_profile(&path).unwrap())
    }

    #[test]
    fn saves_colour_and_bw_profiles_in_the_stock_schema() {
        let directory = tempfile::tempdir().unwrap();
        for (stock, is_bw) in [("kodak_gold_200", false), ("kodak_doublex", true)] {
            let mut profile = data_profile(stock).unwrap();
            profile.info.name = Some(format!("{stock} custom"));
            profile.data.log_sensitivity[0][0] = f64::NAN;
            let path = directory.path().join(format!("{stock}.json"));
            save_profile(&path, &profile).unwrap();

            let json: serde_json::Value =
                serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
            assert!(json["data"]["log_sensitivity"][0][0].is_null());
            assert_eq!(json["data"]["base_density"][50].is_array(), is_bw);
            assert_eq!(json["data"].get("development_time").is_some(), is_bw);

            let loaded = load_profile(&path).unwrap();
            assert_eq!(loaded.info.name, profile.info.name);
            assert_eq!(loaded.metadata.license, profile.metadata.license);
            assert_eq!(loaded.data.development_time, profile.data.development_time);
            assert!(loaded.data.log_sensitivity[0][0].is_nan());
            let time = loaded.data.development_time.last().copied();
            let rendered = resolve_for_render(loaded, time);
            let original = resolve_for_render(profile, time);
            for (resaved, previous) in rendered
                .data
                .density_curves
                .iter()
                .flatten()
                .zip(original.data.density_curves.iter().flatten())
            {
                assert!((resaved - previous).abs() < 1e-14);
            }
        }
    }

    #[test]
    fn saving_an_invalid_profile_preserves_the_previous_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.json");
        let mut profile = data_profile("kodak_gold_200").unwrap();
        save_profile(&path, &profile).unwrap();
        let previous = std::fs::read(&path).unwrap();
        profile.data.log_sensitivity[0][0] = f64::INFINITY;
        assert!(matches!(
            save_profile(&path, &profile),
            Err(ProfileError::Validation(_))
        ));
        assert_eq!(std::fs::read(path).unwrap(), previous);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_incompatible_profile_axes_and_channel_rows() {
        let Some(color) = data_profile("kodak_portra_400") else {
            return;
        };
        let mut profile = color.clone();
        profile.data.log_sensitivity.pop();
        assert_eq!(
            validate_profile(&profile).unwrap_err().to_string(),
            "invalid profile: log_sensitivity must have 81 wavelength rows, found 80"
        );

        let mut profile = color.clone();
        profile.data.channel_density[6].pop();
        assert_eq!(
            validate_profile(&profile).unwrap_err().to_string(),
            "invalid profile: channel_density row 7 must have 3 columns, found 2"
        );

        let mut profile = color.clone();
        profile.data.density_curves[0].push(0.0);
        assert_eq!(
            validate_profile(&profile).unwrap_err().to_string(),
            "invalid profile: density_curves row 1 must have 3 columns, found 4"
        );

        let mut profile = color.clone();
        profile.data.base_density_rows[0].push(0.0);
        assert_eq!(
            validate_profile(&profile).unwrap_err().to_string(),
            "invalid profile: base_density row 1 must have 1 column, found 2"
        );

        let mut profile = color;
        profile.data.wavelengths.remove(0);
        assert_eq!(
            validate_profile(&profile).unwrap_err().to_string(),
            "invalid profile: wavelengths must have 81 entries when provided, found 80"
        );
        profile.data.wavelengths.clear();
        profile.data.base_density_rows.clear();
        profile.data.midscale_neutral_density.clear();
        assert!(validate_profile(&profile).is_ok());
    }

    #[test]
    fn shipped_profiles_have_compatible_shapes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/profiles");
        let mut count = 0;
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                load_profile(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                count += 1;
            }
        }
        assert!(count > 0, "no bundled profiles found");
    }

    /// B&W profiles carry whole-field nulls, single-channel arrays, and a
    /// development-time family — all of which the loader must accept.
    #[test]
    fn loads_bw_development_time_family() {
        let Some(p) = data_profile("kodak_doublex") else {
            return;
        };
        assert!(p.is_bw());
        assert_eq!(p.data.development_time, vec![4.0, 5.0, 6.5, 9.0, 12.0]);
        assert!(p.data.log_sensitivity.iter().all(|r| r.len() == 1));
        assert!(p.data.density_curves.iter().all(|r| r.len() == 5));
        assert!(p.data.base_density_rows.iter().all(|r| r.len() == 5));
        // load_profile resolves the default (floor-middle) base column.
        assert_eq!(p.data.base_density.len(), 81);
        // Whole-field nulls parse as empty.
        assert!(p.data.midscale_neutral_density.is_empty());
        assert!(p.data.hanatos2025_adaptation_window_params.is_empty());
    }

    /// Resolving collapses the family to the requested (nearest) time and
    /// broadcasts the single channel to the 3-channel engine layout with the
    /// `[dye, 0, 0]` spectral encoding.
    #[test]
    fn resolve_bw_selects_time_and_broadcasts() {
        let Some(p) = data_profile("kodak_doublex") else {
            return;
        };
        // 10.0 is nearest to 9.0 (index 3).
        let want_curve: Vec<f64> = p.data.density_curves.iter().map(|r| r[3]).collect();
        let r = resolve_for_render(p, Some(10.0));
        assert_eq!(r.data.development_time, vec![9.0]);
        assert!(
            r.data
                .log_sensitivity
                .iter()
                .all(|row| row.len() == 3 && row.iter().all(|v| v.to_bits() == row[0].to_bits()))
        );
        for (row, want) in r.data.density_curves.iter().zip(&want_curve) {
            assert_eq!(row.len(), 3);
            assert_eq!(row[0], *want);
            assert_eq!(row[0], row[1]);
            assert_eq!(row[1], row[2]);
        }
        // Spectral dye encoding: [dye, 0, 0] so every integration computes
        // the upstream single-channel density · dye_spectrum.
        assert!(
            r.data
                .channel_density
                .iter()
                .all(|row| row.len() == 3 && row[1] == 0.0 && row[2] == 0.0)
        );
        let model = r.data.density_curves_model.as_ref().unwrap();
        assert_eq!(model.n_channels(), 3);
        assert_eq!(model.centers[0], model.centers[1]);
    }

    /// Colour profiles pass through `resolve_for_render` untouched.
    #[test]
    fn resolve_is_identity_for_colour() {
        let Some(p) = data_profile("kodak_gold_200") else {
            return;
        };
        let before = p.data.density_curves.clone();
        let r = resolve_for_render(p, Some(5.0));
        assert!(!r.is_bw());
        assert_eq!(r.data.density_curves, before);
    }

    #[test]
    fn test_load_kodak_portra_400() {
        // This test requires the data directory to be present
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("data/profiles/kodak_portra_400.json");
        if !path.exists() {
            eprintln!("Skipping test — profile not found at {}", path.display());
            return;
        }
        let profile = load_profile(&path).unwrap();
        assert_eq!(profile.info.stock.as_deref(), Some("kodak_portra_400"));
        assert_eq!(profile.info.film_type, "negative");
        assert_eq!(profile.info.support, "film");
        assert_eq!(profile.data.wavelengths.len(), 81);
        assert_eq!(profile.data.log_exposure.len(), 256);
        assert_eq!(profile.data.density_curves.len(), 256);
    }
}
