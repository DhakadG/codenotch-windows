#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]
// These two lints flag the shape of the module documentation, not the code: continuation
// lines in the numbered lists that explain each provider's data paths. Reflowing that prose
// would be churn of exactly the kind we chose to avoid by leaving `cargo fmt` out of CI,
// and it would bury a real diff under whitespace in review. The docs render correctly as
// written; revisit if they ever stop doing so.
#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

mod autostart;
mod config;
mod doctor;
mod focus;
mod hooks_install;
mod notify;
mod placement;
mod oauth;
mod window_start;
mod i18n;
mod server;
mod state;
mod tray;
mod usage;
mod codex;
mod cursor;
mod antigravity;
mod glyphs;
mod activity;
mod diag;
mod watcher;

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Logical size of the notch window: the 70 pt pill column on the right plus room for the hover card on the left.
pub const NOTCH_W: f64 = 340.0;
/// Build identity, stamped by build.rs from the git commit rather than typed by hand.
///
/// Written to run.log at startup and printed by `codenotch.exe version`, so a running copy
/// can always be matched to the source it was built from - and, just as importantly, so an
/// installer that failed to replace the executable is immediately obvious instead of
/// looking exactly like one that worked.
pub const BUILD: &str = env!("CODENOTCH_BUILD");
/// Unix seconds at which this binary was compiled.
pub const BUILT_AT: &str = env!("CODENOTCH_BUILT_AT");

/// One line identifying exactly what is running.
pub fn version_line() -> String {
    let built = BUILT_AT.parse::<i64>().ok().and_then(|s| {
        chrono::DateTime::from_timestamp(s, 0).map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
    });
    format!(
        "Codenotch {} (build {}, compiled {})",
        env!("CARGO_PKG_VERSION"),
        BUILD,
        built.unwrap_or_else(|| "unknown".into())
    )
}
pub const NOTCH_H: f64 = 460.0; // 300 clipped the card once it held three window blocks plus the session list

pub struct AppState {
    pub store: Mutex<state::Store>,
    pub cfg: Mutex<config::Config>,
    pub usage: Mutex<usage::UsageSnapshot>,
    /// Codex snapshot (same UsageSnapshot shape; status may also be none/absent)
    pub codex: Mutex<usage::UsageSnapshot>,
    pub cursor: Mutex<usage::UsageSnapshot>,
    pub antigravity: Mutex<usage::UsageSnapshot>,
    /// Provider glyph cache, collected at launch and again on a tray refresh
    pub glyphs: Mutex<std::collections::HashMap<String, glyphs::Glyph>>,
    /// Working state of the non-Claude providers (Cursor reports it; Codex and Antigravity are inferred from recent writes)
    pub activity: Mutex<Vec<activity::Activity>>,
}

fn resolved_lang(raw: &str) -> String {
    if raw == "auto" {
        i18n::resolve_auto().to_string()
    } else {
        raw.to_string()
    }
}

pub fn broadcast(app: &AppHandle) {
    let st = app.state::<AppState>();
    let snap = {
        let store = st.store.lock().unwrap();
        let cfg = st.cfg.lock().unwrap();
        store.snapshot(&cfg.lang, &resolved_lang(&cfg.lang), false)
    };
    let _ = app.emit("state", &snap);
}

/// Every attached display, as `placement` wants them.
///
/// Tauri gives names, positions, sizes and scale factors already, so nothing here needs Win32.
/// The name is what gets persisted, because index and position both change when a display is
/// unplugged and a remembered index would silently mean a different screen.
fn monitors_of(w: &tauri::WebviewWindow) -> Vec<(String, placement::Rect, bool, f64)> {
    let primary = w
        .primary_monitor()
        .ok()
        .flatten()
        .and_then(|m| m.name().cloned());
    w.available_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let name = m.name().cloned().unwrap_or_default();
            let is_primary = primary.as_ref() == Some(&name);
            (
                name,
                placement::Rect {
                    x: m.position().x,
                    y: m.position().y,
                    w: m.size().width as i32,
                    h: m.size().height as i32,
                },
                is_primary,
                m.scale_factor(),
            )
        })
        .collect()
}

/// The monitor the pill is currently placed on, for the full-screen watcher to compare against.
static PILL_MONITOR: Mutex<Option<placement::Rect>> = Mutex::new(None);

