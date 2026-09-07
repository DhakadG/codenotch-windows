//! transcript 监视器：Claude Code **桌面版** 的兜底数据源。
//! 背景：桌面版 Windows 下 settings.json hooks 存在不触发的已知 bug（2026-05），
//! 因此监视 ~/.claude/projects/**/*.jsonl 的追加行为，推断会话状态：
//!   - 文件在追加                          → running
//!   - 末行=assistant 纯文本 且静默 >2.5s  → done
//!   - 末行=assistant tool_use 且静默 >20s → attention（等待批准，推断，可能误报慢工具）
//! 仲裁：state.rs 里有新鲜 hook 数据（5min 内）的会话忽略本推断（CLI 走 hook 更准）。

use crate::state::HookEvent;
use crate::AppState;
use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

const QUIET_DONE_MS: u64 = 2_500;
const QUIET_ATTN_MS: u64 = 20_000;
/// 末条为 user 且长静默：可能是强制停止/放弃的回合，不能一直绿着
/// （阈值要容忍长思考——太短会把"深度思考中"误判为完成）
const QUIET_USER_DONE_MS: u64 = 75_000;
/// 自愈扫描周期与新鲜窗口：不赌 notify 事件一个不漏
const RESCAN_SECS: u64 = 45;
const FRESH_WINDOW_MS: u64 = 10 * 60 * 1000;
/// 单条消息（含整文件写入）常超 16KB，尾窗必须够大，否则末行截断解析失败=永远哑火
const TAIL_BYTES: u64 = 256 * 1024;

/// 运行日志：%APPDATA%\codenotch\watch.log（启动时清空，方便排查）
pub fn wlog(msg: &str) {
    let Some(dir) = dirs::config_dir() else { return };
    let p = dir.join("codenotch").join("watch.log");
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "[{}] {}", now_ms(), msg);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    User,
    AsstText,
    AsstTool,
    Other,
}

struct Trk {
    session: String,
    cwd: String,
    last_append: u64,
    kind: Kind,
    sent: &'static str, // 上次已推送的状态，防重复
    /// 末条 user 消息含中断标记（"[Request interrupted...]"）——强停快速判完
    interrupted: bool,
    prompt: String,
    model: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 监视根目录：CLI 的 ~/.claude/projects + 桌面端（Cowork）的会话镜像目录
/// （桌面端每个会话有独立的 .claude/projects，位于 %APPDATA%\Claude\local-agent-mode-sessions 下）
pub fn roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".claude").join("projects"));
    }
    if let Some(c) = dirs::config_dir() {
        v.push(c.join("Claude").join("local-agent-mode-sessions"));
    }
    // 桌面版若为打包应用（MSIX），其 AppData 被虚拟化，真实落盘在
    // %LOCALAPPDATA%\Packages\<含 claude/anthropic 的包>\LocalCache\Roaming\Claude\...
    if let Some(local) = dirs::data_local_dir() {
        if let Ok(rd) = std::fs::read_dir(local.join("Packages")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                if name.contains("claude") || name.contains("anthropic") {
                    v.push(
                        e.path()
                            .join("LocalCache")
                            .join("Roaming")
                            .join("Claude")
                            .join("local-agent-mode-sessions"),
                    );
                }
            }
        }
    }
    v
}

