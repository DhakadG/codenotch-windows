//! 非 Claude 提供商的"在干活吗"探测（Claude 走 hooks + transcript watcher 的四态引擎，不在这里）。
//!
//! 三家都没有像 Claude Code 那样的状态字段，能拿到什么就如实标注什么（上游同款取舍）：
//!   - Cursor：编辑器状态库 `state.vscdb` 的 `composerHeaders` 行（JSON）——`unfinishedRunAt` 在一轮运行中
//!     被设置、结束即清；`hasBlockingPendingActions` / `hasPendingPlan` = 在等你。这是**真状态**。
//!     库是 WAL 模式：必须用普通只读打开（immutable 会忽略 WAL，看到的是上次 checkpoint 的旧世界）。
//!   - Codex：桌面版在 `~/.codex/thread_history_1.sqlite` 的 `thread_turns` 里维护回合状态
//!     （status=inProgress 且 completed_at 为空 = 正在跑）——真状态；CLI / VS Code 扩展退回按 rollout
//!     尾条目判步骤，静默阈值按步骤类型放宽。
//!   - Claude 云端会话：本地无 transcript，按桌面应用进程的网络收发速率推断（标 ~）。
//!   - Antigravity：transcript.jsonl 在一轮运行中被追加（每步只在完成后才写，status 全是 DONE 没法用），
//!     最近 45 s 内写过 = 在干活（模型两步之间可能想很久，窗口放宽）。
//! 轮询 2 s（上游节奏），只在有变化时广播。成本纪律：数据库连接持久化、库文件 mtime 没变不重查、
//! rollout 尾部 mtime 没变不重读、PowerShell 只在找网络进程 pid 时偶尔起一次、线程降优先级。

use crate::AppState;
use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const INTERVAL: Duration = Duration::from_secs(2);
const ANTIGRAVITY_STALE_MS: u64 = 45_000;

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Activity {
    /// claude 之外的提供商 id：codex / cursor / gemini
    pub provider: String,
    /// busy | waiting
    pub state: String,
    pub name: String,
    pub detail: String,
    /// ms epoch
    pub since: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mtime_ms(p: &std::path::Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}


/// 持久连接 + 变更门控：只有库文件（含 -wal）的 mtime 变了才重新查询，否则直接复用上次结果。
/// Cursor 的 state.vscdb 有 2 GB 以上，每 2 s 重新打开并全表扫描会拖慢整机（实测输入都卡）。
struct DbCache {
    path: std::path::PathBuf,
    conn: Option<rusqlite::Connection>,
    sig: (u64, u64),
    last: Vec<Activity>,
    checked_once: bool,
}

impl DbCache {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path, conn: None, sig: (0, 0), last: Vec::new(), checked_once: false }
    }
    fn signature(&self) -> (u64, u64) {
        let wal = {
            let mut o = self.path.as_os_str().to_owned();
            o.push("-wal");
            std::path::PathBuf::from(o)
        };
        (mtime_ms(&self.path).unwrap_or(0), mtime_ms(&wal).unwrap_or(0))
    }
    /// 变了（或首次）才调用 f；f 返回 None 表示查询失败 → 丢弃连接下次重开
    fn refresh<F: FnOnce(&rusqlite::Connection) -> Option<Vec<Activity>>>(&mut self, f: F) -> Vec<Activity> {
        let sig = self.signature();
        if self.checked_once && sig == self.sig {
            return self.last.clone();
        }
        self.sig = sig;
        self.checked_once = true;
        if self.conn.is_none() {
            self.conn = open_ro(&self.path);
        }
        let Some(conn) = self.conn.as_ref() else {
            self.last.clear();
            return Vec::new();
        };
        match f(conn) {
            Some(v) => self.last = v,
            None => {
                self.conn = None;
                self.last.clear();
            }
        }
        self.last.clone()
    }
}