/// Put the pill on the configured edge of the configured display.
///
/// Was twenty lines pinning it to the right edge of the primary monitor. Each of those is now a
/// setting, and the mixed-DPI care the original needed applies to all of them: a second monitor
/// at a different scale can have the physical size computed with the *other* monitor's factor,
/// which left the WebView 256 logical pixels wide instead of 340. So the size is pinned from the
/// chosen monitor's own scale before positioning, and pinned again if it still disagrees.
pub fn place_notch(app: &AppHandle) {
    let Some(w) = app.get_webview_window("notch") else {
        return;
    };
    let (target, edge, along) = {
        let st = app.state::<AppState>();
        let c = st.cfg.lock().unwrap();
        (
            placement::Target::parse(&c.monitor),
            placement::Edge::parse(&c.edge),
            c.notch_y.clamp(0.0, 1.0),
        )
    };

    let all = monitors_of(&w);
    let simple: Vec<(String, placement::Rect, bool)> =
        all.iter().map(|(n, r, p, _)| (n.clone(), *r, *p)).collect();
    let cursor = app.cursor_position().ok().map(|c| (c.x as i32, c.y as i32));
    let Some((name, mon, _)) = placement::choose_monitor(&target, &simple, cursor) else {
        applog("notch placement: no monitors reported; leaving the window where it is");
        return;
    };
    // Said once when it happens, because a pill that moved on its own is otherwise inexplicable.
    if let placement::Target::Named(wanted) = &target {
        if wanted != name {
            applog(&format!(
                "notch placement: display {wanted} is not attached; using {name} instead"
            ));
        }
    }
    let ms = all
        .iter()
        .find(|(n, _, _, _)| n == name)
        .map(|(_, _, _, s)| *s)
        .unwrap_or(1.0);

    let (lw, lh) = edge.size();
    let target_size =
        tauri::PhysicalSize::new((lw * ms).round() as u32, (lh * ms).round() as u32);
    let _ = w.set_size(target_size);
    // Measured rather than derived: computing the position from the scale factor pushed the
    // window past the edge at 125 % and 150 %, clipping the ring.
    let (ww, wh) = w
        .outer_size()
        .map(|s| (s.width as i32, s.height as i32))
        .unwrap_or((target_size.width as i32, target_size.height as i32));
    let (x, y) = placement::window_origin(mon, edge, along, (ww, wh));
    let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    if w.outer_size().map(|s| s.width != target_size.width).unwrap_or(false) {
        let _ = w.set_size(target_size);
        let (x2, y2) = placement::window_origin(
            mon,
            edge,
            along,
            (target_size.width as i32, target_size.height as i32),
        );
        let _ = w.set_position(tauri::PhysicalPosition::new(x2, y2));
    }
    *PILL_MONITOR.lock().unwrap() = Some(mon);

    // Placement log line: the first thing to check when the notch is not visible.
    let log = config::config_path().with_file_name("run.log");
    let _ = std::fs::write(
        log,
        format!(
            "notch placed build={BUILD}: edge={} display={name} pos=({x},{y}) size=({ww}x{wh}) mon_scale={ms} monitor=({},{} {}x{})\n",
            edge.as_str(),
            mon.x,
            mon.y,
            mon.w,
            mon.h
        ),
    );
}

/// Hide the pill while something is full screen on its own display, and bring it back after.
///
/// Polled rather than hooked. `SetWinEventHook` would be the tidy answer and needs a message
/// loop of its own on a thread that must then outlive every other thread here; a second of
/// latency on a state that changes when someone alt-tabs into a game is not worth that.
#[cfg(windows)]
pub fn start_fullscreen_watcher(app: AppHandle) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

    std::thread::spawn(move || {
        let mut hidden = false;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let enabled = config::load().hide_on_fullscreen;
            let pill = *PILL_MONITOR.lock().unwrap();
            let Some(pill) = pill else { continue };

            let foreground = unsafe {
                let hwnd = GetForegroundWindow();
                if hwnd.0.is_null() {
                    None
                } else {
                    let mut wr = RECT::default();
                    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut mi = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    if GetWindowRect(hwnd, &mut wr).is_ok()
                        && GetMonitorInfoW(hmon, &mut mi).as_bool()
                    {
                        let to_rect = |r: RECT| placement::Rect {
                            x: r.left,
                            y: r.top,
                            w: r.right - r.left,
                            h: r.bottom - r.top,
                        };
                        Some((to_rect(wr), to_rect(mi.rcMonitor)))
                    } else {
                        None
                    }
                }
            };
            // Our own window is never the reason to hide our own window. It is borderless and
            // always on top, so on a small display its rect can cover the monitor and the check
            // would hide the pill the moment it was clicked.
            let foreground = foreground.filter(|(win, _)| {
                app.get_webview_window("notch")
                    .and_then(|w| w.outer_position().ok().zip(w.outer_size().ok()))
                    .map(|(p, s)| {
                        !(win.x == p.x && win.y == p.y
                            && win.w == s.width as i32
                            && win.h == s.height as i32)
                    })
                    .unwrap_or(true)
            });

            let want_hidden = placement::should_hide(enabled, foreground, pill);
            if want_hidden != hidden {
                if let Some(w) = app.get_webview_window("notch") {
                    let _ = if want_hidden { w.hide() } else { w.show() };
                }
                applog(&format!(
                    "fullscreen watcher: {} the pill",
                    if want_hidden { "hid" } else { "restored" }
                ));
                hidden = want_hidden;
            }
        }
    });
}

#[cfg(not(windows))]
pub fn start_fullscreen_watcher(_app: AppHandle) {}

/// Older entry point name still used by tray.rs
pub fn reset_bar(app: &AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.notch_y = 0.5;
        config::save(&c);
    }
    place_notch(app);
}

/// Drag along the right edge. The page calls this once after a press on the pill moves more than
/// 4 px; from then on a Rust thread follows the system cursor (WebView mousemove is unreliable
/// once the window itself starts moving). Releasing the left button ends the drag and the centre
/// ratio is written back to the config.
static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
fn left_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}
#[cfg(not(windows))]
fn left_button_down() -> bool {
    false
}

