use crate::hooks_install;
use crate::i18n::tr;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let lang = {
        let st = app.state::<crate::AppState>();
        let c = st.cfg.lock().unwrap();
        c.lang.clone()
    };
    let menu = build_menu(app, &lang)?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip(concat!("Codenotch v", env!("CARGO_PKG_VERSION")))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, ev| handle(app, ev.id().as_ref()))
        .build(app)?;
    Ok(())
}

/// The tray menu, deliberately short.
///
/// It used to carry everything: a Providers submenu, a Ring shows submenu, a six-item Show
/// submenu, three display modes, the float toggle, sign in and out, two window-starter items,
/// the hook shortcut, a language submenu, and five plain items. That is not a menu, it is a
/// settings window that opens sideways and forgets its own state - and the entry in the roadmap
/// saying so was written when it held a third of that.
///
/// What is left is the things you reach for *from the tray*: the state the tray is for
/// (sign in, hooks), the action you want without opening anything (refresh), and the way in to
/// everything else. The rest lives in the settings window, where a setting can have a sentence
/// next to it explaining what it costs.
pub fn build_menu(app: &AppHandle, lang: &str) -> tauri::Result<Menu<Wry>> {
    let install = MenuItemBuilder::with_id("install", tr(lang, "install")).build(app)?;
    let uninstall = MenuItemBuilder::with_id("uninstall", tr(lang, "uninstall")).build(app)?;
    // Only the action that applies. Offering "install hooks" to someone who already has them,
    // next to "uninstall hooks", makes the user work out the current state from a menu that
    // could simply have told them.
    let hook_item: &dyn tauri::menu::IsMenuItem<Wry> = if hooks_install::is_installed() {
        &uninstall
    } else {
        &install
    };

    // Sign in, or sign out - never both, for the same reason.
    let signed_in = crate::oauth::is_signed_in();
    let sign_item = MenuItemBuilder::with_id(
        if signed_in { "sign-out" } else { "sign-in" },
        tr(lang, if signed_in { "sign_out" } else { "sign_in" }),
    )
    .build(app)?;

    // Offered only when it would change something: a faster shell exists, and the one in use is
    // slow enough for the difference to matter. Measured once per process, not per rebuild.
    let faster_shell = crate::hooks_install::cached_faster_shell();
    let speed_up =
        MenuItemBuilder::with_id("speed-up-hooks", tr(lang, "speed_up_hooks")).build(app)?;

    let start_window = MenuItemBuilder::with_id("start-window", tr(lang, "start_window"))
        .enabled(signed_in)
        .build(app)?;
    let settings = MenuItemBuilder::with_id("settings", tr(lang, "settings")).build(app)?;
    let refresh = MenuItemBuilder::with_id("refresh", tr(lang, "refresh")).build(app)?;
    let refresh_creds =
        MenuItemBuilder::with_id("refresh-creds", tr(lang, "refresh_creds")).build(app)?;
    let auto = CheckMenuItemBuilder::with_id("autostart", tr(lang, "autostart"))
        .checked(crate::autostart::is_enabled())
        .build(app)?;
    let quit = MenuItemBuilder::with_id("quit", tr(lang, "quit")).build(app)?;

    let mut builder = MenuBuilder::new(app).items(&[hook_item]);
    if faster_shell.is_some() {
        builder = builder.item(&speed_up);
    }
    builder
        .item(&sign_item)
        .item(&start_window)
        .separator()
        .item(&settings)
        .item(&refresh)
        .item(&refresh_creds)
        .item(&auto)
        .separator()
        .item(&quit)
        .build()
}

/// Rebuild the tray menu from the current config. Public so a change made elsewhere - the
/// pill's own right-click menu, for instance - leaves the tray showing what is actually set.
pub fn rebuild(app: &AppHandle) -> tauri::Result<()> {
    refresh_menu(app);
    Ok(())
}

