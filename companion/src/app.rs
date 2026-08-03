use std::ops::RangeInclusive;

use egui::{Color32, TextEdit, Ui};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CredentialState {
    Valid,
    Invalid,
    #[default]
    Unknown,
}

impl CredentialState {
    fn label(self) -> &'static str {
        match self {
            Self::Valid => "Valid",
            Self::Invalid => "Invalid",
            Self::Unknown => "Unknown",
        }
    }

    fn color(self) -> [f32; 4] {
        match self {
            Self::Valid => [0.30, 0.78, 0.42, 1.0],
            Self::Invalid => [0.92, 0.32, 0.28, 1.0],
            Self::Unknown => [0.65, 0.65, 0.65, 1.0],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShockMode {
    #[default]
    Interval,
    Fixed,
}

impl ShockMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interval => "Interval",
            Self::Fixed => "Fixed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ListeningState {
    Listening,
    #[default]
    NotListening,
}

impl ListeningState {
    fn label(self) -> &'static str {
        match self {
            Self::Listening => "Server listening",
            Self::NotListening => "Server not listening",
        }
    }

    fn color(self) -> [f32; 4] {
        match self {
            Self::Listening => [0.30, 0.78, 0.42, 1.0],
            Self::NotListening => [0.65, 0.65, 0.65, 1.0],
        }
    }
}

#[derive(Debug, Default)]
pub struct AppState {
    pub api_key: String,
    pub user_id: String,
    pub credential_state: CredentialState,
    pub shock_mode: ShockMode,
    pub min_intensity: f32,
    pub max_intensity: f32,
    pub intensity: f32,
    pub min_duration: f32,
    pub max_duration: f32,
    pub duration: f32,
    pub log_path: String,
    pub listening_state: ListeningState,
}

impl AppState {
    pub fn credentials_present(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.user_id.trim().is_empty()
    }

    pub fn draw(&mut self, ui: &mut Ui) -> bool {
        let mut connect_requested = false;

        ui.heading("Credentials");
        text_input(ui, "API key", &mut self.api_key, true);
        text_input(ui, "User ID", &mut self.user_id, false);

        let credentials_present = self.credentials_present();
        ui.add_enabled_ui(credentials_present, |ui| {
            connect_requested = ui
                .add_sized([ui.available_width(), 0.0], egui::Button::new("Connect"))
                .clicked();
        });
        status_line(
            ui,
            self.credential_state.label(),
            self.credential_state.color(),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        ui.heading("Shock mode");
        ui.horizontal(|ui| {
            ui.label("Mode");
            egui::ComboBox::from_id_salt("shock-mode")
                .selected_text(self.shock_mode.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.shock_mode, ShockMode::Interval, "Interval");
                    ui.selectable_value(&mut self.shock_mode, ShockMode::Fixed, "Fixed");
                });
        });
        ui.add_space(4.0);

        match self.shock_mode {
            ShockMode::Interval => {
                slider_input(
                    ui,
                    "Minimum intensity",
                    &mut self.min_intensity,
                    0.0..=100.0,
                    "",
                );
                slider_input(
                    ui,
                    "Maximum intensity",
                    &mut self.max_intensity,
                    0.0..=100.0,
                    "",
                );
                slider_input(
                    ui,
                    "Minimum duration",
                    &mut self.min_duration,
                    0.0..=3.0,
                    " s",
                );
                slider_input(
                    ui,
                    "Maximum duration",
                    &mut self.max_duration,
                    0.0..=3.0,
                    " s",
                );
            }
            ShockMode::Fixed => {
                slider_input(ui, "Intensity", &mut self.intensity, 0.0..=100.0, "");
                slider_input(ui, "Duration", &mut self.duration, 0.0..=3.0, " s");
            }
        }

        text_input(ui, "Log path", &mut self.log_path, false);
        let _ = ui.add_sized(
            [ui.available_width(), 0.0],
            egui::Button::new("Auto-detect"),
        );
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        status_line(
            ui,
            self.listening_state.label(),
            self.listening_state.color(),
        );

        connect_requested
    }
}

fn input_background() -> Color32 {
    Color32::from_rgb(38, 38, 42)
}
fn text_input(ui: &mut Ui, label: &str, value: &mut String, password: bool) {
    ui.label(label);
    ui.add(
        TextEdit::singleline(value)
            .password(password)
            .desired_width(f32::INFINITY)
            .background_color(input_background()),
    );
}
fn slider_input(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    suffix: &str,
) {
    ui.label(label);
    ui.scope(|ui| {
        let available_width = ui.available_width();
        ui.spacing_mut().slider_width = available_width * 0.8;

        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.bg_fill = input_background();
        visuals.widgets.hovered.bg_fill = input_background();
        visuals.widgets.active.bg_fill = input_background();

        ui.add(egui::Slider::new(value, range).suffix(suffix));
    });
}

fn status_line(ui: &mut Ui, value: &str, color: [f32; 4]) {
    ui.colored_label(to_color(color), value);
}

fn to_color(color: [f32; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(
        (color[0].clamp(0.0, 1.0) * 255.0) as u8,
        (color[1].clamp(0.0, 1.0) * 255.0) as u8,
        (color[2].clamp(0.0, 1.0) * 255.0) as u8,
        (color[3].clamp(0.0, 1.0) * 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_require_two_non_whitespace_values() {
        let mut state = AppState::default();
        assert!(!state.credentials_present());

        state.api_key = "key".into();
        assert!(!state.credentials_present());

        state.user_id = "  ".into();
        assert!(!state.credentials_present());

        state.user_id = "user".into();
        assert!(state.credentials_present());
    }

    #[test]
    fn both_shock_modes_render_draw_data() {
        for mode in [ShockMode::Interval, ShockMode::Fixed] {
            let context = egui::Context::default();
            let mut state = AppState {
                shock_mode: mode,
                ..AppState::default()
            };
            let mut connect_requested = false;
            let output = context.run_ui(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    connect_requested = state.draw(ui);
                });
            });

            assert!(!connect_requested);
            assert!(!output.shapes.is_empty());
        }
    }
}
