//! Codex 用量适配器——按上游 Codenotch 的接口事实独立实现。
//!
//! 两条数据路径（上游同款取舍）：
//!   1. 活读：起一个 `codex app-server`（stdio JSON-RPC），发 initialize / initialized /
//!      `account/rateLimits/read`，回复 `result.rateLimits.{primary,secondary}` 带
//!      `usedPercent / windowDurationMins / resetsAt(秒)`，另有 `planType`、`rateLimitReachedType`。
//!      这是"现在"的数字。答完即结束进程（Windows 上必须连子进程树一起收，否则 node 孤儿常驻）。
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
//! 无凭证、无网络：数字来自 Codex 自己的工具，Codex 没装就是 absent（cell 不显示）。

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const POLL_SECS: u64 = 300; // Codex 没有会话态可依据，固定 5min（活读要起进程，不宜更勤）
const TAIL_BYTES: u64 = 256 * 1024;
const CURRENT_FOR_MS: u64 = 5 * 60 * 1000;
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);

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

// ---------------- 活读：app-server ----------------

fn spawn_app_server(exe: &Path) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW（否则闪黑框）
    }
    cmd.spawn()
}

/// Windows 上 .cmd → node → 原生 exe 是三层进程；kill 父进程不会带走子进程，必须 /T 收树。
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

const REQUEST_ID: u64 = 2;

fn handshake() -> String {
    format!(
        concat!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"clientInfo\":{{\"name\":\"codenotch\",\"title\":\"Codenotch\",\"version\":\"{}\"}}}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{{}}}}\n",
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"account/rateLimits/read\",\"params\":null}}\n"
        ),
        env!("CARGO_PKG_VERSION"),
        REQUEST_ID
    )
}

/// 一次问答：三条消息一次写出（服务端按序读），按 id 从交错的通知里挑出回复。
fn live_reply(exe: &Path) -> Result<serde_json::Value, String> {
    let mut child = spawn_app_server(exe).map_err(|e| format!("启动 app-server 失败: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("无 stdin")?;
    let stdout = child.stdout.take().ok_or("无 stdout")?;
    if let Err(e) = stdin.write_all(handshake().as_bytes()) {
        kill_tree(&mut child);
        return Err(format!("写入失败: {e}"));
    }
    let _ = stdin.flush();
    // 读线程 + 超时：服务端不答就收树
    let (tx, rx) = std::sync::mpsc::channel::<Option<serde_json::Value>>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut head = String::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if head.len() < 400 {
                head.push_str(&line);
                head.push('\n');
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("id").and_then(|x| x.as_u64()) == Some(REQUEST_ID) && v.get("result").is_some() {
                    let _ = tx.send(Some(v));
                    return;
                }
            }
        }
        let _ = tx.send(None);
    });
    let got = rx.recv_timeout(APP_SERVER_TIMEOUT);
    drop(stdin); // 关 stdin：规矩的 stdio 服务端见 EOF 自退
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    kill_tree(&mut child);
    match got {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err("app-server 未回答 rateLimits".into()),
        Err(_) => Err("app-server 超时".into()),
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

/// 活读回复 → 窗口（camelCase：usedPercent / windowDurationMins / resetsAt 秒）
fn windows_from_live(v: &serde_json::Value) -> (Vec<LimitWindow>, Option<String>, Option<String>) {
    let rl = v.pointer("/result/rateLimits");
    let mut out = Vec::new();
    if let Some(rl) = rl {
        for id in ["primary", "secondary"] {
            let Some(w) = rl.get(id) else { continue };
            let Some(pct) = num(w.get("usedPercent")) else { continue };
            out.push(LimitWindow {
                id: id.into(),
                label: label_for(num(w.get("windowDurationMins")), id),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: num(w.get("resetsAt")).map(|s| (s * 1000.0) as u64), ..Default::default()
            });
        }
    }
    let plan = rl.and_then(|r| r.get("planType")).and_then(|x| x.as_str()).map(String::from);
    let blocked = rl
        .and_then(|r| r.get("rateLimitReachedType"))
        .and_then(|x| x.as_str())
        .map(|t| match t {
            "rate_limit_reached" => "Paused".to_string(),
            "workspace_owner_credits_depleted" | "workspace_member_credits_depleted" => {
                "Workspace credits used up".to_string()
            }
            "workspace_owner_usage_limit_reached" | "workspace_member_usage_limit_reached" => {
                "Workspace limit reached".to_string()
            }
            _ => "Paused".to_string(),
        });
    (out, plan, blocked)
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
    find_executable().is_some() || codex_home().map(|h| h.join("sessions").is_dir()).unwrap_or(false)
}

fn read_once() -> UsageSnapshot {
    let mut snap = UsageSnapshot::default();
    if let Some(exe) = find_executable() {
        match live_reply(&exe) {
            Ok(v) => {
                let (windows, plan, blocked) = windows_from_live(&v);
                if windows.len() < 2 {
                    // 诊断：实机只出了 Weekly limit，5h 窗口缺席——记下 rateLimits 的键与 primary 形状
                    let rl = v.pointer("/result/rateLimits");
                    let keys: Vec<String> = rl
                        .and_then(|r| r.as_object())
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default();
                    let primary = rl.and_then(|r| r.get("primary")).map(|p| p.to_string()).unwrap_or_else(|| "缺".into());
                    crate::applog(&format!("codex: rateLimits 键={keys:?} primary={}", primary.chars().take(200).collect::<String>()));
                }
                if !windows.is_empty() {
                    snap.status = "ok".into();
                    snap.windows = windows;
                    snap.fetched_at = now_ms();
                    snap.note = match (blocked, plan) {
                        (Some(b), _) => b,
                        (None, Some(p)) => format!("{} · via Codex", cap(&p)),
                        _ => String::new(),
                    };
                    return snap;
                }
                crate::applog("codex: app-server 回复里没有可识别的窗口，退回 rollout");
            }
            Err(e) => crate::applog(&format!("codex: 活读失败（{e}），退回 rollout")),
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
        }
        None => {
            snap.status = if present() { "none" } else { "absent" }.into();
            snap.note = if present() {
                "Codex has not recorded a usage snapshot yet".into()
            } else {
                String::new()
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
            broadcast(&app, snap);
            for _ in 0..POLL_SECS {
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
        "Codex: 可执行 {} | 最新 rollout {}（改动于 {}）",
        exe.map(|p| p.display().to_string()).unwrap_or_else(|| "未找到".into()),
        roll.map(|p| p.display().to_string()).unwrap_or_else(|| "无".into()),
        age
    )
}
