//! 把 codenotch-hook.exe 合并写入 ~/.claude/settings.json（不覆盖用户已有 hooks）。
//! 识别标记：command 里包含 "codenotch-hook"。写入前自动备份。

use serde_json::{json, Value};
use std::path::PathBuf;

/// (Claude Code 事件名, 是否需要 matcher, 上报给 Codenotch 的内部事件)
const WIRING: &[(&str, bool, &str)] = &[
    ("SessionStart", false, "session_start"),
    ("UserPromptSubmit", false, "running"),
    ("PreToolUse", true, "running"),
    ("PostToolUse", true, "running"),
    ("Notification", false, "attention"),
    ("Stop", false, "done"),
    ("SessionEnd", false, "session_end"),
];

fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"]
        .as_array()
        .map(|hs| {
            hs.iter().any(|h| {
                h["command"]
                    .as_str()
                    .map(|c| c.contains("codenotch-hook") || c.contains("eatbean-hook") || c.contains("pacman-hook"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn load(path: &PathBuf) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}))
}

fn backup_and_write(path: &PathBuf, root: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if path.exists() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::copy(path, path.with_extension(format!("json.codenotch-bak-{ts}")));
    }
    let txt = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(path, txt).map_err(|e| e.to_string())
}

pub fn is_installed() -> bool {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.contains("codenotch-hook"))
        .unwrap_or(false)
}

pub fn install() -> Result<String, String> {
    let path = settings_path().ok_or("找不到用户目录")?;
    let hook_exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("无法定位程序目录")?
        .join("codenotch-hook.exe");
    if !hook_exe.exists() {
        return Err(format!("缺少 {}", hook_exe.display()));
    }

    let mut root = load(&path);
    if !root.is_object() {
        root = json!({});
    }
    if !root["hooks"].is_object() {
        root["hooks"] = json!({});
    }

    for (event, need_matcher, internal) in WIRING {
        let arr = root["hooks"][*event].as_array().cloned().unwrap_or_default();
        // 先清掉旧的自己
        let mut arr: Vec<Value> = arr.into_iter().filter(|e| !is_ours(e)).collect();
        let cmd = format!("\"{}\" {}", hook_exe.display(), internal);
        let mut entry = json!({
            "hooks": [{ "type": "command", "command": cmd, "timeout": 5 }]
        });
        if *need_matcher {
            entry["matcher"] = json!("*");
        }
        arr.push(entry);
        root["hooks"][*event] = json!(arr);
    }

    backup_and_write(&path, &root)?;
    Ok(format!("已写入 {}（共 {} 个事件）", path.display(), WIRING.len()))
}

pub fn uninstall() -> Result<String, String> {
    let path = settings_path().ok_or("找不到用户目录")?;
    if !path.exists() {
        return Ok("settings.json 不存在，无需卸载".into());
    }
    let mut root = load(&path);
    let Some(hooks) = root["hooks"].as_object_mut() else {
        return Ok("未发现 hooks 配置".into());
    };
    let mut removed = 0;
    for (_, v) in hooks.iter_mut() {
        if let Some(arr) = v.as_array() {
            let filtered: Vec<Value> = arr.iter().filter(|e| !is_ours(e)).cloned().collect();
            removed += arr.len() - filtered.len();
            *v = json!(filtered);
        }
    }
    backup_and_write(&path, &root)?;
    Ok(format!("已移除 {removed} 条 Codenotch hook"))
}
