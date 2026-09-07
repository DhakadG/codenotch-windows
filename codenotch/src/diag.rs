//! `codenotch.exe doctor deep`：给"在干活吗"探测找信号用的深度诊断。
//! 只打印结构、时间和很短的标量；任何长字符串只报长度，不会把 token / 对话内容打出来。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn mtime_ms(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

/// 最近 `within_s` 秒内改动过的文件（限深度），按新旧排序
fn recent_files(root: &Path, depth: usize, within_s: u64, out: &mut Vec<(u64, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let now = now_ms();
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if p.is_dir() {
            if depth > 0 && !name.starts_with("node_modules") && name != "Cache" && name != "Code Cache" && name != "GPUCache" {
                recent_files(&p, depth - 1, within_s, out);
            }
            continue;
        }
        if let Some(m) = mtime_ms(&p) {
            let age = now.saturating_sub(m) / 1000;
            if age <= within_s {
                out.push((age, p));
            }
        }
    }
}

fn short(v: &rusqlite::types::Value) -> String {
    use rusqlite::types::Value::*;
    match v {
        Null => "NULL".into(),
        Integer(i) => i.to_string(),
        Real(f) => format!("{f}"),
        Text(t) => {
            if t.len() > 60 {
                format!("<text {} chars>", t.len())
            } else {
                format!("{t:?}")
            }
        }
        Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

/// 一个 SQLite 库的结构 + 每张表按"时间样"列取最新一行（短值）
fn dump_sqlite(path: &Path) -> String {
    use rusqlite::OpenFlags;
    let mut o = format!("--- {} ({}，改动于 {}s 前)\n", path.display(), if path.is_file() { "存在" } else { "不存在" }, now_ms().saturating_sub(mtime_ms(path).unwrap_or(0)) / 1000);
    if !path.is_file() {
        return o;
    }
    let conn = match rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) {
        Ok(c) => c,
        Err(e) => {
            o += &format!("  打不开: {e}\n");
            return o;
        }
    };
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.flatten().collect()))
        .unwrap_or_default();
    for t in tables.iter().take(25) {
        let cols: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info(\"{t}\")"))
            .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(1)).map(|rows| rows.flatten().collect()))
            .unwrap_or_default();
        let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0)).unwrap_or(-1);
        o += &format!("  表 {t} ({count} 行): {}\n", cols.join(", "));
        // 时间样列：updated/created/_at/time/recency
        let timeish: Vec<&String> = cols
            .iter()
            .filter(|c| {
                let l = c.to_lowercase();
                l.contains("updated") || l.contains("created") || l.ends_with("_at") || l.contains("time") || l.contains("recency") || l.contains("modified")
            })
            .collect();
        if let Some(tc) = timeish.first() {
            let sql = format!("SELECT * FROM \"{t}\" ORDER BY \"{tc}\" DESC LIMIT 1");
            if let Ok(mut s) = conn.prepare(&sql) {
                let n = s.column_count();
                if let Ok(mut rows) = s.query([]) {
                    if let Ok(Some(row)) = rows.next() {
                        let mut parts = Vec::new();
                        for i in 0..n {
                            let v: rusqlite::types::Value = row.get(i).unwrap_or(rusqlite::types::Value::Null);
                            parts.push(format!("{}={}", cols.get(i).cloned().unwrap_or_default(), short(&v)));
                        }
                        o += &format!("    最新一行(按 {tc}): {}\n", parts.join(" | "));
                    }
                }
            }
        }
    }
    o
}

/// JSON 文件：只打印标量键（短字符串/数字/布尔），长字符串报长度，嵌套只报类型
fn dump_json_scalars(path: &Path) -> String {
    let mut o = format!("--- {} (改动于 {}s 前)\n", path.display(), now_ms().saturating_sub(mtime_ms(path).unwrap_or(0)) / 1000);
    let Ok(t) = std::fs::read_to_string(path) else {
        o += "  读不到\n";
        return o;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
        o += "  非 JSON\n";
        return o;
    };
    fn walk(v: &serde_json::Value, prefix: &str, depth: usize, o: &mut String) {
        if let Some(obj) = v.as_object() {
            for (k, x) in obj.iter().take(60) {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match x {
                    serde_json::Value::Object(_) if depth < 2 => walk(x, &key, depth + 1, o),
                    serde_json::Value::Object(m) => o.push_str(&format!("  {key}: <object {} keys>\n", m.len())),
                    serde_json::Value::Array(a) => o.push_str(&format!("  {key}: <array {}>\n", a.len())),
                    serde_json::Value::String(s) if s.len() > 40 => o.push_str(&format!("  {key}: <string {} chars>\n", s.len())),
                    other => o.push_str(&format!("  {key}: {other}\n")),
                }
            }
        }
    }
    walk(&v, "", 0, &mut o);
    o
}

pub fn run() -> String {
    let mut o = String::from("== doctor deep：活动态信号侦察 ==\n（请在 Codex 桌面版和 Claude 桌面版都正在工作时运行）\n\n");
    let home = dirs::home_dir().unwrap_or_default();
    let local = dirs::data_local_dir().unwrap_or_default();

    o += "## 最近 120 s 内改动过的文件\n";
    let mut recent = Vec::new();
    recent_files(&home.join(".codex"), 2, 120, &mut recent);
    if let Ok(rd) = std::fs::read_dir(local.join("Packages")) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.contains("claude") || n.contains("anthropic") {
                recent_files(&e.path().join("LocalCache").join("Roaming").join("Claude"), 3, 120, &mut recent);
            }
        }
    }
    recent_files(&dirs::config_dir().unwrap_or_default().join("Claude"), 2, 120, &mut recent);
    recent_files(&dirs::config_dir().unwrap_or_default().join("Cursor").join("User").join("globalStorage"), 1, 120, &mut recent);
    recent.sort();
    for (age, p) in recent.iter().take(60) {
        o += &format!("  {age:>4}s 前  {}\n", p.display());
    }
    if recent.is_empty() {
        o += "  （没有）\n";
    }

    o += "\n## Codex 的 SQLite 库\n";
    for rel in [
        "state_5.sqlite",
        "thread_history_1.sqlite",
        "sqlite/codex-dev.db",
        "goals_1.sqlite",
        "queue_1.sqlite",
    ] {
        o += &dump_sqlite(&home.join(".codex").join(rel));
    }

    o += "\n## Codex 全局状态 JSON（仅标量键）\n";
    o += &dump_json_scalars(&home.join(".codex").join(".codex-global-state.json"));

    o += "\n## Codex session_index.jsonl 末行（键名）\n";
    if let Ok(t) = std::fs::read_to_string(home.join(".codex").join("session_index.jsonl")) {
        if let Some(last) = t.lines().rev().find(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<serde_json::Value>(last) {
                Ok(v) => {
                    let keys: Vec<String> = v.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
                    o += &format!("  键: {}\n", keys.join(", "));
                }
                Err(_) => o += "  非 JSON\n",
            }
        }
    }

    o += "\n## Codex 进程（含命令行前 120 字符）\n";
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name LIKE 'codex%' OR Name LIKE 'ChatGPT%'\" | ForEach-Object { \"$($_.ProcessId)`t$($_.Name)`t$($_.CommandLine)\" }",
        ]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
        if let Ok(out) = cmd.output() {
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                let l: String = l.chars().take(160).collect();
                o += &format!("  {l}\n");
            }
        }
    }
    o
}