#[tauri::command]
fn drag_begin(app: AppHandle) {
    if DRAGGING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let Some(w) = app.get_webview_window("notch") else {
            DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let (Ok(start_cur), Ok(start_pos), Ok(size)) =
            (app.cursor_position(), w.outer_position(), w.outer_size())
        else {
            DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        // The monitor the pill is actually on, not the primary one. Dragging a pill that lives
        // on a second display used to be clamped to the primary display's height, which on a
        // taller secondary meant it stopped half way down and on a shorter one meant it could be
        // dragged off the bottom.
        let edge = placement::Edge::parse(&app.state::<AppState>().cfg.lock().unwrap().edge);
        let Some(mon) = *PILL_MONITOR.lock().unwrap() else {
            DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let win = (size.width as i32, size.height as i32);
        let mut last = (start_pos.x, start_pos.y);
        let mut moved = false;
        loop {
            if !left_button_down() {
                break;
            }
            if let Ok(cur) = app.cursor_position() {
                // Only the axis the edge runs along moves. The other stays welded, which is what
                // makes this a slide rather than a free drag - the pill belongs to an edge, and
                // letting it come away from one would need somewhere to put it back.
                let along = if edge.is_vertical() {
                    let y = (start_pos.y as f64 + (cur.y - start_cur.y)).round() as i32;
                    placement::along_from_origin(mon, edge, (start_pos.x, y), win)
                } else {
                    let x = (start_pos.x as f64 + (cur.x - start_cur.x)).round() as i32;
                    placement::along_from_origin(mon, edge, (x, start_pos.y), win)
                };
                let next = placement::window_origin(mon, edge, along, win);
                if next != last {
                    last = next;
                    moved = true;
                    let _ = w.set_position(tauri::PhysicalPosition::new(next.0, next.1));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        if moved {
            let ratio = placement::along_from_origin(mon, edge, last, win);
            let st = app.state::<AppState>();
            let mut c = st.cfg.lock().unwrap();
            c.notch_y = ratio;
            config::save(&c);
            applog(&format!(
                "notch drag: edge={} pos=({},{}) ratio={ratio:.3}",
                edge.as_str(),
                last.0,
                last.1
            ));
        }
        DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = app.emit("drag_end", moved);
    });
}

pub fn place_bar(app: &AppHandle) {
    place_notch(app);
}
pub fn toggle_drag(app: &AppHandle) {
    // The notch stays welded to the edge; kept as a no-op for the tray menu code path
    let _ = app;
}

pub fn apply_lang(app: &AppHandle, lang: &str) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.lang = lang.to_string();
        config::save(&c);
    }
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = tray::build_menu(app, lang) {
            let _ = tray.set_menu(Some(menu));
        }
    }
    broadcast(app);
}

/// The notch must never take focus: WS_EX_NOACTIVATE + WS_EX_TOOLWINDOW
#[cfg(windows)]
fn noactivate(app: &AppHandle) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };
    if let Some(w) = app.get_webview_window("notch") {
        if let Ok(h) = w.hwnd() {
            unsafe {
                let hwnd =
                    windows::Win32::Foundation::HWND(h.0 as isize as *mut core::ffi::c_void);
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
                );
            }
        }
    }
}
#[cfg(not(windows))]
fn noactivate(_app: &AppHandle) {}

// ---------------- commands ----------------

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> state::Snapshot {
    let store = state.store.lock().unwrap();
    let cfg = state.cfg.lock().unwrap();
    store.snapshot(&cfg.lang, &resolved_lang(&cfg.lang), false)
}

#[tauri::command]
fn get_usage(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.usage.lock().unwrap().clone()
}

#[tauri::command]
fn refresh_usage(app: AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut u = st.usage.lock().unwrap();
        u.backoff_until = 0;
    }
    usage::request_refresh();
    codex::request_refresh();
    cursor::request_refresh();
    antigravity::request_refresh();
}

#[tauri::command]
fn get_antigravity(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.antigravity.lock().unwrap().clone()
}

#[tauri::command]
fn get_activity(state: tauri::State<AppState>) -> Vec<activity::Activity> {
    state.activity.lock().unwrap().clone()
}

#[tauri::command]
fn get_glyphs(state: tauri::State<AppState>) -> std::collections::HashMap<String, glyphs::Glyph> {
    state.glyphs.lock().unwrap().clone()
}

/// Collects the glyphs again and pushes them to the page (tray refresh, or the user just dropped in an override)
pub fn reload_glyphs(app: &AppHandle) {
    let m = glyphs::collect();
    let st = app.state::<AppState>();
    *st.glyphs.lock().unwrap() = m.clone();
    let _ = app.emit("glyphs", &m);
}

