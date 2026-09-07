//! Antigravity（Google 的 IDE，Gemini 额度）用量适配器——按上游 Codenotch 的接口事实独立实现。
//!
//! Google 不对第三方公布用量：`cloudcode-pa` 的 `retrieveUserQuotaSummary` 对个人账号答 403 #3501
//! "no valid license"（它看"谁在问"，我们不能冒充 Antigravity）。上游的解法也是 Antigravity 自己的解法：
//! 问本机正在运行的 language_server——它握着凭证和客户端身份，由它去问 Google。
//!
//! 数据路（按诚实度排序，上游同款）：
//!   1. 本机桥：找到 `language_server*` 进程（命令行带 `--csrf_token <t>`，端口 `--https_server_port 0`
//!      运行时随机，只能从监听表找；它开两个端口只有一个答这个 RPC，逐个试），
//!      POST `https://127.0.0.1:<port>/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary`
//!      头 `x-codeium-csrf-token: <t>`（Antigravity 基于 Codeium 栈，头名没改），体 `{"forceRefresh":true}`
//!      （否则服务端从 QuotaSummaryCache 回旧值）。自签证书 → 仅对 127.0.0.1 放行校验。
//!      响应 `{response:{groups:[{displayName, buckets:[{bucketId, displayName, remainingFraction, resetTime}]}]}}`，
//!      **报的是剩余**，used = 1 - remainingFraction；标签用 group.displayName（bucket 只会说 "Weekly Limit Remaining"）。
//!   2. 桥过去答过、现在不答 = Antigravity 关了（端口每次启动都变）：保留上次百分比标 stale，不换成计数。
//!   3. 凭证路（有 Google token 时）：Windows 凭据管理器 target `gemini:antigravity`（Go keyring：service:user），
//!      值为 JSON `{auth_method, token:{access_token, expiry(RFC3339 带偏移)}}`，macOS 上会带 `go-keyring-base64:` 前缀，
//!      两种都认。POST `:loadCodeAssist`（`{"metadata":{"pluginType":"GEMINI"}}`，不是 ANTIGRAVITY）拿 tier 名；
//!      再试 `:retrieveUserQuotaSummary`（空体 `{}`），有授权账号才 200，解析时自我怀疑（无正 limit 或 used>1.5×limit 丢弃）。
//!   4. 兜底：数 `~/.gemini/antigravity/brain/*/.system_generated/logs/transcript.jsonl` 里 `source=="MODEL"` 的
//!      当天步数（created_at 是 UTC，按本地日比较）。这是**计数不是百分比**——没有公布的分母，环只画轨道。
//!
//! 只读、不缓存 token 值、任何日志不带 token。

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const POLL_SECS: u64 = 300;
const LOAD_CODE_ASSIST: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const QUOTA_SUMMARY: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const LS_SERVICE: &str = "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";
const CSRF_HEADER: &str = "x-codeium-csrf-token";

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

fn state_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("antigravity"))
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("antigravity.json")
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

/// 装了 Antigravity 吗：状态目录存在，或凭据管理器里有它的 token
pub fn present() -> bool {
    state_root().map(|p| p.is_dir()).unwrap_or(false) || read_credential_raw().is_some()
}

// ---------------- 1. 本机桥 ----------------

#[derive(Clone, Debug, PartialEq)]
struct Endpoint {
    ports: Vec<u16>,
    csrf: String,
}

