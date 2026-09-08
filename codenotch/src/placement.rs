//! Where the pill goes, on which display, and when it should get out of the way.
//!
//! `place_notch` used to be twenty lines that pinned the window to the right edge of the primary
//! monitor. Every part of that sentence is now a choice - which monitor, which edge, how far
//! along it - and one of them (the monitor) has to be re-answered whenever displays change.
//!
//! The geometry is pure functions with the Tauri and Win32 calls kept to the outside, because
//! the interesting cases are the ones that are painful to reproduce by hand: a second monitor at
//! a different scale, a monitor arranged above the primary so its coordinates are negative, a
//! display that is unplugged while the app is running, a game going full screen on the *other*
//! screen. All of those are a few numbers, and numbers can be tested.

use serde::{Deserialize, Serialize};

/// Logical size of the window for each orientation.
///
/// The pill is a column on a side edge and a row on a top or bottom one - upstream's reasoning,
/// and it is arithmetic rather than taste: four cells stacked vertically are around 400 pt long,
/// and hanging that off the top of the screen would reach a quarter of the way down it. So the
/// window turns with the pill, and the hover card turns with it too.
pub const SIDE_SIZE: (f64, f64) = (340.0, 460.0);
pub const FLAT_SIZE: (f64, f64) = (460.0, 340.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Right,
    Left,
    Top,
    Bottom,
}

impl Edge {
    pub fn parse(s: &str) -> Edge {
        match s {
            "left" => Edge::Left,
            "top" => Edge::Top,
            "bottom" => Edge::Bottom,
            // Anything unrecognised is the edge this app has always used. A typo in a config
            // file should not leave the pill somewhere the user cannot find it.
            _ => Edge::Right,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Edge::Right => "right",
            Edge::Left => "left",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    /// True when the pill runs down the screen rather than across it.
    pub fn is_vertical(self) -> bool {
        matches!(self, Edge::Right | Edge::Left)
    }

    pub fn size(self) -> (f64, f64) {
        if self.is_vertical() { SIDE_SIZE } else { FLAT_SIZE }
    }
}

/// A rectangle in physical pixels, as Windows reports monitors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// Where on the chosen monitor the window's top-left corner goes.
///
/// `along` is the position on the edge as a fraction of its length, measuring the window's
/// centre - the same number the drag has always saved, now applied to whichever axis the edge
/// runs along.
///
/// Everything is clamped to the monitor. A monitor smaller than the window is not a hypothetical
/// - a 460 pt window on a 1024x768 secondary at 150 % scale does not fit - and the clamp has to
/// hold rather than wrap, so `max(0)` guards every subtraction.
pub fn window_origin(mon: Rect, edge: Edge, along: f64, win: (i32, i32)) -> (i32, i32) {
    let (ww, wh) = win;
    let along = along.clamp(0.0, 1.0);
    match edge {
        Edge::Right | Edge::Left => {
            let x = match edge {
                Edge::Right => mon.x + mon.w - ww,
                _ => mon.x,
            };
            let y = (mon.y as f64 + mon.h as f64 * along - wh as f64 / 2.0).round() as i32;
            (x, y.clamp(mon.y, mon.y + (mon.h - wh).max(0)))
        }
        Edge::Top | Edge::Bottom => {
            let y = match edge {
                Edge::Bottom => mon.y + mon.h - wh,
                _ => mon.y,
            };
            let x = (mon.x as f64 + mon.w as f64 * along - ww as f64 / 2.0).round() as i32;
            (x.clamp(mon.x, mon.x + (mon.w - ww).max(0)), y)
        }
    }
}

/// The fraction along the edge that a dragged window has landed on.
///
/// The inverse of `window_origin`, so a drag and a redraw agree. Kept here beside it for the
/// same reason: two functions that must be inverses drift apart when they live in two files.
pub fn along_from_origin(mon: Rect, edge: Edge, origin: (i32, i32), win: (i32, i32)) -> f64 {
    let (ww, wh) = win;
    if edge.is_vertical() {
        if mon.h == 0 {
            return 0.5;
        }
        ((origin.1 + wh / 2 - mon.y) as f64 / mon.h as f64).clamp(0.0, 1.0)
    } else {
        if mon.w == 0 {
            return 0.5;
        }
        ((origin.0 + ww / 2 - mon.x) as f64 / mon.w as f64).clamp(0.0, 1.0)
    }
}

/// Which monitor the pill belongs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The primary display. What the app has always done.
    Primary,
    /// Whichever display the pointer is on right now, re-decided as the pointer moves between
    /// them. For one-screen machines this is the same as `Primary` and costs nothing.
    Cursor,
    /// A specific display, remembered by the name Windows gives it.
    Named(String),
}

impl Target {
    pub fn parse(s: &str) -> Target {
        match s {
            "" | "primary" => Target::Primary,
            "cursor" => Target::Cursor,
            other => Target::Named(other.to_string()),
        }
    }
}

/// Choose a monitor, given what is currently attached.
///
/// The named case is the one that needs care: a display can be unplugged, switched off, or come
/// back with a different name after a driver update, and a pill placed on coordinates that no
/// longer exist is a pill nobody can see and nobody can find the setting for. So a name that
/// does not match falls back to the primary rather than to nothing, and the caller says so in
/// the log once.
pub fn choose_monitor<'a>(
    target: &Target,
    monitors: &'a [(String, Rect, bool)],
    cursor: Option<(i32, i32)>,
) -> Option<(&'a str, Rect, bool)> {
    if monitors.is_empty() {
        return None;
    }
    let primary = || {
        monitors
            .iter()
            .find(|(_, _, is_primary)| *is_primary)
            .or_else(|| monitors.first())
            .map(|(n, r, p)| (n.as_str(), *r, *p))
    };
    match target {
        Target::Primary => primary(),
        Target::Cursor => cursor
            .and_then(|(cx, cy)| monitors.iter().find(|(_, r, _)| r.contains(cx, cy)))
            .map(|(n, r, p)| (n.as_str(), *r, *p))
            // A cursor between monitors, or one this process cannot read, is not an error worth
            // moving the pill for.
            .or_else(primary),
        Target::Named(want) => monitors
            .iter()
            .find(|(name, _, _)| name == want)
            .map(|(n, r, p)| (n.as_str(), *r, *p))
            .or_else(primary),
    }
}

/// Whether the pill should get out of the way.
///
/// The rule people expect is "hide when something is full screen", and on one monitor that is
/// what this is. On several it is not: a game full screen on the left display is no reason to
/// hide a pill sitting on the right one, and hiding it anyway is the behaviour that makes people
/// switch the feature off.
///
/// So the test is per monitor. A window counts as full screen when it covers its own monitor's
/// entire bounds - not the work area, which excludes the taskbar and is what a merely maximised
/// window fills. That difference is the whole check: maximised is not full screen, and hiding
/// for a maximised browser would make the pill useless.
pub fn should_hide(
    enabled: bool,
    foreground: Option<(Rect, Rect)>,
    pill_monitor: Rect,
) -> bool {
    if !enabled {
        return false;
    }
    let Some((window, monitor)) = foreground else {
        return false;
    };
    if monitor != pill_monitor {
        return false;
    }
    // Covering, not equalling. Some players sit a pixel outside the monitor on one side, and a
    // strict equality test would call that windowed.
    window.x <= monitor.x
        && window.y <= monitor.y
        && window.x + window.w >= monitor.x + monitor.w
        && window.y + window.h >= monitor.y + monitor.h
}

#[cfg(test)]
#[path = "placement_tests.rs"]
mod tests;
