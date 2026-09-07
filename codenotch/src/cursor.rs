//! Cursor 用量适配器——按上游 Codenotch 的接口事实独立实现。
//!
//! 数据路（上游同款取舍，"借编辑器自己的会话"）：
//!   1. 凭证：Cursor 编辑器把登录态存在它从 VS Code 继承的全局状态库
//!      `%APPDATA%\Cursor\User\globalStorage\state.vscdb`（SQLite，表 ItemTable(key,value)）：
//!      `cursorAuth/accessToken` + `cursorAuth/stripeMembershipAuthId`，
//!      两者拼成 Cookie `WorkosCursorSessionToken=<authId>::<token>`。
//!      非秘密的身份缓存：`cursorAuth/cachedEmail`、`cursorAuth/stripeMembershipType`（只取 plan 显示）。
//!   2. 端点：`GET https://cursor.com/api/usage-summary`（Cookie + Accept: application/json，15s）。
//!      响应：{ billingCycleEnd, membershipType, isUnlimited,
//!              individualUsage: { plan: { totalPercentUsed, apiPercentUsed, used, limit, breakdown },
//!                                 onDemand: { enabled, used, limit } } }
//!      Cursor 计的是"额度百分比"不是请求数：仪表盘的 "Included usage · N% used" = totalPercentUsed；
//!      免费版 used/limit 恒为 0（额度以 breakdown.bonus 形式到账），读 used/limit 会把 10% 报成 0%。
//!      0 是读数不是缺失（上游教训）。apiPercentUsed>0 时单列 "API usage"；onDemand 有真实 limit 时列 "On demand"。
//!
//! SQLite 打开纪律：先 `mode=ro`（能看到 WAL 里编辑器刚轮换的 token），失败再 `immutable=1`
//! （编辑器退出、-shm 消失后 mode=ro 会打不开；此时 WAL 已 checkpoint，忽略它零代价）。
//! 我们只读，永不写；token 值不进日志/事件/UI。

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

/// Windows: %APPDATA%\Cursor\User\globalStorage\state.vscdb（macOS 对应 ~/Library/Application Support/Cursor/...）
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

// ---------------- SQLite 只读 ----------------

/// mode=ro 优先，immutable=1 兜底（见文件头）
fn open_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return None;
    }
    if let Ok(c) = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        // 真正验证一下能读（-shm 缺失时 open 可能成功而首次查询失败）
        if c.prepare("SELECT 1 FROM ItemTable LIMIT 1").and_then(|mut s| s.query([]).map(|_| ())).is_ok() {
            return Some(c);
        }
    }
    // URI 形式才能带 immutable=1；Windows 路径要转成 file:///C:/... 且 \ → /
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
    plan: Option<String>,
}

/// 每次都重读：编辑器会轮换 token，抱着旧值等于自己把自己登出
fn read_credentials() -> Option<Creds> {
    let path = store_url()?;
    let conn = open_ro(&path)?;
    let token = item(&conn, "cursorAuth/accessToken")?;
    let auth_id = item(&conn, "cursorAuth/stripeMembershipAuthId")?;
    let plan = item(&conn, "cursorAuth/stripeMembershipType");
    Some(Creds { cookie: format!("WorkosCursorSessionToken={auth_id}::{token}"), plan })
}

/// doctor 用：不含秘密值
pub fn probe() -> String {
    let Some(p) = store_url() else { return "Cursor: 无法定位 %APPDATA%".into() };
    if !p.is_file() {
        return format!("Cursor: 未找到 {}（未安装或未登录）", p.display());
    }
    match read_credentials() {
        Some(c) => format!(
            "Cursor: 会话已借到（cookie {} 字符，plan={}）",
            c.cookie.len(),
            c.plan.unwrap_or_else(|| "?".into())
        ),
        None => format!("Cursor: {} 存在但读不到 cursorAuth/*（编辑器未登录，或 SQLite 打开失败）", p.display()),
    }
}

// ---------------- 解析 ----------------

fn pct(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64()).map(|p| (p / 100.0).clamp(0.0, 1.0))
}

fn parse_iso(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

/// usage-summary → (窗口, 说明)。窗口为空时说明为"为何没得量"（Unlimited / 免费无额度）
pub fn parse_summary(v: &serde_json::Value) -> (Vec<LimitWindow>, String) {
    let resets_at = parse_iso(v.get("billingCycleEnd"));
    let usage = v.get("individualUsage").cloned().unwrap_or(serde_json::Value::Null);
    let plan = usage.get("plan").cloned().unwrap_or(serde_json::Value::Null);
    let mut out = Vec::new();
    // 头条=仪表盘那个数；0 也是读数
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
    let membership = v.get("membershipType").and_then(|x| x.as_str()).unwrap_or("this");
    let note = if v.get("isUnlimited").and_then(|x| x.as_bool()) == Some(true) {
        format!("Unlimited on the {membership} plan — nothing to meter")
    } else {
        format!("The {membership} plan has nothing for Cursor to meter yet")
    };
    (out, note)
}

enum FetchErr {
    NeedsAuth,
    Other(String),
}

fn fetch_once(cookie: &str) -> Result<serde_json::Value, FetchErr> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    match agent.get(ENDPOINT).set("Cookie", cookie).set("Accept", "application/json").call() {
        Ok(r) => r.into_json::<serde_json::Value>().map_err(|e| FetchErr::Other(format!("解析失败: {e}"))),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => Err(FetchErr::NeedsAuth),
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
        Err(FetchErr::Other(msg)) => {
            // 陈旧优于编造：保留旧读数标 stale
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
                sleep_interruptible(600); // 没装 Cursor：每 10 分钟看一眼
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
            let snap = read_once(&prev);
            if snap.status == "error" || snap.status == "stale" {
                crate::applog(&format!("cursor: {}", snap.note));
            }
            broadcast(&app, snap);
            sleep_interruptible(POLL_SECS);
        }
    });
}
