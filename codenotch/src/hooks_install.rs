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

/// When the shell is slow, the three events that still earn their cost.
///
/// `Notification` is the one signal the transcript watcher genuinely cannot replace: a
/// permission prompt is Claude Code waiting on a person, and nothing is written to the
/// transcript while it waits. Session start and end are bookends that fire once each.
///
/// `UserPromptSubmit` and `Stop` are dropped rather than kept, because both are visible in the
/// transcript within a second or two anyway - they buy latency, not information, and latency is
/// exactly what is expensive on a slow shell.
const WIRING_FRUGAL: &[(&str, bool, &str)] = &[
    ("SessionStart", false, "session_start"),
    ("Notification", false, "attention"),
    ("SessionEnd", false, "session_end"),
];

/// The single event worth paying a very slow shell for.
const WIRING_MINIMAL: &[(&str, bool, &str)] = &[("Notification", false, "attention")];

/// How much shell start-up is acceptable per hook, in milliseconds.
///
/// Not guesses. Measured on the machine this port is developed on, where `bash` on PATH is the
/// WSL one: five runs of `bash -c "exit 0"` gave 4095, 196, 199, 159 and 172 ms - a first-run
/// cost of over four seconds. Git Bash, already installed on the same machine and unused, gave
/// 100, 75, 71, 78 and 84 ms.
///
/// A hundred and fifty sits comfortably above a healthy shell and far below an unhealthy one,
/// so the classification is not a close call in either direction.
const SHELL_FAST_MS: u128 = 150;
/// Above this, only the event the watcher cannot replace is worth paying for.
const SHELL_VERY_SLOW_MS: u128 = 600;

/// Which events to wire, given what the shell costs.
///
/// Every hook costs one shell start-up, and that cost is the shell's rather than the
/// messenger's - 57 ms for `codenotch-hook.exe` against up to 4.2 seconds for the shell around
/// it. So the honest unit of this decision is milliseconds per hook, and the only lever this
/// application has is how many hooks it asks for.
pub(crate) fn wiring_for(shell_ms: u128) -> &'static [(&'static str, bool, &'static str)] {
    if shell_ms <= SHELL_FAST_MS {
        WIRING
    } else if shell_ms <= SHELL_VERY_SLOW_MS {
        WIRING_FRUGAL
    } else {
        WIRING_MINIMAL
    }
}

/// What the shell costs to start, measured rather than assumed.
pub struct ShellProbe {
    /// The shell Claude Code will use.
    pub path: String,
    /// Median of several runs of `bash -c "exit 0"`, in milliseconds.
    ///
    /// Median, not mean: the first start of WSL bash took 4.2 seconds and the rest took under
    /// 200 ms, and an average is a poor summary of a distribution shaped like that. The median
    /// describes the typical hook, which is what this decision is about.
    pub median_ms: u128,
    /// A faster shell that is installed but not being used, if there is one.
    pub faster: Option<String>,
}

/// Time the shell Claude Code will actually invoke.
///
/// This measures the invocation, not the messenger. Measuring the binary was the original
/// mistake: `codenotch-hook.exe` runs in 57 ms, which looked fine and explained nothing,
/// because Claude Code does not run it directly. It runs it through a POSIX shell, and on a
/// Windows machine whose `bash` is the WSL one that shell is the entire cost.
pub fn probe_shell() -> ShellProbe {
    let path = which_bash().unwrap_or_default();
    let median_ms = if path.is_empty() { 0 } else { median_start_ms(&path) };
    ShellProbe {
        faster: faster_shell(&path, median_ms),
        path,
        median_ms,
    }
}

/// The bash Claude Code will pick.
///
/// `CLAUDE_CODE_GIT_BASH_PATH` first, because Claude Code consults it before PATH on Windows.
/// Timing PATH's bash while Claude Code is using a different one would describe a shell that
/// nothing runs.
fn which_bash() -> Option<String> {
    if let Some(p) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH") {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Some(p.display().to_string());
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("bash.exe"))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

/// Git Bash, when it is installed and the shell in use is materially slower.
///
/// "Materially" is doing real work here. Changing which shell Claude Code uses is a change to
/// the user's machine, and it is only worth suggesting when the difference is the difference
/// between a session that stutters and one that does not.
pub(crate) fn faster_shell(current: &str, current_ms: u128) -> Option<String> {
    if current_ms <= SHELL_FAST_MS {
        return None;
    }
    [
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files\\Git\\usr\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
    ]
    .iter()
    .find(|c| !c.eq_ignore_ascii_case(current) && std::path::Path::new(c).is_file())
    .map(|c| c.to_string())
}

