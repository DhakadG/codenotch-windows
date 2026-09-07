//! `codenotch.exe doctor` —— 自诊断：不猜，直接看。
//! 检查：配置/端口占用/监视根目录/最新会话文件/尾行解析结果，
//! 输出到 stdout + %APPDATA%\codenotch\doctor.log。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn collect(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, SystemTime)>) {
    if depth > 10 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, depth + 1, out);
        } else if crate::watcher::is_session_jsonl(&p) {
            if let Ok(m) = e.metadata() {
                if let Ok(t) = m.modified() {
                    out.push((p, t));
                }
            }
        }
    }
}

fn age_secs(t: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn run() -> String {
    let mut o = String::new();
    o += &format!("== Codenotch doctor v{} ==\n", env!("CARGO_PKG_VERSION"));

    let cfg = crate::config::load();
    o += &format!(
        "配置: port={} lang={} ({})\n",
        cfg.port,
        cfg.lang,
        crate::config::config_path().display()
    );

    match std::net::TcpListener::bind(("127.0.0.1", cfg.port)) {
        Ok(_) => o += "端口: 空闲 —— 当前【没有】Codenotch 实例在运行\n",
        Err(_) => o += "端口: 被占用 —— 已有实例在运行（确认面板头部版本号是否 v0.1.1，防止跑的是旧版）\n",
    }

    for root in crate::watcher::roots() {
        if !root.exists() {
            o += &format!("根目录: {} 【不存在】\n", root.display());
            continue;
        }
        o += &format!("根目录: {} 存在，扫描最新会话…\n", root.display());
        let mut files = Vec::new();
        collect(&root, 0, &mut files);
        files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
        if files.is_empty() {
            o += "  （没有任何会话 transcript）\n";
        }
        for (p, m) in files.into_iter().take(5) {
            o += &format!("  {}s 前更新  {}\n", age_secs(m), p.display());
            match crate::watcher::tail_entry(&p) {
                Some(v) => {
                    o += &format!(
                        "    尾行解析 OK: type={} sessionId={}\n",
                        v.get("type").and_then(|x| x.as_str()).unwrap_or("?"),
                        v.get("sessionId").and_then(|x| x.as_str()).unwrap_or("(缺失,将用文件名)")
                    );
                }
                None => o += "    尾行解析失败（30 行内无有效 JSON——请把此文件路径发给开发者）\n",
            }
        }
    }

    o += &format!("\n用量数据源:\n  {}\n  {}\n", crate::usage::probe_credentials(), crate::codex::probe());
    o += &format!("  {}\n", crate::cursor::probe());
    o += &format!("  {}\n", crate::antigravity::probe());
    o += &format!("\n提供商图标:\n{}\n", crate::glyphs::probe());
    o += &format!("\n活动态:\n  {}\n", crate::activity::probe());

    o += "\nwatch.log（若存在，最近运行的监视日志）:\n";
    if let Some(dir) = dirs::config_dir() {
        let p = dir.join("codenotch").join("watch.log");
        match std::fs::read_to_string(&p) {
            Ok(t) if !t.trim().is_empty() => {
                for line in t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                    o += &format!("  {}\n", line);
                }
            }
            _ => o += "  （空——主程序尚未启动过（首次运行正常），或跑的是不含 watcher 的旧版本）\n",
        }
    }
    o
}
