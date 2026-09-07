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
    /// Individually switchable parts of a cell.
    ///
    /// Upstream has no equivalent: its settings are about what the notch *is* (which edge,
    /// how visible, which providers), not about which decorations a cell carries. This comes
    /// from the taskbar mod, where every element of a bar is switchable, and it earns its keep
    /// for the same reason there — 70 pt is not much room, and what counts as the useful part
    /// differs per person. Someone who only wants a ring and a colour should be able to have
    /// that without four lines of text under it.
    ///
    /// All default to on, so the app looks exactly as it did before any of these existed and
    /// a new one cannot silently switch itself off for someone upgrading.
    #[serde(default = "yes")]
    pub show_percent: bool,
    /// Time until the ring's window resets, under the percentage.
    #[serde(default = "yes")]
    pub show_countdown: bool,
    /// The mark on the ring showing how far through the reset window you are.
    #[serde(default = "yes")]
    pub show_pace_tick: bool,
    /// The inner arc: turning while a session works, amber while one waits on you.
    #[serde(default = "yes")]
    pub show_activity_arc: bool,
}

fn yes() -> bool {
    true
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
            show_percent: true,
            show_countdown: true,
            show_pace_tick: true,
            show_activity_arc: true,
        }
    }
}

/// Whether the user has switched a provider off.
///
/// Read from disk on each call rather than from the shared config, so a provider loop can ask
/// without taking the config lock the UI thread also holds - and so a change made while a loop
/// is mid-sleep is seen on its next pass with no extra wiring to wake it.
///
/// Upstream is explicit that switching a provider off is not merely hiding it: "the store stops
/// fetching it, so its credential is never read at all." This port had only the hiding half. A
/// provider switched off went on polling its endpoint forever, which on this machine meant two
/// switched-off providers still spending requests - one of them re-sending a token that had
/// expired months earlier.
pub fn is_disconnected(id: &str) -> bool {
    load().is_disconnected(id)
}

impl Config {
    /// The decision itself, separated from reading the file so it can be tested against a
    /// config that was never written to disk.
    pub fn is_disconnected(&self, id: &str) -> bool {
        self.hidden_providers.iter().any(|h| h == id)
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

/// Replace the config file atomically: write a sibling, then rename over the old one.
///
/// `fs::write` truncates first, so a reader arriving mid-write got an empty or partial file,
/// fell back to `Config::default()`, and saw every provider as connected. That window is
/// microseconds wide and `is_disconnected` is now read from disk by four polling loops, so the
/// consequence was a provider the user had switched off firing exactly the request this port
/// keeps having to stop it firing. The same rename also means a crash or a full disk mid-save
/// leaves the previous settings intact instead of an empty file.
///
/// `fs::rename` maps to `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on Windows, so the
/// destination already existing is not an error. If the rename fails the temporary file is
/// removed rather than left beside the real one for someone to find later and wonder about.
pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(txt) = serde_json::to_string_pretty(cfg) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, txt).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    /// The ids here are the ones the tray writes into `hidden_providers`, and "gemini" is
    /// deliberately not the module's own name - `antigravity.rs` asks for "gemini" because
    /// that is what the tray and the page call the provider. A rename on one side only would
    /// silently restore the endless polling this predicate exists to stop, and nothing else in
    /// the build would notice.
    #[test]
    fn only_the_listed_provider_is_disconnected() {
        const TRAY_IDS: [&str; 4] = ["claude", "codex", "cursor", "gemini"];
        // Every id positively, one at a time: asserting only that the *others* come back
        // connected would pass just as happily if a loop asked for an id the tray never
        // writes, which is the mistake this is here to catch.
        for id in TRAY_IDS {
            let cfg = Config {
                hidden_providers: vec![id.to_string()],
                ..Default::default()
            };
            assert!(cfg.is_disconnected(id), "{id} should be disconnected");
            for other in TRAY_IDS.into_iter().filter(|o| *o != id) {
                assert!(!cfg.is_disconnected(other), "{other} should be untouched by {id}");
            }
        }
        // Exact match, not a prefix or substring: "cursor" must not switch off a future
        // "cursor-cli", and "claud" must not switch off "claude".
        let cfg = Config {
            hidden_providers: vec!["cursor".into(), "claud".into()],
            ..Default::default()
        };
        assert!(!cfg.is_disconnected("cursor-cli"));
        assert!(!cfg.is_disconnected("claude"));
        // Nothing switched off is the default, and it must not disconnect anything.
        for id in TRAY_IDS {
            assert!(!Config::default().is_disconnected(id));
        }
    }
}