#[tauri::command]
fn open_data_dir() {
    let dir = config::config_path().parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let _ = std::fs::create_dir_all(glyphs::user_dir());
    let mut cmd = std::process::Command::new("explorer");
    cmd.arg(dir.as_os_str());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

#[tauri::command]
fn get_cursor(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.cursor.lock().unwrap().clone()
}

#[tauri::command]
fn get_codex(state: tauri::State<AppState>) -> usage::UsageSnapshot {
    state.codex.lock().unwrap().clone()
}

/// The subset of the config the pill needs: which providers to draw and which window the
/// ring should follow. Kept to exactly that, so the UI never has to know about ports, drag
/// state or window geometry.
#[derive(serde::Serialize, Clone)]
pub struct Prefs {
    pub hidden_providers: Vec<String>,
    pub ring_window: String,
    pub show_percent: bool,
    pub show_countdown: bool,
    pub show_pace_tick: bool,
    pub show_activity_arc: bool,
    pub show_weekly_ring: bool,
    pub show_hour_marks: bool,
    pub remaining_mode: bool,
    pub colorblind: bool,
    pub show_stale_warning: bool,
    pub red_threshold: f64,
    pub edge: String,
    pub opacity: f64,
    pub float_pill: bool,
}

impl Prefs {
    /// Built in one place so a field added here cannot reach the page through the initial
    /// read and then go missing from the update, or the other way round.
    fn from_config(c: &config::Config) -> Self {
        Prefs {
            hidden_providers: c.hidden_providers.clone(),
            ring_window: c.ring_window.clone(),
            show_percent: c.show_percent,
            show_countdown: c.show_countdown,
            show_pace_tick: c.show_pace_tick,
            show_activity_arc: c.show_activity_arc,
            show_weekly_ring: c.show_weekly_ring,
            show_hour_marks: c.show_hour_marks,
            remaining_mode: c.remaining_mode,
            colorblind: c.colorblind,
            show_stale_warning: c.show_stale_warning,
            red_threshold: c.red_threshold,
            edge: c.edge.clone(),
            opacity: c.opacity.clamp(0.25, 1.0),
            float_pill: c.float_pill,
        }
    }
}

#[tauri::command]
fn get_prefs(state: tauri::State<AppState>) -> Prefs {
    Prefs::from_config(&state.cfg.lock().unwrap())
}

/// Pushes the current preferences to the pill. Called after any tray toggle.
pub fn broadcast_prefs(app: &AppHandle) {
    let prefs = {
        let st = app.state::<AppState>();
        let c = st.cfg.lock().unwrap();
        Prefs::from_config(&c)
    };
    let _ = app.emit("prefs", &prefs);
}

/// Re-poll one provider, or every provider when given "all".
///
/// This is what a single click on a cell now does. It only asks the existing poll loop to
/// wake early; the refetch floor and the request ceiling still apply, so leaning on the
/// mouse cannot turn into a burst of requests.
#[tauri::command]
fn refresh_provider(app: AppHandle, provider: String) {
    let all = provider == "all";
    if all || provider == "claude" {
        {
            let st = app.state::<AppState>();
            let mut u = st.usage.lock().unwrap();
            u.backoff_until = 0;
        }
        usage::request_refresh();
    }
    if all || provider == "codex" {
        codex::request_refresh();
    }
    if all || provider == "cursor" {
        cursor::request_refresh();
    }
    if all || provider == "gemini" {
        antigravity::request_refresh();
    }
}

/// Hide a provider from the pill, from its own right-click menu.
///
/// The same setting the tray offers; having it here too means the cell can be dismissed
/// where it is, rather than by hunting for the tray icon.
#[tauri::command]
fn hide_provider(app: AppHandle, provider: String) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        if !c.hidden_providers.contains(&provider) {
            c.hidden_providers.push(provider);
            config::save(&c);
        }
    }
    broadcast_prefs(&app);
    let _ = tray::rebuild(&app);
}

