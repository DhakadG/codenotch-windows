use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    /// "auto" | "zh" | "en" | "ja" | "ko"
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default)]
    pub bar_x: Option<i32>,
    #[serde(default)]
    pub bar_y: Option<i32>,
    /// Logical width of the bar (wheel-adjustable, 220-520); None = default 360
    #[serde(default)]
    pub bar_w: Option<u32>,
    /// Allow dragging + wheel resizing (tray toggle, off by default to prevent accidental drags)
    #[serde(default)]
    pub drag_enabled: bool,
    /// Vertical position of the notch: the window centre as a fraction of the primary monitor's height (0 = top, 1 = bottom), default 0.5; saved after a drag
    #[serde(default = "default_notch_y")]
    pub notch_y: f64,
    /// Provider ids the user has switched off: "claude", "codex", "cursor", "gemini".
    ///
    /// Hidden, not uninstalled - a hidden provider keeps its stored reading, so switching it
    /// back on shows the last number rather than an empty ring while it polls again.
    #[serde(default)]
    pub hidden_providers: Vec<String>,
    /// Which window the ring shows: "auto" | "session" | "weekly".
    ///
    /// "auto" is the upstream behaviour and the default - whichever window is most used, i.e.
    /// the one that will stop you first. The other two pin the ring to a particular window for
    /// people who only care about one of them, and the hover card still lists them all.
    #[serde(default = "default_ring_window")]
    pub ring_window: String,
}

fn default_ring_window() -> String {
    "auto".into()
}

fn default_notch_y() -> f64 {
    0.5
}

fn default_port() -> u16 {
    48666
}
fn default_lang() -> String {
    "auto".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            lang: default_lang(),
            bar_x: None,
            bar_y: None,
            bar_w: None,
            drag_enabled: false,
            notch_y: default_notch_y(),
            hidden_providers: Vec::new(),
            ring_window: default_ring_window(),
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codenotch")
        .join("config.json")
}

pub fn load() -> Config {
    let path = config_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(txt) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, txt);
    }
}
