use nitely_core::issues::{Ask, Project, TimelineEntry};
use nitely_core::db::PrRecord;
use nitely_core::{remote, Actor, Board, Config, Db, IssueRecord};
use serde::Serialize;
use std::time::Duration;

/// Bitbucket and Linear are read this often; git and agent state on every board refresh.
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// How often the watcher checks for changes; each check is a pragma and a few file stats.
const WATCH_EVERY: Duration = Duration::from_millis(500);
/// Sent to the frontend when anything it shows may have changed.
const CHANGED_EVENT: &str = "nitely://changed";

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
    prs: Vec<PrRecord>,
    reviews: Vec<nitely_core::submit::ReviewRecord>,
}

#[tauri::command]
async fn issue_detail(key: String) -> Result<IssueDetail, String> {
    let db = db()?;
    let issue = db.issue(&key).map_err(err)?.ok_or_else(|| format!("no issue {key}"))?;
    let timeline = db.timeline(&issue.key).map_err(err)?;
    let prs = db.prs_for_issue(&issue.key).map_err(err)?;
    let reviews = db.reviews(&issue.key).map_err(err)?;
    Ok(IssueDetail { issue, timeline, prs, reviews })
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

/// What adding a repo would do: its project, playbook, and every agent config change.
#[tauri::command]
async fn connect_plan(path: String, key: Option<String>, name: Option<String>) -> Result<nitely_core::connect::ConnectPlan, String> {
    let config = Config::load().map_err(err)?;
    nitely_core::connect::plan(&db()?, &config, &path, key.as_deref(), name.as_deref()).map_err(err)
}

/// Adds the repo. The plan is recomputed here rather than taken from the page, so what is written
/// is what Nitely decides now.
#[tauri::command]
async fn connect_apply(path: String, key: Option<String>, name: Option<String>) -> Result<nitely_core::connect::Applied, String> {
    let mut config = Config::load().map_err(err)?;
    let db = db()?;
    let plan = nitely_core::connect::plan(&db, &config, &path, key.as_deref(), name.as_deref()).map_err(err)?;
    nitely_core::connect::apply(&db, &mut config, &plan).map_err(err)
}

/// What it takes to bring the agents' Nitely setup up to this version; empty when nothing watches a repo.
#[tauri::command]
async fn agent_setup() -> Result<Vec<nitely_core::connect::AgentPlan>, String> {
    let config = Config::load().map_err(err)?;
    Ok(nitely_core::connect::agent_setup(&config))
}

#[derive(Serialize)]
struct AgentsUpdated {
    done: Vec<String>,
    failed: Vec<String>,
}

/// Updates the agents' setup, recomputed here like connect_apply.
#[tauri::command]
async fn agent_setup_apply() -> Result<AgentsUpdated, String> {
    let config = Config::load().map_err(err)?;
    let agents = nitely_core::connect::agent_setup(&config);
    let (done, failed) = nitely_core::connect::apply_agents(&agents);
    Ok(AgentsUpdated { done, failed })
}

fn run_sync() -> anyhow::Result<remote::SyncReport> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    Ok(remote::sync(&config, &mut db))
}

/// Pushes a change event to the frontend whenever the database or a watched repo changes.
fn watch(app: tauri::AppHandle) {
    use tauri::Emitter;
    let mut watcher = loop {
        match nitely_core::watch::Watcher::new() {
            Ok(w) => break w,
            Err(e) => {
                eprintln!("watch: {e:#}");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };
    loop {
        std::thread::sleep(WATCH_EVERY);
        match watcher.changed() {
            Ok(true) => {
                app.emit(CHANGED_EVENT, ()).ok();
            }
            Ok(false) => {}
            Err(e) => eprintln!("watch: {e:#}"),
        }
    }
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
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || watch(handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            sync_now,
            projects,
            create_project,
            issues,
            issue_detail,
            move_issue,
            answer_ask,
            connect_plan,
            connect_apply,
            agent_setup,
            agent_setup_apply
        ])
        .run(tauri::generate_context!())
        .expect("error while running Nitely");
}
