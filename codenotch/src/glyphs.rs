//! 提供商图标。
//!
//! 纪律：**我们不自己画任何厂商 logo**，只用现成素材。取用顺序：
//!   1. 用户覆盖：`%APPDATA%\codenotch\glyphs\<id>.svg|.png` 或 exe 同目录 `glyphs\`；
//!   2. 内置（个人自用、非商业）：编译进 exe 的 `glyphs/*.svg`，
//!      来自 npm `@lobehub/icons-static-svg` 1.95.0（MIT），原文件未改；商标声明见 glyphs/NOTICE.md；
//!   3. 本机已安装应用自己的图标（PrivateExtractIconsW 取 exe 资源 64px → PNG）；
//!   都没有 → 前端退回占位字母。
//! SVG 以文本内联进 DOM（`fill="currentColor"` 随 CSS 变白/变暗）；PNG/应用图标走 <img>。
//! id 与前端/上游一致：claude / codex / cursor / gemini。

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Debug, Default)]
pub struct Glyph {
    /// svg = 内联 SVG（单色，随 currentColor）；png = 位图素材；appicon = 应用图标（彩色，圆角缩小）
    pub kind: String,
    /// png/appicon 的 data: URL
    pub url: String,
    /// svg 的文本（已去 script / on* 事件属性）
    pub svg: String,
    /// 来源说明（doctor 用）
    pub source: String,
}

pub const IDS: [&str; 4] = ["claude", "codex", "cursor", "gemini"];

/// 内置素材（@lobehub/icons-static-svg，MIT）：codex 用 OpenAI 标（与上游 glyph 选择一致），gemini 用 Antigravity 标
const BUILTIN: [(&str, &str); 4] = [
    ("claude", include_str!("../glyphs/claude.svg")),
    ("codex", include_str!("../glyphs/codex.svg")),
    ("cursor", include_str!("../glyphs/cursor.svg")),
    ("gemini", include_str!("../glyphs/gemini.svg")),
];

/// 最低限度的 SVG 清洗：内联进 DOM 前去掉 <script> 块与 on*="…" 事件属性（内置文件本无，用户文件防手滑）。
/// 所有切片位置都来自 ASCII 模式匹配，落在字符边界上，对中文/emoji 内容安全。
fn sanitize_svg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let lower = s.to_ascii_lowercase();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<script") {
        out.push_str(&s[i..i + rel]);
        match lower[i + rel..].find("</script>") {
            Some(e) => i = i + rel + e + "</script>".len(),
            None => {
                i = s.len();
                break;
            }
        }
    }
    out.push_str(&s[i..]);
    let lo = out.to_ascii_lowercase();
    let mut res = String::with_capacity(out.len());
    let mut i = 0;
    loop {
        let Some(rel) = lo[i..].find(" on") else { break };
        let start = i + rel;
        let name_len = lo[start + 3..].bytes().take_while(|b| b.is_ascii_alphanumeric()).count();
        let eq = start + 3 + name_len;
        if name_len > 0 && lo.as_bytes().get(eq) == Some(&b'=') {
            if let Some(&q) = lo.as_bytes().get(eq + 1) {
                if q == b'"' || q == b'\'' {
                    if let Some(close) = lo[eq + 2..].find(q as char) {
                        res.push_str(&out[i..start]);
                        i = eq + 2 + close + 1;
                        continue;
                    }
                }
            }
        }
        res.push_str(&out[i..start + 3]);
        i = start + 3;
    }
    res.push_str(&out[i..]);
    res
}

fn glyph_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = crate::config::config_path().parent() {
        v.push(d.join("glyphs"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.join("glyphs"));
        }
    }
    v
}

pub fn user_dir() -> PathBuf {
    crate::config::config_path().with_file_name("glyphs")
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn from_file(p: &Path) -> Option<Glyph> {
    let bytes = std::fs::read(p).ok()?;
    if bytes.is_empty() || bytes.len() > 512 * 1024 {
        return None;
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "svg" => Some(Glyph {
            kind: "svg".into(),
            svg: sanitize_svg(&String::from_utf8_lossy(&bytes)),
            source: p.display().to_string(),
            ..Default::default()
        }),
        "png" => Some(Glyph {
            kind: "png".into(),
            url: format!("data:image/png;base64,{}", b64(&bytes)),
            source: p.display().to_string(),
            ..Default::default()
        }),
        _ => None,
    }
}

/// 本机应用 exe 候选（Windows）；MSIX 商店版装在 WindowsApps 下普通进程读不到，取不到就算了
fn app_candidates(id: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let Some(local) = dirs::data_local_dir() else { return v };
    let programs = local.join("Programs");
    match id {
        "claude" => {
            v.push(local.join("AnthropicClaude").join("claude.exe"));
            if let Ok(rd) = std::fs::read_dir(local.join("AnthropicClaude")) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with("app-") {
                        v.push(e.path().join("claude.exe"));
                    }
                }
            }
            v.push(programs.join("Claude").join("Claude.exe"));
            v.push(programs.join("claude-desktop").join("Claude.exe"));
        }
        "codex" => {
            v.push(programs.join("ChatGPT").join("ChatGPT.exe"));
            v.push(programs.join("Codex").join("Codex.exe"));
            if let Some(exe) = crate::codex::find_executable() {
                v.push(exe);
            }
        }
        "cursor" => {
            v.push(programs.join("cursor").join("Cursor.exe"));
            v.push(programs.join("Cursor").join("Cursor.exe"));
        }
        "gemini" => {
            v.push(programs.join("Antigravity").join("Antigravity.exe"));
            v.push(programs.join("antigravity").join("Antigravity.exe"));
        }
        _ => {}
    }
    v.into_iter().filter(|p| p.is_file()).collect()
}

