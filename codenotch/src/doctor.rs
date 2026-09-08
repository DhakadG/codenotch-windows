//! `codenotch.exe doctor` — self-diagnosis: look instead of guessing.
//! Checks the config, port occupancy, watch roots, the newest session file and how its tail parses,
//! and writes to stdout plus %APPDATA%\codenotch\doctor.log.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn collect(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, SystemTime)>) {
    if depth > 10 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, depth + 1, out);
        } else if crate::watcher::is_session_jsonl(&p) {
            if let Ok(m) = e.metadata() {
                if let Ok(t) = m.modified() {
                    out.push((p, t));
                }
            }
        }
    }
}

fn age_secs(t: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn run() -> String {
    let mut o = String::new();
    // The exact binary first, so a report can never be attributed to the wrong build.
    o += &format!("== doctor: {} ==\n", crate::version_line());

    // What a hook costs on this machine, which is the question that took three wrong answers
    // to reach. The cost is the shell's, not the messenger's, and it is not knowable from the
    // code - only from running it here.
    {
        let probe = crate::hooks_install::probe_shell();
        if probe.path.is_empty() {
            o += "hooks: no bash on PATH - Claude Code cannot run hook commands at all
";
        } else {
            let wiring = crate::hooks_install::wiring_for(probe.median_ms);
            o += &format!(
                "hooks: shell {} at {} ms per hook -> {} event(s) wired
",
                probe.path,
                probe.median_ms,
                wiring.len()
            );
            if let Some(faster) = &probe.faster {
                o += &format!(
                    "hooks: {faster} is installed and faster - tray has \"Speed up hooks\", or set CLAUDE_CODE_GIT_BASH_PATH to it
"
                );
            }
        }
    }

    // What the app has actually spent against the usage endpoint's per-token limit, so
    // "am I the reason for this 429" is answerable from the report rather than guessed at.
    {
        let u = crate::usage::load_persisted();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let recent = u
            .request_log
            .iter()
            .filter(|t| now.saturating_sub(**t) < 60 * 60 * 1000)
            .count();
        o += &format!(
            "usage budget: {recent} request(s) in the last hour, own ceiling {}\n",
            crate::usage::MAX_REQUESTS_PER_HOUR
        );
    }

    let cfg = crate::config::load();
    o += &format!(
        "config: port={} lang={} ({})\n",
        cfg.port,
        cfg.lang,
        crate::config::config_path().display()
    );

    match std::net::TcpListener::bind(("127.0.0.1", cfg.port)) {
        Ok(_) => o += "port: free — no Codenotch instance is running\n",
        Err(_) => o += "port: in use — an instance is already running (quit it from the tray before starting a new build)\n",
    }

    for root in crate::watcher::roots() {
        if !root.exists() {
            o += &format!("root: {} [missing]\n", root.display());
            continue;
        }
        o += &format!("root: {} exists, scanning for the newest session…\n", root.display());
        let mut files = Vec::new();
        collect(&root, 0, &mut files);
        files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
        if files.is_empty() {
            o += "  (no session transcripts)\n";
        }
        for (p, m) in files.into_iter().take(5) {
            o += &format!("  updated {}s ago  {}\n", age_secs(m), p.display());
            match crate::watcher::tail_entry(&p) {
                Some(v) => {
                    o += &format!(
                        "    tail parses OK: type={} sessionId={}\n",
                        v.get("type").and_then(|x| x.as_str()).unwrap_or("?"),
                        v.get("sessionId").and_then(|x| x.as_str()).unwrap_or("(missing, the file name will be used)")
                    );
                }
                None => o += "    tail failed to parse (no valid JSON in the last 30 lines — please report this file)\n",
            }
        }
    }

    o += &format!("\nusage sources:\n  {}\n  {}\n", crate::usage::probe_credentials(), crate::codex::probe());
    o += &format!("  {}\n", crate::cursor::probe());
    o += &format!("  {}\n", crate::antigravity::probe());
    o += &format!("\nprovider glyphs:\n{}\n", crate::glyphs::probe());
    o += &format!("\nworking state:\n  {}\n", crate::activity::probe());

    o += "\nwatch.log (the most recent watcher log, if any):\n";
    if let Some(dir) = dirs::config_dir() {
        let p = dir.join("codenotch").join("watch.log");
        match std::fs::read_to_string(&p) {
            Ok(t) if !t.trim().is_empty() => {
                for line in t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                    o += &format!("  {}\n", line);
                }
            }
            _ => o += "  (empty — the app has not run yet, which is normal on first use, or an older build without the watcher)\n",
        }
    }
    redact(&o)
}