/// 只接受真正的会话 transcript：必须在 .claude 目录树内，排除审计日志与子代理
pub fn is_session_jsonl(p: &Path) -> bool {
    if p.extension().map(|e| e == "jsonl").unwrap_or(false) == false {
        return false;
    }
    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if name == "audit.jsonl" {
        return false;
    }
    let mut in_claude = false;
    for c in p.components() {
        let s = c.as_os_str().to_string_lossy();
        if s == "subagents" {
            return false;
        }
        if s == ".claude" {
            in_claude = true;
        }
    }
    in_claude
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let (tx, rx) = channel();
        let Ok(mut w) = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) else {
            return;
        };
        // 清空上次日志
        if let Some(dir) = dirs::config_dir() {
            let _ = std::fs::write(dir.join("codenotch").join("watch.log"), "");
        }
        wlog(&format!("watcher 启动 v{}", env!("CARGO_PKG_VERSION")));
        let mut pending: Vec<PathBuf> = roots();
        let mut watching = 0usize;
        let mut last_retry = std::time::Instant::now();
        pending.retain(|r| {
            if r.exists() && w.watch(r, RecursiveMode::Recursive).is_ok() {
                watching += 1;
                wlog(&format!("正在监视: {}", r.display()));
                false
            } else {
                wlog(&format!("暂不可用(将每60s重试): {}", r.display()));
                true
            }
        });
        let mut tracks: HashMap<PathBuf, Trk> = HashMap::new();
        rescan(&app, &mut tracks); // 启动即扫一次：接管启动前就在活跃的会话
        let mut last_scan = std::time::Instant::now();
        // 节流（系统级卡顿根因）——桌面版流式输出时 transcript 每秒触发几十次
        // modify 事件，此前每个事件都做一次 256KB 尾读 + JSON 解析，与 Claude 桌面版争抢
        // 同一文件的 IO/CPU。现在同一文件 INGEST_MIN_GAP 内只 ingest 一次，其余合并成脏标记。
        const INGEST_MIN_GAP: Duration = Duration::from_millis(800);
        let mut last_ingest: HashMap<PathBuf, std::time::Instant> = HashMap::new();
        let mut dirty: std::collections::HashSet<PathBuf> = Default::default();
        loop {
            match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(Ok(ev)) => {
                    for p in ev.paths {
                        if is_session_jsonl(&p) {
                            dirty.insert(p);
                        }
                    }
                    // 把队列里已积压的事件一口气吸干，避免逐条唤醒
                    while let Ok(Ok(ev)) = rx.try_recv() {
                        for p in ev.paths {
                            if is_session_jsonl(&p) {
                                dirty.insert(p);
                            }
                        }
                    }
                }
                Ok(Err(_)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !dirty.is_empty() {
                let now = std::time::Instant::now();
                let due: Vec<PathBuf> = dirty
                    .iter()
                    .filter(|p| {
                        last_ingest
                            .get(*p)
                            .map(|t| now.duration_since(*t) >= INGEST_MIN_GAP)
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect();
                for p in due {
                    dirty.remove(&p);
                    last_ingest.insert(p.clone(), now);
                    ingest(&app, &mut tracks, &p);
                }
                if last_ingest.len() > 512 {
                    last_ingest.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
                }
            }
            evaluate(&app, &mut tracks);
            // 周期自愈扫描：新会话目录 / notify 漏事件兜底
            if last_scan.elapsed() > Duration::from_secs(RESCAN_SECS) {
                last_scan = std::time::Instant::now();
                rescan(&app, &mut tracks);
            }
            // 尚未存在的根目录每 60s 重试（如从未跑过 CLI）
            if !pending.is_empty() && last_retry.elapsed() > Duration::from_secs(60) {
                last_retry = std::time::Instant::now();
                pending.retain(|r| {
                    !(r.exists() && w.watch(r, RecursiveMode::Recursive).is_ok())
                });
            }
            let _ = watching; // 全部失败也保持线程存活，等待重试
        }
    });
}

pub struct TailInfo {
    /// 最新一条对话主体条目（user/assistant，跳过记账行）
    pub entry: serde_json::Value,
    /// 最近一次真正的用户输入（跳过 tool_result 型 user 条目）
    pub prompt: String,
    /// 会话实际模型（assistant 条目 message.model）
    pub model: String,
}

/// 用户输入文本提取：content 为字符串，或数组中的 text 块；含 tool_result 的不算
fn user_text(v: &serde_json::Value) -> Option<String> {
    let c = v.pointer("/message/content")?;
    if let Some(s) = c.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.chars().take(120).collect());
    }
    if let Some(arr) = c.as_array() {
        if arr
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        {
            return None;
        }
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = b.get("text").and_then(|x| x.as_str()) {
                    let s = s.trim();
                    if !s.is_empty() {
                        return Some(s.chars().take(120).collect());
                    }
                }
            }
        }
    }
    None
}

/// 读文件尾部，向前回溯：主体条目 + 最近用户输入 + 模型，一次拿齐
/// （末行可能是半行/被尾窗截断的巨行/记账行——统统向前回退）
pub fn tail_info(path: &Path) -> Option<TailInfo> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).ok()?;
    let buf = String::from_utf8_lossy(&raw);
    let mut entry: Option<serde_json::Value> = None;
    let mut prompt = String::new();
    let mut model = String::new();
    for line in buf.lines().rev().filter(|l| !l.trim().is_empty()).take(80) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if t != "user" && t != "assistant" {
            continue;
        }
        if entry.is_none() {
            entry = Some(v.clone());
        }
        if model.is_empty() && t == "assistant" {
            if let Some(m) = v.pointer("/message/model").and_then(|x| x.as_str()) {
                model = m.to_string();
            }
        }
        if prompt.is_empty() && t == "user" {
            if let Some(p) = user_text(&v) {
                prompt = p;
            }
        }
        if entry.is_some() && !model.is_empty() && !prompt.is_empty() {
            break;
        }
    }
    entry.map(|e| TailInfo {
        entry: e,
        prompt,
        model,
    })
}

/// doctor 用的简化入口
pub fn tail_entry(path: &Path) -> Option<serde_json::Value> {
    tail_info(path).map(|t| t.entry)
}

