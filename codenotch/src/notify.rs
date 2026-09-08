//! A desktop notification when a window crosses into the red, and the arming that stops it
//! becoming noise.
//!
//! Ported from the taskbar mod, which has had this since before this port existed. The pill
//! answers "how much is left" whenever you look at it; this is for when you are not looking,
//! which is most of the time and is exactly when a window fills up.
//!
//! # Arming, which is the whole feature
//!
//! A notification that fires whenever usage is above a threshold fires on every poll for hours,
//! and gets muted. So the state remembered per window is not "is it above" but "have we already
//! said so": each window is armed while it sits below the threshold, fires once on the way up,
//! and re-arms when it drops back. The mod calls this `redState` and keeps three values rather
//! than two - unknown, armed, fired - so that the first reading after a restart primes the
//! state instead of firing on it. Somebody who launches the app at 85 % should see the number,
//! not a notification about a crossing that happened before the app was running.

use std::collections::HashMap;

/// A window's position relative to the threshold, as far as notifications are concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arm {
    /// Nothing has been seen yet. The next reading primes without firing.
    Unknown,
    /// Below the threshold. A crossing from here fires.
    Below,
    /// Above, and already reported.
    Fired,
}

/// One reading, reduced to what the decision needs.
pub struct Reading<'a> {
    pub key: &'a str,
    pub used: f64,
}

/// Decide what to say, and update the arming as a side effect.
///
/// Split from everything that touches the operating system so the sequence of readings that
/// produces exactly one notification can be tested directly, which is the part that is easy to
/// get subtly wrong and hard to notice.
pub fn crossings(state: &mut HashMap<String, Arm>, readings: &[Reading], threshold: f64) -> Vec<String> {
    let mut fired = Vec::new();
    for r in readings {
        let before = *state.get(r.key).unwrap_or(&Arm::Unknown);
        let above = r.used >= threshold;
        let after = if above { Arm::Fired } else { Arm::Below };
        // Only an armed window fires. `Unknown` primes silently, which is what makes starting
        // the app at 90 % a number rather than an alert about something that already happened.
        if above && before == Arm::Below {
            fired.push(r.key.to_string());
        }
        state.insert(r.key.to_string(), after);
    }
    fired
}

/// Raise a balloon from a hidden tray icon of our own.
///
/// The mod does this with `Shell_NotifyIcon` and it works, so this does too rather than adding
/// a notification plugin: the dependency would be new, the capability plumbing would be new,
/// and the result on Windows is the same balloon. The icon is created hidden and reused, so
/// nothing extra appears in the tray beside the one this app already owns.
#[cfg(windows)]
pub fn toast(title: &str, body: &str) {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_STATE, NIIF_WARNING, NIM_ADD, NIM_MODIFY,
        NIS_HIDDEN, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, LoadIconW, RegisterClassExW, HWND_MESSAGE, IDI_WARNING,
        WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW,
    };

    /// The message-only window that owns the balloon, created once and reused.
    static WND: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    unsafe {
        let mut hwnd = HWND(WND.load(Ordering::SeqCst) as *mut core::ffi::c_void);
        if hwnd.0.is_null() {
            let class = w!("CodenotchNotify");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: HINSTANCE::default(),
                lpszClassName: class,
                ..Default::default()
            };
            // A second registration answers ERROR_CLASS_ALREADY_EXISTS, which is fine: the
            // window below reuses the class either way, and this only runs once per process
            // anyway.
            RegisterClassExW(&wc);
            let Ok(created) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                HINSTANCE::default(),
                None,
            ) else {
                return;
            };
            hwnd = created;

            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_STATE,
                dwState: NIS_HIDDEN,
                dwStateMask: NIS_HIDDEN,
                ..Default::default()
            };
            nid.hIcon = LoadIconW(None, IDI_WARNING).unwrap_or_default();
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                return;
            }
            WND.store(hwnd.0 as isize, Ordering::SeqCst);
        }

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_WARNING,
            ..Default::default()
        };
        write_utf16(&mut nid.szInfoTitle, title);
        write_utf16(&mut nid.szInfo, body);
        // A failed balloon is not worth reporting anywhere the user would see it: notifications
        // can be switched off system-wide, and complaining about that on screen would be the
        // app arguing with a setting.
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Copy a string into a fixed UTF-16 buffer, truncated to fit with room for the terminator.
///
/// Truncation is by UTF-16 unit rather than by character on purpose: these buffers are sized in
/// units, and a title cut mid-surrogate would be a malformed string handed to the shell. Taking
/// whole `encode_utf16` items and stopping one short of the end cannot split a pair, because a
/// pair is two consecutive items and the loop simply stops before the second.
#[cfg(windows)]
fn write_utf16(buf: &mut [u16], s: &str) {
    let mut units: Vec<u16> = s.encode_utf16().take(buf.len().saturating_sub(1)).collect();
    // A trailing lone surrogate can only be a high one left by the truncation above; drop it.
    if matches!(units.last(), Some(u) if (0xD800..0xDC00).contains(u)) {
        units.pop();
    }
    for (slot, u) in buf.iter_mut().zip(units.iter()) {
        *slot = *u;
    }
    if let Some(slot) = buf.get_mut(units.len()) {
        *slot = 0;
    }
}

