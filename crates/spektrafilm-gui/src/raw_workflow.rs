use eframe::egui;
use serde::{Deserialize, Serialize};
use spektrafilm_io::raw::{DevelopOptions, WhiteBalance};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Balance {
    #[default]
    AsShot,
    Daylight,
    Tungsten,
    Custom,
}

impl Balance {
    fn label(self) -> &'static str {
        match self {
            Self::AsShot => "As shot",
            Self::Daylight => "Daylight",
            Self::Tungsten => "Tungsten",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct RawDevelopSettings {
    #[serde(rename = "white_balance")]
    balance: Balance,
    #[serde(rename = "temperature")]
    kelvin: f64,
    tint: f64,
    lens_correction: bool,
}

impl Default for RawDevelopSettings {
    fn default() -> Self {
        Self {
            balance: Balance::AsShot,
            kelvin: 5500.0,
            tint: 1.0,
            lens_correction: false,
        }
    }
}

impl RawDevelopSettings {
    pub(super) fn options(self) -> DevelopOptions {
        let white_balance = match self.balance {
            Balance::AsShot => WhiteBalance::AsShot,
            Balance::Daylight => WhiteBalance::Daylight,
            Balance::Tungsten => WhiteBalance::Tungsten,
            Balance::Custom => WhiteBalance::Custom {
                kelvin: self.kelvin,
                tint: self.tint,
            },
        };
        DevelopOptions {
            white_balance,
            lens_correction: self.lens_correction,
        }
    }

    pub(super) fn controls(&mut self, ui: &mut egui::Ui, loaded_raw: bool) -> bool {
        egui::ComboBox::from_label("White balance")
            .selected_text(self.balance.label())
            .show_ui(ui, |ui| {
                for balance in [
                    Balance::AsShot,
                    Balance::Daylight,
                    Balance::Tungsten,
                    Balance::Custom,
                ] {
                    ui.selectable_value(&mut self.balance, balance, balance.label());
                }
            });
        if self.balance == Balance::Custom {
            ui.add(
                egui::DragValue::new(&mut self.kelvin)
                    .range(1667.0..=25_000.0)
                    .suffix(" K"),
            );
            ui.add(
                egui::DragValue::new(&mut self.tint)
                    .range(0.01..=10.0)
                    .speed(0.01)
                    .prefix("Tint "),
            );
        }
        ui.checkbox(&mut self.lens_correction, "Lens correction")
            .on_hover_text("Apply the Lensfun profile that matches the camera and lens metadata.");
        ui.add_enabled(loaded_raw, egui::Button::new("Re-develop RAW"))
            .on_hover_text("Apply these settings to the opened RAW without reopening the file.")
            .on_disabled_hover_text("Open a camera RAW to re-develop it.")
            .clicked()
    }
}