fn run_hidden(program: &str, args: &[&str]) -> String {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// 进程表是唯一真相：token 在命令行上，端口没写在任何文件里
#[cfg(windows)]
fn discover() -> Option<Endpoint> {
    // PowerShell 的 CIM 查询：每行 "pid<TAB>commandline"
    let table = run_hidden(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name LIKE '%language_server%'\" | ForEach-Object { \"$($_.ProcessId)`t$($_.CommandLine)\" }",
        ],
    );
    let line = table.lines().find(|l| l.contains("--csrf_token"))?;
    let (pid_s, cmdline) = line.split_once('\t')?;
    let pid: u32 = pid_s.trim().parse().ok()?;
    let csrf = flag_value(cmdline, "--csrf_token")?;
    let ports = listening_ports(pid);
    if ports.is_empty() {
        return None;
    }
    Some(Endpoint { ports, csrf })
}
#[cfg(not(windows))]
fn discover() -> Option<Endpoint> {
    let table = run_hidden("ps", &["-Ao", "pid,command"]);
    let line = table.lines().find(|l| l.contains("language_server") && l.contains("--csrf_token"))?;
    let pid: u32 = line.trim().split_whitespace().next()?.parse().ok()?;
    let csrf = flag_value(line, "--csrf_token")?;
    let out = run_hidden("lsof", &["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"]);
    let ports: Vec<u16> = out
        .lines()
        .filter_map(|l| l.split_whitespace().rev().find(|w| w.contains(':')))
        .filter_map(|a| a.rsplit(':').next()?.parse().ok())
        .collect();
    if ports.is_empty() {
        return None;
    }
    Some(Endpoint { ports, csrf })
}

fn flag_value(line: &str, flag: &str) -> Option<String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let i = parts.iter().position(|p| *p == flag)?;
    parts.get(i + 1).map(|s| s.trim_matches('"').to_string())
}

/// netstat -ano：`TCP 127.0.0.1:PORT 0.0.0.0:0 LISTENING PID`
#[cfg(windows)]
fn listening_ports(pid: u32) -> Vec<u16> {
    let out = run_hidden("netstat", &["-ano", "-p", "TCP"]);
    let pid_s = pid.to_string();
    let mut ports: Vec<u16> = out
        .lines()
        .filter(|l| l.contains("LISTENING"))
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            if cols.len() < 5 || cols[4] != pid_s {
                return None;
            }
            cols[1].rsplit(':').next()?.parse::<u16>().ok()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// 仅信任回环：自签证书只在 127.0.0.1 放行（不用于任何公网请求）
fn local_agent() -> Option<ureq::Agent> {
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .ok()?;
    Some(ureq::AgentBuilder::new().tls_connector(Arc::new(tls)).timeout(Duration::from_secs(10)).build())
}

fn bridge_quota(ep: &Endpoint) -> Result<Vec<LimitWindow>, String> {
    let agent = local_agent().ok_or("TLS 初始化失败")?;
    let mut last = String::from("no port answered");
    for port in &ep.ports {
        let url = format!("https://127.0.0.1:{port}{LS_SERVICE}");
        match agent
            .post(&url)
            .set("Content-Type", "application/json")
            .set(CSRF_HEADER, &ep.csrf)
            .send_string(r#"{"forceRefresh":true}"#)
        {
            Ok(r) => match r.into_json::<serde_json::Value>() {
                Ok(v) => {
                    let w = windows_from_bridge(&v);
                    if !w.is_empty() {
                        return Ok(w);
                    }
                    last = format!("port {port}: 无可识别的 groups");
                }
                Err(e) => last = format!("port {port}: {e}"),
            },
            Err(ureq::Error::Status(code, _)) => last = format!("port {port}: HTTP {code}"),
            Err(e) => last = format!("port {port}: {e}"),
        }
    }
    Err(last)
}

fn parse_iso(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

/// 服务端报"剩余"，notch 显示"已用"：在这里翻转，不让视图层知道提供商差异
pub fn windows_from_bridge(v: &serde_json::Value) -> Vec<LimitWindow> {
    let mut out = Vec::new();
    let Some(groups) = v.pointer("/response/groups").and_then(|g| g.as_array()) else { return out };
    for g in groups {
        let gname = g.get("displayName").and_then(|x| x.as_str());
        let Some(buckets) = g.get("buckets").and_then(|b| b.as_array()) else { continue };
        for b in buckets {
            let Some(rem) = b.get("remainingFraction").and_then(|x| x.as_f64()) else { continue };
            if !(0.0..=1.0).contains(&rem) {
                continue;
            }
            let bname = b.get("displayName").and_then(|x| x.as_str());
            out.push(LimitWindow {
                id: b.get("bucketId").and_then(|x| x.as_str()).or(gname).unwrap_or("quota").to_string(),
                label: gname.or(bname).unwrap_or("Usage").to_string(),
                used: (1.0 - rem).clamp(0.0, 1.0),
                resets_at: parse_iso(b.get("resetTime")),
                ..Default::default()
            });
        }
    }
    out
}

// ---------------- 3. 凭证路 ----------------

struct Creds {
    access_token: String,
    expired: bool,
    auth_method: String,
}

/// Windows 凭据管理器：通用凭据 target = "gemini:antigravity"（Go keyring 的 service:user 命名）
#[cfg(windows)]
fn read_credential_raw() -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
    let target: Vec<u16> = "gemini:antigravity".encode_utf16().chain(std::iter::once(0)).collect();
    let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        if CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut pcred).is_err() || pcred.is_null() {
            return None;
        }
        let c = &*pcred;
        let blob = if c.CredentialBlobSize > 0 && !c.CredentialBlob.is_null() {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        } else {
            Vec::new()
        };
        CredFree(pcred as *const core::ffi::c_void);
        if blob.is_empty() {
            None
        } else {
            Some(blob)
        }
    }
}
#[cfg(not(windows))]
fn read_credential_raw() -> Option<Vec<u8>> {
    None
}