/// Hand a URL or a path to the Windows shell, which opens it with whatever owns it.
///
/// `ShellExecuteW`, not `cmd /C start`. The shell was fine while every URL here was a bare
/// page address, and stops being fine the moment one carries a query: `cmd.exe` reparses its
/// command line and treats `&` as a command separator, so an OAuth authorize URL - which is
/// nothing but `&`-joined parameters - was cut off at the first one. The sign-in would have
/// failed with an error from Anthropic about a missing parameter, pointing at the wrong thing
/// entirely. No shell, no reparsing, no quoting rules to get right.
pub fn open_in_browser(target: &str) {
    #[cfg(windows)]
    {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
    #[cfg(not(windows))]
    let _ = target;
}

/// A click on a cell opens that provider's usage page
#[tauri::command]
fn open_provider_page(provider: String) {
    let url = match provider.as_str() {
        "codex" => "https://chatgpt.com/#settings/Account",
        "cursor" => "https://cursor.com/dashboard",
        "gemini" => "https://antigravity.google",
        _ => "https://claude.ai/settings/usage",
    };
    open_in_browser(url);
}

/// Start a sign-in: open Anthropic's authorization page and the local page that collects the
/// code it hands back.
///
/// Two tabs rather than one, because Anthropic's callback shows the code on its own page for
/// copying instead of redirecting to a loopback port - so something has to be waiting to take
/// the paste. That something is the event server this app already runs, which means no dialog
/// code, no second window, and a page that can say what went wrong in a sentence.
pub fn begin_sign_in(app: &AppHandle) {
    match oauth::begin() {
        Ok(url) => {
            let port = {
                let st = app.state::<AppState>();
                let c = st.cfg.lock().unwrap();
                c.port
            };
            open_in_browser(&url);
            open_in_browser(&format!("http://127.0.0.1:{port}/signin"));
        }
        Err(e) => {
            applog(&format!("sign-in could not start: {e}"));
            let _ = app.emit("notice", format!("Sign-in could not start: {e}"));
        }
    }
}


/// The whole configuration, plus the things the settings window needs to render choices it
/// cannot work out on its own.
///
/// The config is sent as itself rather than through a hand-written data transfer object. A DTO
/// would be one more list of fields to keep in step with `Config`, and the failure mode of
/// forgetting is a setting that silently cannot be changed - which is exactly the class of bug
/// this window exists to remove.
#[derive(serde::Serialize)]
pub struct SettingsView {
    #[serde(flatten)]
    cfg: config::Config,
    /// Build identity, so a screenshot of this window is attributable to a build.
    build: String,
    monitors: Vec<MonitorChoice>,
}

#[derive(serde::Serialize)]
pub struct MonitorChoice {
    /// What gets stored: the device name, or the two special values.
    id: String,
    /// What a person can recognise.
    label: String,
}

#[tauri::command]
fn settings_load(app: AppHandle, state: tauri::State<AppState>) -> SettingsView {
    let cfg = state.cfg.lock().unwrap().clone();
    let mut monitors = vec![
        MonitorChoice { id: "primary".into(), label: "Primary display".into() },
        MonitorChoice { id: "cursor".into(), label: "Wherever the pointer is".into() },
    ];
    if let Some(w) = app.get_webview_window("notch") {
        for (name, rect, is_primary, scale) in monitors_of(&w) {
            // The device name is unreadable on its own - "\\.\DISPLAY2" tells nobody which
            // screen that is - so it is offered with the size and position that identify it.
            monitors.push(MonitorChoice {
                label: format!(
                    "{}\u{00d7}{} at ({}, {}){}{}",
                    rect.w,
                    rect.h,
                    rect.x,
                    rect.y,
                    if is_primary { " \u{2013} primary" } else { "" },
                    if (scale - 1.0).abs() > 0.01 {
                        format!(" \u{2013} {}%", (scale * 100.0).round())
                    } else {
                        String::new()
                    }
                ),
                id: name,
            });
        }
    }
    SettingsView { cfg, build: version_line(), monitors }
}

#[tauri::command]
fn settings_save(app: AppHandle, next: config::Config) {
    // Clamped here rather than trusted from the page. The window is ours, but it is still the
    // outside of this boundary, and an opacity of zero or a threshold above one would be a
    // setting the user cannot see well enough to undo.
    let mut next = next;
    next.opacity = next.opacity.clamp(0.25, 1.0);
    next.red_threshold = next.red_threshold.clamp(0.5, 0.95);
    next.notch_y = next.notch_y.clamp(0.0, 1.0);
    next.edge = placement::Edge::parse(&next.edge).as_str().to_string();

    let lang_changed = {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        let changed = c.lang != next.lang;
        *c = next;
        config::save(&c);
        changed
    };
    broadcast_prefs(&app);
    place_notch(&app);
    if lang_changed {
        apply_lang(&app, &config::load().lang);
    }
    let _ = tray::rebuild(&app);
    applog("settings: saved");
}

#[tauri::command]
fn open_data_folder() {
    let dir = config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let _ = std::fs::create_dir_all(&dir);
    open_in_browser(&dir.display().to_string());
}

#[tauri::command]
fn reset_position(app: AppHandle) {
    reset_bar(&app);
}

/// Show the settings window, creating it if it has been closed.
///
/// Closing a Tauri window destroys it, so a second open has to build it again rather than
/// unhide something that is no longer there.
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    match tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("Codenotch settings")
    .inner_size(640.0, 720.0)
    .min_inner_size(460.0, 420.0)
    .center()
    .build()
    {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => applog(&format!("settings window could not be created: {e}")),
    }
}

/// Card expansion state: Some(hot rectangles, in **physical pixels** relative to the window's
/// top-left as x,y,w,h) = expanded; None = collapsed. The page converts the rectangles with its
/// own devicePixelRatio before reporting them, so no scale conversion happens on this side —
/// WebView2's DPR and the window's scale_factor can disagree (see report_dpr).
static HOT: Mutex<Option<Vec<[f64; 4]>>> = Mutex::new(None);

#[tauri::command]
fn set_expanded(on: bool, rects: Option<Vec<[f64; 4]>>) {
    *HOT.lock().unwrap() = if on { Some(rects.unwrap_or_default()) } else { None };
}

/// Everything the page currently draws, in the same physical-pixel window coordinates as `HOT`:
/// the pill, and the card, menu and notice while they are on screen.
///
/// The window is 340×460 but the pill only uses a 70 pt column of it, so the rest is a
/// transparent sheet that used to swallow every click aimed at whatever sits underneath.
///
/// This is upstream's `interactiveRects` under a different mechanism. `NotchHostingView` on
/// macOS overrides `hitTest` and returns nil outside those rectangles, so AppKit resolves the
/// click to whatever is behind — exact, per event, free. WebView2 offers no equivalent hook, so
/// the same rule is applied the only way Windows allows: the watchdog compares the system cursor
/// against these rectangles and toggles `WS_EX_TRANSPARENT` on the whole window. Same rule, same
/// rectangles, sampled rather than exact — hence the margin in `point_in_rects`.
///
/// Empty means "the page has not told us yet", and click-through stays off — a page that fails
/// to report must leave the app usable, not make it impossible to click.
static INTERACTIVE_RECTS: Mutex<Vec<[f64; 4]>> = Mutex::new(Vec::new());

#[tauri::command]
fn set_interactive_rects(rects: Vec<[f64; 4]>) {
    *INTERACTIVE_RECTS.lock().unwrap() = rects;
}

/// The WebView zoom currently applied (1.0 = uncorrected)
static ZOOM: Mutex<f64> = Mutex::new(1.0);

