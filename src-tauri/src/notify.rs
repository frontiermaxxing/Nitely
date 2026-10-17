//! Telling the person something waits on them, outside the window: a toast for new decisions and
//! the count of waiting ones on the taskbar button. What to say and when is decided by the
//! frontend from the Inbox's own list (src/notify.ts); this only shows it: a Windows toast, a Mac
//! Notification Center banner, or a Linux desktop notification, each opening its decision when
//! clicked.

#[cfg(any(windows, target_os = "linux"))]
use tauri::Emitter;
use tauri::{AppHandle, Manager};

/// Sent with a decision's id when the person clicks its toast; the Inbox opens on it.
pub const OPEN_DECISION_EVENT: &str = "nitely://open-decision";

/// Shows one toast. `decision` is the decision a click opens, or none for a batch (the Inbox).
#[tauri::command]
pub fn notify(app: AppHandle, title: String, body: String, decision: Option<String>) -> Result<(), String> {
    if crate::harness::on() {
        // Under the test harness: written down for the test, and shown only when it asks.
        crate::harness::record_notification(&title, &body, decision.as_deref());
        if !crate::harness::real_toasts() {
            return Ok(());
        }
    }
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

#[cfg(target_os = "macos")]
fn show(app: &AppHandle, title: &str, body: &str, decision: Option<String>) -> Result<(), String> {
    mac::show(app, title, body, decision)
}

/// Readies the Mac's notifications when the app starts (elsewhere there is nothing to ready).
pub fn init(_app: &AppHandle) {
    #[cfg(target_os = "macos")]
    if !crate::harness::on() {
        mac::init(_app);
    }
}

// The Mac: Notification Center, through UNUserNotificationCenter. It works only for an app bundle
// (a bare binary from `tauri dev` has no bundle id, and the center would abort it), so outside one
// nothing is shown; the badge still counts.
#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };
    use std::sync::Once;
    use tauri::{AppHandle, Emitter};

    /// A notification's identifier: the decision a click opens, after this prefix.
    const DECISION: &str = "decision:";

    struct Ivars {
        app: AppHandle,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and Delegate does not implement Drop.
        #[unsafe(super(NSObject))]
        #[name = "NitelyNotificationDelegate"]
        #[ivars = Ivars]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            // Clicked: open the decision (or the Inbox) in the window.
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, done: &block2::DynBlock<dyn Fn()>) {
                let id = response.notification().request().identifier().to_string();
                let app = &self.ivars().app;
                app.emit(super::OPEN_DECISION_EVENT, id.strip_prefix(DECISION).unwrap_or_default().to_string()).ok();
                crate::focus_main(app);
                done.call(());
            }

            // Shown even while Nitely is the app in front, as on Windows.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(&self, _center: &UNUserNotificationCenter, _n: &UNNotification, done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>) {
                done.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
            }
        }
    );

    impl Delegate {
        fn new(app: AppHandle) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { app });
            // SAFETY: NSObject's init, on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        }
    }

    fn in_bundle() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    /// Sets the click handler and asks once for permission to notify. Called when the app starts,
    /// so a click on a notification that launched it is handled too.
    pub fn init(app: &AppHandle) {
        static ONCE: Once = Once::new();
        if !in_bundle() {
            return;
        }
        ONCE.call_once(|| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = Delegate::new(app.clone());
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // The center holds its delegate weakly: this one lives as long as the app.
            std::mem::forget(delegate);
            let asked = block2::RcBlock::new(|granted: objc2::runtime::Bool, _error: *mut NSError| {
                if !granted.as_bool() {
                    eprintln!("notifications: not allowed in System Settings, so none will show");
                }
            });
            center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge, &asked);
        });
    }

    pub fn show(app: &AppHandle, title: &str, body: &str, decision: Option<String>) -> Result<(), String> {
        if !in_bundle() {
            return Ok(());
        }
        init(app);
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        // A decision's notification replaces an earlier one for it; a batch gets its own.
        let id = match decision {
            Some(d) => format!("{DECISION}{d}"),
            None => format!("inbox:{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)),
        };
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);
        let added = block2::RcBlock::new(|error: *mut NSError| {
            // SAFETY: the center passes a valid NSError or null.
            if let Some(e) = unsafe { error.as_ref() } {
                eprintln!("notifications: {}", e.localizedDescription());
            }
        });
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, Some(&added));
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn show(app: &AppHandle, title: &str, body: &str, decision: Option<String>) -> Result<(), String> {
    let shown = notify_rust::Notification::new()
        .appname("Nitely")
        .summary(title)
        .body(body)
        // The click on the notification itself.
        .action("default", "Open")
        .show()
        .map_err(|e| format!("could not show the notification: {e}"))?;
    let clicked = app.clone();
    // Waits for the click or for the notification to close, off the command's thread.
    std::thread::spawn(move || {
        shown.wait_for_action(|action| {
            if action == "default" {
                clicked.emit(OPEN_DECISION_EVENT, decision.clone().unwrap_or_default()).ok();
                crate::focus_main(&clicked);
            }
        })
    });
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn show(_app: &AppHandle, _title: &str, _body: &str, _decision: Option<String>) -> Result<(), String> {
    Ok(())
}

/// The count of waiting decisions on the taskbar button: an overlay icon on Windows (drawn by the
/// frontend as `size` by `size` RGBA), the badge elsewhere. Zero clears it.
#[tauri::command]
pub fn set_badge(app: AppHandle, count: u32, rgba: Option<Vec<u8>>, size: u32) -> Result<(), String> {
    if crate::harness::on() {
        crate::harness::record_badge(count, rgba.as_ref().is_some_and(|px| px.len() == (size * size * 4) as usize));
    }
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
