//! Telling the person something waits on them, outside the window: a toast for new decisions and
//! the count of waiting ones on the taskbar button. What to say and when is decided by the
//! frontend from the Inbox's own list (src/notify.ts); this only shows it.

use tauri::{AppHandle, Emitter, Manager};

/// Sent with a decision's id when the person clicks its toast; the Inbox opens on it.
pub const OPEN_DECISION_EVENT: &str = "nitely://open-decision";

/// Shows one toast. `decision` is the decision a click opens, or none for a batch (the Inbox).
#[tauri::command]
pub fn notify(app: AppHandle, title: String, body: String, decision: Option<String>) -> Result<(), String> {
    show(&app, &title, &body, decision)
}

#[cfg(windows)]
fn show(app: &AppHandle, title: &str, body: &str, decision: Option<String>) -> Result<(), String> {
    use tauri_winrt_notification::Toast;
    // An installed Nitely has its own app id (the installer's shortcut carries it); a dev build
    // has none registered, and Windows shows toasts only for a registered one.
    let id = if cfg!(debug_assertions) { Toast::POWERSHELL_APP_ID.to_string() } else { app.config().identifier.clone() };
    let clicked = app.clone();
    Toast::new(&id)
        .title(title)
        .text1(body)
        .on_activated(move |_| {
            clicked.emit(OPEN_DECISION_EVENT, decision.clone().unwrap_or_default()).ok();
            crate::focus_main(&clicked);
            Ok(())
        })
        .show()
        .map_err(|e| format!("could not show the notification: {e}"))
}

#[cfg(not(windows))]
fn show(_app: &AppHandle, _title: &str, _body: &str, _decision: Option<String>) -> Result<(), String> {
    // The Mac and Linux get theirs later (filed as a follow-up of NIT-124); the badge below works there now.
    Ok(())
}

/// The count of waiting decisions on the taskbar button: an overlay icon on Windows (drawn by the
/// frontend as `size` by `size` RGBA), the badge elsewhere. Zero clears it.
#[tauri::command]
pub fn set_badge(app: AppHandle, count: u32, rgba: Option<Vec<u8>>, size: u32) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else { return Ok(()) };
    #[cfg(windows)]
    {
        let icon = match rgba {
            Some(px) if count > 0 && px.len() == (size * size * 4) as usize => Some(tauri::image::Image::new_owned(px, size, size)),
            _ => None,
        };
        window.set_overlay_icon(icon).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = (rgba, size);
        window.set_badge_count(if count > 0 { Some(count as i64) } else { None }).map_err(|e| e.to_string())
    }
}