/// 探测线程的全部可复用状态
struct Ctx {
    cursor: DbCache,
    codex_turns: DbCache,
    codex_names: Option<rusqlite::Connection>,
    rollout_path: Option<std::path::PathBuf>,
    rollout_checked_at: u64,
    rollout_sig: u64,
    rollout_last: Vec<Activity>,
}

impl Ctx {
    fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            cursor: DbCache::new(crate::cursor::store_url().unwrap_or_default()),
            codex_turns: DbCache::new(home.join(".codex").join("thread_history_1.sqlite")),
            codex_names: None,
            rollout_path: None,
            rollout_checked_at: 0,
            rollout_sig: 0,
            rollout_last: Vec::new(),
        }
    }
}

// ---------------- Cursor ----------------

fn open_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

fn cursor_activity(ctx: &mut Ctx) -> Vec<Activity> {
    ctx.cursor.refresh(|conn| {
        let mut stmt = conn.prepare("SELECT value FROM composerHeaders WHERE isArchived = 0 ORDER BY recency DESC LIMIT 40").ok()?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).ok()?;
        let mut out = Vec::new();
        for json in rows.flatten() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else { continue };
            if v.get("composerId").and_then(|x| x.as_str()).is_none() {
                continue;
            }
            let blocked = v.get("hasBlockingPendingActions").and_then(|x| x.as_bool()) == Some(true)
                || v.get("hasPendingPlan").and_then(|x| x.as_bool()) == Some(true);
            let running = v.get("unfinishedRunAt").and_then(|x| x.as_f64());
            let state = if blocked {
                "waiting"
            } else if running.is_some() {
                "busy"
            } else {
                continue; // 四十条闲置的历史对话不是四十件正在发生的事
            };
            let since = running
                .or_else(|| v.get("lastUpdatedAt").and_then(|x| x.as_f64()))
                .or_else(|| v.get("createdAt").and_then(|x| x.as_f64()))
                .map(|ms| ms as u64)
                .unwrap_or_else(now_ms);
            out.push(Activity {
                provider: "cursor".into(),
                state: state.into(),
                name: v.get("name").and_then(|x| x.as_str()).unwrap_or("Untitled chat").to_string(),
                detail: if blocked {
                    "needs your input".into()
                } else {
                    v.get("subtitle").and_then(|x| x.as_str()).unwrap_or("Working").to_string()
                },
                since,
            });
        }
        out.sort_by(|a, b| b.since.cmp(&a.since));
        Some(out)
    })
}

// ---------------- Codex ----------------

/// rollout 尾部最后一条"有意义"的记录说明 Codex 正处在哪一步。
/// 行形如 {"timestamp","type":"response_item"|"turn_context"|"event_msg"|…,"payload":{…}}；
/// task_started/task_complete 这类事件不落盘，所以只能靠条目类型 + 静默时长判断：
///   函数调用（工具在跑，或在等你批准）→ 忙，最多认 10 分钟；
///   工具输出 / 用户消息 / 回合上下文 / 推理 → 模型在想下一步，静默 <120 s 算忙（长思考要容忍）；
///   助手消息 → 可能是终答也可能是过程旁白，静默 <4 s 算忙；
///   turn_aborted → 闲。token_count 之类的记账行跳过。
#[derive(Clone, Copy, PartialEq, Debug)]
enum CodexStep {
    Tool,
    Thinking,
    AsstMsg,
    Aborted,
}

