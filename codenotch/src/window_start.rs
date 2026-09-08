//! Open the five-hour window on purpose, instead of by accident.
//!
//! # The problem
//!
//! The five-hour window does not run on a clock; it starts when you send your first message and
//! ends five hours later. So the window you get is decided by the first thing you happen to do,
//! which is rarely when you would have chosen. Send one message at 09:55 to check something and
//! the window you were saving for the afternoon is now half spent by lunch.
//!
//! Deciding when it starts is the whole feature. One cheap message, sent when *you* want the
//! clock to start, and the next five hours are aligned with the work rather than with whatever
//! you did first.
//!
//! # One correction to the obvious design
//!
//! The natural way to picture this is "keep a chat open with Haiku and say hi to it now and
//! then, having told it once not to reply at length". There is no chat to keep. The Messages
//! API is stateless: every request carries whatever history you choose to send, and nothing
//! persists between calls. So there is no first message to establish an agreement with, and no
//! conversation accumulating context and cost.
//!
//! What is left is better than the picture. Each call is a fresh two-line request - a system
//! line saying what this is, and the word "hi" - with `max_tokens` set low enough that a long
//! reply is impossible rather than merely discouraged. Nothing to remember, nothing to grow,
//! and the instruction cannot be forgotten because it is re-sent every time.
//!
//! # What it costs, and what it spends
//!
//! Haiku, a handful of tokens in and at most a handful out. The point is not that it is free:
//! it is that it spends the smallest amount that still counts as a message, because *counting
//! as a message* is the entire purpose. A window opened deliberately costs one Haiku call; a
//! window opened by accident costs whatever the afternoon needed.
//!
//! # What it will not do
//!
//! It will not send anything on a borrowed credential. Reading Claude Code's token to *display*
//! a number is one thing; using it to generate inference on someone's account is another, and
//! this app does not do the second. Signing in gives it a session of its own, and this only
//! works from that.

use crate::oauth;

/// Haiku: the cheapest model that still opens the window.
const MODEL: &str = "claude-haiku-4-5-20251001";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// The same beta header the usage endpoint requires for an OAuth token.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Enough for "Noted." and not enough for anything else.
///
/// A cap rather than an instruction. Asking politely for a short reply is a request the model
/// may reasonably interpret; four tokens is arithmetic.
const MAX_TOKENS: u32 = 8;

/// Never twice inside this. The window lasts five hours, so a second call inside a few minutes
/// cannot be starting anything - it is a double click, or a menu clicked twice because the
/// first one seemed not to do anything.
const MIN_INTERVAL_SECS: u64 = 300;

static LAST_SENT: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether starting a window now would do anything.
///
/// Split from the sending so the decision can be tested without a network, and so the tray can
/// ask the same question it would be answered with after clicking.
///
/// `open_until` is the moment the current five-hour window resets, if one is running.
pub(crate) fn why_not(
    signed_in: bool,
    open_until: Option<u64>,
    last_sent: u64,
    now: u64,
) -> Option<String> {
    if !signed_in {
        return Some(
            "Sign in to Claude first - Codenotch will not send anything on Claude Code's borrowed credential.".into(),
        );
    }
    if last_sent != 0 && now.saturating_sub(last_sent) < MIN_INTERVAL_SECS {
        let wait = MIN_INTERVAL_SECS - now.saturating_sub(last_sent);
        return Some(format!("Just did that. Try again in {wait}s if it did not take."));
    }
    if let Some(until) = open_until {
        if until > now {
            let mins = (until - now) / 60;
            let (h, m) = (mins / 60, mins % 60);
            return Some(format!(
                "A five-hour window is already running - it resets in {h}h {m:02}m. Starting one now would spend a message for nothing."
            ));
        }
    }
    None
}

/// The request body. Separated so its shape is testable; it is the whole contract with the API.
fn body() -> serde_json::Value {
    serde_json::json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        // Said every time, because there is no conversation for it to persist in. That is a
        // feature: the instruction cannot drift out of context, because it is never in one.
        "system": "You are being pinged by a usage meter purely to start a rate-limit window. \
                   Reply with the single word: Noted.",
        "messages": [{ "role": "user", "content": "hi" }],
    })
}

