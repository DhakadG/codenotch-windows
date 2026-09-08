//! A Claude session this application owns, instead of one it borrows.
//!
//! # Why this exists
//!
//! Until now the only Claude credential here was Claude Code's own, read out of
//! `~/.claude/.credentials.json`. That is deliberate and it is upstream's rule - "minting a new
//! token would mean writing a credential this app does not own" - and the risk behind the rule
//! is real: Anthropic rotates refresh tokens, so a second client refreshing behind Claude
//! Code's back invalidates the token Claude Code still holds and signs the user out of the tool
//! they were actually using.
//!
//! But a borrowed credential cannot be renewed by the borrower, and that turned out to be the
//! whole user experience. When it expired, the app could only ask its owner to fix it, and
//! neither `claude auth status` nor the tray's Refresh credentials actually refreshes anything -
//! the only cure was opening Claude Code and signing in again, every time. That is not a usage
//! indicator, it is a chore with a ring on it.
//!
//! Upstream names the way out. `WebSessionProvider` is described there as "a session this app
//! owns, rather than one borrowed, which is the only route by which a real sign-out or in-app
//! sign-in is possible". This is that, for Windows, and the flow is not invented here either:
//! the companion taskbar mod has run exactly this authorization-code-with-PKCE exchange against
//! the same public client for months, so what follows is a port of something already working
//! rather than a design.
//!
//! # What it does not do
//!
//! It does not touch `~/.claude/.credentials.json`, read or write. The two sessions are
//! separate, refresh separately and can be signed out separately, which is the entire point:
//! rotating ours can no longer disturb Claude Code's. The borrowed credential remains the
//! fallback for anyone who has not signed in here, so nothing changes for them.

use std::sync::Mutex;

/// The public client the official CLI uses. A sign-in here runs the same flow the CLI runs; it
/// does not impersonate the CLI's session, and the token it returns is a separate one.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// Anthropic shows the code on this page for the user to copy, rather than redirecting to a
/// loopback port. That is why the sign-in has a paste step at all.
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPE: &str = "org:create_api_key user:profile user:inference";

/// Where the token lives: Windows Credential Manager, not a file.
///
/// The same store the Antigravity credential is read from, and for the same reason - it is
/// encrypted to the user account, so a token here is no more readable by another user than
/// Antigravity's is. A JSON file under AppData would have been less code and worse.
const CRED_TARGET: &str = "codenotch:anthropic-oauth";

/// Refresh this far before the token actually expires.
///
/// Five minutes, so a request is never sent with a token that expires while it is in flight,
/// and so a clock a couple of minutes out of step does not produce a 401 that looks like a
/// sign-out.
const REFRESH_SKEW_SECS: u64 = 300;

/// Bytes of randomness behind the PKCE verifier and the `state`, before base64url.
///
/// Thirty-two for both, matching the reference implementation this flow was ported from - the
/// companion taskbar mod, which signs in against this same client successfully.
///
/// The verifier's size is fixed by RFC 7636. The `state`'s is not, and sixteen bytes is ample
/// entropy for what `state` is *for*, so this started at sixteen on that reasoning. Anthropic's
/// authorize page answered "Authorization failed - Invalid request format" and nothing else in
/// the two URLs differed, so the length is evidently load-bearing to that endpoint whatever the
/// specification says. One constant for both now, so the two cannot drift apart again and so
/// the next person does not repeat the same reasonable-sounding deviation.
const PKCE_BYTES: usize = 32;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds. Zero means the response carried no expiry, which is treated as "refresh
    /// before every use" rather than "never expires" - the pessimistic reading is the safe one.
    pub expires_at: u64,
}

impl Token {
    fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at == 0 || self.expires_at <= now + REFRESH_SKEW_SECS
    }
}

