use crate::hooks_install;
use crate::i18n::tr;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
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

pub fn build_menu(app: &AppHandle, lang: &str) -> tauri::Result<Menu<Wry>> {
    let install = MenuItemBuilder::with_id("install", tr(lang, "install")).build(app)?;
    let uninstall = MenuItemBuilder::with_id("uninstall", tr(lang, "uninstall")).build(app)?;
    let l_auto = CheckMenuItemBuilder::with_id("lang-auto", tr(lang, "lang_auto"))
        .checked(lang == "auto")
        .build(app)?;
    let l_zh = CheckMenuItemBuilder::with_id("lang-zh", "中文")
        .checked(lang == "zh")
        .build(app)?;
    let l_en = CheckMenuItemBuilder::with_id("lang-en", "English")
        .checked(lang == "en")
        .build(app)?;
    let l_ja = CheckMenuItemBuilder::with_id("lang-ja", "日本語")
        .checked(lang == "ja")
        .build(app)?;
    let l_ko = CheckMenuItemBuilder::with_id("lang-ko", "한국어")
        .checked(lang == "ko")
        .build(app)?;
    let lang_menu = SubmenuBuilder::new(app, tr(lang, "language"))
        .items(&[&l_auto, &l_zh, &l_en, &l_ja, &l_ko])
        .build()?;
    // Providers the user can switch off, and which window the ring follows. Both read their
    // checked state from the config, so the menu always shows what is actually in effect
    // rather than what was in effect when the menu was last built.
    let (hidden, ring_window) = {
        let st = app.state::<crate::AppState>();
        let c = st.cfg.lock().unwrap();
        (c.hidden_providers.clone(), c.ring_window.clone())
    };
    let mut provider_items = Vec::new();
    for (id, label) in [
        ("claude", "Claude"),
        ("codex", "Codex"),
        ("cursor", "Cursor"),
        ("gemini", "Antigravity"),
    ] {
        provider_items.push(
            CheckMenuItemBuilder::with_id(format!("prov-{id}"), label)
                .checked(!hidden.iter().any(|h| h == id))
                .build(app)?,
        );
    }
    let provider_menu = SubmenuBuilder::new(app, tr(lang, "providers"))
        .items(&provider_items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<Wry>).collect::<Vec<_>>())
        .build()?;

    let r_auto = CheckMenuItemBuilder::with_id("ring-auto", tr(lang, "ring_auto"))
        .checked(ring_window == "auto")
        .build(app)?;
    let r_session = CheckMenuItemBuilder::with_id("ring-session", tr(lang, "ring_session"))
        .checked(ring_window == "session")
        .build(app)?;
    let r_weekly = CheckMenuItemBuilder::with_id("ring-weekly", tr(lang, "ring_weekly"))
        .checked(ring_window == "weekly")
        .build(app)?;
    let ring_menu = SubmenuBuilder::new(app, tr(lang, "ring_shows"))
        .items(&[&r_auto, &r_session, &r_weekly])
        .build()?;

    // Parts of a cell, each switchable on its own. The id after "show-" is the config field,
    // so adding one here and in `Config` is the whole change.
    let show = {
        let st = app.state::<crate::AppState>();
        let c = st.cfg.lock().unwrap();
        [
            ("percent", c.show_percent),
            ("countdown", c.show_countdown),
            ("pace_tick", c.show_pace_tick),
            ("activity_arc", c.show_activity_arc),
        ]
    };
    let mut show_items = Vec::new();
    for (name, on) in show {
        show_items.push(
            CheckMenuItemBuilder::with_id(format!("show-{name}"), tr(lang, &format!("show_{name}")))
                .checked(on)
                .build(app)?,
        );
    }
    let show_menu = SubmenuBuilder::new(app, tr(lang, "show"))
        .items(&show_items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<Wry>).collect::<Vec<_>>())
        .build()?;

    let refresh = MenuItemBuilder::with_id("refresh", tr(lang, "refresh")).build(app)?;
    let refresh_creds =
        MenuItemBuilder::with_id("refresh-creds", tr(lang, "refresh_creds")).build(app)?;
    let reset = MenuItemBuilder::with_id("reset", tr(lang, "reset_pos")).build(app)?;
    let open_data = MenuItemBuilder::with_id("open-data", tr(lang, "open_data")).build(app)?;
    let auto = CheckMenuItemBuilder::with_id("autostart", tr(lang, "autostart"))
        .checked(crate::autostart::is_enabled())
        .build(app)?;
    let quit = MenuItemBuilder::with_id("quit", tr(lang, "quit")).build(app)?;
    // Only the action that applies. Offering "install hooks" to someone who already has
    // them, next to "uninstall hooks", makes the user work out the current state from a
    // menu that could simply have told them.
    let hook_item: &dyn tauri::menu::IsMenuItem<Wry> = if hooks_install::is_installed() {
        &uninstall
    } else {
        &install
    };
    MenuBuilder::new(app)
        .items(&[hook_item])
        .separator()
        .item(&provider_menu)
        .item(&ring_menu)
        .item(&show_menu)
        .item(&lang_menu)
        .item(&refresh)
        .item(&refresh_creds)
        .item(&reset)
        .item(&open_data)
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
        id if id.starts_with("show-") => {
            {
                let st = app.state::<crate::AppState>();
                let mut c = st.cfg.lock().unwrap();
                match id.trim_start_matches("show-") {
                    "percent" => c.show_percent = !c.show_percent,
                    "countdown" => c.show_countdown = !c.show_countdown,
                    "pace_tick" => c.show_pace_tick = !c.show_pace_tick,
                    "activity_arc" => c.show_activity_arc = !c.show_activity_arc,
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
        "reset" => crate::reset_bar(app),
        "open-data" => {
            let dir = crate::config::config_path().parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::create_dir_all(crate::glyphs::user_dir());
            // `explorer <path>` spawned with CREATE_NO_WINDOW reported "Location is not
            // available" for a directory that plainly existed and opened fine from a normal
            // shell. explorer.exe is a shell process rather than a console program, and
            // suppressing its console this way loses the argument. The shell verb is what
            // the rest of this file already uses to open URLs, so use it here too.
            let mut cmd = std::process::Command::new("cmd");
            cmd.args(["/C", "start", ""]).arg(dir.as_os_str());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // hides cmd's own console, not explorer's
            }
            let _ = cmd.spawn();
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
