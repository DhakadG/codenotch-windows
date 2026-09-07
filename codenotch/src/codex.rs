//! Codex 用量适配器——按上游 Codenotch 的接口事实独立实现。
//!
//! 两条数据路径（上游 1.5.0 同款取舍）：
//!   1. 活读：借 Codex 自己的登录态（`~/.codex/auth.json` → `tokens.access_token` +
//!      `tokens.account_id`）直接 GET `https://chatgpt.com/backend-api/wham/usage`，回复
//!      `rate_limit.{primary_window,secondary_window}` 带 `used_percent / limit_window_seconds /
//!      reset_at(秒) | reset_after_seconds`，顶层另有 `plan_type`。这是"现在"的数字，不起任何
//!      进程；token 只读、不刷新、不写回，401/403 就报 needsAuth，让 Codex 自己去续。
//!      （早先走 `codex app-server` JSON-RPC：每 5 分钟拉一棵 node 进程树、还得 taskkill 收尾，
//!      而且它的回复只带 weekly 一个窗口——换端点后 5h 窗口才回来。）
//!   2. 兜底：Codex 会把每回合看到的限额快照写进线程 rollout 日志
//!      `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`，行形如
//!      `{"timestamp":"…","type":"event_msg","payload":{"type":"token_count","rate_limits":{
//!         "primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1790585719},
//!         "secondary":{…}|null,"plan_type":"free"}}}`
//!      重置时刻是 **resets_at 绝对秒**（文档写的 resets_in_seconds 也兼容）。这是"上次用时"的
//!      数字——文件读取永远瞬间成功，所以必须按行内 timestamp 标 stale（>5min）。
//!   上游用 state_5.sqlite 的线程索引找最新 rollout；我们直接按目录日期倒序 + mtime 找，
//!   零 SQLite 依赖（immutable/WAL 的坑整个绕开）。
//!
//! 凭证只借不管：数字来自 Codex 自己的登录态和它自己的端点；既没登录也没会话记录就是 absent（cell 不显示）。

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const POLL_SECS: u64 = 300; // Codex 没有会话态可依据，固定 5min（上游节奏；托盘刷新可打断）
const TAIL_BYTES: u64 = 256 * 1024;
const CURRENT_FOR_MS: u64 = 5 * 60 * 1000;
const ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const BACKOFF_MIN_SECS: u64 = 60; // 429 时至少等这么久，Retry-After 只作下限

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 服务端给的重试截止（ms epoch）：手动刷新、重启都不能绕过它
static BACKOFF_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex"))
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("codex.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            BACKOFF_UNTIL.store(s.backoff_until, std::sync::atomic::Ordering::Relaxed);
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

// ---------------- 可执行文件定位 ----------------

/// 候选顺序：npm 全局包内的原生 exe（最干净，不经 cmd/node 包装）→ ~/.codex/bin →
/// PATH 上的 codex.exe / codex.cmd。
pub fn find_executable() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = dirs::config_dir() {
        let pkg = appdata.join("npm").join("node_modules").join("@openai").join("codex");
        if let Ok(rd) = std::fs::read_dir(pkg.join("bin")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("codex-") && n.contains("windows") && n.ends_with(".exe") {
                    cands.push(e.path());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(pkg.join("vendor")) {
            // 新版包把原生 exe 放在 vendor/<triple>/codex/codex.exe
            for e in rd.flatten() {
                let p = e.path().join("codex").join("codex.exe");
                if p.exists() {
                    cands.push(p);
                }
            }
        }
        cands.push(appdata.join("npm").join("codex.cmd"));
    }
    if let Some(h) = codex_home() {
        cands.push(h.join("bin").join("codex.exe"));
        cands.push(h.join("bin").join("codex"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            cands.push(dir.join("codex.exe"));
            cands.push(dir.join("codex.cmd"));
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

// ---------------- 活读：usage 端点 ----------------

fn auth_path() -> Option<PathBuf> {
    codex_home().map(|h| h.join("auth.json"))
}

struct Credential {
    access_token: String,
    account_id: String,
    /// id_token 里的 chatgpt_plan_type（pro / plus / free…），只作标签
    plan: Option<String>,
    /// access_token 的 exp 已过：请求照发（服务端说了算），只影响 401 时的提示语
    expired: bool,
}

/// JWT 第二段（base64url）→ claims。只取标签和本地过期提示，不做任何校验——那是服务端的事
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let part = token.split('.').nth(1)?;
    let raw = crate::antigravity::b64_decode(part)?;
    serde_json::from_slice(&raw).ok()
}

/// 只读 Codex 的登录态；缺文件、缺字段都视为"没登录"
fn load_credential() -> Option<Credential> {
    let text = std::fs::read_to_string(auth_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tokens = v.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?.trim().to_string();
    let account_id = tokens.get("account_id")?.as_str()?.trim().to_string();
    if access_token.is_empty() || account_id.is_empty() {
        return None;
    }
    let expired = jwt_claims(&access_token)
        .and_then(|c| c.get("exp").and_then(|x| x.as_f64()))
        .map(|exp| exp * 1000.0 <= now_ms() as f64)
        .unwrap_or(false);
    let plan = tokens
        .get("id_token")
        .and_then(|x| x.as_str())
        .and_then(jwt_claims)
        .and_then(|c| {
            c.get("https://api.openai.com/auth")?
                .get("chatgpt_plan_type")?
                .as_str()
                .map(String::from)
        });
    Some(Credential { access_token, account_id, plan, expired })
}

enum LiveErr {
    NeedsAuth,
    /// 建议等待秒数（已含 BACKOFF_MIN_SECS 下限）
    RateLimited(u64),
    Other(String),
}

fn fetch_usage(cred: &Credential) -> Result<serde_json::Value, LiveErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {}", cred.access_token))
        .set("ChatGPT-Account-Id", &cred.account_id)
        .set("Accept", "application/json")
        .set("Cache-Control", "no-cache, no-store")
        .set("User-Agent", concat!("codenotch/", env!("CARGO_PKG_VERSION"), " (Windows)"))
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => r.into_json().map_err(|e| LiveErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(code @ (401 | 403), r)) => {
            // 401 是 token 的事；403 也可能是边缘节点拦了 UA——把状态码和响应体开头记下来，别把两者混成一句"请登录"
            let head: String = r
                .into_string()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect();
            crate::applog(&format!("codex: usage 端点 HTTP {code}: {head}"));
            Err(LiveErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            Err(LiveErr::RateLimited(ra.max(BACKOFF_MIN_SECS)))
        }
        Err(ureq::Error::Status(code, _)) => Err(LiveErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(LiveErr::Other(format!("{e}"))),
    }
}

/// 上游同款标签规则：Codex 只按时长命名窗口，"5h limit" 比 "primary" 有信息量
fn label_for(window_minutes: Option<f64>, id: &str) -> String {
    match window_minutes {
        Some(m) if m > 0.0 => {
            if m < 60.0 {
                format!("{}m limit", m as i64)
            } else if m < 60.0 * 24.0 {
                format!("{}h limit", (m / 60.0) as i64)
            } else {
                let days = (m / (60.0 * 24.0)).round() as i64;
                match days {
                    7 => "Weekly limit".into(),
                    30 => "Monthly limit".into(),
                    d => format!("{d}d limit"),
                }
            }
        }
        _ => {
            if id == "primary" {
                "Current session".into()
            } else {
                "Longer window".into()
            }
        }
    }
}

fn num(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
}

/// usage 回复 → 窗口。`additional_rate_limits`、`code_review_rate_limit` 计的是别的东西，不进环。
/// 窗口 id 记"来自哪个字段"（primary/secondary），标签按时长推——账号不同，primary 未必是 5h
/// （免费档见过 30 天），按固定时长认窗口会把真实在用的窗口整个丢掉。
fn windows_from_usage(v: &serde_json::Value) -> Vec<LimitWindow> {
    let now = now_ms();
    let mut out = Vec::new();
    for (id, key) in [("primary", "primary_window"), ("secondary", "secondary_window")] {
        let Some(w) = v.pointer(&format!("/rate_limit/{key}")).filter(|x| x.is_object()) else { continue };
        let Some(pct) = num(w.get("used_percent")) else { continue };
        let resets_at = num(w.get("reset_at"))
            .map(|s| (s * 1000.0) as u64)
            .or_else(|| num(w.get("reset_after_seconds")).map(|s| now + (s * 1000.0) as u64));
        out.push(LimitWindow {
            id: id.into(),
            label: label_for(num(w.get("limit_window_seconds")).map(|s| s / 60.0), id),
            used: (pct / 100.0).clamp(0.0, 1.0),
            resets_at,
            ..Default::default()
        });
    }
    out
}

// ---------------- 兜底：rollout 快照 ----------------

/// 最近改动的 rollout：sessions/YYYY/MM/DD 目录名倒序，只看最近 3 个"有文件的日子"
pub fn newest_rollout() -> Option<PathBuf> {
    let root = codex_home()?.join("sessions");
    let mut days: Vec<PathBuf> = Vec::new();
    let mut years = list_dirs(&root);
    years.sort_by(|a, b| b.cmp(a));
    'outer: for y in years {
        let mut months = list_dirs(&y);
        months.sort_by(|a, b| b.cmp(a));
        for m in months {
            let mut ds = list_dirs(&m);
            ds.sort_by(|a, b| b.cmp(a));
            for d in ds {
                days.push(d);
                if days.len() >= 3 {
                    break 'outer;
                }
            }
        }
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for d in days {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let Ok(mt) = md.modified() else { continue };
                if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
                    best = Some((mt, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn list_dirs(p: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

pub fn tail_text(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// rollout 尾部最后一条 rate_limits 快照 → (窗口, 记录时刻 ms, plan)
pub fn snapshot_from_rollout(text: &str) -> Option<(Vec<LimitWindow>, Option<u64>, Option<String>)> {
    for line in text.lines().rev().filter(|l| l.contains("rate_limits")) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        // rate_limits 可能在顶层，也可能在 payload 下
        let rl = v
            .get("rate_limits")
            .or_else(|| v.pointer("/payload/rate_limits"))
            .filter(|x| x.is_object());
        let Some(rl) = rl else { continue };
        let recorded = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis().max(0) as u64);
        let now = now_ms();
        let mut out = Vec::new();
        for id in ["primary", "secondary"] {
            let Some(w) = rl.get(id).filter(|x| x.is_object()) else { continue };
            let Some(pct) = num(w.get("used_percent")) else { continue };
            let resets_at = num(w.get("resets_at"))
                .map(|s| (s * 1000.0) as u64)
                .or_else(|| num(w.get("resets_in_seconds")).map(|s| now + (s * 1000.0) as u64));
            out.push(LimitWindow {
                id: id.into(),
                label: label_for(num(w.get("window_minutes")), id),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at, ..Default::default()
            });
        }
        if out.is_empty() {
            continue;
        }
        let plan = rl.get("plan_type").and_then(|x| x.as_str()).map(String::from);
        return Some((out, recorded, plan));
    }
    None
}

// ---------------- 汇总 ----------------

/// Codex 在这台机上存在吗（装了 CLI 或有过会话）——都没有则 cell 不显示
pub fn present() -> bool {
    find_executable().is_some()
        || auth_path().map(|p| p.is_file()).unwrap_or(false)
        || codex_home().map(|h| h.join("sessions").is_dir()).unwrap_or(false)
}

fn read_once() -> UsageSnapshot {
    let mut snap = UsageSnapshot::default();
    // 活读失败时附在兜底读数上的说明；needs_auth 决定"连兜底都没有"时显示哪种空态
    let mut live_note: Option<String> = None;
    let mut needs_auth = false;
    let held_until = BACKOFF_UNTIL.load(std::sync::atomic::Ordering::Relaxed);
    let now = now_ms();
    if held_until > now {
        snap.backoff_until = held_until;
        live_note = Some(format!("Rate limited — retrying in {}s", (held_until - now) / 1000));
    } else {
        match load_credential() {
            None => {
                if auth_path().map(|p| p.is_file()).unwrap_or(false) {
                    crate::applog("codex: auth.json 里没有可用的 access_token/account_id，退回 rollout");
                }
            }
            Some(cred) => match fetch_usage(&cred) {
                Ok(v) => {
                    let windows = windows_from_usage(&v);
                    if !windows.is_empty() {
                        let plan = v.get("plan_type").and_then(|x| x.as_str()).map(String::from).or(cred.plan);
                        snap.status = "ok".into();
                        snap.windows = windows;
                        snap.fetched_at = now_ms();
                        snap.note = plan.map(|p| format!("{} · via Codex", cap(&p))).unwrap_or_default();
                        return snap;
                    }
                    let keys: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                    crate::applog(&format!("codex: usage 回复里没有窗口（顶层键 {keys:?}），退回 rollout"));
                    live_note = Some("Codex reported no usage windows".into());
                }
                Err(LiveErr::NeedsAuth) => {
                    needs_auth = true;
                    live_note = Some(if cred.expired {
                        "Codex sign-in expired — open Codex once to refresh it".into()
                    } else {
                        "Codex rejected its sign-in — sign in to Codex again".into()
                    });
                }
                Err(LiveErr::RateLimited(secs)) => {
                    let until = now_ms() + secs * 1000;
                    BACKOFF_UNTIL.store(until, std::sync::atomic::Ordering::Relaxed);
                    snap.backoff_until = until;
                    live_note = Some(format!("Rate limited — retrying in {secs}s"));
                    crate::applog(&format!("codex: usage 端点 429，{secs}s 后重试"));
                }
                Err(LiveErr::Other(e)) => {
                    crate::applog(&format!("codex: 活读失败（{e}），退回 rollout"));
                    live_note = Some(format!("Live read failed ({e})"));
                }
            },
        }
    }
    // 兜底：rollout
    match newest_rollout().and_then(|p| tail_text(&p)).and_then(|t| snapshot_from_rollout(&t)) {
        Some((windows, recorded, plan)) => {
            let rec = recorded.unwrap_or(0);
            let fresh = rec > 0 && now_ms().saturating_sub(rec) <= CURRENT_FOR_MS;
            snap.status = if fresh { "ok" } else { "stale" }.into();
            snap.windows = windows;
            snap.fetched_at = rec; // 以"记录时刻"为准，UI 据此显示 Updated N ago
            snap.note = match plan {
                Some(p) => format!("{} · from last Codex run", cap(&p)),
                None => "from last Codex run".into(),
            };
            if let Some(n) = live_note {
                snap.note = format!("{n} · {}", snap.note);
            }
        }
        None => {
            snap.status = if needs_auth {
                "needsAuth"
            } else if present() {
                "none"
            } else {
                "absent"
            }
            .into();
            snap.note = match live_note {
                Some(n) => n,
                None if present() => "Codex has not recorded a usage snapshot yet".into(),
                None => String::new(),
            };
        }
    }
    snap
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.codex.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("codex", &snap);
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        {
            let st = app.state::<AppState>();
            let snap = st.codex.lock().unwrap().clone();
            let _ = app.emit("codex", &snap);
        }
        if !present() {
            broadcast(&app, UsageSnapshot { status: "absent".into(), ..Default::default() });
            // 没装 Codex：每 10 分钟看一眼有没有装上
            loop {
                for _ in 0..600 {
                    if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                if present() {
                    break;
                }
            }
        }
        loop {
            let snap = read_once();
            let hold = snap.backoff_until.saturating_sub(now_ms()) / 1000;
            broadcast(&app, snap);
            for _ in 0..POLL_SECS.max(hold) {
                if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

/// doctor 用：不含任何秘密
pub fn probe() -> String {
    let auth = match load_credential() {
        Some(c) => format!(
            "auth.json 可用{}{}",
            if c.expired { "（access_token 已过期）" } else { "" },
            c.plan.map(|p| format!("，plan={p}")).unwrap_or_default()
        ),
        None if auth_path().map(|p| p.is_file()).unwrap_or(false) => "auth.json 存在但缺 token".to_string(),
        None => "auth.json 不存在".to_string(),
    };
    let exe = find_executable();
    let roll = newest_rollout();
    let age = roll
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| format!("{} 分钟前", d.as_secs() / 60))
        .unwrap_or_else(|| "?".into());
    format!(
        "Codex: {auth} | 可执行 {} | 最新 rollout {}（改动于 {}）",
        exe.map(|p| p.display().to_string()).unwrap_or_else(|| "未找到".into()),
        roll.map(|p| p.display().to_string()).unwrap_or_else(|| "无".into()),
        age
    )
}