/// A sign-in that has been started and is waiting for the user to paste the code back.
///
/// Held in memory only. A verifier that outlived the process would be a secret persisted for no
/// reason - if the app restarts mid-sign-in, starting again costs one click.
#[derive(Clone)]
pub struct Pending {
    pub verifier: String,
    pub state: String,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- PKCE primitives

/// base64url without padding, which is what PKCE and the `state` parameter want.
///
/// Hand-rolled rather than pulled in: this is the only place in the crate that needs the
/// URL-safe alphabet, and `glyphs::b64` next door is the standard one for data: URLs.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let take = chunk.len() + 1; // 3 bytes -> 4 chars, 2 -> 3, 1 -> 2
        for i in 0..take {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Cryptographically strong random bytes, from the operating system.
///
/// `BCryptGenRandom` rather than a random crate: the `windows` crate is already a dependency,
/// this is the platform's own generator, and a PKCE verifier built from anything weaker is the
/// kind of shortcut that is invisible until it matters.
#[cfg(windows)]
fn random_bytes(n: usize) -> Vec<u8> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut buf = vec![0u8; n];
    unsafe {
        // A failure here would mean the system RNG is unavailable, which is not a condition
        // this application can paper over with a fallback - an all-zero verifier would still
        // "work" against the server and would be worthless. Panic-free, but the caller sees an
        // empty vector and refuses to start a sign-in.
        if BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG).is_err() {
            return Vec::new();
        }
    }
    buf
}

#[cfg(not(windows))]
fn random_bytes(_n: usize) -> Vec<u8> {
    Vec::new()
}

/// SHA-256 through the platform's own primitives, for the same reason as `random_bytes`.
#[cfg(windows)]
fn sha256(data: &[u8]) -> Vec<u8> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
        BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE,
        BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
    };
    let algorithm: Vec<u16> = "SHA256\0".encode_utf16().collect();
    let mut alg = BCRYPT_ALG_HANDLE::default();
    let mut hash = BCRYPT_HASH_HANDLE::default();
    let mut out = vec![0u8; 32];
    unsafe {
        if BCryptOpenAlgorithmProvider(
            &mut alg,
            PCWSTR(algorithm.as_ptr()),
            PCWSTR::null(),
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .is_err()
        {
            return Vec::new();
        }
        let ok = BCryptCreateHash(alg, &mut hash, None, None, 0).is_ok()
            && BCryptHashData(hash, data, 0).is_ok()
            && BCryptFinishHash(hash, &mut out, 0).is_ok();
        if !hash.is_invalid() {
            let _ = BCryptDestroyHash(hash);
        }
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        if !ok {
            return Vec::new();
        }
    }
    out
}

#[cfg(not(windows))]
fn sha256(_data: &[u8]) -> Vec<u8> {
    Vec::new()
}

// ---------------------------------------------------------------- credential storage

/// Read the stored token, or None when nobody has signed in here.
#[cfg(windows)]
pub fn load() -> Option<Token> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
    let target: Vec<u16> = format!("{CRED_TARGET}\0").encode_utf16().collect();
    unsafe {
        let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
        if CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut pcred).is_err()
            || pcred.is_null()
        {
            return None;
        }
        let cred = &*pcred;
        // A generic credential can legitimately carry no blob - something else may have
        // written the target name with only a user name - and `from_raw_parts` on a null
        // pointer is undefined behaviour rather than an empty slice.
        let blob = if cred.CredentialBlob.is_null() || cred.CredentialBlobSize == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize).to_vec()
        };
        CredFree(pcred as *const _);
        serde_json::from_slice(&blob).ok()
    }
}

#[cfg(not(windows))]
pub fn load() -> Option<Token> {
    None
}

#[cfg(windows)]
fn store(token: &Token) -> bool {
    use windows::core::PWSTR;
    use windows::Win32::Security::Credentials::{
        CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };
    let Ok(json) = serde_json::to_vec(token) else {
        return false;
    };
    let mut target: Vec<u16> = format!("{CRED_TARGET}\0").encode_utf16().collect();
    let mut blob = json;
    unsafe {
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        CredWriteW(&cred, 0).is_ok()
    }
}

#[cfg(not(windows))]
fn store(_token: &Token) -> bool {
    false
}

/// Forget the stored token. Signing out here does not touch Claude Code's credential.
#[cfg(windows)]
pub fn sign_out() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
    *CACHED.lock().unwrap() = None;
    let target: Vec<u16> = format!("{CRED_TARGET}\0").encode_utf16().collect();
    unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0).is_ok() }
}

#[cfg(not(windows))]
pub fn sign_out() -> bool {
    false
}

pub fn is_signed_in() -> bool {
    load().is_some()
}