/// Send it. Returns what to show the user either way.
pub fn start_window(open_until: Option<u64>) -> Result<String, String> {
    let now = now_secs();
    let last = *LAST_SENT.lock().unwrap();
    if let Some(reason) = why_not(oauth::is_signed_in(), open_until, last, now) {
        return Err(reason);
    }
    let token = oauth::access_token()
        .ok_or("Signed in, but the token could not be refreshed - sign in again.")?;

    let resp = ureq::post(MESSAGES_URL)
        .set("authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .set("user-agent", "codenotch")
        .timeout(std::time::Duration::from_secs(20))
        .send_json(body());

    match resp {
        Ok(_) => {
            *LAST_SENT.lock().unwrap() = now;
            crate::applog("window start: sent");
            Ok("Five-hour window started. It resets five hours from now.".into())
        }
        Err(ureq::Error::Status(429, _)) => {
            // Recorded even so. A 429 means the account is already at a limit, and hammering
            // the inference endpoint to find out again is exactly what everything else in this
            // application is built to avoid.
            *LAST_SENT.lock().unwrap() = now;
            Err("Rate limited - the window cannot be started while the account is at a limit.".into())
        }
        Err(ureq::Error::Status(code, r)) => {
            let detail: String = r.into_string().unwrap_or_default().chars().take(200).collect();
            crate::applog(&format!("window start: HTTP {code}: {detail}"));
            Err(format!("Anthropic refused the message ({code})."))
        }
        Err(e) => {
            crate::applog(&format!("window start: {e}"));
            Err("Could not reach Anthropic.".into())
        }
    }
}

#[cfg(test)]
#[path = "window_start_tests.rs"]
mod tests;

// ---------------------------------------------------------------- keeping it running

/// How long after a reset to send. Not zero: the reset is a server-side boundary and this
/// machine's clock is not the one that decides it, so arriving a few seconds early would open
/// the *old* window again and waste the message. Ten seconds is longer than any clock skew
/// worth worrying about and short enough that nobody notices the gap.
const AFTER_RESET_SECS: u64 = 10;

/// How stale a reading may be and still be trusted to schedule from.
///
/// A reset time is an absolute instant, so an old reading is not wrong about *when* - but it
/// can be wrong about whether a window exists at all, and acting on that spends a message. Ten
/// minutes matches the refetch floor, so this never asks for fresher data than the app collects.
const MAX_READING_AGE_SECS: u64 = 600;

/// What the watcher should do next, given what it knows.
///
/// Every decision in one pure function so the whole schedule can be tested at any point in a
/// five-hour cycle without waiting for one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Send now.
    Send,
    /// Do nothing for this long, then look again.
    Wait(u64),
}

pub(crate) fn next_action(
    enabled: bool,
    signed_in: bool,
    reading_age: Option<u64>,
    resets_at: Option<u64>,
    now: u64,
) -> Next {
    // The idle tick. Long enough to cost nothing, short enough that switching the toggle on
    // does something within a minute rather than feeling broken.
    const IDLE: u64 = 30;
    if !enabled || !signed_in {
        return Next::Wait(IDLE);
    }
    // No reading, or one too old to act on. Waiting is the only honest move: the alternative is
    // sending a message on the strength of information that may be five hours out of date.
    match reading_age {
        None => return Next::Wait(IDLE),
        Some(age) if age > MAX_READING_AGE_SECS => return Next::Wait(IDLE),
        _ => {}
    }
    match resets_at {
        // A fresh reading with no five-hour window in it means none is running, which is
        // exactly the state this feature exists to leave behind.
        None => Next::Send,
        Some(reset) => {
            let due = reset + AFTER_RESET_SECS;
            if now >= due {
                Next::Send
            } else {
                // Sleep to the moment itself when it is close, so the message lands within
                // seconds of the reset rather than up to a tick later; otherwise idle.
                Next::Wait((due - now).min(IDLE))
            }
        }
    }
}

/// Keep the five-hour window running back to back while the toggle is on.
pub fn start_watcher(app: tauri::AppHandle) {
    use tauri::Manager;
    std::thread::spawn(move || loop {
        let enabled = crate::config::load().auto_start_window;
        let (age, resets_at) = {
            let st = app.state::<crate::AppState>();
            let u = st.usage.lock().unwrap();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let age = if u.fetched_at == 0 {
                None
            } else {
                Some(now_ms.saturating_sub(u.fetched_at) / 1000)
            };
            let reset = u
                .windows
                .iter()
                .find(|w| w.id == "session" || w.id == "five_hour")
                .and_then(|w| w.resets_at)
                .map(|ms| ms / 1000);
            (age, reset)
        };
        match next_action(enabled, oauth::is_signed_in(), age, resets_at, now_secs()) {
            Next::Wait(secs) => std::thread::sleep(std::time::Duration::from_secs(secs)),
            Next::Send => {
                // `start_window` re-checks everything, including the five-minute floor, so a
                // scheduling mistake here cannot become a burst of messages.
                let msg = match start_window(resets_at) {
                    Ok(m) => format!("auto: {m}"),
                    Err(e) => format!("auto: {e}"),
                };
                crate::applog(&format!("window start {msg}"));
                crate::usage::request_refresh();
                // Long enough for the refresh to land, so the next pass reads the new window
                // rather than deciding again from the reading that prompted this one.
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    });
}