/// 原始 JSON 或 `go-keyring-base64:` 前缀的 base64（也兼容 UTF-16 存储）
fn decode_credential(raw: &[u8]) -> Option<Creds> {
    let mut text = String::from_utf8(raw.to_vec()).unwrap_or_else(|_| {
        // 有些写入方把 blob 存成 UTF-16LE
        let u16s: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u16s)
    });
    text = text.trim_matches('\0').trim().to_string();
    if let Some(rest) = text.strip_prefix("go-keyring-base64:") {
        let bytes = b64_decode(rest.trim())?;
        text = String::from_utf8(bytes).ok()?;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let access = v.pointer("/token/access_token")?.as_str()?.to_string();
    let expiry = v.pointer("/token/expiry").and_then(|x| x.as_str()).unwrap_or("");
    let expired = chrono::DateTime::parse_from_rfc3339(expiry)
        .map(|d| (d.timestamp_millis().max(0) as u64) <= now_ms())
        .unwrap_or(false);
    let auth_method = v.get("auth_method").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(Creds { access_token: access, expired, auth_method })
}

/// 零依赖 base64（标准表，容忍 URL-safe 与缺省 padding）
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for c in s.bytes() {
        let sextet: u8 = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        let v = sextet as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn read_credentials() -> Option<Creds> {
    decode_credential(&read_credential_raw()?)
}

/// tier 名（"Personal"/"Pro"…）；401/403 → NeedsAuth
fn load_tier(token: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    match agent
        .post(LOAD_CODE_ASSIST)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(r#"{"metadata":{"pluginType":"GEMINI"}}"#)
    {
        Ok(r) => {
            let v: serde_json::Value = r.into_json().map_err(|e| e.to_string())?;
            let tier = v
                .get("currentTier")
                .or_else(|| {
                    v.get("allowedTiers").and_then(|a| a.as_array()).and_then(|a| {
                        a.iter().find(|t| t.get("isDefault").and_then(|x| x.as_bool()) == Some(true)).or(a.first())
                    })
                })
                .and_then(|t| t.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or("Gemini");
            Ok(tier.to_string())
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => Err("needsAuth".into()),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(e.to_string()),
    }
}

/// 授权账号的直连配额；个人账号 403 → None（不是错误）
fn direct_quota(token: &str) -> Option<Vec<LimitWindow>> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let r = agent
        .post(QUOTA_SUMMARY)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string("{}")
        .ok()?;
    let v: serde_json::Value = r.into_json().ok()?;
    let mut buckets: Vec<serde_json::Value> = Vec::new();
    if let Some(groups) = v.get("quotaGroups").and_then(|g| g.as_array()) {
        for g in groups {
            if let Some(bs) = g.get("buckets").and_then(|b| b.as_array()) {
                buckets.extend(bs.iter().cloned());
            }
        }
    }
    if let Some(bs) = v.get("buckets").and_then(|b| b.as_array()) {
        buckets.extend(bs.iter().cloned());
    }
    let out: Vec<LimitWindow> = buckets
        .iter()
        .filter_map(|b| {
            let limit = b.get("limit").and_then(|x| x.as_f64())?;
            let used = b.get("used").and_then(|x| x.as_f64())?;
            if limit <= 0.0 || used < 0.0 || used > limit * 1.5 {
                return None; // 自我怀疑：形状不对就不画环
            }
            let label = b
                .get("displayName")
                .or_else(|| b.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or("Usage")
                .to_string();
            Some(LimitWindow {
                id: b.get("name").and_then(|x| x.as_str()).unwrap_or(&label).to_string(),
                label,
                used: (used / limit).clamp(0.0, 1.0),
                resets_at: parse_iso(b.get("resetTime")),
                ..Default::default()
            })
        })
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------- 4. 兜底计数 ----------------

/// 当天 MODEL 步数（UTC 时间戳按本地日比较）
pub fn requests_today() -> (u64, Option<u64>) {
    use chrono::{Datelike, Local, TimeZone};
    let Some(root) = state_root().map(|r| r.join("brain")) else { return (0, None) };
    let Ok(rd) = std::fs::read_dir(&root) else { return (0, None) };
    let today = Local::now().date_naive();
    let mut count = 0u64;
    let mut latest: Option<u64> = None;
    for e in rd.flatten() {
        let p = e.path().join(".system_generated").join("logs").join("transcript.jsonl");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        for line in text.lines() {
            if !line.contains("\"MODEL\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v.get("source").and_then(|x| x.as_str()) != Some("MODEL") {
                continue;
            }
            let Some(ts) = v.get("created_at").and_then(|x| x.as_str()) else { continue };
            let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else { continue };
            let ms = dt.timestamp_millis().max(0) as u64;
            latest = Some(latest.map_or(ms, |l| l.max(ms)));
            let local = Local.timestamp_millis_opt(ms as i64).single();
            if let Some(l) = local {
                if l.date_naive() == today {
                    count += 1;
                }
            }
            let _ = today.year(); // 保持 Datelike 引入有用
        }
    }
    (count, latest)
}

// ---------------- 汇总 ----------------

struct Runtime {
    endpoint: Option<Endpoint>,
    ever_bridged: bool,
}

fn read_once(rt: &mut Runtime, prev: &UsageSnapshot) -> UsageSnapshot {
    let mut snap = UsageSnapshot::default();
    // 1. 本机桥（缓存的端点先试；端口每次启动都变，失效属正常）
    let mut bridge_err = String::new();
    let mut tried = false;
    if let Some(ep) = rt.endpoint.clone() {
        tried = true;
        match bridge_quota(&ep) {
            Ok(w) => {
                rt.ever_bridged = true;
                snap.status = "ok".into();
                snap.windows = w;
                snap.fetched_at = now_ms();
                snap.note = "via Antigravity".into();
                return snap;
            }
            Err(e) => {
                bridge_err = e;
                rt.endpoint = None;
            }
        }
    }
    if let Some(ep) = discover() {
        tried = true;
        match bridge_quota(&ep) {
            Ok(w) => {
                rt.endpoint = Some(ep);
                rt.ever_bridged = true;
                snap.status = "ok".into();
                snap.windows = w;
                snap.fetched_at = now_ms();
                snap.note = "via Antigravity".into();
                return snap;
            }
            Err(e) => bridge_err = e,
        }
    }
    if tried && !bridge_err.is_empty() {
        crate::applog(&format!("antigravity: 本机桥失败（{bridge_err}）"));
    }
    // 2. 曾经桥通过：保留上次百分比标 stale，不降级成计数（8%→31 看着像坏了）
    if rt.ever_bridged && !prev.windows.is_empty() {
        snap = prev.clone();
        snap.status = "stale".into();
        snap.note = "Antigravity is closed — last reading kept".into();
        return snap;
    }
    // 3. 凭证路
    let mut tier: Option<String> = None;
    match read_credentials() {
        Some(c) if !c.expired => match load_tier(&c.access_token) {
            Ok(t) => {
                tier = Some(t);
                if let Some(w) = direct_quota(&c.access_token) {
                    snap.status = "ok".into();
                    snap.windows = w;
                    snap.fetched_at = now_ms();
                    snap.note = format!("{} · via Google", tier.clone().unwrap_or_default());
                    return snap;
                }
            }
            Err(e) if e == "needsAuth" => {
                snap.status = "needsAuth".into();
                snap.note = "Antigravity's Google session was rejected — sign in again in Antigravity".into();
                return snap;
            }
            Err(e) => crate::applog(&format!("antigravity: loadCodeAssist {e}")),
        },
        Some(c) => {
            // 过期≠登出：Antigravity 下次运行会自己刷新；tier 用 auth_method 顶一下
            tier = Some(if c.auth_method == "consumer" { "Personal".into() } else { c.auth_method.clone() });
        }
        None => {}
    }
    // 4. 计数兜底（derived：卡片加 ~ 前缀，环只画轨道）
    let (n, latest) = requests_today();
    snap.status = "ok".into();
    snap.fetched_at = latest.unwrap_or_else(now_ms);
    snap.windows = vec![LimitWindow {
        id: "requests".into(),
        label: "Requests today · no limit published".into(),
        used: 0.0,
        resets_at: None,
        count: Some(n as i64),
        derived: true,
    }];
    snap.note = match tier {
        Some(t) => format!("{t} · Google publishes no quota for this account"),
        None => "Open Antigravity to read its quota".into(),
    };
    snap
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.antigravity.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("antigravity", &snap);
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
            let snap = st.antigravity.lock().unwrap().clone();
            let _ = app.emit("antigravity", &snap);
        }
        if !present() {
            broadcast(&app, UsageSnapshot { status: "absent".into(), ..Default::default() });
            loop {
                sleep_interruptible(600);
                if present() {
                    break;
                }
            }
        }
        let mut rt = Runtime { endpoint: None, ever_bridged: false };
        loop {
            let prev = {
                let st = app.state::<AppState>();
                let s = st.antigravity.lock().unwrap().clone();
                s
            };
            let snap = read_once(&mut rt, &prev);
            broadcast(&app, snap);
            sleep_interruptible(POLL_SECS);
        }
    });
}

/// doctor 用：不含秘密
pub fn probe() -> String {
    let root = state_root().map(|p| p.display().to_string()).unwrap_or_default();
    let has_root = state_root().map(|p| p.is_dir()).unwrap_or(false);
    let cred = read_credentials();
    let ep = discover();
    format!(
        "Antigravity: 状态目录 {}（{}） | 凭据管理器 gemini:antigravity {} | language_server {}",
        root,
        if has_root { "存在" } else { "不存在" },
        match cred {
            Some(c) => format!("已找到（{}，{}）", c.auth_method, if c.expired { "已过期" } else { "未过期" }),
            None => "未找到".into(),
        },
        match ep {
            Some(e) => format!("在跑，端口 {:?}", e.ports),
            None => "未运行".into(),
        }
    )
}