// ---------------------------------------------------------------- the flow

/// Begin a sign-in: returns the URL to open in a browser.
///
/// The verifier and state stay in memory until the user pastes the code back. Starting a second
/// sign-in replaces the first, which is what someone who clicked twice means.
pub fn begin() -> Result<String, String> {
    let verifier_bytes = random_bytes(PKCE_BYTES);
    let state_bytes = random_bytes(PKCE_BYTES);
    if verifier_bytes.is_empty() || state_bytes.is_empty() {
        return Err("the system random number generator is unavailable".into());
    }
    let verifier = b64url(&verifier_bytes);
    let state = b64url(&state_bytes);
    let digest = sha256(verifier.as_bytes());
    if digest.is_empty() {
        return Err("SHA-256 is unavailable".into());
    }
    let challenge = b64url(&digest);
    let url = authorize_url(&challenge, &state);
    // Logged, because the last failure here was invisible from this side: the browser said
    // "Invalid request format" and this application had no record of what it had asked for.
    // The challenge and the state are public by design; the verifier, which is the secret, is
    // not in the URL.
    crate::applog(&format!("oauth: authorize {url}"));
    *PENDING.lock().unwrap() = Some(Pending { verifier, state });
    Ok(url)
}

/// The authorization URL, given an already-computed challenge and state.
///
/// Separated from `begin` so the exact string can be tested. Parameter names, order and
/// encoding are matched to the reference implementation deliberately: this endpoint rejected a
/// URL that differed from it in the length of one value alone, so "equivalent" is not a useful
/// standard here. Identical is.
fn authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code\
&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        urlencode(REDIRECT_URI),
        urlencode(SCOPE),
    )
}

/// Percent-encode everything outside the unreserved set.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Finish a sign-in with what the user pasted.
///
/// The callback page shows `code#state`; people paste the whole thing, or just the code, or the
/// whole URL from the address bar. All three are accepted, because a sign-in that fails on a
/// stray character is a sign-in that gets abandoned.
pub fn complete(pasted: &str) -> Result<(), String> {
    let Some(pending) = PENDING.lock().unwrap().clone() else {
        return Err("no sign-in is in progress - start one first".into());
    };
    let (code, state) = split_pasted(pasted);
    if code.is_empty() {
        return Err("that does not look like a sign-in code".into());
    }
    // The state check is the reason state exists: it proves the code came back from the
    // authorization this process started, not from one a page somewhere else began. A pasted
    // code without a state cannot be checked, and is accepted only because the callback page
    // does not always include it.
    if !state.is_empty() && state != pending.state {
        return Err("this code belongs to a different sign-in - start again".into());
    }

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": pending.state,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": pending.verifier,
    });
    let token = post_token(&body)?;
    if !store(&token) {
        return Err("signed in, but the token could not be saved".into());
    }
    *CACHED.lock().unwrap() = Some(token);
    *LAST_REFRESH_FAILURE.lock().unwrap() = 0;
    *PENDING.lock().unwrap() = None;
    Ok(())
}

/// `code#state`, a bare code, or the full callback URL.
fn split_pasted(pasted: &str) -> (String, String) {
    let text = pasted.trim().trim_matches('"').trim();
    // Full URL: take the query's `code` and `state`.
    if text.starts_with("http://") || text.starts_with("https://") {
        let query = text.split_once('?').map(|x| x.1).unwrap_or("");
        let mut code = String::new();
        let mut state = String::new();
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("code", v)) => code = v.to_string(),
                Some(("state", v)) => state = v.to_string(),
                _ => {}
            }
        }
        // A code taken from a URL can itself be `code#state`, since the fragment is not part of
        // the query.
        if let Some((c, s)) = code.split_once('#') {
            return (c.to_string(), if state.is_empty() { s.to_string() } else { state });
        }
        return (code, state);
    }
    match text.split_once('#') {
        Some((c, s)) => (c.trim().to_string(), s.trim().to_string()),
        None => (text.to_string(), String::new()),
    }
}

