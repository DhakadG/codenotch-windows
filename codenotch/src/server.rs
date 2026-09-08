//! Local server on 127.0.0.1, doing two jobs.
//!
//! `POST /event?e=<event>&ppid=<pid>` receives codenotch-hook's forwarded Claude Code hook, with
//! the hook's stdin JSON as the body. Lenient parsing: no missing field is an error.
//!
//! `GET /signin` and `POST /signin` are the sign-in page. Anthropic's OAuth callback shows the
//! code on its own page for the user to copy rather than redirecting to a loopback port, so the
//! paste has to land somewhere - and a page served by a server this app already runs is less
//! machinery than a second window would be. It is reachable only from this machine, and it
//! carries no token: it hands the pasted string to `oauth::complete` and reports what happened.

use crate::state::HookEvent;
use crate::AppState;
use std::io::Read;
use tauri::{AppHandle, Manager};

pub fn start(app: AppHandle, port: u16) {
    std::thread::spawn(move || {
        let server = match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[codenotch] failed to bind port {port}: {e} (is another instance running?)");
                return;
            }
        };
        for mut req in server.incoming_requests() {
            let url = req.url().to_string();
            let mut body = String::new();
            let _ = req
                .as_reader()
                .take(256 * 1024)
                .read_to_string(&mut body);
            //
            // tiny_http serves this loop on one thread, and the sender is codenotch-hook,
            // which Claude Code runs before and after every tool call. Applying the event
            // and repainting the notch before replying put all of that on Claude Code's
            // critical path: a slow broadcast, a locked mutex or a busy WebView became
            // seconds of latency in someone else's editor, and further hook connections
            // queued behind it. The reply carries no information - it is the literal string
            // "ok" - so there is nothing to be gained by making the caller wait for it.
            // The sign-in page answers with its own body, so it is handled before the blanket
            // "ok". Everything else keeps the answer-first rule below.
            if url.starts_with("/signin") {
                let is_post = *req.method() == tiny_http::Method::Post;
                let html = if is_post {
                    let pasted = form_field(&body, "code");
                    match crate::oauth::complete(&pasted) {
                        Ok(()) => {
                            crate::applog("oauth: signed in");
                            crate::usage::request_refresh();
                            let _ = crate::tray::rebuild(&app);
                            signin_page(Some(Ok(())))
                        }
                        Err(e) => {
                            crate::applog(&format!("oauth: sign-in failed: {e}"));
                            signin_page(Some(Err(e)))
                        }
                    }
                } else {
                    signin_page(None)
                };
                let header = "Content-Type: text/html; charset=utf-8".parse::<tiny_http::Header>();
                let mut response = tiny_http::Response::from_string(html);
                if let Ok(h) = header {
                    response.add_header(h);
                }
                let _ = req.respond(response);
                continue;
            }

            // Answer first, work afterwards.
            let _ = req.respond(tiny_http::Response::from_string("ok"));

            if url.starts_with("/event") {
                let ev = parse(&url, &body);
                let state = app.state::<AppState>();
                let changed = {
                    let mut store = state.store.lock().unwrap();
                    store.apply(ev)
                };
                if changed {
                    crate::broadcast(&app);
                }
            }
        }
    });
}

fn query_param(url: &str, key: &str) -> String {
    let q = url.split_once('?').map(|x| x.1).unwrap_or("");
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().unwrap_or("").to_string();
        }
    }
    String::new()
}

fn parse(url: &str, body: &str) -> HookEvent {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    // tool_input.command (Bash etc.) feeds the "last action" summary
    let tool_cmd = v
        .get("tool_input")
        .and_then(|t| t.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    HookEvent {
        e: query_param(url, "e"),
        session_id: {
            let id = s("session_id");
            if id.is_empty() { "unknown".into() } else { id }
        },
        ppid: query_param(url, "ppid").parse().unwrap_or(0),
        cwd: s("cwd"),
        prompt: s("prompt"),
        message: s("message"),
        tool_name: s("tool_name"),
        tool_cmd,
        model: s("model"),
        src: "hook",
    }
}

/// One field out of an `application/x-www-form-urlencoded` body.
fn form_field(body: &str, key: &str) -> String {
    for pair in body.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k == key {
            return percent_decode(&v.replace('+', " "));
        }
    }
    String::new()
}

/// Percent-decoding, tolerant of a stray `%` that is not followed by two hex digits - a pasted
/// code containing one should produce a clear rejection from the token endpoint, not a parse
/// failure here that blames the user for the wrong thing.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The sign-in page: a form, and the outcome of the last submission if there was one.
///
/// Deliberately plain and self-contained - no scripts, no fonts, nothing fetched. It exists for
/// perhaps thirty seconds in someone's life and its only job is to be unambiguous about whether
/// the sign-in worked.
fn signin_page(outcome: Option<Result<(), String>>) -> String {
    let banner = match &outcome {
        Some(Ok(())) => "<p class=ok>Signed in. You can close this tab - the notch will update on its next poll.</p>".to_string(),
        Some(Err(e)) => format!("<p class=bad>{}</p>", html_escape(e)),
        None => String::new(),
    };
    format!(
        "<!doctype html><meta charset=utf-8><title>Sign in to Claude</title><style>body{{font:15px/1.55 \"Segoe UI\",system-ui,sans-serif;background:#111;color:#eee;margin:0;display:flex;min-height:100vh;align-items:center;justify-content:center}}main{{max-width:34rem;padding:2rem}}h1{{font-size:1.3rem;margin:0 0 .75rem}}p{{color:#bbb}}input{{width:100%;padding:.6rem;font:inherit;background:#1c1c1c;color:#eee;border:1px solid #3a3a3a;border-radius:6px;box-sizing:border-box}}button{{margin-top:.75rem;padding:.6rem 1.1rem;font:inherit;background:#c96442;color:#fff;border:0;border-radius:6px;cursor:pointer}}.ok{{color:#7bd88f}}.bad{{color:#ff8f8f}}</style><main><h1>Sign in to Claude</h1>{banner}<p>The other tab asked Anthropic to authorize Codenotch. Approve it there, then copy the code it shows you and paste it below. The whole <code>code#state</code> string is fine, and so is the address bar.</p><form method=post><input name=code autofocus autocomplete=off placeholder=\"Paste the code here\"><button type=submit>Finish signing in</button></form><p style=\"margin-top:1.5rem;font-size:13px\">Codenotch stores this token in Windows Credential Manager and refreshes it on its own. It does not read or change Claude Code&rsquo;s credential.</p></main>"
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
