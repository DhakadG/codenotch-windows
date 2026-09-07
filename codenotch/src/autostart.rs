//! 开机自启：HKCU\...\Run 注册表项（per-user，无需管理员）。
//! 自启命令带 --silent：后台待命，无会话时不显示悬浮条，有会话自动现身。
//! 用 reg.exe 实现，零新依赖。

use std::process::Command;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "Codenotch";

fn reg(args: &[&str]) -> Option<(bool, String)> {
    let mut c = Command::new("reg");
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.output().ok().map(|o| {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.success(), text)
    })
}

pub fn is_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", NAME])
        .map(|(ok, out)| ok && out.contains(NAME))
        .unwrap_or(false)
}

pub fn enable() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let val = format!("\"{}\" --silent", exe.display());
    match reg(&["add", RUN_KEY, "/v", NAME, "/t", "REG_SZ", "/d", &val, "/f"]) {
        Some((true, _)) => Ok("开机自启已启用（静默待命，有会话自动现身）".into()),
        Some((false, out)) => Err(out),
        None => Err("reg.exe 执行失败".into()),
    }
}

pub fn disable() -> Result<String, String> {
    match reg(&["delete", RUN_KEY, "/v", NAME, "/f"]) {
        Some((true, _)) => Ok("开机自启已关闭".into()),
        Some((false, out)) => {
            if out.to_lowercase().contains("unable to find") || out.contains("找不到") {
                Ok("开机自启本就未启用".into())
            } else {
                Err(out)
            }
        }
        None => Err("reg.exe 执行失败".into()),
    }
}
