//! Cursor usage adapter, implemented from the upstream Codenotch's documented behaviour.
//!
//! Data path (same trade-off as upstream: borrow the editor's own session):
//!   1. Credential: the editor keeps its sign-in in the global state database it inherited from
//!      VS Code, `%APPDATA%\Cursor\User\globalStorage\state.vscdb` (SQLite, table ItemTable(key,value)):
//!      `cursorAuth/accessToken` + `cursorAuth/stripeMembershipAuthId`, joined into the cookie
//!      `WorkosCursorSessionToken=<authId>::<token>`. Non-secret identity cache:
//!      `cursorAuth/cachedEmail`, `cursorAuth/stripeMembershipType` (only the plan is shown).
//!   2. Endpoint: `GET https://cursor.com/api/usage-summary` (Cookie + Accept: application/json, 15 s).
//!      Reply: { billingCycleEnd, membershipType, isUnlimited,
//!              individualUsage: { plan: { totalPercentUsed, apiPercentUsed, used, limit, breakdown },
//!                                 onDemand: { enabled, used, limit } } }
//!      Cursor meters a percentage of the allowance, not requests: the dashboard's
//!      "Included usage · N% used" is totalPercentUsed. On the free plan used/limit are always 0
//!      (the allowance arrives as breakdown.bonus), so reading used/limit would report 10 % as 0 %.
//!      0 is a reading, not a gap (upstream's lesson). "API usage" is listed separately when
//!      apiPercentUsed > 0; "On demand" when onDemand has a real limit.
//!
//! SQLite opening rule: `mode=ro` first (it sees the token the editor just rotated into the WAL),
//! then `immutable=1` (once the editor has exited and the -shm is gone, mode=ro fails to open; by
//! then the WAL has been checkpointed, so ignoring it costs nothing).
//! Read only, never written; token values never reach logs, events or the UI.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://cursor.com/api/usage-summary";
const POLL_SECS: u64 = 300;

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Windows: %APPDATA%\Cursor\User\globalStorage\state.vscdb (macOS: ~/Library/Application Support/Cursor/...)
pub fn store_url() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("Cursor").join("User").join("globalStorage").join("state.vscdb"))
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("cursor.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
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

pub fn present() -> bool {
    store_url().map(|p| p.is_file()).unwrap_or(false)
}

// ---------------- SQLite, read only ----------------

/// mode=ro first, immutable=1 as the fallback (see the module doc)
fn open_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return None;
    }
    if let Ok(c) = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        // Actually verify that reads work (with the -shm missing, open can succeed and the first query fail)
        if c.prepare("SELECT 1 FROM ItemTable LIMIT 1").and_then(|mut s| s.query([]).map(|_| ())).is_ok() {
            return Some(c);
        }
    }
    // Only the URI form takes immutable=1; a Windows path becomes file:///C:/... with \ → /
    let mut uri = String::from("file:///");
    uri.push_str(&path.to_string_lossy().replace('\\', "/").trim_start_matches('/').replace('#', "%23").replace('?', "%3F"));
    uri.push_str("?immutable=1");
    rusqlite::Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn item(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        .ok()
        .filter(|s| !s.is_empty())
}

struct Creds {
    cookie: String,
    /// The raw access token, kept so its own `exp` claim can be checked before a request is
    /// spent on it. Never logged or persisted.
    token: String,
    plan: Option<String>,
}

/// The `sub` claim of the stored access token, which is the account id the session cookie
/// needs — `google-oauth2|…`, `auth0|…`, and so on.
///
/// Nothing is verified here; the token is the editor's and the server checks it. This only
/// reads a field the editor already put on disk.
fn jwt_sub(token: &str) -> Option<String> {
    let sub = jwt_claims(token)?.get("sub")?.as_str()?.to_string();
    if sub.is_empty() {
        return None;
    }
    Some(sub)
}

/// The second JWT segment, decoded. Nothing is verified; the token is the editor's and the
/// server checks it. This only reads fields the editor already wrote to disk.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let part = token.split('.').nth(1)?;
    let raw = crate::antigravity::b64_decode(part)?;
    serde_json::from_slice(&raw).ok()
}

/// True when the stored token's own `exp` claim is in the past.
///
/// Cursor's access token is short-lived and the editor refreshes it. With the editor closed
/// the stored copy simply ages out, and sending it earns a rejection that is indistinguishable
/// from a real sign-out - which is what "Cursor session was rejected, sign in again" used to
/// tell people who were perfectly well signed in. Reading `exp` first costs nothing, spends no
/// request, and lets the note name the actual remedy: open the editor.
fn token_expired(token: &str) -> bool {
    jwt_claims(token)
        .and_then(|v| v.get("exp").and_then(|x| x.as_f64()))
        .map(|exp| (exp * 1000.0) as u64 <= now_ms())
        .unwrap_or(false)
}

