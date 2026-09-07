//! Tests for the Claude Code hook wiring.
//!
//! This code edits a file the user owns and did not create, `~/.claude/settings.json`,
//! which is the single most destructive thing the application does. The properties that
//! matter are therefore pinned here: installing twice is the same as installing once,
//! nothing the user configured is disturbed, and uninstall is the exact inverse of
//! install.
//!
//! Everything runs against in-memory documents through `merge_install`/`merge_uninstall`,
//! so no test can reach the real settings file.

use super::*;

const HOOK: &str = r"C:\Program Files\Codenotch\codenotch-hook.exe";

fn install_fresh() -> Value {
    let mut root = json!({});
    merge_install(&mut root, HOOK);
    root
}

fn entries(root: &Value, event: &str) -> Vec<Value> {
    root["hooks"][event].as_array().cloned().unwrap_or_default()
}

fn ours(root: &Value, event: &str) -> Vec<Value> {
    entries(root, event).into_iter().filter(is_ours).collect()
}

// ---------------- Recognising our own entries ----------------

#[test]
fn is_ours_matches_the_current_and_the_legacy_hook_names() {
    // The session engine came from the author's earlier Pac-Man project, so installs made
    // by those builds must still be recognised or uninstall would orphan them forever.
    for name in ["codenotch-hook", "eatbean-hook", "pacman-hook"] {
        let e = json!({ "hooks": [{ "type": "command", "command": format!("\"C:\\x\\{name}.exe\" running") }] });
        assert!(is_ours(&e), "{name} should be recognised as ours");
    }
}

#[test]
fn is_ours_does_not_claim_someone_elses_hook() {
    for e in [
        json!({ "hooks": [{ "type": "command", "command": "npm run lint" }] }),
        json!({ "hooks": [] }),
        json!({ "hooks": [{ "type": "command" }] }),
        json!({ "hooks": "not an array" }),
        json!({}),
        json!(null),
    ] {
        assert!(!is_ours(&e), "wrongly claimed {e}");
    }
}

// ---------------- Install ----------------

#[test]
fn install_wires_every_event_once() {
    let root = install_fresh();
    for (event, _, _) in WIRING {
        assert_eq!(ours(&root, event).len(), 1, "{event} should have one entry");
    }
    assert_eq!(
        root["hooks"].as_object().map(|o| o.len()),
        Some(WIRING.len())
    );
}

