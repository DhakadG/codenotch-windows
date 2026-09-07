//! Claude usage adapter (official), implemented from the upstream Codenotch's documented behaviour.
//! Endpoint: GET https://api.anthropic.com/api/oauth/usage
//! Headers: Authorization: Bearer <token>; anthropic-beta: oauth-2025-04-20; 15 s timeout
//! Rules (upstream's discipline):
//!   - the credential comes from Claude Code's own store (Windows: ~/.claude/.credentials.json), read only
//!   - 401/403 → re-read the credential once and retry (Claude Code may have just refreshed the token) → still failing means needsAuth
//!   - 429 → back off 60 s × 2^n capped at 15 min, Retry-After only raises it; the deadline is persisted
//!   - never invent a percentage on failure: keep the last reading marked stale, and the UI shows how old it is
//! Reply (snake_case): { limits:[{kind,percent,resets_at}], five_hour:{utilization,resets_at}, seven_day:{...} }
//! limits is the forward-compatible main shape; five_hour/seven_day are merged in as a fallback (a window that just rolled over disappears from limits).

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const POLL_ACTIVE_SECS: u64 = 60;
const POLL_IDLE_SECS: u64 = 300;
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 900;
/// How often the app may ask Claude Code to refresh its own credential. Ten minutes is
/// frequent enough that an expiry is picked up promptly and rare enough that a machine left
/// signed out does not spawn a process every minute forever.
const NUDGE_EVERY_MS: u64 = 10 * 60 * 1000;
/// Minimum age of the last good reading before the endpoint is asked again.
///
/// `/api/oauth/usage` is rate-limited per token, and a persisted reading is the same answer
/// it would give. Without this floor every launch fetched immediately, so restarting the app
/// - which the hook does whenever it is not running - became a tight loop against a limiter
/// that answers with an hour of 429. Five minutes is what other readers of this endpoint
/// settled on independently, and a usage percentage does not move meaningfully faster.
pub const MIN_REFETCH_SECS: u64 = 300;