fn median_start_ms(shell: &str) -> u128 {
    let mut samples: Vec<u128> = (0..5)
        .map(|_| {
            let t = std::time::Instant::now();
            let mut cmd = std::process::Command::new(shell);
            cmd.args(["-c", "exit 0"]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let _ = cmd.status();
            t.elapsed().as_millis()
        })
        .collect();
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Point Claude Code at a faster shell, for new terminals.
///
/// `setx` rather than a registry write: it is the documented way to set a persistent user
/// variable, it broadcasts the change, and it is one line instead of a registry dependency.
/// Only ever called from an explicit menu action - this changes the user's environment, and
/// nothing here does that on its own initiative.
pub fn use_faster_shell(path: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new("setx");
    cmd.args(["CLAUDE_CODE_GIT_BASH_PATH", path]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    match cmd.status() {
        Ok(st) if st.success() => Ok(format!(
            "Claude Code will use {path} for hooks. Open a new terminal for it to take effect."
        )),
        Ok(st) => Err(format!("setx exited with {st}")),
        Err(e) => Err(format!("could not run setx: {e}")),
    }
}

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
fn merge_install(root: &mut Value, hook_exe: &str, wiring: &[(&str, bool, &str)]) {
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

    for (event, need_matcher, internal) in wiring {
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
/// Removes our commands from one entry, leaving anyone else's, and reports how many went.
///
/// An entry's `hooks` array can hold several commands, and nothing stops a user putting one
/// of theirs beside one of ours - Claude Code's own documentation shows multiple commands per
/// entry. Judging the entry as a whole and deleting it therefore deletes their command too,
/// silently, in a file they own. Removal has to happen one command at a time.
fn strip_ours(entry: &mut Value) -> usize {
    let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
        return 0;
    };
    let before = hooks.len();
    hooks.retain(|h| {
        !h["command"]
            .as_str()
            .map(|c| HOOK_COMMAND_MARKERS.iter().any(|m| c.contains(m)))
            .unwrap_or(false)
    });
    before - hooks.len()
}

/// True when an entry has no commands left and is therefore only an empty shell.
fn is_empty_entry(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false)
}

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
        if let Some(arr) = v.as_array_mut() {
            let mut dropped_commands = 0;
            // Strip our commands from inside each entry rather than judging the entry as a
            // whole. An entry may legitimately hold one of ours beside one of the user's, and
            // deleting the entry would take theirs with it - silent data loss in a file this
            // application does not own.
            //
            // `touched` records which entries we actually took something from, because only
            // those may be removed when they end up empty. An entry that arrived empty was
            // the user's to keep, odd as it is, and tidying it away would be this code editing
            // a file it does not own for cosmetic reasons.
            let mut touched = Vec::with_capacity(arr.len());
            for entry in arr.iter_mut() {
                let n = strip_ours(entry);
                dropped_commands += n;
                touched.push(n > 0);
            }
            let mut i = 0;
            arr.retain(|e| {
                let keep = !(touched[i] && is_empty_entry(e));
                i += 1;
                keep
            });
            removed += dropped_commands;
            if arr.is_empty() && dropped_commands > 0 {
                emptied.push(event.clone());
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

    // Measured before deciding, every time, because the answer is a property of this machine
    // and it can change: installing Git Bash, or pointing Claude Code at it, moves this by an
    // order of magnitude.
    let probe = probe_shell();
    let wiring = wiring_for(probe.median_ms);

    let mut root = load(&path);
    merge_install(&mut root, &hook_exe.display().to_string(), wiring);
    backup_and_write(&path, &root)?;

    let mut msg = format!(
        "wrote {} ({} events, shell {} ms per hook)",
        path.display(),
        wiring.len(),
        probe.median_ms
    );
    if wiring.len() < WIRING.len() {
        msg.push_str(" - fewer than usual, because that shell is slow");
    }
    if probe.faster.is_some() {
        msg.push_str("; tray has \"Speed up hooks\"");
    }
    Ok(msg)
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

/// `probe_shell().faster`, measured once per process.
///
/// The probe starts a shell five times, which is fine at install time and not fine on every
/// tray-menu rebuild - the menu is rebuilt on every toggle, and five WSL starts is four seconds
/// of a menu that has not appeared yet. The answer only changes when the user installs a shell
/// or changes an environment variable, neither of which happens mid-session without a restart.
pub fn cached_faster_shell() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| probe_shell().faster).clone()
}