#[test]
fn install_passes_the_internal_event_name_to_the_hook_binary() {
    let root = install_fresh();
    let cmd = root["hooks"]["Notification"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    // The path is quoted because Program Files contains a space.
    assert_eq!(cmd, format!("\"{HOOK}\" attention"));
    assert_eq!(root["hooks"]["Stop"][0]["hooks"][0]["command"], json!(format!("\"{HOOK}\" done")));
    assert_eq!(root["hooks"]["SessionEnd"][0]["hooks"][0]["command"], json!(format!("\"{HOOK}\" session_end")));
}

#[test]
fn install_sets_a_timeout_so_a_wedged_hook_cannot_hang_claude_code() {
    let root = install_fresh();
    for (event, _, _) in WIRING {
        assert_eq!(root["hooks"][*event][0]["hooks"][0]["timeout"], json!(5), "{event}");
    }
}

#[test]
fn a_matcher_is_present_exactly_where_the_wiring_says() {
    let root = install_fresh();
    for (event, need_matcher, _) in WIRING {
        let has = root["hooks"][*event][0].get("matcher").is_some();
        assert_eq!(has, *need_matcher, "matcher presence wrong for {event}");
    }
}

#[test]
fn no_hook_is_wired_to_a_per_tool_call_event() {
    // The guard for the defect that made sessions freeze. Claude Code starts a POSIX shell
    // for every hook invocation, and on a machine whose `bash` is the WSL one that shell took
    // up to 4.2 seconds to start - against 57 ms for the messenger it runs. Wired to
    // PreToolUse and PostToolUse, that cost was paid twice on every tool call.
    //
    // Anything that fires per tool call belongs nowhere near this list, however useful the
    // event is: the transcript watcher already reports tool-level activity without spawning
    // anything at all.
    const PER_TOOL_CALL: &[&str] = &["PreToolUse", "PostToolUse"];
    for (event, _, _) in WIRING {
        assert!(
            !PER_TOOL_CALL.contains(event),
            "{event} fires on every tool call and must not be wired to a hook"
        );
    }
    let root = install_fresh();
    for event in PER_TOOL_CALL {
        assert!(
            ours(&root, event).is_empty(),
            "{event} must not receive one of our entries"
        );
    }
}

#[test]
fn installing_twice_is_the_same_as_installing_once() {
    let once = install_fresh();
    let mut twice = install_fresh();
    merge_install(&mut twice, HOOK);
    assert_eq!(once, twice);
}

#[test]
fn reinstalling_after_a_move_replaces_the_stale_path() {
    // Upgrading or relocating the application must not leave a hook pointing at a binary
    // that no longer exists; Claude Code would report a failing hook on every event.
    let mut root = json!({});
    merge_install(&mut root, r"C:\Old\codenotch-hook.exe");
    merge_install(&mut root, HOOK);
    let cmds: Vec<String> = entries(&root, "Stop")
        .iter()
        .map(|e| e["hooks"][0]["command"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(cmds, [format!("\"{HOOK}\" done")]);
}

#[test]
fn install_preserves_the_users_own_hooks_and_settings() {
    let mut root = json!({
        "model": "opus",
        "hooks": {
            // An event we do wire, so the sharing behaviour is exercised.
            "Stop": [
                { "hooks": [{ "type": "command", "command": "echo audit" }] }
            ],
            // An event we never wire, and one that fires per tool call, so both kinds of
            // "not ours" are covered.
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo pretool" }] }
            ],
            "PreCompact": [
                { "hooks": [{ "type": "command", "command": "echo compacting" }] }
            ]
        }
    });
    merge_install(&mut root, HOOK);

    // Unrelated top-level settings are untouched.
    assert_eq!(root["model"], json!("opus"));
    // Events we do not wire are untouched, including the user's own per-tool-call hook.
    assert_eq!(entries(&root, "PreCompact").len(), 1);
    assert_eq!(entries(&root, "PreToolUse").len(), 1);
    assert_eq!(entries(&root, "PreToolUse")[0]["hooks"][0]["command"], json!("echo pretool"));
    // An event we share keeps the user's entry, and ours is appended rather than inserted
    // ahead of it, so their ordering assumptions survive.
    let stop = entries(&root, "Stop");
    assert_eq!(stop.len(), 2);
    assert_eq!(stop[0]["hooks"][0]["command"], json!("echo audit"));
    assert!(is_ours(&stop[1]));
}

#[test]
fn install_repairs_a_settings_file_it_cannot_understand() {
    // A corrupt or non-object document must not stop the wiring; `load` already degrades a
    // damaged file to `{}`, and a `hooks` key of the wrong type is replaced rather than
    // indexed into, which would panic.
    for mut root in [
        json!(null),
        json!([1, 2, 3]),
        json!("nonsense"),
        json!({ "hooks": "nonsense" }),
        json!({ "hooks": [] }),
    ] {
        merge_install(&mut root, HOOK);
        assert_eq!(ours(&root, "Stop").len(), 1, "failed to repair");
    }
}

#[test]
fn install_survives_an_event_array_holding_junk() {
    let mut root = json!({ "hooks": { "Stop": [null, 7, { "hooks": "x" }] } });
    merge_install(&mut root, HOOK);
    // Nothing is discarded, because none of it is ours to discard.
    assert_eq!(entries(&root, "Stop").len(), 4);
    assert_eq!(ours(&root, "Stop").len(), 1);
}

// ---------------- Uninstall ----------------

#[test]
fn uninstall_is_the_exact_inverse_of_install() {
    let original = json!({
        "model": "opus",
        "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo audit" }] }
            ],
            "PreCompact": [
                { "hooks": [{ "type": "command", "command": "echo compacting" }] }
            ]
        }
    });
    let mut root = original.clone();
    merge_install(&mut root, HOOK);
    let removed = merge_uninstall(&mut root);

    assert_eq!(removed, WIRING.len());
    // Byte-for-byte the document we started with: the events we created are gone rather
    // than left behind as empty arrays, and the ones we shared are back to one entry.
    assert_eq!(root, original);
}

#[test]
fn uninstall_from_an_empty_document_leaves_an_empty_hooks_object() {
    let mut root = install_fresh();
    assert_eq!(merge_uninstall(&mut root), WIRING.len());
    // The `hooks` key itself is kept. It is now empty and inert, and removing a key the
    // user may have written themselves is a bigger liberty than leaving one behind.
    assert_eq!(root, json!({ "hooks": {} }));
}

#[test]
fn uninstall_removes_legacy_entries_too() {
    let mut root = json!({
        "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": "\"C:\\old\\pacman-hook.exe\" done" }] },
                { "hooks": [{ "type": "command", "command": "\"C:\\old\\eatbean-hook.exe\" done" }] }
            ]
        }
    });
    assert_eq!(merge_uninstall(&mut root), 2);
    assert_eq!(root, json!({ "hooks": {} }));
}

#[test]
fn uninstall_touches_nothing_when_we_were_never_installed() {
    let original = json!({
        "model": "opus",
        "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "echo bye" }] }] }
    });
    let mut root = original.clone();
    assert_eq!(merge_uninstall(&mut root), 0);
    assert_eq!(root, original);
}

#[test]
fn uninstall_is_idempotent() {
    let mut root = install_fresh();
    assert_eq!(merge_uninstall(&mut root), WIRING.len());
    let after_first = root.clone();
    assert_eq!(merge_uninstall(&mut root), 0);
    assert_eq!(root, after_first);
}

#[test]
fn uninstall_ignores_a_document_with_no_hooks_section() {
    for mut root in [json!({}), json!({ "model": "opus" }), json!({ "hooks": "nonsense" }), json!(null)] {
        let before = root.clone();
        assert_eq!(merge_uninstall(&mut root), 0);
        assert_eq!(root, before);
    }
}