/// Re-read every time: the editor rotates the token, and holding on to an old value signs us out
fn read_credentials() -> Option<Creds> {
    let path = store_url()?;
    let conn = open_ro(&path)?;
    let token = item(&conn, "cursorAuth/accessToken")?;
    // `stripeMembershipAuthId` is not written for every account — a free account has no
    // Stripe membership and so never gets the key. Requiring it took the whole Cursor cell
    // dark for those users. The id it holds is the token's own `sub` claim, so read that
    // when the key is absent rather than giving up.
    let auth_id = item(&conn, "cursorAuth/stripeMembershipAuthId").or_else(|| jwt_sub(&token))?;
    let plan = item(&conn, "cursorAuth/stripeMembershipType");
    Some(Creds {
        cookie: format!("WorkosCursorSessionToken={auth_id}::{token}"),
        token,
        plan,
    })
}

/// For doctor: contains no secret values.
///
/// Each failure gets its own message. "not signed in, or SQLite failed to open" told the
/// reader two different fixes and left them to guess which applied.
pub fn probe() -> String {
    let Some(p) = store_url() else { return "Cursor: cannot locate %APPDATA%".into() };
    if !p.is_file() {
        return format!("Cursor: {} not found (not installed, or never signed in)", p.display());
    }
    let Some(conn) = open_ro(&p) else {
        return format!("Cursor: {} exists but SQLite could not open it (is the editor mid-write?)", p.display());
    };
    let Some(token) = item(&conn, "cursorAuth/accessToken") else {
        return format!("Cursor: {} opened, but cursorAuth/accessToken is absent — sign in to the editor", p.display());
    };
    let stripe_id = item(&conn, "cursorAuth/stripeMembershipAuthId");
    let source = if stripe_id.is_some() { "stripeMembershipAuthId" } else { "the token's sub claim" };
    if stripe_id.is_none() && jwt_sub(&token).is_none() {
        return format!(
            "Cursor: signed in (token {} chars) but no account id — stripeMembershipAuthId is absent and the token carries no readable sub claim",
            token.len()
        );
    }
    let plan = item(&conn, "cursorAuth/stripeMembershipType").unwrap_or_else(|| "?".into());
    format!("Cursor: session borrowed (token {} chars, account id from {source}, plan={plan})", token.len())
}

// ---------------- Parsing ----------------

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;

fn pct(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64()).map(|p| (p / 100.0).clamp(0.0, 1.0))
}

fn parse_iso(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

/// usage-summary → (windows, note). When there are no windows the note says why (Unlimited / free plan without an allowance)
pub fn parse_summary(v: &serde_json::Value) -> (Vec<LimitWindow>, String) {
    let resets_at = parse_iso(v.get("billingCycleEnd"));
    let usage = v.get("individualUsage").cloned().unwrap_or(serde_json::Value::Null);
    let plan = usage.get("plan").cloned().unwrap_or(serde_json::Value::Null);
    let mut out = Vec::new();
    // Headline = the dashboard number; 0 is a reading too
    if let Some(total) = pct(plan.get("totalPercentUsed")) {
        out.push(LimitWindow { id: "included".into(), label: "Included usage".into(), used: total, resets_at, ..Default::default() });
    }
    if let Some(api) = pct(plan.get("apiPercentUsed")) {
        if api > 0.0 {
            out.push(LimitWindow { id: "api".into(), label: "API usage".into(), used: api, resets_at, ..Default::default() });
        }
    }
    if let Some(od) = usage.get("onDemand") {
        let enabled = od.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let limit = od.get("limit").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let used = od.get("used").and_then(|x| x.as_f64());
        if enabled && limit > 0.0 {
            if let Some(u) = used {
                out.push(LimitWindow {
                    id: "on_demand".into(),
                    label: "On demand".into(),
                    used: (u / limit).clamp(0.0, 1.0),
                    resets_at, ..Default::default()
                });
            }
        }
    }
    if !out.is_empty() {
        return (out, String::new());
    }
    // Named plan or not, the sentence has to read. Substituting a placeholder into "the
    // {} plan" produced "The this plan has nothing for Cursor to meter yet".
    let membership = v
        .get("membershipType")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty());
    let unlimited = v.get("isUnlimited").and_then(|x| x.as_bool()) == Some(true);
    let note = match (unlimited, membership) {
        (true, Some(m)) => format!("Unlimited on the {m} plan — nothing to meter"),
        (true, None) => "Unlimited on this plan — nothing to meter".to_string(),
        (false, Some(m)) => format!("The {m} plan has nothing for Cursor to meter yet"),
        (false, None) => "This plan has nothing for Cursor to meter yet".to_string(),
    };
    (out, note)
}

