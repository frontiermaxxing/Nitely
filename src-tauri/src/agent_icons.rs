//! Each agent drawn with its own icon, read from the copy installed on this machine: the agent's
//! desktop app, or its command-line program when that carries an icon. Nitely ships no vendor
//! marks (src/ui/vendors/README.md); what the person has installed is what they see, cached as PNG
//! in the app data dir. An agent with nothing installed keeps its lettered mark.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Icons are read again after this long, so an updated app's new icon shows up.
const REFRESH: Duration = Duration::from_secs(7 * 24 * 3600);
/// Read at this size; AgentMark draws them at 12 to 22px, sharp on high-density screens.
const SIZE: u32 = 64;

/// Where an agent's icon may come from, most specific first: shell parsing names, either a
/// program's path or `shell:AppsFolder\<app id>` for an app installed from the Store. Windows
/// only: elsewhere `read` reads nothing yet, so there is nowhere to look.
#[cfg(not(windows))]
fn sources(_vendor: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn sources(vendor: &str) -> Vec<String> {
    let mut out = Vec::new();
    match vendor {
        "claude" => {
            out.push(r"shell:AppsFolder\Claude_pzs8sxrjxfjjc!Claude".to_string());
            out.extend(on_path("claude.exe"));
        }
        "codex" => {
            // OpenAI ships its desktop app (ChatGPT, with Codex) as the OpenAI.Codex package.
            out.push(r"shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App".to_string());
            out.extend(on_path("codex.exe"));
        }
        "cursor" => {
            // The `cursor` on PATH is a script in <install>\resources\app\bin; the app is beside it.
            for script in on_path("cursor.cmd").into_iter().chain(on_path("cursor")) {
                if let Some(dir) = Path::new(&script).ancestors().nth(4) {
                    out.push(dir.join("Cursor.exe").to_string_lossy().into_owned());
                }
            }
            for base in ["LOCALAPPDATA", "ProgramFiles"] {
                if let Some(b) = std::env::var_os(base) {
                    let sub = if base == "LOCALAPPDATA" { "Programs\\cursor" } else { "cursor" };
                    out.push(std::path::PathBuf::from(b).join(sub).join("Cursor.exe").to_string_lossy().into_owned());
                }
            }
        }
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.to_lowercase()));
    out
}

/// The full paths of `file` in each folder on PATH.
#[cfg(windows)]
fn on_path(file: &str) -> Vec<String> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(file)).filter(|f| f.is_file()).map(|f| f.to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// The icon of each known agent as PNG bytes, or none when nothing installed here has one.
#[tauri::command]
pub async fn agent_icons(app: tauri::AppHandle) -> Result<HashMap<String, Option<Vec<u8>>>, String> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("agent-icons");
    // The shell's image calls want a thread of their own with COM set up.
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&dir);
        ["claude", "codex", "cursor"].into_iter().map(|v| (v.to_string(), icon(&dir, v))).collect()
    })
    .join()
    .map_err(|_| "reading the agents' icons failed".to_string())
}

fn icon(dir: &Path, vendor: &str) -> Option<Vec<u8>> {
    let cached = dir.join(format!("{vendor}.png"));
    let fresh = std::fs::metadata(&cached).and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < REFRESH);
    if fresh {
        if let Ok(png) = std::fs::read(&cached) {
            return Some(png);
        }
    }
    let png = sources(vendor).iter().find_map(|s| read(s).and_then(|rgba| encode(&rgba, SIZE)));
    match &png {
        Some(bytes) => {
            let _ = std::fs::write(&cached, bytes);
        }
        None => {
            let _ = std::fs::remove_file(&cached);
        }
    }
    png
}

fn encode(rgba: &[u8], size: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().ok()?.write_image_data(rgba).ok()?;
    Some(out)
}

/// The icon the shell shows for `source`, as straight-alpha RGBA, SIZE by SIZE. A program with no
/// icon of its own gives none (not Windows' generic one).
#[cfg(windows)]
fn read(source: &str) -> Option<Vec<u8>> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{ExtractIconExW, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY, SIIGBF_SCALEUP};

    let name = HSTRING::from(source);
    if !source.starts_with("shell:") {
        if !Path::new(source).is_file() {
            return None;
        }
        // How many icons the file carries; none means the shell would draw its generic one.
        let count = unsafe { ExtractIconExW(&name, -1, None, None, 0) };
        if count == 0 {
            return None;
        }
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&name, None).ok()?;
        // Scaled up when the icon is smaller, so it is always SIZE square; anything else is skipped.
        let bitmap = factory.GetImage(SIZE { cx: SIZE as i32, cy: SIZE as i32 }, SIIGBF_ICONONLY | SIIGBF_SCALEUP).ok()?;
        let mut dims = BITMAP::default();
        let got = GetObjectW(HGDIOBJ(bitmap.0), std::mem::size_of::<BITMAP>() as i32, Some((&mut dims as *mut BITMAP).cast()));
        if got == 0 || dims.bmWidth != SIZE as i32 || dims.bmHeight.unsigned_abs() != SIZE {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return None;
        }
        let dc = CreateCompatibleDC(None);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE as i32,
                // Negative: rows top to bottom.
                biHeight: -(SIZE as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
        let rows = GetDIBits(dc, bitmap, 0, SIZE, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        if rows == 0 {
            return None;
        }
        Some(straight_rgba(px))
    }
}

#[cfg(not(windows))]
fn read(_source: &str) -> Option<Vec<u8>> {
    // The Mac reads the app bundle's icon later; until then agents keep their letters there.
    None
}

/// The shell's premultiplied BGRA as straight RGBA, which PNG wants.
#[cfg(windows)]
fn straight_rgba(mut px: Vec<u8>) -> Vec<u8> {
    for p in px.chunks_exact_mut(4) {
        let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
        let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
        p[0] = un(r);
        p[1] = un(g);
        p[2] = un(b);
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn premultiplied_bgra_becomes_straight_rgba() {
        // Half-transparent pure red, premultiplied: B 0, G 0, R 128, A 128.
        assert_eq!(straight_rgba(vec![0, 0, 128, 128]), vec![255, 0, 0, 128]);
        assert_eq!(straight_rgba(vec![10, 20, 30, 0]), vec![0, 0, 0, 0]);
        assert_eq!(straight_rgba(vec![40, 80, 120, 255]), vec![120, 80, 40, 255]);
    }

    #[test]
    #[cfg(windows)]
    fn every_agent_has_somewhere_to_look_and_unknown_ones_none() {
        for v in ["claude", "codex", "cursor"] {
            assert!(!sources(v).is_empty(), "{v}");
        }
        assert!(sources("gemini").is_empty());
    }

    /// Which agents give an icon on this machine, with the first one written to the temp folder to
    /// look at: `cargo test -p nitely-app agent_icons -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn report_what_this_machine_has() {
        let dir = std::env::temp_dir().join("nitely-agent-icons");
        let _ = std::fs::create_dir_all(&dir);
        for v in ["claude", "codex", "cursor"] {
            let found: Vec<_> = sources(v).into_iter().filter(|s| read(s).is_some()).collect();
            if let Some(png) = found.first().and_then(|s| read(s)).and_then(|px| encode(&px, SIZE)) {
                let _ = std::fs::write(dir.join(format!("{v}.png")), png);
            }
            println!("{v}: {found:?}");
        }
        println!("written to {}", dir.display());
    }
}
