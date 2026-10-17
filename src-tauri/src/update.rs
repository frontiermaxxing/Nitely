//! Auto-update (docs/releasing.md): the app checks its channel's manifest on start and every few
//! hours, downloads a newer version in the background, and installs it only when the person says
//! "Restart to update" or quits. Never while a land runs: it waits for every land to finish. The
//! installer then puts the new `nitely` CLI in place the way `nitely install` does, so agents in
//! the middle of a call keep the copy they started with. Dev builds never check.

use nitely_core::update::{self as up, BUILD_CHANNEL, VERSION};
use nitely_core::Config;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// How often the app asks its channel whether there is something newer.
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);
/// How often a waiting install looks again whether the lands have finished.
const LAND_POLL: Duration = Duration::from_secs(5);
/// Sent to the page whenever the update status changes.
pub const UPDATE_EVENT: &str = "nitely://update";

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateStatus {
    pub version: String,
    /// The channel this build came from ("dev" for a build from source).
    pub build_channel: String,
    /// The channel the app updates from.
    pub channel: String,
    pub channels: Vec<ChannelChoice>,
    /// Why the app does not check: a dev build, or no updater key in this build. None when it does.
    pub disabled: Option<String>,
    pub checking: bool,
    /// Unix seconds of the last finished check.
    pub checked_at: Option<i64>,
    /// A newer version: downloading, or ready to install.
    pub available: Option<Available>,
    /// Repos whose land the install is waiting for.
    pub waiting_for_lands: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelChoice {
    pub name: String,
    /// What to tell the person before they move to this channel (an older one than the build's).
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub version: String,
    pub notes_url: String,
    pub ready: bool,
}

#[derive(Default)]
pub struct UpdateState {
    status: Mutex<UpdateStatus>,
    /// The downloaded update, verified against the updater key, waiting to be installed.
    ready: Mutex<Option<(Update, Vec<u8>)>>,
    /// Moves on when the person picks another channel: a check or download started before then
    /// belongs to the old channel, and its result is dropped.
    generation: AtomicU64,
}

fn pubkey(app: &AppHandle) -> String {
    app.config().plugins.0.get("updater").and_then(|u| u.get("pubkey")).and_then(|k| k.as_str()).unwrap_or("").trim().to_string()
}

fn chosen_channel() -> String {
    Config::load().ok().and_then(|c| c.update_channel).filter(|c| up::CHANNELS.contains(&c.as_str())).unwrap_or_else(|| up::default_channel().to_string())
}

fn base_status(app: &AppHandle) -> UpdateStatus {
    let disabled = if !up::CHANNELS.contains(&BUILD_CHANNEL) {
        Some("a build from source: it never updates itself".to_string())
    } else if pubkey(app).is_empty() {
        Some("this build has no updater key, so it can't check updates; install a newer release by hand".to_string())
    } else {
        None
    };
    UpdateStatus {
        version: VERSION.into(),
        build_channel: BUILD_CHANNEL.into(),
        channel: chosen_channel(),
        channels: up::CHANNELS.iter().map(|c| ChannelChoice { name: c.to_string(), warning: up::older_channel_warning(BUILD_CHANNEL, c) }).collect(),
        disabled,
        ..Default::default()
    }
}

/// The status, after `change` updated it; tells the page.
fn change(app: &AppHandle, f: impl FnOnce(&mut UpdateStatus)) -> UpdateStatus {
    let state = app.state::<UpdateState>();
    let mut s = state.status.lock().unwrap();
    f(&mut s);
    let out = s.clone();
    drop(s);
    app.emit(UPDATE_EVENT, &out).ok();
    out
}

/// Starts the checks: one now, then every few hours.
pub fn start(app: &AppHandle) {
    // Land locks an update held when the installer ended the previous app.
    if let Ok(config) = Config::load() {
        let freed = up::release_update_reservations(&config);
        if freed > 0 {
            eprintln!("update: released {freed} land lock(s) the last update held");
        }
    }
    let first = base_status(app);
    *app.state::<UpdateState>().status.lock().unwrap() = first.clone();
    if first.disabled.is_some() {
        return;
    }
    // Keep the channel this copy started on: a beta build that the beta channel updates to a
    // stable release stays on beta, rather than following the new build's own channel.
    if let Ok(mut config) = Config::load() {
        if config.update_channel.is_none() {
            config.update_channel = Some(first.channel.clone());
            if let Err(e) = config.save() {
                eprintln!("update: keeping the channel: {e:#}");
            }
        }
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        tauri::async_runtime::block_on(check(&app));
        std::thread::sleep(CHECK_EVERY);
    });
}