enum FetchErr {
    NeedsAuth,
    /// Suggested wait in seconds, from Retry-After where the server sent one.
    RateLimited(u64),
    Other(String),
}

fn fetch_once(cookie: &str) -> Result<serde_json::Value, FetchErr> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    match agent.get(ENDPOINT).set("Cookie", cookie).set("Accept", "application/json").call() {
        Ok(r) => r.into_json::<serde_json::Value>().map_err(|e| FetchErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => Err(FetchErr::NeedsAuth),
        // 429 was previously indistinguishable from any other failure, so a rate-limited
        // Cursor kept being asked every five minutes with no acknowledgement that it had
        // said no. Named so the caller can hold off and say why.
        Err(ureq::Error::Status(429, r)) => Err(FetchErr::RateLimited(
            r.header("retry-after").and_then(|s| s.parse::<u64>().ok()).unwrap_or(300),
        )),
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn read_once(prev: &UsageSnapshot) -> UsageSnapshot {
    let mut snap = prev.clone();
    let Some(creds) = read_credentials() else {
        snap.status = "needsAuth".into();
        snap.note = "Sign in to Cursor (the editor) to see usage.".into();
        return snap;
    };
    // Do not spend a request on a token that has already expired on its own clock. Keep the
    // last reading rather than blanking it: an aged-out token says nothing about the account.
    if token_expired(&creds.token) {
        snap.status = if snap.windows.is_empty() { "needsAuth".into() } else { "stale".into() };
        snap.note = "Cursor's stored session has expired. Open the Cursor editor to refresh it — Codenotch borrows its session and cannot sign in.".into();
        return snap;
    }
    match fetch_once(&creds.cookie) {
        Ok(v) => {
            let (windows, note) = parse_summary(&v);
            snap.fetched_at = now_ms();
            if windows.is_empty() {
                snap.status = "none".into();
                snap.windows.clear();
                snap.note = note;
            } else {
                snap.status = "ok".into();
                snap.windows = windows;
                snap.note = match (&creds.plan, v.get("membershipType").and_then(|x| x.as_str())) {
                    (_, Some(m)) => format!("{} · via Cursor", cap(m)),
                    (Some(p), None) => format!("{} · via Cursor", cap(p)),
                    _ => String::new(),
                };
            }
        }
        Err(FetchErr::NeedsAuth) => {
            snap.status = "needsAuth".into();
            snap.note = "Cursor session was rejected — sign in again in the editor".into();
        }
        Err(FetchErr::RateLimited(secs)) => {
            // Hold off rather than asking again on the next tick. A poll that keeps firing
            // into a rate limit is how you stay rate limited.
            snap.status = if snap.windows.is_empty() { "backoff" } else { "stale" }.into();
            snap.note = format!("Cursor rate limited this reading, retrying in {secs}s");
            snap.backoff_until = now_ms() + secs * 1000;
        }
        Err(FetchErr::Other(msg)) => {
            // Stale beats invented: keep the old reading, marked stale
            snap.status = if snap.windows.is_empty() { "error" } else { "stale" }.into();
            snap.note = msg;
        }
    }
    snap
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.cursor.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("cursor", &snap);
}

fn sleep_interruptible(secs: u64) {
    for _ in 0..secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        {
            let st = app.state::<AppState>();
            let snap = st.cursor.lock().unwrap().clone();
            let _ = app.emit("cursor", &snap);
        }
        if !present() {
            broadcast(&app, UsageSnapshot { status: "absent".into(), ..Default::default() });
            loop {
                sleep_interruptible(600); // Cursor is not installed: look again every 10 minutes
                if present() {
                    break;
                }
            }
        }
        loop {
            let prev = {
                let st = app.state::<AppState>();
                let s = st.cursor.lock().unwrap().clone();
                s
            };
            // A reading restored from disk is the same answer the endpoint would give. The
            // hook restarts this app whenever it is not running, and without this every
            // start spent a request here too - the same restart storm that rate-limited the
            // Claude endpoint, just against a different host.
            if crate::usage::too_fresh(prev.fetched_at, now_ms()) {
                sleep_interruptible(60);
                continue;
            }
            // Respect a backoff the server asked for, across restarts.
            if prev.backoff_until > now_ms() {
                sleep_interruptible(((prev.backoff_until - now_ms()) / 1000).clamp(1, 60));
                continue;
            }
            let snap = read_once(&prev);
            if snap.status == "error" || snap.status == "stale" {
                crate::applog(&format!("cursor: {}", snap.note));
            }
            broadcast(&app, snap);
            sleep_interruptible(POLL_SECS);
        }
    });
}