fn codex_last_step(text: &str) -> Option<(CodexStep, u64)> {
    for line in text.lines().rev().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis().max(0) as u64)
            .unwrap_or(0);
        let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let p = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let pt = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let step = match kind {
            "turn_context" => Some(CodexStep::Thinking),
            "response_item" => match pt {
                "function_call" | "local_shell_call" | "custom_tool_call" | "web_search_call" => Some(CodexStep::Tool),
                "function_call_output" | "custom_tool_call_output" | "reasoning" => Some(CodexStep::Thinking),
                "message" => match p.get("role").and_then(|x| x.as_str()).unwrap_or("") {
                    "assistant" => Some(CodexStep::AsstMsg),
                    "user" => Some(CodexStep::Thinking),
                    _ => None, // system/developer 消息不说明状态
                },
                _ => None,
            },
            "event_msg" => match pt {
                "turn_aborted" | "task_complete" => Some(CodexStep::Aborted), // 新版会落盘 task_complete：明确结束
                "task_started" | "item_started" | "exec_command_begin" => Some(CodexStep::Thinking),
                "user_message" => Some(CodexStep::Thinking),
                "agent_message" => Some(CodexStep::AsstMsg),
                "agent_reasoning" | "agent_reasoning_raw_content" => Some(CodexStep::Thinking),
                _ => None, // token_count 等记账行
            },
            _ => None,
        };
        if let Some(st) = step {
            return Some((st, ts));
        }
    }
    None
}

/// 桌面版 Codex 的真状态：`~/.codex/thread_history_1.sqlite` 表 `thread_turns`
/// （status = inProgress / completed…，started_at 秒，completed_at 空 = 还在跑）。
/// 这是应用自己维护的回合表，比看文件 mtime 可靠得多。防"崩溃后永远 inProgress"：
/// 该线程最近 10 分钟内没有新 item（`thread_items.created_at_ms`）且回合已开始超过 2 分钟 → 视为陈旧。
fn codex_turns_in_progress(ctx: &mut Ctx) -> Vec<Activity> {
    let now = now_ms();
    if ctx.codex_names.is_none() {
        ctx.codex_names = dirs::home_dir().and_then(|h| open_ro(&h.join(".codex").join("state_5.sqlite")));
    }
    let names = ctx.codex_names.as_ref();
    ctx.codex_turns.refresh(|conn| {
        let mut stmt = conn
            .prepare("SELECT thread_id, started_at FROM thread_turns WHERE status = 'inProgress' ORDER BY started_at DESC LIMIT 8")
            .ok()?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, rusqlite::types::Value>(1)?))).ok()?;
        let mut out = Vec::new();
        for (thread_id, started) in rows.flatten() {
            let started_ms = match started {
                rusqlite::types::Value::Integer(i) => (i as u64) * if i > 10_000_000_000 { 1 } else { 1000 },
                rusqlite::types::Value::Real(f) => (f * if f > 10_000_000_000.0 { 1.0 } else { 1000.0 }) as u64,
                _ => 0,
            };
            // 该线程最近一条 item：新鲜度 + 是否在等批准
            let (last_ms, last_type): (Option<i64>, Option<String>) = conn
                .query_row(
                    "SELECT created_at_ms, item_type FROM thread_items WHERE thread_id = ?1 ORDER BY created_at_ms DESC LIMIT 1",
                    [&thread_id],
                    |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap_or((None, None));
            let last = last_ms.map(|v| v as u64).unwrap_or(started_ms);
            let fresh = now.saturating_sub(last) <= 10 * 60_000 || now.saturating_sub(started_ms) <= 2 * 60_000;
            if !fresh {
                continue;
            }
            let mut name = String::new();
            if let Some(c) = names {
                if let Ok((title, first, nick)) = c.query_row(
                    "SELECT COALESCE(title,''), COALESCE(first_user_message,''), COALESCE(agent_nickname,'') FROM threads WHERE id = ?1",
                    [&thread_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                ) {
                    name = if !title.trim().is_empty() {
                        title
                    } else if !first.trim().is_empty() {
                        first.chars().take(40).collect()
                    } else if !nick.trim().is_empty() {
                        format!("Agent {nick}")
                    } else {
                        String::new()
                    };
                }
            }
            if name.is_empty() {
                name = "Codex".into();
            }
            let lt = last_type.unwrap_or_default().to_lowercase();
            let waiting = lt.contains("approval") || lt.contains("permission") || lt.contains("request_user");
            out.push(Activity {
                provider: "codex".into(),
                state: if waiting { "waiting" } else { "busy" }.into(),
                name,
                detail: if waiting { "needs your input".into() } else { "Working".into() },
                since: started_ms,
            });
        }
        Some(out)
    })
}