/// Whether a persisted reading is young enough to serve instead of asking again.
///
/// Shared by every provider, because the problem is shared: the hook restarts this app
/// whenever it is not running, and each start used to fetch from all four providers at once.
/// A reading restored from disk is the same answer the endpoint would give, so honouring its
/// age turns a restart storm into no requests at all. `fetched_at == 0` means there has never
/// been a reading, which is exactly when one should be fetched.
pub fn too_fresh(fetched_at: u64, now: u64) -> bool {
    fetched_at > 0 && now.saturating_sub(fetched_at) / 1000 < MIN_REFETCH_SECS
}
/// Hard ceiling on requests to the usage endpoint in any rolling hour.
///
/// The refetch floor already spaces normal polling to twelve an hour. This is the backstop
/// for everything that bypasses it - manual refreshes, repeated restarts, a future code
/// path that forgets the floor exists - so that no sequence of events can turn this app
/// into the thing that rate-limits the user's account. Deliberately above normal usage and
/// well below the point where the endpoint objects.
pub const MAX_REQUESTS_PER_HOUR: usize = 20;
const HOUR_MS: u64 = 60 * 60 * 1000;
/// How long `claude auth status` gets before it is killed. Measured at about half a second
/// on a healthy install; ten is generous for a slow disk and still short enough that a
/// wedged CLI cannot stall the poll thread or the tray menu.
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Immediate refresh from the tray or a command
pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Sleep in slices so request_refresh can interrupt it.
///
/// Returns true when it was cut short by a refresh request. The caller needs to know,
/// because the request is consumed here: without the return value an explicit "refresh now"
/// that arrived during a wait would be swallowed, and the minimum-refetch floor would then
/// decline to fetch on the very tick the user asked for one.
#[must_use]
fn sleep_interruptible(total_secs: u64) -> bool {
    for _ in 0..total_secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    /// 0.0–1.0 (fraction used)
    pub used: f64,
    /// Reset time, ms epoch (None = unknown)
    pub resets_at: Option<u64>,
    /// Pure count window (no published denominator, e.g. Antigravity's requests today) — the cell shows ~N and the ring draws only its track
    #[serde(default)]
    pub count: Option<i64>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSnapshot {
    /// ok | stale | needsAuth | backoff | error
    pub status: String,
    pub windows: Vec<LimitWindow>,
    pub fetched_at: u64,
    pub note: String,
    #[serde(default)]
    pub backoff_until: u64,
    /// Epoch-ms of each request made to the usage endpoint in the last hour.
    ///
    /// Persisted deliberately. The endpoint rate-limits per token, and the failure mode
    /// that actually happened here was a burst spread across many short-lived processes:
    /// the hook restarts this app whenever it is not running, and every start used to
    /// fetch. An in-memory counter resets with the process and so cannot see that at all.
    #[serde(default)]
    pub request_log: Vec<u64>,
    /// Which credential earned `backoff_until`, as a non-secret fingerprint.
    ///
    /// `backoff_until` survives a restart but the in-memory note of *whose* backoff it was
    /// did not, so a 429 against credential A followed by a restart and a fresh credential
    /// B left the app waiting out A's deadline for no reason. Storing the fingerprint makes
    /// the association survive too. It is a hash, never the token.
    #[serde(default)]
    pub backoff_for: String,
}

fn store_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("usage.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into(); // an old reading after a restart is labelled as such
            }
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

/// Reads Claude Code's OAuth credential. Returns (token, expired hint).
fn read_credentials() -> Option<(String, bool)> {
    let home = dirs::home_dir()?;
    for name in [".credentials.json", "credentials.json"] {
        let p = home.join(".claude").join(name);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let oauth = v.get("claudeAiOauth").unwrap_or(&v);
        if let Some(tok) = oauth.get("accessToken").and_then(|x| x.as_str()) {
            let expired = oauth
                .get("expiresAt")
                .and_then(|x| x.as_f64())
                .map(|ms| (ms as u64) <= now_ms())
                .unwrap_or(false);
            return Some((tok.to_string(), expired));
        }
    }
    None
}

/// Where Claude Code's CLI lives, if it is installed.
///
/// PATH first, then the two places the installers actually put it. Mirrors what
/// `codex::find_executable` does for the same reason: a user who installed through npm has
/// no `claude.exe` on PATH for a GUI process, because the shim is a `.cmd`.
fn claude_exe() -> Option<std::path::PathBuf> {
    if let Ok(p) = which_on_path("claude.exe") {
        return Some(p);
    }
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local").join("bin").join("claude.exe"),
        dirs::data_dir()?.join("npm").join("claude.cmd"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn which_on_path(name: &str) -> Result<std::path::PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .ok_or(())
}

/// Asks Claude Code to put its own credential in order, and reports whether that changed it.
///
/// Codenotch never mints or rotates a Claude token. The macOS original is explicit about
/// why - "minting a new token would mean writing a credential this app does not own" - and
/// the risk is concrete: refresh tokens rotate, so a second client refreshing behind Claude
/// Code's back can invalidate the token Claude Code still holds and sign the user out of
/// the tool they were using. So the only move available is to ask the owner to do it.
///
/// `claude auth status` is the cheapest way to ask: about half a second, no session, and no
/// transcript written - which matters, because a transcript would appear in this very app
/// as a phantom working session.
///
/// Whether it *also* refreshes an expired token is not documented, and could not be
/// verified without waiting for a real expiry. So this reports what actually happened
/// rather than assuming: the caller logs it, and the log is the evidence for whether a
/// heavier nudge (starting and killing a real session) is ever warranted.
pub fn nudge_claude_credential() -> String {
    let Some(exe) = claude_exe() else {
        return "no claude executable found on PATH or in the usual install locations".into();
    };
    let before = read_credentials().map(|(t, _)| t);

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("auth")
        .arg("status")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // `output()` waits forever. This runs on the poll thread and from the tray menu, so a
    // `claude` that hangs - a stuck update check, a prompt nobody can see, a wedged network
    // call - would take the notch's polling with it and freeze the menu handler. Spawn,
    // wait with a deadline, and kill anything still running past it.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("claude auth status failed to run: {e}"),
    };
    let deadline = std::time::Instant::now() + AUTH_STATUS_TIMEOUT;
    let out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().ok(),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return format!(
                    "claude auth status did not finish within {}s and was stopped",
                    AUTH_STATUS_TIMEOUT.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return format!("claude auth status could not be waited on: {e}"),
        }
    };
    let Some(out) = out else {
        return "claude auth status produced no output".into();
    };

    // `loggedIn:false` is a real sign-out and no amount of waiting will fix it, which is a
    // different message to the user than a token that merely aged out.
    let text = String::from_utf8_lossy(&out.stdout);
    let logged_in = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("loggedIn").and_then(|x| x.as_bool()));

    let after = read_credentials().map(|(t, _)| t);
    let changed = before != after;
    let still_expired = matches!(read_credentials(), Some((_, true)));

    let have_credential = after.is_some();
    match (logged_in, changed, still_expired, have_credential) {
        (Some(false), _, _, _) => "claude reports signed out - run `claude` and sign in".into(),
        // Signed in as far as the CLI is concerned, yet nothing readable on disk. Without
        // its own arm this fell through to "already valid", while the poll loop found no
        // credential at all and quietly skipped every request - two components disagreeing,
        // and the user told the reassuring half.
        (_, _, _, false) => {
            "Claude reports signed in, but Codenotch cannot find a usable credential - run `claude` once".into()
        }
        (_, true, false, _) => "credential refreshed by Claude Code".into(),
        (_, true, true, _) => "Claude Code rewrote the credential but it is still expired".into(),
        (_, false, true, _) => {
            "`claude auth status` did not refresh the expired credential - run `claude` once".into()
        }
        (_, false, false, _) => "credential was already valid".into(),
    }
}