fn refresh_menu(app: &AppHandle) {
    let lang = {
        let st = app.state::<crate::AppState>();
        let c = st.cfg.lock().unwrap();
        c.lang.clone()
    };
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_menu(app, &lang) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn handle(app: &AppHandle, id: &str) {
    match id {
        // Rebuild afterwards so the menu swaps to the opposite action straight away.
        "install" => {
            notice(app, hooks_install::install());
            refresh_menu(app);
        }
        "uninstall" => {
            notice(app, hooks_install::uninstall());
            refresh_menu(app);
        }
        // Hiding a provider keeps its stored reading, so switching it back on shows the last
        // number immediately instead of an empty ring while it polls again.
        id if id.starts_with("prov-") => {
            let provider = id.trim_start_matches("prov-").to_string();
            {
                let st = app.state::<crate::AppState>();
                let mut c = st.cfg.lock().unwrap();
                if let Some(pos) = c.hidden_providers.iter().position(|h| *h == provider) {
                    c.hidden_providers.remove(pos);
                } else {
                    c.hidden_providers.push(provider);
                }
                crate::config::save(&c);
            }
            crate::broadcast_prefs(app);
            refresh_menu(app);
        }
        "float-pill" => {
            {
                let st = app.state::<crate::AppState>();
                let mut c = st.cfg.lock().unwrap();
                c.float_pill = !c.float_pill;
                crate::config::save(&c);
            }
            crate::broadcast_prefs(app);
            refresh_menu(app);
        }
        id if id.starts_with("show-") => {
            {
                let st = app.state::<crate::AppState>();
                let mut c = st.cfg.lock().unwrap();
                match id.trim_start_matches("show-") {
                    "percent" => c.show_percent = !c.show_percent,
                    "countdown" => c.show_countdown = !c.show_countdown,
                    "pace_tick" => c.show_pace_tick = !c.show_pace_tick,
                    "activity_arc" => c.show_activity_arc = !c.show_activity_arc,
                    "weekly_ring" => c.show_weekly_ring = !c.show_weekly_ring,
                    "hour_marks" => c.show_hour_marks = !c.show_hour_marks,
                    "stale_warning" => c.show_stale_warning = !c.show_stale_warning,
                    "notify_threshold" => c.notify_threshold = !c.notify_threshold,
                    "remaining_mode" => c.remaining_mode = !c.remaining_mode,
                    "colorblind" => c.colorblind = !c.colorblind,
                    // An id built here that nothing matches would silently do nothing, which is
                    // the failure mode worth naming rather than the one worth ignoring.
                    other => crate::applog(&format!("tray: unknown show toggle {other:?}")),
                }
                crate::config::save(&c);
            }
            crate::broadcast_prefs(app);
            refresh_menu(app);
        }
        id if id.starts_with("ring-") => {
            let choice = id.trim_start_matches("ring-").to_string();
            {
                let st = app.state::<crate::AppState>();
                let mut c = st.cfg.lock().unwrap();
                c.ring_window = choice;
                crate::config::save(&c);
            }
            crate::broadcast_prefs(app);
            refresh_menu(app);
        }
        "auto-window" => {
            let on = {
                let st = app.state::<crate::AppState>();
                let mut c = st.cfg.lock().unwrap();
                c.auto_start_window = !c.auto_start_window;
                crate::config::save(&c);
                c.auto_start_window
            };
            // Said out loud, because this is the one setting here that spends something on its
            // own. Somebody who turns it on should be told what they just agreed to.
            let _ = app.emit(
                "notice",
                if on {
                    "Codenotch will start a new five-hour window a few seconds after each reset. One Haiku message each time."
                } else {
                    "Automatic window starts are off."
                },
            );
            refresh_menu(app);
        }
        "start-window" => {
            // The reset time of the five-hour window as last read, so the request can be
            // refused when one is already running rather than spending a message to learn it.
            let open_until = {
                let st = app.state::<crate::AppState>();
                let u = st.usage.lock().unwrap();
                u.windows
                    .iter()
                    .find(|w| w.id == "session" || w.id == "five_hour")
                    .and_then(|w| w.resets_at)
                    .map(|ms| ms / 1000)
            };
            let app = app.clone();
            // Off the menu thread: this is a network call, and a tray menu that stays open
            // while it waits looks like the click did nothing.
            std::thread::spawn(move || {
                let msg = match crate::window_start::start_window(open_until) {
                    Ok(m) => m,
                    Err(e) => e,
                };
                crate::applog(&format!("window start: {msg}"));
                let _ = app.emit("notice", msg);
                crate::usage::request_refresh();
            });
        }
        "sign-in" => crate::begin_sign_in(app),
        "sign-out" => {
            // Only this application's own session. Claude Code's credential is not touched,
            // which is the point of having a separate one - and after this the app falls back
            // to borrowing that credential exactly as it did before anyone signed in here.
            let gone = crate::oauth::sign_out();
            crate::applog(&format!("oauth: sign out {}", if gone { "ok" } else { "found nothing" }));
            // Deliberately no forced refresh. `request_refresh` skips the ten-minute refetch
            // floor, and signing out repeatedly would then be a way to spend the hourly
            // ceiling on a number that has not changed - the reading is about the account,
            // and the account is the same one whether this app or Claude Code is holding the
            // credential. The next scheduled poll picks up the fallback on its own.
            refresh_menu(app);
        }
        "speed-up-hooks" => {
            match crate::hooks_install::cached_faster_shell() {
                Some(path) => notice(app, crate::hooks_install::use_faster_shell(&path)),
                None => notice(app, Ok("The shell Claude Code uses is already fast.".into())),
            }
            refresh_menu(app);
        }
        "settings" => crate::open_settings(app),
        "reset" => crate::reset_bar(app),
        "open-data" => {
            let dir = crate::config::config_path().parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::create_dir_all(crate::glyphs::user_dir());
            // `explorer <path>` spawned with CREATE_NO_WINDOW reported "Location is not
            // available" for a directory that plainly existed and opened fine from a normal
            // shell: explorer.exe is a shell process rather than a console program, and
            // suppressing its console that way loses the argument.
            //
            // The shell verb is the answer, but not through `cmd /C start`. That reparses its
            // command line, and a profile path containing an `&` - which Windows allows -
            // would be cut in half exactly as an OAuth URL was. `ShellExecuteW` takes the path
            // as one argument and hands it to the shell without a parser in between.
            crate::open_in_browser(&dir.display().to_string());
        }
        // Re-acquire credentials, then re-poll. Distinct from "refresh now", which only
        // re-polls: this is for the case where a reading is missing because the *sign-in*
        // is stale rather than because the number is old.
        //
        // Only Claude has a nudge worth making. Codex's session is refreshed by the Codex
        // CLI and is read the same way; Cursor's is borrowed from the editor, so signing in
        // there is the only fix; Antigravity's lives in Credential Manager and is renewed
        // by its own IDE. For those three the honest action is to drop any cached copy and
        // read again, which is what request_refresh does, and to say so rather than imply
        // the app can re-authenticate them.
        "refresh-creds" => {
            let claude = crate::usage::nudge_claude_credential();
            {
                let st = app.state::<crate::AppState>();
                let mut u = st.usage.lock().unwrap();
                u.backoff_until = 0;
            }
            crate::usage::request_refresh();
            crate::codex::request_refresh();
            crate::cursor::request_refresh();
            crate::antigravity::request_refresh();
            notice(
                app,
                Ok(format!(
                    "Claude: {claude}. Codex, Cursor and Antigravity re-read from their own stores."
                )),
            );
        }
        "refresh" => {
            {
                let st = app.state::<crate::AppState>();
                let mut u = st.usage.lock().unwrap();
                u.backoff_until = 0;
            }
            crate::usage::request_refresh();
            crate::codex::request_refresh();
            crate::cursor::request_refresh();
            crate::antigravity::request_refresh();
            let a = app.clone();
            std::thread::spawn(move || crate::reload_glyphs(&a));
        }
        "autostart" => {
            let r = if crate::autostart::is_enabled() {
                crate::autostart::disable()
            } else {
                crate::autostart::enable()
            };
            notice(app, r);
            refresh_menu(app); // refresh the check marks
        }
        "quit" => {
            // Record that this was deliberate, so codenotch-hook does not relaunch the
            // application on the user's very next Claude Code tool call. main() clears the
            // marker on every start, so it never outlives the decision it records.
            crate::mark_user_quit();
            app.exit(0)
        }
        _ if id.starts_with("lang-") => crate::apply_lang(app, &id[5..]),
        _ => {}
    }
}

fn notice(app: &AppHandle, r: Result<String, String>) {
    let msg = match r {
        Ok(m) => m,
        Err(e) => format!("Error: {e}"),
    };
    let _ = app.emit("notice", &msg);
}