fn codex_activity(ctx: &mut Ctx) -> Vec<Activity> {
    // 1. 桌面版真状态
    let turns = codex_turns_in_progress(ctx);
    if !turns.is_empty() {
        return turns;
    }
    // 2. CLI / 扩展：rollout 路径 30 s 找一次；文件 mtime 没变就不重读 256 KB 尾部
    let now = now_ms();
    if now.saturating_sub(ctx.rollout_checked_at) > 30_000 || ctx.rollout_path.is_none() {
        ctx.rollout_checked_at = now;
        ctx.rollout_path = crate::codex::newest_rollout();
    }
    let Some(p) = ctx.rollout_path.clone() else { return vec![] };
    let mtime = mtime_ms(&p).unwrap_or(0);
    if mtime == ctx.rollout_sig {
        // 内容没变：只重算"静默是否超时"
        return ctx
            .rollout_last
            .iter()
            .filter(|a| now.saturating_sub(a.since) <= 10 * 60_000)
            .cloned()
            .collect();
    }
    ctx.rollout_sig = mtime;
    ctx.rollout_last.clear();
    if let Some(text) = crate::codex::tail_text(&p) {
        if let Some((step, ts)) = codex_last_step(&text) {
            let at = ts.max(mtime);
            let quiet = now.saturating_sub(at);
            let busy = match step {
                CodexStep::Tool => quiet <= 10 * 60_000,
                CodexStep::Thinking => quiet <= 120_000,
                CodexStep::AsstMsg => quiet <= 4_000,
                CodexStep::Aborted => false,
            };
            if busy {
                ctx.rollout_last = vec![Activity { provider: "codex".into(), state: "busy".into(), name: "Codex".into(), detail: "Working".into(), since: at }];
            }
        }
    }
    ctx.rollout_last.clone()
}

// ---------------- Claude 桌面版（云端会话）：网络活动启发式 ----------------

/// 云端会话不落本地 transcript，四态引擎看不见。退而求其次：Claude 桌面应用的进程在流式
/// 输出时会持续从网络收数据（Winsock 走 AFD 的 IOCTL，计入进程 IO 计数的 Other 项）。
/// 每 2 s 采样所有 claude.exe 的 Other+Read 传输量，速率超过阈值即"在流式输出"。
/// 明确标为推断（~），阈值可在校准后调整；前 60 次采样写 run.log 供校准。
struct IoSample {
    at: u64,
    other: u64,
    read: u64,
}
static CLAUDE_IO: std::sync::Mutex<Option<IoSample>> = std::sync::Mutex::new(None);
static CLAUDE_LAST_ACTIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const CLAUDE_RATE_BPS: f64 = 2_500.0; // 只看网络服务进程的 socket 收发；空闲心跳远低于此，流式输出高于此
const CLAUDE_HOLD_MS: u64 = 10_000; // 工具调用之间常有 2–4 s 的零流量间隙，10 s 保持避免闪断
static CLAUDE_HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Claude 桌面应用（Electron）的网络服务子进程 pid：命令行含 `network.mojom.NetworkService`。
/// 所有 socket 收发都经它，GPU/渲染进程的 IOCTL 噪声（显卡驱动调用也计入 Other）与它无关。
/// 找一次缓存起来，进程消失或每 5 分钟重找；用 PowerShell 查命令行，代价只在重找时付。
static CLAUDE_NET_PID: std::sync::Mutex<(u32, u64)> = std::sync::Mutex::new((0, 0));

