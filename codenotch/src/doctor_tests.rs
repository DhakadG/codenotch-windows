//! Tests for the guarantee that `codenotch.exe doctor` never prints a secret.
//!
//! Two layers, because neither is sufficient alone:
//!
//! 1. `redact` is tested exhaustively against credential shapes and, just as importantly,
//!    against the diagnostic detail it must leave alone. A redactor that eats session ids
//!    and project paths passes a security review and fails every support conversation.
//! 2. `no_secret_from_the_real_credential_files_reaches_the_report` reads whatever
//!    credentials exist on this machine and asserts that none of those exact strings
//!    appear in a real report. That is the actual guarantee: it does not guess at shapes,
//!    it compares against the true values.
//!
//! The second test is `#[ignore]`d, because it runs the real probes — binding a port,
//! reading SQLite, and reaching a local HTTPS bridge — which is machine dependent and not
//! something a hosted CI runner should be made to sit through. Run it with
//! `cargo test -- --ignored` on a developer machine that is signed in to the providers.

use super::*;

// ---------------- redact: things that must go ----------------

#[test]
fn a_jwt_is_redacted() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let out = redact(&format!("claude: token {jwt} ok"));
    assert_eq!(out, "claude: token [redacted] ok");
    assert!(!out.contains("eyJ"));
}

#[test]
fn a_prefixed_api_key_is_redacted() {
    assert_eq!(
        redact("key=sk-ant-api03-AAAABBBBCCCCDDDD end"),
        "key=[redacted] end"
    );
    assert_eq!(redact("key=sk-proj-AAAABBBB end"), "key=[redacted] end");
}

#[test]
fn a_bearer_token_is_redacted_whatever_its_shape() {
    // The keyword is proof enough; the token after it need not look like anything.
    // `Bearer` itself is kept: it says which scheme the request used, which is worth
    // knowing in a diagnostic and gives nothing away.
    assert_eq!(
        redact("Authorization: Bearer abc123 (15s timeout)"),
        "Authorization: Bearer [redacted] (15s timeout)"
    );
    assert_eq!(redact("authorization: bearer x"), "authorization: bearer [redacted]");
    // A trailing "Bearer" with nothing after it must not panic or slice mid-character.
    assert_eq!(redact("header: Bearer "), "header: Bearer ");
}

#[test]
fn redaction_applies_to_every_line_not_just_the_first() {
    let s = "line one is fine\ntoken eyJhbGci.eyJzdWIi.sig\nline three is fine\n";
    let out = redact(s);
    assert!(out.contains("line one is fine"));
    assert!(out.contains("line three is fine"));
    assert!(!out.contains("eyJ"));
    // Line structure and the trailing newline survive, so the report still reads as a report.
    assert_eq!(out.lines().count(), 3);
    assert!(out.ends_with('\n'));
}

// ---------------- redact: things that must stay ----------------

#[test]
fn a_session_uuid_survives() {
    // 36 characters of token-shaped text, and the single most useful identifier in a bug
    // report. Any length-based redaction rule would remove it, which is why there is none.
    let line = "  updated 12s ago  89e0da6d-74c3-4d5e-aad8-2d044821da60.jsonl";
    assert_eq!(redact(line), line);
}

#[test]
fn a_flattened_project_path_survives() {
    // Claude Code flattens a working directory into one long directory name. This one is
    // 74 characters with no separator a scanner could use to break it up.
    let line = r"root: C:\Users\me\.claude\projects\C--Users-me-Downloads-Programs-VS-Code-Works-codenotch-portToWindows exists";
    assert_eq!(redact(line), line);
}

#[test]
fn ordinary_diagnostic_detail_survives() {
    for line in [
        "== Codenotch doctor v0.3.0 ==",
        "config: port=51789 lang=auto (C:\\Users\\me\\AppData\\Roaming\\codenotch\\config.json)",
        "port: free - no Codenotch instance is running",
        "    tail parses OK: type=assistant sessionId=89e0da6d-74c3-4d5e-aad8-2d044821da60",
        "codex: signed in, plan=pro, 5h limit 40%",
        "cursor: state.vscdb found, membership=pro, billing cycle ends 2026-10-01T00:00:00Z",
        "antigravity: language_server on 127.0.0.1:51234, quota 25% remaining",
        "  (no session transcripts)",
    ] {
        assert_eq!(redact(line), line, "wrongly redacted: {line}");
    }
}

#[test]
fn a_sentence_is_not_swallowed_by_the_jwt_rule() {
    // The dot only continues a run that already began with `eyJ`, so ordinary prose and
    // dotted file names keep their shape.
    let line = "tail failed to parse. see watch.log for the last 20 lines.";
    assert_eq!(redact(line), line);
}

#[test]
fn redact_is_a_no_op_on_an_empty_report() {
    assert_eq!(redact(""), "");
    assert_eq!(redact("\n"), "\n");
}

// ---------------- the real guarantee ----------------

/// Reads every credential file this machine has and returns the token-ish values in them.
///
/// Only values are collected, never keys: a report is allowed to say the word
/// `accessToken`, it is not allowed to print one.
fn secrets_on_this_machine() -> Vec<String> {
    fn harvest(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => {
                // Short values are plan names, e-mail addresses and ISO dates, not secrets,
                // and searching a report for "pro" would fail on every line.
                if s.len() >= 20 {
                    out.push(s.clone());
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| harvest(x, out)),
            serde_json::Value::Object(o) => o.values().for_each(|x| harvest(x, out)),
            _ => {}
        }
    }

    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    for rel in [".claude/.credentials.json", ".codex/auth.json"] {
        let path = home.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        harvest(&v, &mut out);
    }
    out
}

#[test]
#[ignore = "runs the real probes (port bind, SQLite, local HTTPS bridge); run with --ignored on a signed-in machine"]
fn no_secret_from_the_real_credential_files_reaches_the_report() {
    let secrets = secrets_on_this_machine();
    assert!(
        !secrets.is_empty(),
        "no credentials found on this machine, so this test would pass vacuously; \
         sign in to Claude Code or Codex before running it"
    );

    let report = run();
    for secret in &secrets {
        assert!(
            !report.contains(secret.as_str()),
            "doctor leaked a credential value of {} characters",
            secret.len()
        );
        // A leak that survives the report being split across lines is still a leak.
        let head: String = secret.chars().take(20).collect();
        assert!(
            !report.contains(&head),
            "doctor leaked the first 20 characters of a credential"
        );
    }
}
