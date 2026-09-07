#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod autostart;
mod config;
mod doctor;
mod focus;
mod hooks_install;
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
mod watcher;

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// notch 窗口逻辑尺寸：右列 70pt 胶囊 + 左侧悬停细节卡的空间
pub const NOTCH_W: f64 = 340.0;
/// 轮次水印：每轮改动 +1，run.log 与卡片右上角都显示，杜绝"跑的是旧 exe"误判
pub const BUILD: &str = "r22";
pub const NOTCH_H: f64 = 460.0; // 300 装不下 3 个窗口块+会话列表（卡片上下被裁）

pub struct AppState {
    pub store: Mutex<state::Store>,
    pub cfg: Mutex<config::Config>,
    pub usage: Mutex<usage::UsageSnapshot>,
    /// Codex 适配器快照（同一 UsageSnapshot 形状；status 另有 none/absent）
    pub codex: Mutex<usage::UsageSnapshot>,
    pub cursor: Mutex<usage::UsageSnapshot>,
    pub antigravity: Mutex<usage::UsageSnapshot>,
    /// 提供商图标缓存（启动收集；托盘刷新时重收集）
    pub glyphs: Mutex<std::collections::HashMap<String, glyphs::Glyph>>,
    /// 非 Claude 提供商的活动态（Cursor 真状态；Codex/Antigravity 按最近写入推断）
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

/// notch 贴屏幕右缘、垂直居中（W4 再做四边停靠）
pub fn place_notch(app: &AppHandle) {
    let Some(w) = app.get_webview_window("notch") else {
        return;
    };
    let scale = w.scale_factor().unwrap_or(1.0);
    if let Ok(Some(mon)) = w.primary_monitor() {
        // 双显示器不同缩放（实测 150%/200% 混用）：窗口建在哪块屏、被搬到哪块屏，
        // 物理尺寸都可能按"另一块屏"的 scale 换算，导致 WebView 逻辑宽只剩 ~256 而非 340。
        // 对策：一律按目标显示器 mon.scale_factor() 直接钉物理尺寸，再定位；定位后若
        // 窗口自报 scale 仍不一致，再钉一次。
        let ms = mon.scale_factor();
        let target = tauri::PhysicalSize::new((NOTCH_W * ms).round() as u32, (NOTCH_H * ms).round() as u32);
        let _ = w.set_size(target);
        // 用窗口实测物理尺寸定位——按 scale 猜算在 125%/150% 缩放下会把窗口
        // 推出屏幕右缘（W1 实测：环右侧被裁）
        let (ww, wh) = w
            .outer_size()
            .map(|s| (s.width as i32, s.height as i32))
            .unwrap_or(((NOTCH_W * scale) as i32, (NOTCH_H * scale) as i32));
        let x = mon.position().x + mon.size().width as i32 - ww;
        // 垂直位置来自配置比例（允许上下拖动，位置持久化），钳制在屏内
        let ratio = {
            let st = app.state::<AppState>();
            let c = st.cfg.lock().unwrap();
            c.notch_y.clamp(0.0, 1.0)
        };
        let mh = mon.size().height as i32;
        let y = (mon.position().y as f64 + mh as f64 * ratio - wh as f64 / 2.0).round() as i32;
        let y = y.clamp(mon.position().y, mon.position().y + (mh - wh).max(0));
        let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
        if w.outer_size().map(|s| s.width != target.width).unwrap_or(false) {
            let _ = w.set_size(target);
            let x = mon.position().x + mon.size().width as i32 - target.width as i32;
            let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
        }
        // 启动定位日志：排查"看不见"时的第一证据
        let log = config::config_path().with_file_name("run.log");
        let _ = std::fs::write(
            log,
            format!(
                "notch 就位 build={BUILD}: pos=({x},{y}) size=({ww}x{wh}) inner={:?} win_scale={scale} mon_scale={ms} monitor=({},{} {}x{})\n",
                w.inner_size().map(|s| (s.width, s.height)).unwrap_or((0, 0)),
                mon.position().x,
                mon.position().y,
                mon.size().width,
                mon.size().height
            ),
        );
    }
}

/// 兼容 tray.rs 的旧入口名
pub fn reset_bar(app: &AppHandle) {
    {
        let st = app.state::<AppState>();
        let mut c = st.cfg.lock().unwrap();
        c.notch_y = 0.5;
        config::save(&c);
    }
    place_notch(app);
}

/// 沿右缘上下拖动。前端在胶囊上按下并移动 >4px 后调用一次；
/// 之后由 Rust 线程按系统光标驱动（不依赖 WebView 的 mousemove——窗口一动光标事件就不可靠），
/// 左键松开即结束，中心比例写回配置。
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
        let (Ok(start_cur), Ok(start_pos), Ok(size), Ok(Some(mon))) =
            (app.cursor_position(), w.outer_position(), w.outer_size(), w.primary_monitor())
        else {
            DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let (my, mh) = (mon.position().y, mon.size().height as i32);
        let wh = size.height as i32;
        let lo = my;
        let hi = my + (mh - wh).max(0);
        let mut last_y = start_pos.y;
        let mut moved = false;
        loop {
            if !left_button_down() {
                break;
            }
            if let Ok(cur) = app.cursor_position() {
                let ny = (start_pos.y as f64 + (cur.y - start_cur.y)).round() as i32;
                let ny = ny.clamp(lo, hi);
                if ny != last_y {
                    last_y = ny;
                    moved = true;
                    let _ = w.set_position(tauri::PhysicalPosition::new(start_pos.x, ny));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        if moved {
            let ratio = ((last_y + wh / 2 - my) as f64 / mh as f64).clamp(0.0, 1.0);
            let st = app.state::<AppState>();
            let mut c = st.cfg.lock().unwrap();
            c.notch_y = ratio;
            config::save(&c);
            applog(&format!("notch 拖动: y={last_y} ratio={ratio:.3}"));
        }
        DRAGGING.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = app.emit("drag_end", moved);
    });
}
pub fn place_bar(app: &AppHandle) {
    place_notch(app);
}
pub fn toggle_drag(app: &AppHandle) {
    // notch 固定贴边，无拖动语义（保留空实现以兼容托盘菜单代码路径）
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

/// notch 永不抢焦点：WS_EX_NOACTIVATE + WS_EX_TOOLWINDOW
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

/// 重新收集图标并推给前端（托盘刷新 / 用户刚放好素材）
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

/// 点击 cell → 该提供商的用量页
#[tauri::command]
fn open_provider_page(provider: String) {
    let url = match provider.as_str() {
        "codex" => "https://chatgpt.com/#settings/Account",
        "cursor" => "https://cursor.com/dashboard",
        "gemini" => "https://antigravity.google",
        _ => "https://claude.ai/settings/usage",
    };
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", url]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

/// 细节卡展开态：Some(热区矩形列表，**物理像素**，相对窗口左上角 x,y,w,h) = 展开中；None = 收起
/// 前端用自己的 devicePixelRatio 把矩形换成物理像素再上报，Rust 端不再做任何
/// scale 换算——因为 WebView2 的 DPR 与窗口 scale_factor 可能不一致（见 report_dpr）。
static HOT: Mutex<Option<Vec<[f64; 4]>>> = Mutex::new(None);

#[tauri::command]
fn set_expanded(on: bool, rects: Option<Vec<[f64; 4]>>) {
    *HOT.lock().unwrap() = if on { Some(rects.unwrap_or_default()) } else { None };
}

/// 当前已施加的 WebView 缩放（1.0 = 未校正）
static ZOOM: Mutex<f64> = Mutex::new(1.0);

pub fn applog(line: &str) {
    use std::io::Write;
    let log = config::config_path().with_file_name("run.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

/// 根因：双显示器（150%/200%）下 WebView2 的 devicePixelRatio 取了 2.0，而窗口按主屏
/// 1.5 定尺寸 → 页面只有 255 CSS px 宽（设计 340），且所有坐标换算全错（看门狗误判→卡片一闪而过）。
/// 对策：前端上报 DPR，与主屏 scale 不一致时用 set_zoom 把有效 DPR 拉回 scale（CSS px 恢复 340 宽）。
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
        "dpr 上报: dpr={dpr:.3} viewport={w:.0}x{h:.0} 主屏scale={want:.3} 已用zoom={:.3} → 目标zoom={target:.3}",
        *z
    ));
    // 防振荡保险：整个进程最多校正 3 次（若 WebView 的 DPR 不随 zoom 变化，就不再追）
    static APPLIED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if (dpr - want).abs() > 0.02
        && (target - *z).abs() > 0.01
        && (0.25..=4.0).contains(&target)
        && APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
    {
        match win.set_zoom(target) {
            Ok(()) => {
                *z = target;
                applog(&format!("dpr 校正: set_zoom({target:.3}) 成功"));
            }
            Err(e) => applog(&format!("dpr 校正失败: {e}")),
        }
    }
}

/// NOACTIVATE 透明窗里 WebView2 的 mouseleave 不可靠——光标快速离开窗口时经常
/// 收不到 WM_MOUSELEAVE，卡片就一直挂着。不赌 DOM 事件：展开期间 Rust 侧用系统光标坐标
/// 兜底，光标已在窗口矩形之外就发 pointer_left，前端按 250ms 宽限收起。
/// 窗口有 340×460 的透明区，光标离开胶囊但仍在透明区内时"在窗口内"不成立为离开——
/// 改为对比前端上报的热区矩形（胶囊+卡片，及两者之间的空隙），连续 2 拍（300ms）不在热区即收。
fn start_pointer_watchdog(app: AppHandle) {
    std::thread::spawn(move || {
        let mut miss = 0u8;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let rects = match HOT.lock().unwrap().clone() {
                Some(r) => r,
                None => {
                    miss = 0;
                    continue;
                }
            };
            let Some(w) = app.get_webview_window("notch") else { continue };
            let (Ok(pos), Ok(cur)) = (w.outer_position(), app.cursor_position()) else { continue };
            // 光标 → 相对窗口左上角的物理像素；热区已是物理像素，不做任何 scale 换算
            let lx = cur.x - pos.x as f64;
            let ly = cur.y - pos.y as f64;
            const PAD: f64 = 10.0;
            let in_window = w
                .outer_size()
                .map(|s| lx >= 0.0 && ly >= 0.0 && lx < s.width as f64 && ly < s.height as f64)
                .unwrap_or(true);
            let mut inside = in_window && rects.iter().any(|r| {
                lx >= r[0] - PAD && ly >= r[1] - PAD && lx < r[0] + r[2] + PAD && ly < r[1] + r[3] + PAD
            });
            // 热区之间的空隙（胶囊与卡片之间）也算在内：取所有热区的包围盒
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
                    "看门狗: cursor_rel=({lx:.0},{ly:.0}) inside={inside} rects={rects:?} winpos=({},{})",
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

/// 前端日志通道：JS 把关键诊断写进 run.log（invoke 若失败，前端会用 notice 在屏上直接报）
#[tauri::command]
fn log_js(msg: String) {
    applog(&format!("js: {}", msg.chars().take(600).collect::<String>()));
}

#[tauri::command]
fn build_tag() -> String {
    BUILD.to_string()
}

#[tauri::command]
fn open_usage_page() {
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", "https://claude.ai/settings/usage"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
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

/// 看过即清（引擎能力，v0.2.0 语义原样保留）
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
            "autostart" => {
                let r = match args.get(2).map(|s| s.as_str()) {
                    Some("on") => autostart::enable(),
                    Some("off") => autostart::disable(),
                    _ => Err("用法: codenotch.exe autostart on|off".into()),
                };
                report(r);
                return;
            }
            "doctor" => {
                let out = doctor::run();
                println!("{out}");
                let log = config::config_path().with_file_name("doctor.log");
                let _ = std::fs::write(log, &out);
                return;
            }
            _ => {}
        }
    }

    let cfg = config::load();
    let port = cfg.port;

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 重编译后直接再启动，旧实例还在 → 新实例在这里被拦截退出，
            // 看到的仍是旧进程。必须在屏上和日志里都喊出来。
            applog(&format!("单实例: 又一个实例尝试启动并被拦截——当前运行的是 build={BUILD}，若你刚重编译，请先从托盘退出再启动"));
            let _ = app.emit("notice", format!("已在运行（{BUILD}）：重编译后请先托盘退出旧实例再启动"));
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
            get_glyphs,
            get_activity,
            open_data_dir,
            drag_begin,
            open_provider_page,
            refresh_usage,
            open_usage_page,
            set_expanded,
            report_dpr,
            log_js,
            build_tag,
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
            // 图标收集可能要读几个 exe 的资源，放后台线程，收完再推
            let gh = handle.clone();
            std::thread::spawn(move || reload_glyphs(&gh));
            start_pointer_watchdog(handle.clone());
            // 看过即清
            let acker = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                if ack_scan(&acker) {
                    broadcast(&acker);
                }
            });
            // 陈旧会话清理
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
            // 落盘配置（codenotch-hook 读端口用）
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
