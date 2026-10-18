use crate::define_observable;
use serde::Serialize;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsState {
    pub bundle_ids_needing_relaunch: Vec<String>,
    pub cli_command: String,
    pub is_cli_installed: bool,
    pub show_menu_bar_icon: bool,
    pub show_dock_icon: bool,
    pub is_chord_panel_hidden_by_default: bool,
    /// A Caps tap switches input sources instead of toggling Caps Lock.
    pub caps_lock_switches_input_source: bool,
}

impl Default for AppSettingsState {
    fn default() -> Self {
        Self {
            bundle_ids_needing_relaunch: vec![],
            cli_command: if cfg!(debug_assertions) {
                "chordd"
            } else {
                "chord"
            }
            .into(),
            is_cli_installed: false,
            show_menu_bar_icon: true,
            show_dock_icon: true,
            is_chord_panel_hidden_by_default: false,
            caps_lock_switches_input_source: false,
        }
    }
}

define_observable! {
    pub struct AppSettingsObservable(AppSettingsState);
    id: "settings";
}