#[cfg(not(windows))]
pub fn toast(_title: &str, _body: &str) {}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;

// ---------------------------------------------------------------- the watcher

/// Watch every provider's windows and fire once on each crossing into the red.
///
/// One thread reading the snapshots, rather than a hook in each provider's publish path. The
/// providers already write their readings into shared state and there are four of them, so a
/// call site per provider would be four places for the arming rule to drift apart - and the
/// rule is the part that matters. Thirty seconds late is not late for a threshold that took
/// hours to reach.
pub fn start_watcher(app: tauri::AppHandle) {
    use tauri::Manager;
    std::thread::spawn(move || {
        let mut state: HashMap<String, Arm> = HashMap::new();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let cfg = crate::config::load();
            if !cfg.notify_threshold {
                // Still primed while switched off, so turning it back on does not fire for a
                // crossing that happened while it was silent.
                state.clear();
                continue;
            }
            let st = app.state::<crate::AppState>();
            let snaps = [
                ("Claude", "claude", st.usage.lock().unwrap().clone()),
                ("Codex", "codex", st.codex.lock().unwrap().clone()),
                ("Cursor", "cursor", st.cursor.lock().unwrap().clone()),
                ("Antigravity", "gemini", st.antigravity.lock().unwrap().clone()),
            ];
            let mut readings = Vec::new();
            let mut labels: HashMap<String, (String, String, Option<u64>)> = HashMap::new();
            for (display, id, snap) in &snaps {
                // A switched-off provider is not watched. Its reading is frozen at whatever it
                // was, and firing on a stale number would be reporting the past as news.
                if cfg.is_disconnected(id) || snap.status == "needsAuth" || snap.status == "none" {
                    continue;
                }
                for w in &snap.windows {
                    // Only metered windows. A count with no denominator has no threshold to
                    // cross, and inventing one is the thing this app does not do.
                    if w.count.is_some() {
                        continue;
                    }
                    let key = format!("{id}:{}", w.id);
                    labels.insert(key.clone(), ((*display).to_string(), w.label.clone(), w.resets_at));
                    readings.push((key, w.used));
                }
            }
            let refs: Vec<Reading> = readings.iter().map(|(k, u)| Reading { key: k, used: *u }).collect();
            for key in crossings(&mut state, &refs, cfg.red_threshold) {
                let Some((provider, window, resets_at)) = labels.get(&key) else { continue };
                let used = readings.iter().find(|(k, _)| *k == key).map(|(_, u)| *u).unwrap_or(0.0);
                let title = format!("{provider}: {window} at {}%", (used * 100.0).round());
                let body = match resets_at {
                    Some(ms) => format!("Resets {}", crate::notify::until(*ms)),
                    None => "No reset time published for this window".to_string(),
                };
                crate::applog(&format!("notify: {title} - {body}"));
                toast(&title, &body);
            }
        }
    });
}

/// "in 47m", "in 2h 14m", "in 3d". The same shape as the pill's countdown, spelled out because
/// a notification is read once and out of context.
pub(crate) fn until(resets_at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if resets_at_ms <= now {
        return "shortly".into();
    }
    let mins = (resets_at_ms - now) / 60_000;
    if mins < 60 {
        format!("in {}m", mins.max(1))
    } else if mins < 24 * 60 {
        format!("in {}h {:02}m", mins / 60, mins % 60)
    } else {
        format!("in {}d", mins / (24 * 60))
    }
}
