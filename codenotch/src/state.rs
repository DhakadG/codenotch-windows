//! 四态状态机：attention > running > done > idle（注意力成本排序）
//! done 驻留：仅被该会话新的 UserPromptSubmit、用户手动 ✕、或 >24h 陈旧清理清除。

use serde::Serialize;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ST_RUNNING: &str = "running";
pub const ST_ATTENTION: &str = "attention";
pub const ST_DONE: &str = "done";
pub const ST_IDLE: &str = "idle";

const RUNNING_STALE_MS: u64 = 30 * 60 * 1000; // 无事件 30min 的 running 视为异常退出
const DONE_STALE_MS: u64 = 24 * 3600 * 1000; // 陈旧 done 24h 后清理
const IDLE_DROP_MS: u64 = 10 * 60 * 1000; // idle 10min 后从列表移除

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub state: String,
    /// 本轮活动开始时间（ms epoch）
    pub started: u64,
    /// done 时冻结的总耗时（ms）
    pub total: u64,
    pub last: String,
    /// attention 时的提示内容（权限请求/提问摘要）
    pub attn: String,
    /// 用户最近一次输入（面板副标题："你: …"，agent-notch 式——展示你的话而非代理动作）
    pub prompt: String,
    /// 会话实际使用的模型（transcript assistant 条目 message.model）
    pub model: String,
    #[serde(skip)]
    pub ppid: u32,
    #[serde(skip)]
    pub last_event: u64,
    #[serde(skip)]
    pub cwd: String,
    /// 最近一次真实 hook 事件的时间；有新鲜 hook 数据时忽略 watcher 推断
    #[serde(skip)]
    pub last_hook: u64,
}

/// hook 数据在此时间窗内视为新鲜，watcher 推断退让
const HOOK_FRESH_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub agg: String,
    pub counts: HashMap<String, usize>,
    /// 用户设置的语言（可能是 "auto"，面板按钮高亮用）
    pub lang: String,
    /// Rust 侧解析后的实际语言（WebView2 的 navigator.language 不可靠）
    pub lang_resolved: String,
    /// 是否允许拖动/滚轮调宽（bar 前端据此启用手势）
    pub drag: bool,
}

#[derive(Default)]
pub struct Store {
    map: HashMap<String, Session>,
}

pub struct HookEvent {
    pub e: String,
    pub session_id: String,
    pub ppid: u32,
    pub cwd: String,
    pub prompt: String,
    pub message: String,
    pub tool_name: String,
    pub tool_cmd: String,
    pub model: String,
    /// "hook"（真实事件）或 "watch"（transcript 推断，桌面版兜底）
    pub src: &'static str,
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

fn title_of(cwd: &str, id: &str) -> String {
    let base = cwd
        .replace('\\', "/")
        .rsplit('/')
        .find(|p| !p.is_empty())
        .unwrap_or("claude")
        .to_string();
    let short: String = id.chars().take(4).collect();
    format!("{base} · {short}")
}

impl Store {
    pub fn apply(&mut self, ev: HookEvent) -> bool {
        let now = now_ms();
        if ev.e == "session_end" {
            return self.map.remove(&ev.session_id).is_some();
        }
        let s = self
            .map
            .entry(ev.session_id.clone())
            .or_insert_with(|| Session {
                id: ev.session_id.clone(),
                title: title_of(&ev.cwd, &ev.session_id),
                state: ST_IDLE.into(),
                started: now,
                total: 0,
                last: String::new(),
                attn: String::new(),
                prompt: String::new(),
                model: String::new(),
                ppid: 0,
                last_event: now,
                cwd: ev.cwd.clone(),
                last_hook: 0,
            });
        // 数据源仲裁：有新鲜 hook 数据的会话不接受 watcher 推断
        if ev.src == "watch" && s.last_hook > 0 && now.saturating_sub(s.last_hook) < HOOK_FRESH_MS {
            return false;
        }
        if ev.src == "hook" {
            s.last_hook = now;
        }
        let before = (
            s.state.clone(),
            s.last.clone(),
            s.attn.clone(),
            s.prompt.clone(),
            s.model.clone(),
        );
        s.last_event = now;
        if ev.ppid != 0 {
            s.ppid = ev.ppid;
        }
        if !ev.model.is_empty() {
            s.model = ev.model.clone();
        }
        if !ev.cwd.is_empty() && s.cwd.is_empty() {
            s.cwd = ev.cwd.clone();
            s.title = title_of(&ev.cwd, &s.id);
        }
        match ev.e.as_str() {
            "session_start" => {
                if s.state != ST_RUNNING {
                    s.state = ST_IDLE.into();
                }
            }
            "running" => {
                if s.state != ST_RUNNING {
                    s.started = now;
                }
                s.state = ST_RUNNING.into();
                s.attn.clear();
                if !ev.prompt.is_empty() {
                    s.prompt = truncate(&ev.prompt, 120);
                }
                if !ev.tool_name.is_empty() {
                    s.last = if ev.tool_cmd.is_empty() {
                        format!("🔧 {}", ev.tool_name)
                    } else {
                        format!("🔧 {}: {}", ev.tool_name, truncate(&ev.tool_cmd, 60))
                    };
                }
            }
            "attention" => {
                s.state = ST_ATTENTION.into();
                if !ev.message.is_empty() {
                    s.attn = truncate(&ev.message, 200);
                }
            }
            "done" => {
                if s.state != ST_DONE {
                    s.total = now.saturating_sub(s.started);
                }
                s.state = ST_DONE.into();
                s.attn.clear();
            }
            _ => {}
        }
        // 只有可见内容变化才广播，避免 watcher 高频 append 造成风暴
        (
            s.state.clone(),
            s.last.clone(),
            s.attn.clone(),
            s.prompt.clone(),
            s.model.clone(),
        ) != before
    }