pub fn applog(line: &str) {
    use std::io::Write;
    let log = config::config_path().with_file_name("run.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

/// Root cause: with two monitors (150 % / 200 %) WebView2 picked a devicePixelRatio of 2.0 while
/// the window was sized for the primary monitor's 1.5, so the page was 255 CSS px wide instead of
/// the designed 340 and every coordinate conversion was off (the watchdog misfired and the card
/// flashed away). Fix: the page reports its DPR, and when it differs from the primary monitor's
/// scale, set_zoom pulls the effective DPR back to that scale, restoring the 340 px width.
#[tauri::command]
fn report_dpr(app: AppHandle, dpr: f64, w: f64, h: f64) {
    let Some(win) = app.get_webview_window("notch") else { return };
    let want = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or_else(|| win.scale_factor().unwrap_or(1.0));
    let mut z = ZOOM.lock().unwrap();
    let base = if *z > 0.0 { dpr / *z } else { dpr };
    let target = if base > 0.0 { want / base } else { 1.0 };
    applog(&format!(
        "dpr report: dpr={dpr:.3} viewport={w:.0}x{h:.0} monitor_scale={want:.3} zoom_applied={:.3} -> target_zoom={target:.3}",
        *z
    ));
    // Oscillation guard: at most three corrections per process (if the DPR does not follow the zoom, stop chasing it)
    static APPLIED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if (dpr - want).abs() > 0.02
        && (target - *z).abs() > 0.01
        && (0.25..=4.0).contains(&target)
        && APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
    {
        match win.set_zoom(target) {
            Ok(()) => {
                *z = target;
                applog(&format!("dpr correction: set_zoom({target:.3}) ok"));
            }
            Err(e) => applog(&format!("dpr correction failed: {e}")),
        }
    }
}

/// Whether a point lies in any of the rectangles, with a margin.
///
/// The margin matters for click-through rather than for hit-testing: the cursor is sampled on a
/// timer, so a fast approach can be a few pixels short of the pill on the tick before the click
/// arrives. Widening the interactive area slightly costs nothing — the widened band is
/// transparent and does nothing on click — and it removes the dead first click.
fn point_in_rects(x: f64, y: f64, rects: &[[f64; 4]], pad: f64) -> bool {
    rects
        .iter()
        .any(|r| x >= r[0] - pad && y >= r[1] - pad && x < r[0] + r[2] + pad && y < r[1] + r[3] + pad)
}

/// Make the window transparent to the mouse everywhere the page draws nothing.
///
/// Called on every watchdog tick. It only touches the window when the answer changes, because
/// `set_ignore_cursor_events` is a real window-style change and calling it at 20 Hz for no reason
/// is exactly the kind of churn that makes an always-on-top overlay feel unstable.
fn update_click_through(app: &AppHandle) {
    // Never during a drag: the cursor leaves the pill as the window follows it, and turning the
    // window click-through mid-drag would hand the button press to whatever is underneath.
    if DRAGGING.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(w) = app.get_webview_window("notch") else { return };
    let (Ok(pos), Ok(cur)) = (w.outer_position(), app.cursor_position()) else { return };
    let rects = INTERACTIVE_RECTS.lock().unwrap().clone();
    // No rectangles means the whole window stays clickable, and it has to be handled here
    // rather than by returning early: an empty list arriving *after* click-through was turned
    // on would otherwise leave the window transparent to the mouse with nothing able to turn
    // it back, which is the one outcome this guard exists to prevent.
    let want_ignore =
        !rects.is_empty() && !point_in_rects(cur.x - pos.x as f64, cur.y - pos.y as f64, &rects, 6.0);
    static IGNORING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if IGNORING.swap(want_ignore, std::sync::atomic::Ordering::SeqCst) != want_ignore {
        let _ = w.set_ignore_cursor_events(want_ignore);
    }
}

/// WebView2's mouseleave is unreliable inside a NOACTIVATE transparent window — a cursor that
/// leaves quickly often produces no WM_MOUSELEAVE, and the card stays up. Rather than trust DOM
/// events, the Rust side watches the system cursor while the card is expanded and emits
/// pointer_left once the cursor is outside; the page collapses after its 250 ms grace period.
/// "Outside the window" is not the test, though: the window has a 340×460 transparent area, so
/// the cursor is compared against the hot rectangles the page reports (pill, card, and the gap
/// between them), and two consecutive misses (300 ms) count as leaving.
///
/// The same cursor reading drives click-through, on a shorter tick: the window is only
/// interactive where the page actually draws something, so everything else falls through to
/// whatever is underneath. See `INTERACTIVE_RECTS`.
fn start_pointer_watchdog(app: AppHandle) {
    std::thread::spawn(move || {
        let mut miss = 0u8;
        // Three 50 ms ticks per watchdog evaluation keeps the collapse timing exactly as it was
        // (two misses = 300 ms) while click-through reacts within 50 ms. Anything slower is felt
        // as a click that lands nowhere because the cursor arrived at the pill first.
        let mut tick = 0u8;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            update_click_through(&app);
            tick += 1;
            if tick < 3 {
                continue;
            }
            tick = 0;
            let rects = match HOT.lock().unwrap().clone() {
                Some(r) => r,
                None => {
                    miss = 0;
                    continue;
                }
            };
            let Some(w) = app.get_webview_window("notch") else { continue };
            let (Ok(pos), Ok(cur)) = (w.outer_position(), app.cursor_position()) else { continue };
            // Cursor position relative to the window's top-left, in physical pixels; the hot rectangles are physical too, so no scale conversion
            let lx = cur.x - pos.x as f64;
            let ly = cur.y - pos.y as f64;
            const PAD: f64 = 10.0;
            let in_window = w
                .outer_size()
                .map(|s| lx >= 0.0 && ly >= 0.0 && lx < s.width as f64 && ly < s.height as f64)
                .unwrap_or(true);
            let mut inside = in_window && point_in_rects(lx, ly, &rects, PAD);
            // The gap between hot rectangles (pill and card) counts as inside: use the bounding box of all of them
            if !inside && in_window && rects.len() > 1 {
                let x0 = rects.iter().map(|r| r[0]).fold(f64::MAX, f64::min);
                let y0 = rects.iter().map(|r| r[1]).fold(f64::MAX, f64::min);
                let x1 = rects.iter().map(|r| r[0] + r[2]).fold(f64::MIN, f64::max);
                let y1 = rects.iter().map(|r| r[1] + r[3]).fold(f64::MIN, f64::max);
                inside = lx >= x0 && ly >= y0 && lx < x1 && ly < y1;
            }
            static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
                applog(&format!(
                    "watchdog: cursor_rel=({lx:.0},{ly:.0}) inside={inside} rects={rects:?} winpos=({},{})",
                    pos.x, pos.y
                ));
            }
            if inside {
                miss = 0;
            } else {
                miss += 1;
                if miss >= 2 {
                    miss = 0;
                    *HOT.lock().unwrap() = None;
                    let _ = app.emit("pointer_left", ());
                }
            }
        }
    });
}