/// 更新跟踪信息，并推送 running
fn ingest(app: &AppHandle, tracks: &mut HashMap<PathBuf, Trk>, path: &Path) {
    let Some(info) = tail_info(path) else {
        return;
    };
    let v = info.entry;
    // transcript 字段是 camelCase（sessionId），与 hook stdin 的 snake_case 不同！
    let session = v
        .get("sessionId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    if session.is_empty() {
        return;
    }
    let mut cwd = v
        .get("cwd")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    // 桌面端会话的 cwd 是内部 outputs 路径，对用户无意义，换成友好标签
    if cwd.contains("local-agent-mode-sessions") {
        cwd = "Claude桌面".to_string();
    }
    let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let kind = match typ {
        "user" => Kind::User,
        "assistant" => {
            let has_tool = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                })
                .unwrap_or(false);
            if has_tool {
                Kind::AsstTool
            } else {
                Kind::AsstText
            }
        }
        _ => Kind::Other,
    };
    let interrupted = typ == "user"
        && v.get("message")
            .map(|m| m.to_string().to_lowercase().contains("interrupt"))
            .unwrap_or(false);
    let t = tracks.entry(path.to_path_buf()).or_insert(Trk {
        session: session.clone(),
        cwd: cwd.clone(),
        last_append: 0,
        kind,
        sent: "",
        interrupted: false,
        prompt: String::new(),
        model: String::new(),
    });
    t.session = session;
    if !cwd.is_empty() {
        t.cwd = cwd;
    }
    t.last_append = now_ms();
    t.kind = kind;
    t.interrupted = interrupted;
    if !info.prompt.is_empty() {
        t.prompt = info.prompt;
    }
    if !info.model.is_empty() {
        t.model = info.model;
    }
    // 每次追加都推 running（prompt/model 更新也随之送达）；
    // 是否真广播由 state.apply 的可见变更检测去重
    t.sent = "running";
    push(app, "running", t);
}

/// 自愈扫描：走一遍根目录，凡 mtime 比我们记录的 last_append 新且在新鲜窗口内的
/// 会话文件都补一次 ingest——新会话目录、notify 丢事件都由它兜底
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 10 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, depth + 1, out);
        } else if is_session_jsonl(&p) {
            out.push(p);
        }
    }
}

fn rescan(app: &AppHandle, tracks: &mut HashMap<PathBuf, Trk>) {
    let now = now_ms();
    let mut found = Vec::new();
    for r in roots() {
        if r.exists() {
            walk(&r, 0, &mut found);
        }
    }
    for p in found {
        let Some(mtime) = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
        else {
            continue;
        };
        if now.saturating_sub(mtime) > FRESH_WINDOW_MS {
            continue; // 只关心近期活跃的
        }
        let known = tracks.get(&p).map(|t| t.last_append).unwrap_or(0);
        if mtime > known {
            ingest(app, tracks, &p);
        }
    }
}

/// 静默判定：done / attention（推断）
fn evaluate(app: &AppHandle, tracks: &mut HashMap<PathBuf, Trk>) {
    let now = now_ms();
    for t in tracks.values_mut() {
        if t.last_append == 0 {
            continue;
        }
        let quiet = now.saturating_sub(t.last_append);
        match t.kind {
            Kind::AsstText if quiet > QUIET_DONE_MS && t.sent != "done" => {
                t.sent = "done";
                push(app, "done", t);
            }
            Kind::AsstTool if quiet > QUIET_ATTN_MS && t.sent != "attention" => {
                t.sent = "attention";
                push(app, "attention", t);
            }
            // 强制停止：末条 user 带中断标记，快速判完
            Kind::User if t.interrupted && quiet > QUIET_DONE_MS && t.sent != "done" => {
                t.sent = "done";
                push(app, "done", t);
            }
            // 末条 user 长静默（75s）：回合被放弃/停止（阈值容忍长思考）
            Kind::User if quiet > QUIET_USER_DONE_MS && t.sent != "done" => {
                t.sent = "done";
                push(app, "done", t);
            }
            // 安全阀：其他类型 5 分钟无动静，不能让绿灯永远亮着
            Kind::Other if quiet > 5 * 60_000 && t.sent != "done" => {
                t.sent = "done";
                push(app, "done", t);
            }
            _ => {}
        }
    }
}

static PUSH_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn push(app: &AppHandle, e: &str, t: &Trk) {
    // 前 30 条推送落日志，供 doctor/排查（之后静音防日志膨胀）
    if PUSH_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 30 {
        wlog(&format!("推送 {} session={} cwd={}", e, t.session, t.cwd));
    }
    let ev = HookEvent {
        e: e.to_string(),
        session_id: t.session.clone(),
        ppid: 0,
        cwd: t.cwd.clone(),
        prompt: t.prompt.clone(),
        message: String::new(),
        tool_name: String::new(),
        tool_cmd: String::new(),
        model: t.model.clone(),
        src: "watch",
    };
    let changed = {
        let st = app.state::<AppState>();
        let mut store = st.store.lock().unwrap();
        store.apply(ev)
    };
    if changed {
        crate::broadcast(app);
    }
}