fn post_token(body: &serde_json::Value) -> Result<Token, String> {
    let resp = ureq::post(TOKEN_URL)
        .set("content-type", "application/json")
        .set("user-agent", "codenotch")
        .timeout(std::time::Duration::from_secs(20))
        .send_json(body.clone());
    let text = match resp {
        Ok(r) => r.into_string().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            // The endpoint's own words are more useful than ours, but they can be a page rather
            // than a sentence, so they are trimmed to something a notification can hold.
            let detail = detail.chars().take(200).collect::<String>();
            return Err(format!("Anthropic refused the sign-in ({code}): {detail}"));
        }
        Err(e) => return Err(format!("could not reach Anthropic: {e}")),
    };
    parse_token_response(&text, now_secs())
}

/// Turn a token response into a `Token`, given the current time.
///
/// Split out from the request so the parsing can be tested against real response shapes without
/// a network or a clock.
pub(crate) fn parse_token_response(text: &str, now: u64) -> Result<Token, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "Anthropic sent a reply that was not JSON".to_string())?;
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    if access.is_empty() {
        return Err("Anthropic's reply carried no access token".into());
    }
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    // `expires_in` is seconds from now. A response without one is stored as 0, which
    // `needs_refresh` reads as "refresh before every use".
    //
    // Clamped at both ends, because both ends have a failure mode. A lifetime shorter than the
    // refresh skew means every call refreshes, which turns one poll into a token request; a
    // lifetime of years - a malformed or hostile response - means a dead token is used forever
    // and the app looks permanently signed out with no way to notice. A day is longer than
    // anything this endpoint issues and short enough that being wrong costs one refresh.
    const MIN_LIFETIME: u64 = REFRESH_SKEW_SECS + 60;
    const MAX_LIFETIME: u64 = 24 * 60 * 60;
    let expires_at = v
        .get("expires_in")
        .and_then(|x| x.as_u64())
        .map(|s| now + s.clamp(MIN_LIFETIME, MAX_LIFETIME))
        .unwrap_or(0);
    Ok(Token {
        access_token: access,
        refresh_token: refresh,
        expires_at,
    })
}

/// The last token handed out, and when a refresh last failed.
///
/// `access_token` is called several times per poll pass - the loop re-reads the credential at
/// each decision point - so without a cache a token near expiry would fire a refresh request
/// per call. The failure floor is the same idea from the other direction: a rejected refresh
/// must not be retried on every call, or a real sign-out becomes a burst of doomed requests
/// against the token endpoint.
static CACHED: Mutex<Option<Token>> = Mutex::new(None);
static LAST_REFRESH_FAILURE: Mutex<u64> = Mutex::new(0);
const REFRESH_RETRY_FLOOR_SECS: u64 = 60;

/// A usable access token, refreshing first if this one is close to expiry.
///
/// Returns None when nobody has signed in, or when the refresh was rejected - which is what a
/// real sign-out looks like, and the caller should fall back to the borrowed credential rather
/// than treat it as an error.
pub fn access_token() -> Option<String> {
    let now = now_secs();
    if let Some(t) = CACHED.lock().unwrap().as_ref() {
        if !t.needs_refresh(now) {
            return Some(t.access_token.clone());
        }
    }
    let token = load()?;
    if !token.needs_refresh(now) {
        *CACHED.lock().unwrap() = Some(token.clone());
        return Some(token.access_token);
    }
    if token.refresh_token.is_empty() {
        return None;
    }
    {
        let last = *LAST_REFRESH_FAILURE.lock().unwrap();
        if last != 0 && now.saturating_sub(last) < REFRESH_RETRY_FLOOR_SECS {
            return None;
        }
    }
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": token.refresh_token,
        "client_id": CLIENT_ID,
    });
    match post_token(&body) {
        Ok(mut fresh) => {
            // Anthropic rotates refresh tokens, but a response that omits one means keep using
            // the old one. Overwriting it with an empty string would sign the user out on the
            // next expiry for no reason.
            if fresh.refresh_token.is_empty() {
                fresh.refresh_token = token.refresh_token;
            }
            let access = fresh.access_token.clone();
            store(&fresh);
            *CACHED.lock().unwrap() = Some(fresh);
            *LAST_REFRESH_FAILURE.lock().unwrap() = 0;
            Some(access)
        }
        Err(e) => {
            crate::applog(&format!("oauth: refresh failed: {e}"));
            *LAST_REFRESH_FAILURE.lock().unwrap() = now;
            None
        }
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
