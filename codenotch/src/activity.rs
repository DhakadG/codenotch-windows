//! 非 Claude 提供商的"在干活吗"探测（Claude 走 hooks + transcript watcher 的四态引擎，不在这里）。
//!
//! 三家都没有像 Claude Code 那样的状态字段，能拿到什么就如实标注什么（上游同款取舍）：
//!   - Cursor：编辑器状态库 `state.vscdb` 的 `composerHeaders` 行（JSON）——`unfinishedRunAt` 在一轮运行中
//!     被设置、结束即清；`hasBlockingPendingActions` / `hasPendingPlan` = 在等你。这是**真状态**。
//!     库是 WAL 模式：必须用普通只读打开（immutable 会忽略 WAL，看到的是上次 checkpoint 的旧世界）。
//!   - Codex：无状态字段。CLI / VS Code 扩展一轮运行中会持续追加线程的 rollout 日志，桌面版 Codex 则
//!     写自己的线程目录 `~/.codex/sqlite/codex-dev.db`（`local_thread_catalog.source_updated_at`，秒）。
//!     谁最近动过谁在干活；**启发式**——只认最近 8 s 内的写入，宁可早停也不编造。
//!   - Antigravity：transcript.jsonl 在一轮运行中被追加（每步只在完成后才写，status 全是 DONE 没法用），
//!     最近 45 s 内写过 = 在干活（模型两步之间可能想很久，窗口放宽）。
//! 轮询 2 s（上游节奏），只在有变化时广播；每次只做几个 stat 和一条 SQLite 查询，成本可忽略。

use crate::AppState;
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const INTERVAL: Duration = Duration::from_secs(2);
const CODEX_STALE_MS: u64 = 8_000;
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

// ---------------- Cursor ----------------

fn open_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

fn cursor_activity() -> Vec<Activity> {
    let Some(path) = crate::cursor::store_url() else { return vec![] };
    let Some(conn) = open_ro(&path) else { return vec![] };
    let Ok(mut stmt) = conn.prepare("SELECT value FROM composerHeaders WHERE isArchived = 0 ORDER BY recency DESC LIMIT 40") else {
        return vec![];
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else { return vec![] };
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
    out
}

// ---------------- Codex ----------------

fn codex_desktop_db() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("sqlite").join("codex-dev.db"))
}

fn codex_activity() -> Vec<Activity> {
    let now = now_ms();
    let mut candidates: Vec<(String, u64)> = Vec::new();
    if let Some(p) = crate::codex::newest_rollout() {
        if let Some(m) = mtime_ms(&p) {
            candidates.push(("Codex".into(), m));
        }
    }
    if let Some(db) = codex_desktop_db() {
        if let Some(conn) = open_ro(&db) {
            // source_updated_at 是"秒（带小数）"，与隔壁 threads 表的毫秒不同；列类型不赌，三种都认
            let row = conn.query_row(
                "SELECT source_updated_at, display_title FROM local_thread_catalog ORDER BY source_updated_at DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, rusqlite::types::Value>(0)?, r.get::<_, Option<String>>(1)?)),
            );
            if let Ok((at, title)) = row {
                let secs = match at {
                    rusqlite::types::Value::Real(f) => Some(f),
                    rusqlite::types::Value::Integer(i) => Some(i as f64),
                    rusqlite::types::Value::Text(t) => t.parse::<f64>().ok(),
                    _ => None,
                };
                if let Some(secs) = secs {
                    let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| "Codex".into());
                    candidates.push((title, (secs * 1000.0) as u64));
                }
            }
        }
    }
    let Some((name, at)) = candidates.into_iter().max_by_key(|c| c.1) else { return vec![] };
    if now.saturating_sub(at) > CODEX_STALE_MS {
        return vec![];
    }
    vec![Activity { provider: "codex".into(), state: "busy".into(), name, detail: "Working".into(), since: at }]
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
struct Presence {
    cursor: bool,
    codex: bool,
    gemini: bool,
}

fn presence() -> Presence {
    Presence { cursor: crate::cursor::present(), codex: crate::codex::present(), gemini: crate::antigravity::present() }
}

pub fn read_all(p: Presence) -> Vec<Activity> {
    let mut all = Vec::new();
    if p.cursor {
        all.extend(cursor_activity());
    }
    if p.codex {
        all.extend(codex_activity());
    }
    if p.gemini {
        all.extend(antigravity_activity());
    }
    all
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Vec<Activity> = Vec::new();
        let mut pres = presence();
        let mut tick: u32 = 0;
        loop {
            // 存在性探测（找 exe、查凭据）每分钟一次就够；2 s 的节拍只做 stat 和一条查询
            if tick % 30 == 0 {
                pres = presence();
            }
            tick = tick.wrapping_add(1);
            let found = read_all(pres);
            if found != last {
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
