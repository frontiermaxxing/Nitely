use nitely_core::{remote, Board, Config, Db};
use std::time::Duration;

/// Bitbucket and Linear are read this often; git and agent state on every board refresh.
const SYNC_EVERY: Duration = Duration::from_secs(60);

/// The Now board. Git calls block, so this runs on Tauri's async pool, off the UI thread.
#[tauri::command]
async fn snapshot() -> Result<Board, String> {
    let config = Config::load().map_err(|e| format!("{e:#}"))?;
    let db = Db::open_default().map_err(|e| format!("{e:#}"))?;
    nitely_core::snapshot(&config, &db).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn sync_now() -> Result<remote::SyncReport, String> {
    run_sync().map_err(|e| format!("{e:#}"))
}

fn run_sync() -> anyhow::Result<remote::SyncReport> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    Ok(remote::sync(&config, &mut db))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    std::thread::spawn(|| loop {
        if let Err(e) = run_sync() {
            eprintln!("sync: {e:#}");
        }
        std::thread::sleep(SYNC_EVERY);
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![snapshot, sync_now])
        .run(tauri::generate_context!())
        .expect("error while running Nitely");
}