    pub fn dismiss(&mut self, id: &str) -> bool {
        self.map.remove(id).is_some()
    }

    pub fn has_done(&self) -> bool {
        self.map.values().any(|s| s.state == ST_DONE)
    }

    /// 看过即清：匹配谓词的 done 会话转 idle（随后由 sweep 自然清理）
    pub fn ack_done<F: Fn(&Session) -> bool>(&mut self, f: F) -> bool {
        let now = now_ms();
        let mut changed = false;
        for s in self.map.values_mut() {
            if s.state == ST_DONE && f(s) {
                s.state = ST_IDLE.into();
                s.last_event = now;
                changed = true;
            }
        }
        changed
    }

    /// 陈旧清理，返回是否有变化
    pub fn sweep(&mut self) -> bool {
        let now = now_ms();
        let mut changed = false;
        for s in self.map.values_mut() {
            if s.state == ST_RUNNING && now.saturating_sub(s.last_event) > RUNNING_STALE_MS {
                s.state = ST_IDLE.into();
                changed = true;
            }
        }
        let before = self.map.len();
        self.map.retain(|_, s| {
            !(s.state == ST_IDLE && now.saturating_sub(s.last_event) > IDLE_DROP_MS
                || s.state == ST_DONE && now.saturating_sub(s.last_event) > DONE_STALE_MS)
        });
        changed || self.map.len() != before
    }

    pub fn ppid_of(&self, id: &str) -> Option<u32> {
        self.map.get(id).map(|s| s.ppid).filter(|p| *p != 0)
    }

    pub fn snapshot(&self, lang: &str, lang_resolved: &str, drag: bool) -> Snapshot {
        let mut sessions: Vec<Session> = self.map.values().cloned().collect();
        let rank = |st: &str| match st {
            ST_ATTENTION => 0,
            ST_RUNNING => 1,
            ST_DONE => 2,
            _ => 3,
        };
        sessions.sort_by(|a, b| {
            rank(&a.state)
                .cmp(&rank(&b.state))
                .then(b.started.cmp(&a.started))
        });
        let mut counts = HashMap::new();
        for k in [ST_ATTENTION, ST_RUNNING, ST_DONE] {
            counts.insert(
                k.to_string(),
                sessions.iter().filter(|s| s.state == k).count(),
            );
        }
        let agg = [ST_ATTENTION, ST_RUNNING, ST_DONE]
            .iter()
            .find(|k| counts.get(**k).copied().unwrap_or(0) > 0)
            .map(|k| k.to_string())
            .unwrap_or_else(|| ST_IDLE.to_string());
        Snapshot {
            sessions,
            agg,
            counts,
            lang: lang.to_string(),
            lang_resolved: lang_resolved.to_string(),
            drag,
        }
    }
}