/// For doctor: credential probe report (prints no secret values)
pub fn probe_credentials() -> String {
    match read_credentials() {
        Some((tok, expired)) => format!(
            "credential: found (token {} chars, {})",
            tok.len(),
            if expired { "expired — Claude Code refreshes it on its next use" } else { "valid" }
        ),
        None => "credential: ~/.claude/.credentials.json not found (needsAuth; the desktop app may use another store — signing in once with the Claude Code CLI creates it)".into(),
    }
}

fn parse_reset(v: &serde_json::Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

fn label_for(kind: &str) -> String {
    match kind {
        "session" => "Current session".into(),
        "seven_day" | "weekly_all" => "Weekly (all models)".into(),
        "seven_day_opus" | "weekly_opus" => "Weekly (Opus)".into(),
        "seven_day_sonnet" | "weekly_sonnet" => "Weekly (Sonnet)".into(),
        "extra_usage" => "Extra usage".into(),
        "weekly_scoped" => "Weekly (model-scoped)".into(),
        other => {
            // Forward compatibility: an unknown kind gets a readable label
            let mut s = other.replace('_', " ");
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

fn parse_response(v: &serde_json::Value) -> Vec<LimitWindow> {
    let mut out: Vec<LimitWindow> = Vec::new();
    if let Some(arr) = v.get("limits").and_then(|x| x.as_array()) {
        for l in arr {
            let Some(kind) = l.get("kind").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(pct) = l.get("percent").and_then(|x| x.as_f64()) else {
                continue;
            };
            let resets = l.get("resets_at").and_then(parse_reset);
            if resets.is_none() {
                continue; // upstream rule: a window without a reset time is not shown
            }
            out.push(LimitWindow {
                id: kind.to_string(),
                label: label_for(kind),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: resets, ..Default::default()
            });
        }
    }
    // Fallback merge: a window that just rolled over disappears from limits while the named field remains.
    // In practice the kinds in limits are weekly_all/weekly_scoped, not seven_day — deduplicating by id
    // alone would add the seven_day fallback a second time (the card showed "Weekly all" and
    // "Weekly (all models)" as twins). Three dedupe rules: id alias / same resets_at and percentage / same label.
    // The reply carries more named windows than `limits` ever lists. On a Pro account the
    // model-scoped ones are null, but Max and Team accounts populate them, and reading them
    // costs nothing on the plans that do not.
    let aliases: [(&str, &str, &[&str]); 4] = [
        ("five_hour", "session", &["session", "five_hour"]),
        ("seven_day", "seven_day", &["seven_day", "weekly_all", "weekly"]),
        ("seven_day_opus", "seven_day_opus", &["seven_day_opus", "weekly_opus"]),
        ("seven_day_sonnet", "seven_day_sonnet", &["seven_day_sonnet", "weekly_sonnet"]),
    ];
    for (field, id, alias) in aliases {
        let Some(w) = v.get(field) else { continue };
        let Some(u) = w.get("utilization").and_then(|x| x.as_f64()) else { continue };
        let used = (u / 100.0).clamp(0.0, 1.0);
        let resets_at = w.get("resets_at").and_then(parse_reset);
        let label = label_for(id);
        let dup = out.iter().any(|x| {
            alias.contains(&x.id.as_str())
                || x.label == label
                || (resets_at.is_some()
                    && x.resets_at.map(|r| r / 1000) == resets_at.map(|r| r / 1000)
                    && (x.used - used).abs() < 0.005)
        });
        if dup {
            continue;
        }
        out.push(LimitWindow { id: id.into(), label, used, resets_at, ..Default::default() });
    }
    // Paid extra usage, when the account has opted in. Unlike every other window this one
    // is money rather than a share of an allowance: `monthly_limit` and `used_credits` are
    // in cents, and `utilization` is null until the first spend of a cycle - so a bar keyed
    // on utilization alone would vanish at the start of every month. Derive it from the two
    // amounts instead, and show nothing at all when the feature is switched off.
    if let Some(eu) = v.get("extra_usage").filter(|x| x.is_object()) {
        let enabled = eu.get("is_enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let limit_cents = eu.get("monthly_limit").and_then(|x| x.as_f64());
        let used_cents = eu.get("used_credits").and_then(|x| x.as_f64()).unwrap_or(0.0);
        // A null limit with the feature enabled means uncapped: there is no denominator, so
        // there is no honest percentage. Upstream's rule applies - show the count, not an
        // invented share.
        if enabled {
            match limit_cents {
                Some(limit) if limit > 0.0 => out.push(LimitWindow {
                    id: "extra_usage".into(),
                    label: label_for("extra_usage"),
                    used: (used_cents / limit).clamp(0.0, 1.0),
                    resets_at: None,
                    ..Default::default()
                }),
                _ => out.push(LimitWindow {
                    id: "extra_usage".into(),
                    label: label_for("extra_usage"),
                    used: 0.0,
                    resets_at: None,
                    count: Some((used_cents / 100.0).round() as i64),
                    derived: true,
                }),
            }
        }
    }
    // session always comes first (upstream display order)
    out.sort_by_key(|w| if w.id == "session" { 0 } else { 1 });
    out
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64), // suggested wait in seconds (the Retry-After before the floor is applied)
    Other(String),
}

fn fetch_once(token: &str) -> Result<Vec<LimitWindow>, FetchErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
            Ok(parse_response(&v))
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(FetchErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r
                .header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Err(FetchErr::RateLimited(ra))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

/// A short, non-secret fingerprint of a credential.
///
/// Used only to tell one token from another across restarts. It is deliberately not
/// reversible and never leaves the machine; the whole point is to avoid persisting the
/// token itself just to answer "is this still the credential that got rate limited?".
fn credential_fingerprint(token: &str) -> String {
    // FNV-1a. A cryptographic hash would be misleading here - this defends against nothing,
    // it only distinguishes - and pulling in a dependency to do it would be worse.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Drops request timestamps older than an hour and reports how many remain.
///
/// Kept separate from the check so the pruning is testable on its own, and so a caller
/// cannot accidentally test the budget against an unpruned log.
fn prune_request_log(log: &mut Vec<u64>, now: u64) -> usize {
    log.retain(|t| now.saturating_sub(*t) < HOUR_MS);
    log.len()
}

/// Whether another request is allowed right now, and when the budget next frees up.
///
/// Returns `Ok(())` to proceed, or `Err(seconds)` with how long until the oldest request
/// ages out of the window.
fn budget_check(log: &mut Vec<u64>, now: u64) -> Result<(), u64> {
    if prune_request_log(log, now) < MAX_REQUESTS_PER_HOUR {
        return Ok(());
    }
    // The log is full, so the oldest entry is what has to expire first. It exists: the
    // capacity is non-zero, so a full log is a non-empty one.
    let oldest = log.iter().copied().min().unwrap_or(now);
    let wait_ms = HOUR_MS.saturating_sub(now.saturating_sub(oldest));
    Err(wait_ms.div_ceil(1000).max(1))
}

fn backoff_secs(consecutive: u32, retry_after_floor: u64) -> u64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1u64 << consecutive.min(4));
    exp.clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS).max(retry_after_floor)
}

fn set_and_broadcast(app: &AppHandle, mutate: impl FnOnce(&mut UsageSnapshot)) {
    let st = app.state::<AppState>();
    let snap = {
        let mut u = st.usage.lock().unwrap();
        mutate(&mut u);
        u.clone()
    };
    persist(&snap);
    let _ = app.emit("usage", &snap);
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // Broadcast the persisted old reading at startup (stale beats blank)
        {
            let st = app.state::<AppState>();
            let snap = st.usage.lock().unwrap().clone();
            let _ = app.emit("usage", &snap);
        }
        let mut consecutive_429: u32 = 0;
        // When the expired-credential nudge last ran, so it stays occasional.
        let mut last_nudge: Option<u64> = None;
        // Set when an explicit refresh request was consumed by a wait, so the next pass
        // fetches even though the cached reading is still young.
        let mut forced = false;
        loop {
            // A backoff is tied to one credential. Claude Code rewriting .credentials.json
            // means the next request is a different request, and sitting out the remainder
            // of an hour-long Retry-After after the user has already fixed the problem is
            // the difference between "briefly unavailable" and "apparently broken".
            // Compare against the persisted fingerprint rather than an in-memory copy, so
            // the association survives a restart. Without this a 429 earned by credential A
            // outlived the process that learned about it: the deadline came back from disk,
            // the note of whose it was did not, and a fresh credential B waited out A's
            // hour for nothing.
            {
                let current = read_credentials().map(|(t, _)| credential_fingerprint(&t));
                let st = app.state::<AppState>();
                let mut u = st.usage.lock().unwrap();
                let mismatch = u.backoff_until > 0
                    && (u.backoff_for.is_empty() || Some(&u.backoff_for) != current.as_ref());
                if mismatch {
                    let snap = {
                        u.backoff_until = 0;
                        u.backoff_for.clear();
                        u.note.clear();
                        u.clone()
                    };
                    drop(u);
                    persist(&snap);
                    let _ = app.emit("usage", &snap);
                    consecutive_429 = 0;
                }
            }
            // The expired-credential case is settled before the backoff gate, not after.
            // A backoff only governs whether a *request* may be made, and an expired
            // credential means no request is going to be made either way. Checking it
            // second meant a backoff restored from disk at startup - an hour of it, after
            // one 429 - kept showing "Rate limited" when the honest and actionable answer
            // was that the saved credential had expired.
            if let Some((_, true)) = read_credentials() {
                // Ask Claude Code to sort its own credential out, but not on every tick: a
                // process spawn per minute is its own kind of rude.
                let due = last_nudge.map(|t| now_ms().saturating_sub(t) >= NUDGE_EVERY_MS).unwrap_or(true);
                let outcome = if due {
                    last_nudge = Some(now_ms());
                    let r = nudge_claude_credential();
                    crate::applog(&format!("claude credential nudge: {r}"));
                    Some(r)
                } else {
                    None
                };
                // The nudge may have fixed it, in which case fall through and fetch now
                // rather than waiting out another poll interval.
                if !matches!(read_credentials(), Some((_, true))) {
                    continue;
                }
                set_and_broadcast(&app, |u| {
                    // An expired credential is not a sign-out, and the difference matters:
                    // the last reading is still the truth about the account, just old.
                    // Blanking it to needsAuth is what left the ring spinning with no bar
                    // when the only thing wrong was a token that had aged out overnight.
                    u.status = if u.windows.is_empty() { "needsAuth".into() } else { "stale".into() };
                    u.note = match outcome.as_deref() {
                        Some(o) if o.contains("signed out") => {
                            "Claude Code is signed out. Run `claude` in a terminal and sign in.".into()
                        }
                        _ => "Claude Code's saved credential has expired. Run `claude` in a terminal to refresh it — Codenotch reads that file and never rotates it, so that a background refresh cannot sign you out of Claude Code.".to_string()
                    };
                    u.backoff_until = 0;
                });
                consecutive_429 = 0;
                forced = sleep_interruptible(POLL_ACTIVE_SECS);
                continue;
            }
            // No requests inside the backoff window
            let (bu, fetched_at) = {
                let st = app.state::<AppState>();
                let u = st.usage.lock().unwrap();
                (u.backoff_until, u.fetched_at)
            };
            let now = now_ms();
            if bu > now {
                forced = sleep_interruptible(((bu - now) / 1000).clamp(1, 30));
                continue;
            }
            // A reading restored from disk is still a reading. Honour its age rather than
            // re-fetching the moment the process starts, or every relaunch spends a request
            // on a number that has not changed.
            // "No credential at all" is reported before the freshness gate, not after. The
            // gate exists to avoid spending requests, and this case spends none - so
            // sitting behind it only meant a signed-out user kept looking at a five minute
            // old reading with no explanation of why it had stopped moving.
            if read_credentials().is_none() {
                set_and_broadcast(&app, |u| {
                    u.status = if u.windows.is_empty() { "needsAuth".into() } else { "stale".into() };
                    u.note = "No Claude Code credential found. Run `claude` in a terminal and sign in.".into();
                });
                forced = sleep_interruptible(POLL_ACTIVE_SECS);
                continue;
            }

            let age_secs = now.saturating_sub(fetched_at) / 1000;
            if fetched_at > 0 && age_secs < MIN_REFETCH_SECS && !forced {
                forced = sleep_interruptible((MIN_REFETCH_SECS - age_secs).clamp(1, 30));
                continue;
            }
            // No reset needed here: every path out of this iteration ends in a wait, and
            // each of those reassigns `forced` from whether it was interrupted.

            // The hard ceiling, checked on the one path every request passes through. A
            // manual refresh may bypass the five minute floor; nothing bypasses this.
            {
                let st = app.state::<AppState>();
                let mut u = st.usage.lock().unwrap();
                if let Err(wait) = budget_check(&mut u.request_log, now_ms()) {
                    let note = format!(
                        "Holding off: {MAX_REQUESTS_PER_HOUR} usage checks in the last hour is this app's own limit. Next one in {}m.",
                        wait.div_ceil(60)
                    );
                    if u.note != note {
                        crate::applog(&format!("usage budget reached, next request in {wait}s"));
                        u.note = note;
                        if u.windows.is_empty() && u.status != "needsAuth" {
                            u.status = "error".into();
                        } else if !u.windows.is_empty() {
                            u.status = "stale".into();
                        }
                    }
                    drop(u);
                    forced = sleep_interruptible(wait.clamp(1, 30));
                    continue;
                }
                u.request_log.push(now_ms());
            }
            match read_credentials() {
                None => set_and_broadcast(&app, |u| {
                    u.status = "needsAuth".into();
                    u.note = "No Claude Code credential found".into();
                }),
                // Reachable despite the check above: the credential is read again here, and
                // in between it can have expired on the clock, or Claude Code can have
                // replaced it with one that is already expired. Doing nothing in this arm
                // was worse than the bug it guarded - a silent no-op tick - and sending the
                // token is what earns a false 429. Report it and wait.
                Some((_, true)) => set_and_broadcast(&app, |u| {
                    u.status = if u.windows.is_empty() { "needsAuth".into() } else { "stale".into() };
                    u.note = "Claude Code's saved credential has expired. Run `claude` in a terminal to refresh it.".into();
                }),
                // An expired token is never worth a request. Sending one to
                // api.anthropic.com/api/oauth/usage does not come back as 401: the endpoint
                // answers 429 with a Retry-After of an hour, so a single doomed request
                // locks the cell out far longer than refreshing the credential would have
                // taken, and the card then reports a rate limit the account is not under.
                Some((token, _)) => {
                    // On 401/403 re-read the credential and retry once (Claude Code may have just refreshed it)
                    let result = match fetch_once(&token) {
                        // Retry once with a genuinely different, unexpired credential:
                        // Claude Code may have refreshed it mid-flight. `false` matters -
                        // retrying with a replacement that is itself expired just spends a
                        // second request to earn the same rejection.
                        Err(FetchErr::NeedsAuth) => match read_credentials() {
                            Some((t2, false)) if t2 != token => fetch_once(&t2),
                            _ => Err(FetchErr::NeedsAuth),
                        },
                        other => other,
                    };
                    let auth_note = "Claude rejected the saved credential. Run `claude` in a terminal to sign in again, or check whether the account changed.";
                    match result {
                        Ok(windows) => {
                            consecutive_429 = 0;
                            set_and_broadcast(&app, |u| {
                                u.status = "ok".into();
                                u.windows = windows;
                                u.fetched_at = now_ms();
                                u.note.clear();
                                u.backoff_until = 0;
                                u.backoff_for.clear();
                            });
                        }
                        Err(FetchErr::NeedsAuth) => set_and_broadcast(&app, |u| {
                            u.status = "needsAuth".into();
                            u.note = auth_note.into();
                        }),
                        Err(FetchErr::RateLimited(ra)) => {
                            consecutive_429 += 1;
                            let wait = backoff_secs(consecutive_429 - 1, ra);
                            let fingerprint = credential_fingerprint(&token);
                            set_and_broadcast(&app, |u| {
                                u.backoff_for = fingerprint;
                                // The status is always set, never left as whatever the
                                // previous iteration wrote. It used to be updated only when
                                // there were windows to keep, so a needsAuth from an earlier
                                // pass survived alongside a fresh rate-limit note and the
                                // card said "Sign in to Claude Code" and "Rate limited" at
                                // the same time - two different problems, neither of them
                                // the one the user had.
                                u.status = if u.windows.is_empty() { "backoff".into() } else { "stale".into() };
                                u.note = format!("Rate limited, retrying in {wait}s");
                                u.backoff_until = now_ms() + wait * 1000;
                            });
                        }
                        Err(FetchErr::Other(msg)) => set_and_broadcast(&app, |u| {
                            if u.windows.is_empty() {
                                u.status = "error".into();
                            } else {
                                u.status = "stale".into();
                            }
                            u.note = msg;
                        }),
                    }
                }
            }
            // 60 s while a session is active, 300 s otherwise (upstream throttling discipline)
            let active = {
                let st = app.state::<AppState>();
                let store = st.store.lock().unwrap();
                let s = store.snapshot("en", "en", false);
                !s.sessions.is_empty()
            };
            forced = sleep_interruptible(if active {
                POLL_ACTIVE_SECS
            } else {
                POLL_IDLE_SECS
            });
        }
    });
}
