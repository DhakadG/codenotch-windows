//! Merges codenotch-hook.exe into ~/.claude/settings.json without overwriting the user's own hooks.
//! Identification: the command contains "codenotch-hook". A backup is written first.

use serde_json::{json, Value};
use std::path::PathBuf;

/// (Claude Code event name, whether it needs a matcher, the internal event reported to Codenotch)
///
/// `PreToolUse` and `PostToolUse` are deliberately absent, and this is the most important
/// decision in this file.
///
/// Claude Code runs a hook command through a POSIX shell, and the cost of starting that shell
/// is the cost of the hook - not the program it runs. Measured on a Windows machine where the
/// `bash` on PATH is the WSL one: `bash -c true` averaged 673 ms and peaked at 4.2 seconds,
/// against 57 ms for the messenger itself. Wired to the two tool events, with a `*` matcher,
/// that was paid twice on *every single tool call*, which is why sessions visibly froze while
/// this app was installed and recovered the moment it was removed.
///
/// The remaining five fire a handful of times per session - when it starts, when a prompt is
/// submitted, when Claude wants attention, when it stops, when it ends - so the shell cost is
/// paid a handful of times instead of hundreds. Tool-level activity is not lost either: the
/// transcript watcher already reports it, independently of hooks, which is why the notch
/// showed live session state during the period when no hooks were installed at all.
///
/// Anything added here should be judged by how often it fires, not by how useful it is.
const WIRING: &[(&str, bool, &str)] = &[
    ("SessionStart", false, "session_start"),
    ("UserPromptSubmit", false, "running"),
    ("Notification", false, "attention"),
    ("Stop", false, "done"),
    ("SessionEnd", false, "session_end"),
];

#[cfg(test)]
#[path = "hooks_install_tests.rs"]
mod tests;

fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Substrings that identify a hook command as ours.
///
/// The session engine shipped under two earlier names before this app existed, and installs
/// made by those builds are still out there. One list, used by both `is_ours` and
/// `is_installed`, so the two can never disagree about what counts as installed.
const HOOK_COMMAND_MARKERS: &[&str] = &["codenotch-hook", "eatbean-hook", "pacman-hook"];

fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .map(|hs| {
            hs.iter().any(|h| {
                h["command"]
                    .as_str()
                    .map(|c| HOOK_COMMAND_MARKERS.iter().any(|m| c.contains(m)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn load(path: &PathBuf) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}))
}

fn backup_and_write(path: &PathBuf, root: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if path.exists() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::copy(path, path.with_extension(format!("json.codenotch-bak-{ts}")));
    }
    let txt = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(path, txt).map_err(|e| e.to_string())
}

/// Whether any of our hook entries are present, under any name we have ever used.
///
/// This has to agree with `is_ours`, which also claims the two legacy names the session
/// engine shipped under before it was called Codenotch. It used to test for the current
/// name alone, which was harmless while the tray offered install *and* uninstall at all
/// times - a user with only legacy entries could still remove them. Now that the menu shows
/// one action or the other, a narrower predicate here would show "install" to exactly the
/// people who need "uninstall", and put their entries out of reach: `merge_install` only
/// rewrites the seven events in `WIRING`, so a legacy entry filed under any other event
/// would stay in the file forever.
pub fn is_installed() -> bool {
    let Some(text) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return false;
    };
    HOOK_COMMAND_MARKERS.iter().any(|m| text.contains(m))
}

/// Merges our wiring into an already-parsed settings document.
///
/// Split out from [`install`] so the merge rules can be tested without a home directory:
/// the caller owns every filesystem decision, this function owns every JSON decision.
///
/// Idempotent. Our own older entries are removed before the new ones are added, so
/// installing twice leaves exactly one entry per event rather than two.
fn merge_install(root: &mut Value, hook_exe: &str) {
    if !root.is_object() {
        *root = json!({});
    }
    if !root["hooks"].is_object() {
        root["hooks"] = json!({});
    }

    // Sweep our entries out of *every* event first, not just the ones being written back.
    //
    // Without this, dropping an event from WIRING only changes what new installations get.
    // An existing settings.json keeps whatever it was given by an older build, because the
    // loop below never visits an event the current WIRING does not mention. That is not a
    // theoretical leak: PreToolUse and PostToolUse were removed precisely because they made
    // Claude Code start a shell on every tool call, and an upgrade that left them in place
    // would have shipped the fix while the bug carried on running.
    //
    // Reusing the uninstall path means the two can never disagree about what counts as ours,
    // and it inherits its rule about user entries: only our own are removed, and an event we
    // emptied is dropped rather than left behind as a bare `[]`.
    merge_uninstall(root);
    if !root["hooks"].is_object() {
        root["hooks"] = json!({});
    }

    for (event, need_matcher, internal) in WIRING {
        let arr = root["hooks"][*event].as_array().cloned().unwrap_or_default();
        // Remove our own older entries first
        let mut arr: Vec<Value> = arr.into_iter().filter(|e| !is_ours(e)).collect();
        let cmd = format!("\"{hook_exe}\" {internal}");
        let mut entry = json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 5 }]
        });
        if *need_matcher {
            entry["matcher"] = json!("*");
        }
        arr.push(entry);
        root["hooks"][*event] = json!(arr);
    }
}

/// Removes our wiring from an already-parsed settings document, returning how many
/// entries went. The inverse of [`merge_install`]: an event array that we emptied is
/// removed entirely, so a settings file we had added `Stop` to does not keep a `"Stop": []`
/// afterwards. Events the user configured themselves keep their remaining entries.
fn merge_uninstall(root: &mut Value) -> usize {
    // get_mut, not `root["hooks"]`: indexing a Value mutably *inserts* a null for a missing
    // key, so uninstalling from a settings file that has no hooks section would write a
    // `"hooks": null` into it — an edit to the user's file made by the code whose whole job
    // is to leave it as it found it.
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied: Vec<String> = Vec::new();
    for (event, v) in hooks.iter_mut() {
        if let Some(arr) = v.as_array() {
            let filtered: Vec<Value> = arr.iter().filter(|e| !is_ours(e)).cloned().collect();
            let dropped = arr.len() - filtered.len();
            removed += dropped;
            if filtered.is_empty() && dropped > 0 {
                emptied.push(event.clone());
            } else {
                *v = json!(filtered);
            }
        }
    }
    for event in emptied {
        hooks.remove(&event);
    }
    removed
}

pub fn install() -> Result<String, String> {
    let path = settings_path().ok_or("cannot find the user directory")?;
    let hook_exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("cannot locate the program directory")?
        .join("codenotch-hook.exe");
    if !hook_exe.exists() {
        return Err(format!("missing {}", hook_exe.display()));
    }

    let mut root = load(&path);
    merge_install(&mut root, &hook_exe.display().to_string());
    backup_and_write(&path, &root)?;
    Ok(format!("wrote {} ({} events)", path.display(), WIRING.len()))
}

pub fn uninstall() -> Result<String, String> {
    let path = settings_path().ok_or("cannot find the user directory")?;
    if !path.exists() {
        return Ok("settings.json does not exist, nothing to uninstall".into());
    }
    let mut root = load(&path);
    if !root["hooks"].is_object() {
        return Ok("no hooks configuration found".into());
    }
    let removed = merge_uninstall(&mut root);
    backup_and_write(&path, &root)?;
    Ok(format!("removed {removed} Codenotch hook(s)"))
}