#[cfg(windows)]
fn claude_net_pid(maps: &crate::focus::ProcMaps) -> Option<u32> {
    let now = now_ms();
    {
        let g = CLAUDE_NET_PID.lock().unwrap();
        let (pid, at) = *g;
        if pid != 0 && maps.name.get(&pid).map(|n| n == "claude.exe").unwrap_or(false) && now.saturating_sub(at) < 5 * 60_000 {
            return Some(pid);
        }
        // 找不到也要缓存 60 s：否则每 2 s 起一次 PowerShell（每次几百毫秒 CPU）就是新的卡顿源
        if pid == 0 && at != 0 && now.saturating_sub(at) < 60_000 {
            return None;
        }
        // Claude 桌面版根本没开：不必起 PowerShell
        if !maps.name.values().any(|n| n == "claude.exe") {
            return None;
        }
    }
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Get-CimInstance Win32_Process -Filter \"Name='claude.exe'\" | Where-Object { $_.CommandLine -like '*network.mojom.NetworkService*' } | Select-Object -First 1 -ExpandProperty ProcessId",
    ]);
    cmd.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
    let pid: u32 = match cmd.output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()) {
        Some(p) => p,
        None => {
            *CLAUDE_NET_PID.lock().unwrap() = (0, now);
            return None;
        }
    };
    *CLAUDE_NET_PID.lock().unwrap() = (pid, now);
    crate::applog(&format!("claude net pid = {pid}"));
    Some(pid)
}

#[cfg(windows)]
fn claude_io_bytes() -> Option<(u64, u64)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetProcessIoCounters, OpenProcess, IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION};
    let maps = crate::focus::proc_maps();
    let net_pid = claude_net_pid(&maps)?;
    let mut other = 0u64;
    let mut read = 0u64;
    let mut n = 0;
    for (pid, _name) in maps.name.iter() {
        if *pid != net_pid {
            continue;
        }
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, *pid) else { continue };
            let mut io = IO_COUNTERS::default();
            if GetProcessIoCounters(h, &mut io).is_ok() {
                other = other.saturating_add(io.OtherTransferCount);
                read = read.saturating_add(io.ReadTransferCount);
                n += 1;
            }
            let _ = CloseHandle(h);
        }
    }
    if n == 0 {
        None
    } else {
        Some((other, read))
    }
}
#[cfg(not(windows))]
fn claude_io_bytes() -> Option<(u64, u64)> {
    None
}