/// Log channel for the page: JS writes key diagnostics into run.log (if invoke itself fails, the page reports on screen instead)
#[tauri::command]
fn log_js(msg: String) {
    applog(&format!("js: {}", msg.chars().take(600).collect::<String>()));
}

#[tauri::command]
fn open_usage_page() {
    // Through the same launcher as everything else. This one has no query string, so `cmd /C
    // start` happened to work here - but leaving a second way to open a URL is leaving a
    // second place for the next URL with an `&` in it to be cut in half.
    open_in_browser("https://claude.ai/settings/usage");
}

#[tauri::command]
fn focus_session(app: AppHandle, id: String) -> bool {
    let ppid = {
        let st = app.state::<AppState>();
        let store = st.store.lock().unwrap();
        store.ppid_of(&id)
    };
    match ppid {
        Some(p) => focus::focus_terminal(p),
        None => focus::focus_claude_desktop(),
    }
}

#[tauri::command]
fn dismiss_session(app: AppHandle, id: String) {
    {
        let st = app.state::<AppState>();
        let mut store = st.store.lock().unwrap();
        store.dismiss(&id);
    }
    broadcast(&app);
}

#[tauri::command]
fn set_lang(app: AppHandle, lang: String) {
    apply_lang(&app, &lang);
}

/// Seen-clears-it: looking at a session acknowledges it (engine behaviour, unchanged)
#[cfg(windows)]
fn ack_scan(app: &AppHandle) -> bool {
    let need = {
        let st = app.state::<AppState>();
        let store = st.store.lock().unwrap();
        store.has_done()
    };
    if !need {
        return false;
    }
    let fg = focus::fg_pid();
    if fg == 0 {
        return false;
    }
    let maps = focus::proc_maps();
    let fg_name = maps.name.get(&fg).cloned().unwrap_or_default();
    let fg_is_claude_desktop = fg_name.contains("claude") && !fg_name.contains("codenotch");
    let st = app.state::<AppState>();
    let mut store = st.store.lock().unwrap();
    store.ack_done(|s| {
        if s.ppid == 0 {
            fg_is_claude_desktop
        } else {
            focus::pid_hits_chain(fg, &focus::chain_of(s.ppid, &maps.ppid), &maps)
        }
    })
}
#[cfg(not(windows))]
fn ack_scan(_app: &AppHandle) -> bool {
    false
}

// ---------------- main ----------------

#[cfg(windows)]
fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
#[cfg(not(windows))]
fn attach_console() {}

fn report(r: Result<String, String>) {
    let msg = match r {
        Ok(m) => format!("OK: {m}"),
        Err(e) => format!("FAILED: {e}"),
    };
    println!("{msg}");
    let log = config::config_path().with_file_name("install.log");
    let _ = std::fs::write(log, &msg);
}

/// Path of the marker that records a deliberate quit from the tray.
///
/// codenotch-hook reads it before deciding whether to launch the application, so that
/// quitting is not undone by the user's next Claude Code tool call.
pub fn quit_marker_path() -> std::path::PathBuf {
    config::config_path().with_file_name("quit")
}

pub fn mark_user_quit() {
    let p = quit_marker_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, b"");
}