/// Asks the channel for a newer version and downloads it. Does nothing while a check runs or an
/// update already waits to be installed.
pub async fn check(app: &AppHandle) -> UpdateStatus {
    {
        let state = app.state::<UpdateState>();
        let s = state.status.lock().unwrap();
        if s.disabled.is_some() || s.checking || s.available.as_ref().is_some_and(|a| a.ready) {
            return s.clone();
        }
    }
    let channel = chosen_channel();
    let generation = app.state::<UpdateState>().generation.load(Ordering::SeqCst);
    let current = |app: &AppHandle| app.state::<UpdateState>().generation.load(Ordering::SeqCst) == generation;
    change(app, |s| {
        s.checking = true;
        s.channel = channel.clone();
        s.error = None;
    });
    let result = async {
        let url = up::manifest_url(&channel).parse().map_err(|e| format!("{e}"))?;
        let updater = app.updater_builder().pubkey(pubkey(app)).endpoints(vec![url]).map_err(|e| e.to_string())?.build().map_err(|e| e.to_string())?;
        let Some(update) = updater.check().await.map_err(|e| e.to_string())? else { return Ok(None) };
        let version = update.version.clone();
        if current(app) {
            change(app, |s| s.available = Some(Available { version: version.clone(), notes_url: up::release_url(&channel, &version), ready: false }));
        }
        let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(Some((update, bytes)))
    }
    .await;
    // The person picked another channel meanwhile: this result is the old channel's.
    if !current(app) {
        return app.state::<UpdateState>().status.lock().unwrap().clone();
    }
    let now = nitely_core::db::now();
    match result {
        Ok(Some(pair)) => {
            *app.state::<UpdateState>().ready.lock().unwrap() = Some(pair);
            change(app, |s| {
                s.checking = false;
                s.checked_at = Some(now);
                if let Some(a) = &mut s.available {
                    a.ready = true;
                }
            })
        }
        Ok(None) => change(app, |s| {
            s.checking = false;
            s.checked_at = Some(now);
            s.available = None;
        }),
        Err(e) => change(app, |s| {
            s.checking = false;
            s.checked_at = Some(now);
            s.available = None;
            s.error = Some(format!("checking for updates: {e}"));
        }),
    }
}

/// Installs the downloaded update once no land is running; the app closes for the installer,
/// which starts it again. Waits (and says what for) while lands run. It holds every land lock
/// while it installs, so no land starts in the moment the app closes.
pub fn install_when_free(app: &AppHandle) -> UpdateStatus {
    if app.state::<UpdateState>().ready.lock().unwrap().is_none() {
        return change(app, |s| s.error = Some("no update is ready to install".into()));
    }
    let app = app.clone();
    let status = change(&app, |s| s.waiting_for_lands = Config::load().map(|c| up::lands_running(&c)).unwrap_or_default());
    std::thread::spawn(move || loop {
        let config = Config::load().unwrap_or_default();
        match up::reserve_lands(&config) {
            Ok(held) => {
                install_now(&app);
                // Still here: the install did not start, so lands may run again.
                drop(held);
                return;
            }
            Err(running) => {
                change(&app, |s| s.waiting_for_lands = running);
                std::thread::sleep(LAND_POLL);
            }
        }
    });
    status
}

/// Installs a downloaded update if there is one and no land runs; for quitting. Returns whether it
/// started the installer (which ends this process).
pub fn install_on_quit(app: &AppHandle) -> bool {
    if app.state::<UpdateState>().ready.lock().unwrap().is_none() {
        return false;
    }
    let Ok(held) = up::reserve_lands(&Config::load().unwrap_or_default()) else { return false };
    let started = install_now(app);
    drop(held);
    started
}

fn install_now(app: &AppHandle) -> bool {
    let Some((update, bytes)) = app.state::<UpdateState>().ready.lock().unwrap().take() else { return false };
    match update.install(bytes) {
        Ok(()) => true,
        Err(e) => {
            change(app, |s| {
                s.waiting_for_lands.clear();
                s.available = None;
                s.error = Some(format!("installing the update: {e}"));
            });
            false
        }
    }
}

#[tauri::command]
pub async fn update_status(app: AppHandle) -> UpdateStatus {
    app.state::<UpdateState>().status.lock().unwrap().clone()
}

#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> UpdateStatus {
    check(&app).await
}

/// Follows another channel from now on and checks it at once. A downloaded update from the old
/// channel is dropped.
#[tauri::command]
pub async fn set_update_channel(app: AppHandle, channel: String) -> Result<UpdateStatus, String> {
    if !up::CHANNELS.contains(&channel.as_str()) {
        return Err(format!("{channel} is not a channel; choose stable, beta or nightly"));
    }
    let mut config = Config::load().map_err(|e| format!("{e:#}"))?;
    config.update_channel = Some(channel.clone());
    config.save().map_err(|e| format!("{e:#}"))?;
    let state = app.state::<UpdateState>();
    state.generation.fetch_add(1, Ordering::SeqCst);
    state.ready.lock().unwrap().take();
    change(&app, |s| {
        s.channel = channel;
        s.available = None;
        s.error = None;
        // A check of the old channel may still run; it drops its result (generation), and the
        // new channel's check starts now.
        s.checking = false;
    });
    Ok(check(&app).await)
}

#[tauri::command]
pub async fn restart_to_update(app: AppHandle) -> UpdateStatus {
    install_when_free(&app)
}