fn claude_activity() -> Vec<Activity> {
    let now = now_ms();
    let Some((other, read)) = claude_io_bytes() else { return vec![] };
    let mut guard = CLAUDE_IO.lock().unwrap();
    let (rate_other, rate_read) = match guard.as_ref() {
        Some(prev) if now > prev.at && other >= prev.other && read >= prev.read => {
            let dt = (now - prev.at) as f64 / 1000.0;
            ((other - prev.other) as f64 / dt, (read - prev.read) as f64 / dt)
        }
        _ => (0.0, 0.0),
    };
    *guard = Some(IoSample { at: now, other, read });
    drop(guard);
    static SAMPLES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SAMPLES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 240 {
        crate::applog(&format!("claude io: net {:.0} B/s, disk {:.0} B/s", rate_other, rate_read));
    }
    // 连续两次采样（≈4 s）都超阈值才算，单次尖峰（心跳、同步）不算
    if rate_other >= CLAUDE_RATE_BPS {
        if CLAUDE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 1 {
            CLAUDE_LAST_ACTIVE.store(now, std::sync::atomic::Ordering::Relaxed);
        }
    } else {
        CLAUDE_HITS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    let last = CLAUDE_LAST_ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
    if last > 0 && now.saturating_sub(last) <= CLAUDE_HOLD_MS {
        vec![Activity { provider: "claude".into(), state: "busy".into(), name: "Claude".into(), detail: "Streaming (network)".into(), since: last }]
    } else {
        vec![]
    }
}

// ---------------- Antigravity ----------------

fn antigravity_activity() -> Vec<Activity> {
    let Some(root) = dirs::home_dir().map(|h| h.join(".gemini").join("antigravity").join("brain")) else { return vec![] };
    let Ok(rd) = std::fs::read_dir(&root) else { return vec![] };
    let mut newest: Option<(String, u64)> = None;
    for e in rd.flatten() {
        let t = e.path().join(".system_generated").join("logs").join("transcript.jsonl");
        let Some(m) = mtime_ms(&t) else { continue };
        if newest.as_ref().map(|(_, n)| m > *n).unwrap_or(true) {
            newest = Some((e.file_name().to_string_lossy().to_string(), m));
        }
    }
    let Some((_, at)) = newest else { return vec![] };
    if now_ms().saturating_sub(at) > ANTIGRAVITY_STALE_MS {
        return vec![];
    }
    vec![Activity { provider: "gemini".into(), state: "busy".into(), name: "Antigravity".into(), detail: "Working".into(), since: at }]
}

// ---------------- 汇总 ----------------

#[derive(Clone, Copy, Default)]
pub struct Presence {
    cursor: bool,
    codex: bool,
    gemini: bool,
}

fn presence() -> Presence {
    Presence { cursor: crate::cursor::present(), codex: crate::codex::present(), gemini: crate::antigravity::present() }
}

fn read_all(p: Presence, ctx: &mut Ctx) -> Vec<Activity> {
    let mut all = Vec::new();
    all.extend(claude_activity());
    if p.cursor {
        all.extend(cursor_activity(ctx));
    }
    if p.codex {
        all.extend(codex_activity(ctx));
    }
    if p.gemini {
        all.extend(antigravity_activity());
    }
    all
}

/// doctor 用：Codex 活动态判定的原材料
pub fn probe() -> String {
    let now = now_ms();
    let Some(p) = crate::codex::newest_rollout() else { return "Codex 活动态: 未找到 rollout".into() };
    let age = now.saturating_sub(mtime_ms(&p).unwrap_or(0)) / 1000;
    let step = crate::codex::tail_text(&p).and_then(|t| codex_last_step(&t));
    let tail: Vec<String> = crate::codex::tail_text(&p)
        .map(|t| {
            t.lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(6)
                .map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .map(|v| {
                            format!(
                                "{}/{}/{}",
                                v.get("type").and_then(|x| x.as_str()).unwrap_or("?"),
                                v.pointer("/payload/type").and_then(|x| x.as_str()).unwrap_or("-"),
                                v.pointer("/payload/role").and_then(|x| x.as_str()).unwrap_or("-")
                            )
                        })
                        .unwrap_or_else(|_| "（非 JSON 行）".into())
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        "Codex 活动态: rollout={} 改动于 {age}s 前 | 末步判定={:?} | 尾 6 行(type/payload.type/role)=[{}]",
        p.display(),
        step.map(|(s, ts)| format!("{s:?} @{}s前", now.saturating_sub(ts) / 1000)),
        tail.join(", ")
    )
}

#[cfg(windows)]
pub fn lower_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}
#[cfg(not(windows))]
pub fn lower_thread_priority() {}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        lower_thread_priority(); // 探测永远让位于前台输入
        let mut ctx = Ctx::new();
        let mut last: Vec<Activity> = Vec::new();
        let mut pres = presence();
        let mut tick: u32 = 0;
        loop {
            // 存在性探测（找 exe、查凭据）每分钟一次就够；2 s 的节拍只做 stat 和一条查询
            if tick % 30 == 0 {
                pres = presence();
            }
            tick = tick.wrapping_add(1);
            let found = read_all(pres, &mut ctx);
            if found != last {
                // 活动态变化前 20 次落日志（含 Codex 判定原材料），便于校准阈值
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                    crate::applog(&format!("activity: {:?} | {}", found.iter().map(|a| format!("{}:{}", a.provider, a.state)).collect::<Vec<_>>(), probe()));
                }
                last = found.clone();
                {
                    let st = app.state::<AppState>();
                    *st.activity.lock().unwrap() = found.clone();
                }
                let _ = app.emit("activity", &found);
            }
            std::thread::sleep(INTERVAL);
        }
    });
}
