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
    /// The thin weekly ring inside the thick one.
    ///
    /// Both windows at once, because they answer different questions and the answer to one
    /// does not imply the other: a comfortable weekly figure says nothing about the next five
    /// hours, and a spent five-hour window says nothing about the week.
    #[serde(default = "yes")]
    pub show_weekly_ring: bool,
    /// The five faint hour boundaries on the five-hour ring.
    #[serde(default = "yes")]
    pub show_hour_marks: bool,
    /// Raise a desktop notification when a window first crosses into the red.
    ///
    /// On by default, unlike the other additions here, because the whole point is the times
    /// you are not looking at the pill - a warning nobody switched on is a warning nobody
    /// gets. It fires once per crossing and re-arms only when usage drops back, so the cost of
    /// being wrong about that default is one notification, not a stream.
    #[serde(default = "yes")]
    pub notify_threshold: bool,
    /// Where the red starts, as a fraction. Also where the ring's colour turns.
    #[serde(default = "default_red")]
    pub red_threshold: f64,
    /// Show what is left rather than what is spent.
    ///
    /// The mod calls this Remaining mode. Only the number and the arc length change: the colour
    /// stays keyed to usage, so red still means trouble. A palette that inverted with the number
    /// would make a nearly-full green ring mean two opposite things depending on a setting.
    #[serde(default)]
    pub remaining_mode: bool,
    /// A palette that does not rely on telling red from green.
    #[serde(default)]
    pub colorblind: bool,
    /// A mark on a cell whose reading is both stale and failing.
    ///
    /// Dimming already says "old". This says "and it is not coming back on its own", which is
    /// a different message and the one worth acting on.
    #[serde(default = "yes")]
    pub show_stale_warning: bool,
    /// Lift the pill off the screen edge instead of welding it there.
    ///
    /// Upstream is welded on purpose - the fillets that join the pill to the bezel are the
    /// whole shape, and `NotchEdge` exists to choose *which* edge rather than whether there is
    /// one. So this is not parity work; it is an option, off by default, for people who want an
    /// overlay that reads as floating over the desktop rather than growing out of it.
    ///
    /// The window does not move. Only the pill inside it does, which leaves placement, dragging
    /// and the interactive rectangles working exactly as before.
    #[serde(default)]
    pub float_pill: bool,
    /// Which screen edge the pill lives on: "right" | "left" | "top" | "bottom".
    ///
    /// Upstream's `NotchEdge`, and its reasoning: the edge decides which way the stack runs and
    /// which way the hover card leaves. A side edge keeps the vertical column; top and bottom
    /// turn it on its side, because four cells stacked vertically are about 400 pt long and
    /// hanging that off the top of the screen would reach a quarter of the way down it.
    #[serde(default = "default_edge")]
    pub edge: String,
    /// Which display: "primary" | "cursor" | the device name Windows gives it.
    ///
    /// A remembered name that is no longer attached falls back to the primary rather than being
    /// honoured, because coordinates on a screen that does not exist put the pill somewhere the
    /// user can neither see nor reach the setting to fix.
    #[serde(default = "default_monitor")]
    pub monitor: String,
    /// How solid the pill is, 0.25 to 1.
    ///
    /// Applied in the page rather than to the window. A layered window's alpha would fade the
    /// hover card and the menu with it, and those are things being read rather than glanced at.
    #[serde(default = "default_opacity")]
    pub opacity: f64,
    /// Get out of the way when something goes full screen on the pill's own display.
    ///
    /// Per display on purpose. A game full screen on the left monitor is no reason to hide a
    /// pill on the right one, and hiding it anyway is what makes people switch this off.
    #[serde(default)]
    pub hide_on_fullscreen: bool,
    /// Keep the five-hour window running back to back, rather than starting it by accident.
    ///
    /// Off by default and it must stay that way: this spends a message without being asked,
    /// and a setting that costs something has to be chosen rather than inherited.
    #[serde(default)]
    pub auto_start_window: bool,
}

fn yes() -> bool {
    true
}

fn default_edge() -> String {
    "right".into()
}

fn default_monitor() -> String {
    "primary".into()
}

fn default_opacity() -> f64 {
    1.0
}

/// Eighty per cent, matching the ring's own red band and the mod's default.
fn default_red() -> f64 {
    0.8
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
            show_weekly_ring: true,
            show_hour_marks: true,
            notify_threshold: true,
            red_threshold: default_red(),
            remaining_mode: false,
            colorblind: false,
            show_stale_warning: true,
            float_pill: false,
            auto_start_window: false,
            edge: default_edge(),
            monitor: default_monitor(),
            opacity: default_opacity(),
            hide_on_fullscreen: false,
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