/// Last line of defence before diagnostics reach a screen, a log or a bug report.
///
/// Every probe is written not to return a credential, but `doctor` output is pasted into
/// issues by people who cannot audit it first, so "no probe returns a secret" is a promise
/// that needs a guard rather than a convention. Routing the whole assembled report through
/// one function means a future probe cannot leak by forgetting to redact.
///
/// Replaces anything shaped like a credential with `[redacted]`:
///   - JWTs, which every provider here issues (`eyJ…` header followed by a dot);
///   - Anthropic and OpenAI style prefixed API keys (`sk-ant-…`, `sk-…`);
///   - `Bearer <token>` in any casing.
///
/// Deliberately **not** redacted, and this is the whole design decision: anything that is
/// merely long. A "redact runs over N characters" rule looks safer and is worse, because
/// the things it eats are exactly what makes a diagnostic worth reading — session UUIDs
/// (36 characters), and Claude Code's flattened project directory names, which routinely
/// run past 70 characters with no separator a scanner can see. A report that hides which
/// session it read cannot answer the question it was run to answer.
///
/// The guarantee therefore does not rest on this function alone. `doctor_tests` reads the
/// real credential files on the machine and asserts that no token value in them appears
/// anywhere in the report — an exact check rather than a shape-guessing one.
///
/// Also not redacted: file paths (they carry the user name, but the user is the one
/// running and reading this), port numbers, plan names and timestamps.
fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line));
    }
    if s.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// True for the character set a base64url token is built from.
///
/// Note what is absent: `=`, `+` and `/`. They are part of base64, but including them
/// merges a token into whatever precedes it — `key=sk-ant-…` becomes one run beginning
/// `key`, which then matches no prefix and is printed in full. Since redaction keys off
/// prefixes rather than length, the padding characters buy nothing and cost correctness.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn redact_line(line: &str) -> String {
    // `Bearer <token>` first, and for *every* occurrence. This used to redact only the
    // first and return, so a line carrying two credentials - a request log showing an old
    // and a retried header, say - printed the second one in full. A redactor that stops at
    // the first match is worse than none, because it looks like it worked.
    let mut line = std::borrow::Cow::Borrowed(line);
    let mut from = 0usize;
    loop {
        let lowered = line.to_ascii_lowercase();
        let Some(at) = lowered[from..].find("bearer ").map(|i| from + i) else {
            break;
        };
        let after = at + "bearer ".len();
        let rest = &line[after..];
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let lead = rest.len() - trimmed.len();
        let mut end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        // Give back trailing punctuation. A token written as `Bearer abc123, next` ends at
        // the comma, not after it, and swallowing the separator mangles the line it was
        // meant to keep readable.
        while end > 0 {
            let last = trimmed[..end].chars().next_back().unwrap_or(' ');
            if matches!(last, ',' | ';' | ')' | ']' | '}' | '"' | '\'' | '.') {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        let replaced = format!(
            "{}{}[redacted]{}",
            &line[..after],
            &rest[..lead],
            &trimmed[end..]
        );
        // Continue past what was just written, so a later credential on the same line is
        // still found and the loop cannot rediscover the same keyword forever.
        from = after + lead + "[redacted]".len();
        line = std::borrow::Cow::Owned(replaced);
    }
    let line = line.as_ref();

    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let looks_like_a_key = run.starts_with("sk-ant-") || run.starts_with("sk-");
        let looks_like_a_jwt = run.starts_with("eyJ") && run.matches('.').count() >= 1;
        if looks_like_a_key || looks_like_a_jwt {
            out.push_str("[redacted]");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in line.chars() {
        // A dot is part of the run only while the run could still be a JWT; otherwise a
        // sentence would be swallowed whole.
        let in_run = is_token_char(c) || (c == '.' && run.starts_with("eyJ"));
        if in_run {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
