use nitely_core::issues::{Ask, Project, TimelineEntry};
use nitely_core::{remote, Actor, Board, Config, Db, IssueRecord};
use serde::Serialize;
use std::time::Duration;

/// Bitbucket and Linear are read this often; git and agent state on every board refresh.
const SYNC_EVERY: Duration = Duration::from_secs(60);

/// The person at the app is the human: the only place that can approve or answer an ask.
const ME: Actor = Actor::Human;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn db() -> Result<Db, String> {
    Db::open_default().map_err(err)
}

/// The Now board. Git calls block, so this runs on Tauri's async pool, off the UI thread.
#[tauri::command]
async fn snapshot() -> Result<Board, String> {
    let config = Config::load().map_err(err)?;
    nitely_core::snapshot(&config, &db()?).map_err(err)
}

#[tauri::command]
async fn sync_now() -> Result<remote::SyncReport, String> {
    run_sync().map_err(err)
}

#[tauri::command]
async fn projects() -> Result<Vec<Project>, String> {
    db()?.projects().map_err(err)
}

#[tauri::command]
async fn create_project(key: String, name: String) -> Result<Project, String> {
    db()?.create_project(&key, &name).map_err(err)
}

#[tauri::command]
async fn issues(project: String, include_closed: bool) -> Result<Vec<IssueRecord>, String> {
    db()?.list_issues(Some(&project), include_closed).map_err(err)
}

#[derive(Serialize)]
struct IssueDetail {
    issue: IssueRecord,
    timeline: Vec<TimelineEntry>,
}

#[tauri::command]
async fn issue_detail(key: String) -> Result<IssueDetail, String> {
    let db = db()?;
    let issue = db.issue(&key).map_err(err)?.ok_or_else(|| format!("no issue {key}"))?;
    let timeline = db.timeline(&issue.key).map_err(err)?;
    Ok(IssueDetail { issue, timeline })
}

#[tauri::command]
async fn move_issue(key: String, state: String, evidence: Option<String>, reason: Option<String>) -> Result<IssueRecord, String> {
    db()?
        .move_issue(&key, &state, evidence.as_deref(), reason.as_deref(), &ME)
        .map_err(err)
}

#[tauri::command]
async fn answer_ask(id: i64, answer: String) -> Result<Ask, String> {
    db()?.answer_ask(id, &answer, &ME).map_err(err)
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
        .invoke_handler(tauri::generate_handler![
            snapshot,
            sync_now,
            projects,
            create_project,
            issues,
            issue_detail,
            move_issue,
            answer_ask
        ])
        .run(tauri::generate_context!())
        .expect("error while running Nitely");
}