fn main() {
    attach_console();
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1) {
        match cmd.as_str() {
            "install-hooks" => {
                report(hooks_install::install());
                return;
            }
            "uninstall-hooks" => {
                report(hooks_install::uninstall());
                return;
            }
            // The tray item's command-line twin, so the behaviour can be tested and
            // scripted rather than only clicked.
            "refresh-creds" => {
                report(Ok(usage::nudge_claude_credential()));
                return;
            }
            "version" | "--version" | "-V" => {
                report(Ok(version_line()));
                return;
            }
            "autostart" => {
                let r = match args.get(2).map(|s| s.as_str()) {
                    Some("on") => autostart::enable(),
                    Some("off") => autostart::disable(),
                    _ => Err("usage: codenotch.exe autostart on|off".into()),
                };
                report(r);
                return;
            }
            "doctor" => {
                let out = if args.get(2).map(|s| s.as_str()) == Some("deep") { diag::run() } else { doctor::run() };
                println!("{out}");
                let log = config::config_path().with_file_name("doctor.log");
                let _ = std::fs::write(log, &out);
                return;
            }
            _ => {}
        }
    }

    // Only a real launch clears the quit marker, which is why this sits after the
    // subcommands rather than before them. `doctor`, `version` and `refresh-creds` all exit
    // without starting anything, and clearing the marker on their way past told the hook
    // that the user had changed their mind - so running `codenotch.exe doctor` after
    // quitting brought the whole application back on the next tool call.
    let _ = std::fs::remove_file(quit_marker_path());

    let cfg = config::load();
    let port = cfg.port;

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Launching a freshly built exe while the old one is still running lands here: the new
            // instance is turned away and what stays on screen is the old process. Say so loudly.
            applog(&format!("single instance: another launch was refused; the running instance is build={BUILD} — quit it from the tray first if you just rebuilt"));
            let _ = app.emit("notice", format!("Codenotch is already running ({BUILD}) — quit it from the tray before starting a new build"));
        }))
        .manage(AppState {
            store: Mutex::new(Default::default()),
            cfg: Mutex::new(cfg),
            usage: Mutex::new(usage::load_persisted()),
            codex: Mutex::new(codex::load_persisted()),
            cursor: Mutex::new(cursor::load_persisted()),
            antigravity: Mutex::new(antigravity::load_persisted()),
            glyphs: Mutex::new(Default::default()),
            activity: Mutex::new(Vec::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_usage,
            get_codex,
            get_cursor,
            get_antigravity,
            get_prefs,
            settings_load,
            settings_save,
            open_data_folder,
            reset_position,
            refresh_provider,
            hide_provider,
            get_glyphs,
            get_activity,
            open_data_dir,
            drag_begin,
            open_provider_page,
            refresh_usage,
            open_usage_page,
            set_interactive_rects,
            set_expanded,
            report_dpr,
            log_js,
            focus_session,
            dismiss_session,
            set_lang
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            place_notch(&handle);
            noactivate(&handle);
            if let Some(w) = handle.get_webview_window("notch") {
                let _ = w.show();
            }
            tray::setup(&handle)?;
            server::start(handle.clone(), port);
            watcher::start(handle.clone());
            usage::start(handle.clone());
            codex::start(handle.clone());
            cursor::start(handle.clone());
            antigravity::start(handle.clone());
            activity::start(handle.clone());
            window_start::start_watcher(handle.clone());
            start_fullscreen_watcher(handle.clone());
            notify::start_watcher(handle.clone());
            // Collecting glyphs may read icon resources out of a few executables; do it off the main thread and push when done
            let gh = handle.clone();
            std::thread::spawn(move || reload_glyphs(&gh));
            start_pointer_watchdog(handle.clone());
            // Seen-clears-it scan
            let acker = handle.clone();
            std::thread::spawn(move || {
                activity::lower_thread_priority();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if ack_scan(&acker) {
                        broadcast(&acker);
                    }
                }
            });
            // Stale session cleanup
            let sweeper = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let changed = {
                    let st = sweeper.state::<AppState>();
                    let mut s = st.store.lock().unwrap();
                    s.sweep()
                };
                if changed {
                    broadcast(&sweeper);
                }
            });
            // Persist the config (codenotch-hook reads the port from it)
            {
                let st = handle.state::<AppState>();
                let c = st.cfg.lock().unwrap();
                config::save(&c);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Codenotch failed to start");
}

#[cfg(test)]
mod tests {
    use super::point_in_rects;

    /// The pill column of a 340x460 window at 100 %: x 270..340, vertically centred.
    const PILL: [f64; 4] = [270.0, 180.0, 70.0, 100.0];

    #[test]
    fn interactive_rect_test_covers_the_pill_and_nothing_else() {
        assert!(point_in_rects(300.0, 200.0, &[PILL], 0.0), "middle of the pill");
        assert!(!point_in_rects(100.0, 200.0, &[PILL], 0.0), "transparent area left of it");
        assert!(!point_in_rects(300.0, 50.0, &[PILL], 0.0), "transparent area above it");
        // Half-open on the far edges, so two rectangles that share a boundary do not both claim it.
        assert!(point_in_rects(270.0, 180.0, &[PILL], 0.0), "top-left corner is inside");
        assert!(!point_in_rects(340.0, 200.0, &[PILL], 0.0), "right edge is not");
        // The margin exists so a cursor sampled a few pixels short still counts as arriving.
        assert!(point_in_rects(266.0, 200.0, &[PILL], 6.0));
        assert!(!point_in_rects(263.0, 200.0, &[PILL], 6.0));
        // No rectangles means nothing is interactive; the caller is what decides that click-through
        // stays off in that case, and it must not be this returning true by accident.
        assert!(!point_in_rects(300.0, 200.0, &[], 6.0));
    }
}