/// exe 资源里的图标 → 64px RGBA → PNG data URL
#[cfg(windows)]
fn from_exe(p: &Path) -> Option<Glyph> {
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, PrivateExtractIconsW, HICON, ICONINFO};

    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    if wide.len() >= 260 {
        return None;
    }
    let mut name = [0u16; 260];
    name[..wide.len()].copy_from_slice(&wide);
    const SIZE: i32 = 64;
    unsafe {
        let mut icons = [HICON::default(); 1];
        let mut id = 0u32;
        let n = PrivateExtractIconsW(&name, 0, SIZE, SIZE, Some(&mut icons[..]), Some(&mut id as *mut u32), 0);
        if n == 0 || icons[0].is_invalid() {
            return None;
        }
        let hicon = icons[0];
        let mut info = ICONINFO::default();
        let ok = GetIconInfo(hicon, &mut info).is_ok();
        let mut result = None;
        if ok && !info.hbmColor.is_invalid() {
            let mut bm = BITMAP::default();
            GetObjectW(info.hbmColor, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            if w > 0 && h > 0 && w <= 512 && h <= 512 {
                let hdc = GetDC(None);
                let mut bi = BITMAPINFO::default();
                bi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                };
                let mut buf = vec![0u8; (w * h * 4) as usize];
                let lines = GetDIBits(hdc, info.hbmColor, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
                let _ = ReleaseDC(None, hdc);
                if lines > 0 {
                    // BGRA → RGBA；全 0 alpha 的旧式图标按不透明处理
                    let any_alpha = buf.chunks_exact(4).any(|px| px[3] != 0);
                    for px in buf.chunks_exact_mut(4) {
                        px.swap(0, 2);
                        if !any_alpha {
                            px[3] = 255;
                        }
                    }
                    result = encode_png(w as u32, h as u32, &buf).map(|png| Glyph {
                        kind: "appicon".into(),
                        url: format!("data:image/png;base64,{}", b64(&png)),
                        source: p.display().to_string(),
                        ..Default::default()
                    });
                }
            }
        }
        if ok {
            let _ = DeleteObject(info.hbmColor);
            let _ = DeleteObject(info.hbmMask);
        }
        let _ = DestroyIcon(hicon);
        result
    }
}
#[cfg(not(windows))]
fn from_exe(_p: &Path) -> Option<Glyph> {
    None
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().ok()?;
        wr.write_image_data(rgba).ok()?;
        wr.finish().ok()?;
    }
    Some(out)
}

/// 收集全部提供商图标（启动时一次；托盘"刷新"时再来一次）
pub fn collect() -> HashMap<String, Glyph> {
    let mut map = HashMap::new();
    let dirs = glyph_dirs();
    for id in IDS {
        let mut found: Option<Glyph> = None;
        'dirs: for d in &dirs {
            for ext in ["svg", "png"] {
                let p = d.join(format!("{id}.{ext}"));
                if let Some(g) = from_file(&p) {
                    found = Some(g);
                    break 'dirs;
                }
            }
        }
        if found.is_none() {
            if let Some((_, svg)) = BUILTIN.iter().find(|(k, _)| *k == id) {
                found = Some(Glyph {
                    kind: "svg".into(),
                    svg: sanitize_svg(svg),
                    source: "built-in · @lobehub/icons-static-svg 1.95.0 (MIT)".into(),
                    ..Default::default()
                });
            }
        }
        if found.is_none() {
            for exe in app_candidates(id) {
                if let Some(g) = from_exe(&exe) {
                    found = Some(g);
                    break;
                }
            }
        }
        if let Some(g) = found {
            map.insert(id.to_string(), g);
        }
    }
    map
}

/// doctor 用
pub fn probe() -> String {
    let m = collect();
    let mut lines = vec![format!("图标目录: {}（放 claude/codex/cursor/gemini 的 .svg 或 .png）", user_dir().display())];
    for id in IDS {
        lines.push(match m.get(id) {
            Some(g) => format!("  {id}: {} ← {}", g.kind, g.source),
            None => format!("  {id}: 占位字母（内置素材缺失？）"),
        });
    }
    lines.join("\n")
}
