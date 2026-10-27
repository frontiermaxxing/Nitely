mod agent_icons;
mod harness;
mod notify;
mod update;

mod phone;
use nitely_core::issues::{Ask, Project, Relations};
use nitely_core::{remote, rules, Actor, Board, Config, Db, IssueRecord};
use nitely_crew::crew::CrewDb;
use serde::Serialize;
use std::time::Duration;

/// Bitbucket and Linear are read this often; git and agent state on every board refresh.
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// How often the app lands approved changes whose playbook lands by Nitely.
const LAND_APPROVED_EVERY: Duration = Duration::from_secs(15);
/// How often the watcher checks for changes; each check is a pragma and a few file stats.
const WATCH_EVERY: Duration = Duration::from_millis(500);
/// How often the app checks that every project with a crew has a supervisor running.
const KEEP_CREWS_EVERY: Duration = Duration::from_secs(20);
/// How often the app checks whether the Leaderboard's daily send is due.
const LEADERBOARD_EVERY: Duration = Duration::from_secs(3600);
/// A supervisor the app started gets this long to show its heartbeat before it is started again.
const CREW_START_GRACE: Duration = Duration::from_secs(90);
/// Sent to the frontend when anything it shows may have changed.
const CHANGED_EVENT: &str = "nitely://changed";

/// Sent to the frontend with an issue key when a `nitely://issue/KEY` link asks to open it.
const OPEN_ISSUE_EVENT: &str = "nitely://open-issue";

/// The issue a link asked for, kept until the frontend asks: a link that starts the app arrives
/// before the page can listen for events.
#[derive(Default)]
struct PendingLink(std::sync::Mutex<Option<String>>);

/// The person at the app is the human: the only place that can approve or answer an ask.
const ME: Actor = Actor::Human;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn db() -> Result<Db, String> {
    Db::open_default().map_err(err)
}

/// Git state shared by every snapshot until a repo changes, so a database change (an agent's
/// report, a move) refreshes the board without running git.
static GIT: nitely_core::snapshot::GitCache = nitely_core::snapshot::GitCache::new();

/// The Now board. Git calls block, so this runs on Tauri's async pool, off the UI thread.
#[tauri::command]
async fn snapshot() -> Result<Board, String> {
    let config = Config::load().map_err(err)?;
    nitely_core::snapshot::snapshot_cached(&config, &db()?, &GIT).map_err(err)
}

/// The Pulse home view in one read. `since` is when the person last looked and `day_start` the
/// start of their local day, both in seconds.
#[tauri::command]
async fn pulse(since: i64, day_start: i64) -> Result<nitely_crew::Pulse, String> {
    tauri::async_runtime::spawn_blocking(move || nitely_crew::pulse(&db()?, since, day_start).map_err(err)).await.map_err(|e| e.to_string())?
}

/// What happened since `since`, per goal, and every place the person was needed (NIT-247).
#[tauri::command]
async fn goal_report(project: Option<String>, since: i64) -> Result<nitely_core::goal_report::Report, String> {
    db()?.goal_report(project.as_deref(), since).map_err(err)
}

/// What every agent session did over the last `hours`, for the Activity view.
#[tauri::command]
async fn activity(hours: i64) -> Result<nitely_core::activity::Activity, String> {
    let config = Config::load().map_err(err)?;
    let to = nitely_core::db::now();
    nitely_core::activity::activity(&config, &db()?, to - hours.clamp(1, 24 * 7) * 3600, to).map_err(err)
}

/// The issue a `nitely://` link asked to open and nobody has shown yet; taking it clears it.
#[tauri::command]
fn opened_link(pending: tauri::State<'_, PendingLink>) -> Option<String> {
    pending.0.lock().ok()?.take()
}

/// Opens what an issue link names (`nitely://issue/KEY`, or the loopback web form): keeps the key
/// for the page, tells it at once if it is listening, and brings the window forward. Anything else
/// is ignored.
fn open_link(app: &tauri::AppHandle, url: &str) {
    let Some(target) = nitely_core::issues::target_from_link(url) else { return };
    open_target(app, target);
}

/// Shows what a link names: the page is told the issue's key, or `ask:<n>` for an ask (NIT-546).
fn open_target(app: &tauri::AppHandle, target: nitely_core::links::Target) {
    use tauri::{Emitter, Manager};
    let token = target.token();
    if let Some(pending) = app.try_state::<PendingLink>() {
        if let Ok(mut slot) = pending.0.lock() {
            *slot = Some(token.clone());
        }
    }
    app.emit(OPEN_ISSUE_EVENT, &token).ok();
    focus_main(app);
}

/// Who answers issue links on 127.0.0.1:7423 (NIT-229). When the listener that starts at login is
/// registered, it owns the port and hands each link here through nitely://; the app only makes sure
/// it runs. When it is not, the app answers them itself, and lets the port go as soon as the
/// listener is registered. A registration with no nitely here to start it leaves them to the app.
fn answer_links(app: tauri::AppHandle) {
    use nitely_core::{link_service, links};
    fn hand_over() -> bool {
        link_service::registered() && nitely_core::connect::nitely_command_path().is_some()
    }
    let mut told = false;
    loop {
        if hand_over() {
            if links::probe() != Some(links::Role::Service) {
                if let Some(n) = nitely_core::connect::nitely_command_path() {
                    link_service::start(&n).ok();
                }
            }
        } else {
            match links::bind() {
                Ok(listener) => {
                    told = false;
                    let open = app.clone();
                    links::serve(listener, links::Role::App, move |target| open_target(&open, target), || !hand_over());
                    continue;
                }
                Err(e) if !told && links::probe().is_none() => {
                    eprintln!("issue links: {e:#}");
                    told = true;
                }
                Err(_) => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

/// The listener for issue links at login: whether it is on, and who answers links now (Settings).
#[tauri::command]
async fn links_service() -> Result<nitely_core::link_service::Status, String> {
    let nitely = nitely_core::connect::nitely_command_path().ok_or("the nitely command is neither on PATH nor beside this app")?;
    Ok(nitely_core::link_service::status(&Config::load().map_err(err)?, &nitely))
}

/// Turns the listener for issue links at login on or off (Settings).
#[tauri::command]
async fn set_links_service(on: bool) -> Result<nitely_core::link_service::Status, String> {
    if harness::on() {
        return Err("the test harness never registers anything at login".into());
    }
    let nitely = nitely_core::connect::nitely_command_path().ok_or("the nitely command is neither on PATH nor beside this app")?;
    if on {
        nitely_core::link_service::install(&nitely).map_err(err)
    } else {
        nitely_core::link_service::uninstall(&nitely).map_err(err)
    }
}

/// A debug build is a console program (main.rs), so each link it was registered for started a
/// second instance in a console window of its own, only to hand the link over and exit (NIT-259).
/// It registers nitely:// through conhost's headless mode instead, as the links listener runs at
/// login: the second instance gets no window, and the first keeps `npm run tauri dev`'s console.
/// Release builds are GUI programs and keep the plugin's registration.
#[cfg(any(windows, target_os = "linux"))]
fn quiet_dev_links() {
    #[cfg(all(windows, debug_assertions))]
    {
        use std::os::windows::process::CommandExt;
        let Ok(exe) = std::env::current_exe() else { return };
        let command = format!("conhost.exe --headless \"{}\" \"%1\"", exe.display());
        let key = r"HKCU\Software\Classes\nitely\shell\open\command";
        let done = std::process::Command::new("reg")
            .args(["add", key, "/ve", "/t", "REG_SZ", "/d", &command, "/f"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output();
        if !done.is_ok_and(|o| o.status.success()) {
            eprintln!("deep link: could not register nitely:// without a console window");
        }
    }
}

pub(crate) fn focus_main(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        w.unminimize().ok();
        w.show().ok();
        w.set_focus().ok();
    }
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
async fn archived_projects() -> Result<Vec<Project>, String> {
    Ok(db()?.all_projects().map_err(err)?.into_iter().filter(|p| p.archived).collect())
}

#[tauri::command]
async fn set_project_archived(key: String, archived: bool) -> Result<Project, String> {
    db()?.set_project_archived(&key, archived, &ME).map_err(err)
}

#[tauri::command]
async fn create_project(key: String, name: String) -> Result<Project, String> {
    db()?.create_project(&key, &name).map_err(err)
}

#[tauri::command]
async fn rename_project(key: String, name: String) -> Result<Project, String> {
    db()?.rename_project(&key, &name).map_err(err)
}

#[tauri::command]
async fn rekey_project(key: String, new_key: String) -> Result<nitely_core::projects::Rekeyed, String> {
    nitely_core::projects::rekey(&mut db()?, &key, &new_key).map_err(err)
}

/// The words, paths and patterns submit and land keep out of each project's repos. Only the app
/// reads or writes them; no MCP tool or CLI command does (crates/nitely-core/src/private_terms.rs).
#[tauri::command]
async fn private_terms() -> Result<nitely_core::private_terms::PrivateTerms, String> {
    nitely_core::private_terms::load().map_err(err)
}

#[tauri::command]
async fn set_private_terms(terms: nitely_core::private_terms::PrivateTerms) -> Result<nitely_core::private_terms::PrivateTerms, String> {
    nitely_core::private_terms::save(&terms).map_err(err)
}

/// `confirm` must repeat the key when the project still has issues: they are deleted with it.
#[tauri::command]
async fn remove_project(key: String, confirm: Option<String>) -> Result<nitely_core::projects::Removed, String> {
    let with_issues = confirm.is_some_and(|c| c.trim().eq_ignore_ascii_case(&key));
    nitely_core::projects::remove(&mut db()?, &key, with_issues).map_err(err)
}

/// The files each issue of `project` in flight holds: its branch's changes and the paths its body
/// names (overlap.rs), for the branch map's hover.
#[tauri::command]
async fn held_files(project: String) -> Result<Vec<nitely_core::overlap::Held>, String> {
    let config = Config::load().map_err(err)?;
    nitely_core::overlap::held(&config, &db()?, &project).map_err(err)
}

/// Approves the goal plan an issue carries, with the person's edits: creates the goal and files its issues.
#[tauri::command]
async fn approve_goal_plan(key: String, edit: Option<nitely_core::goal_plans::PlanEdit>) -> Result<nitely_core::goal_plans::PlanApproved, String> {
    db()?.approve_goal_plan(&key, &edit.unwrap_or_default(), &ME).map_err(err)
}

/// Files the issue an agent picks up to draft a goal from the person's sentence (the palette).
#[tauri::command]
async fn start_goal(project: String, sentence: String) -> Result<IssueRecord, String> {
    db()?.start_goal(&project, &sentence, &ME).map_err(err)
}

/// Every goal of a project, closed ones too: what the issue panel's goal picker offers.
#[tauri::command]
async fn goal_choices(project: String) -> Result<Vec<nitely_core::goals::Goal>, String> {
    db()?.goals(&project, true).map_err(err)
}

/// Puts an issue in a goal of its project, or takes it out of its goal (`goal` None).
#[tauri::command]
async fn set_issue_goal(key: String, goal: Option<i64>) -> Result<IssueRecord, String> {
    let db = db()?;
    let key = db.current_key(&key).map_err(err)?;
    db.set_issue_goal(&key, goal, &ME).map_err(err)
}

/// Sets the issue's declared review level, or clears it (`level` empty). Only the person uses this.
#[tauri::command]
async fn set_review_level(key: String, level: Option<String>) -> Result<IssueRecord, String> {
    let mut db = db()?;
    let key = db.current_key(&key).map_err(err)?;
    let level = level.as_deref().map(str::trim).filter(|s| !s.is_empty());
    db.set_review_level(&key, level, None, &ME).map_err(err)
}

/// The person's order of a project's active goals (`ids` first): what Crew works first.
#[tauri::command]
async fn goal_order(project: String, ids: Vec<i64>) -> Result<Vec<nitely_core::goals::Goal>, String> {
    db()?.order_goals(&project, &ids, &ME).map_err(err)
}

/// The project's current focus, shared by Crew and agents choosing their next task.
#[tauri::command]
async fn project_focus(project: String) -> Result<nitely_core::focus::ProjectFocus, String> {
    db()?.project_focus(&project).map_err(err)
}

#[tauri::command]
async fn project_review_mode(project: String) -> Result<nitely_core::project_modes::ProjectReviewMode, String> {
    db()?.project_review_mode(&project).map_err(err)
}

#[tauri::command]
async fn project_review_mode_effective(project: String) -> Result<Vec<nitely_core::project_modes::EffectiveRepository>, String> {
    nitely_core::project_modes::effective_repositories(&db()?, &Config::load_read_only().map_err(err)?, &project).map_err(err)
}

/// "How a change lands": each repo's pipeline, every value with its source (NIT-492).
#[tauri::command]
async fn lands_pipeline(project: String) -> Result<Vec<nitely_core::lands_pipeline::Pipeline>, String> {
    nitely_core::lands_pipeline::for_project(&db()?, &Config::load_read_only().map_err(err)?, &project).map_err(err)
}

/// What a change to one path would run, for the strip's dry run.
#[tauri::command]
async fn lands_dry_run(project: String, repo: String, path: String) -> Result<nitely_core::lands_pipeline::DryRun, String> {
    nitely_core::lands_pipeline::dry_run_for(&db()?, &Config::load_read_only().map_err(err)?, &project, &repo, &path).map_err(err)
}

#[tauri::command]
async fn set_project_review_mode(project: String, mode: nitely_core::project_modes::Mode, overrides: nitely_core::project_modes::Options, expected_revision: i64) -> Result<nitely_core::project_modes::ProjectReviewMode, String> {
    db()?.set_project_review_mode(&project, mode, overrides, expected_revision, &ME).map_err(err)
}

#[tauri::command]
async fn periodic_review_status(project: String) -> Result<Vec<nitely_core::periodic_review::RepositoryStatus>, String> {
    nitely_core::periodic_review::status(&db()?, &project).map_err(err)
}

#[tauri::command]
async fn retry_periodic_review(project: String, repo: String, id: i64) -> Result<(), String> {
    nitely_core::periodic_review::retry(&db()?, &project, &repo, id).map_err(err)
}

/// Only the app exposes this human setting; CLI and MCP may read it but cannot enable it.
/// Audit confirmation only tightens verification; it never approves a patch.
#[tauri::command]
async fn confirm_verification_escape(id: i64, rule: String, evidence: String) -> Result<nitely_core::verification_audit::Report, String> {
    nitely_core::verification_audit::confirm_report(&mut db()?, id, &rule, &evidence, &ME).map_err(err)
}

#[tauri::command]
async fn retry_verification_audit(id: i64) -> Result<nitely_core::verification_audit::Report, String> {
    nitely_core::verification_audit::retry_report(&db()?, id, &ME).map_err(err)
}

/// Only the person changes the focus; agents read it through get_project and next_task.
#[tauri::command]
async fn set_project_focus(project: String, goal: Option<i64>, allow_urgent: bool) -> Result<nitely_core::focus::ProjectFocus, String> {
    db()?.set_project_focus(&project, goal, allow_urgent, &ME).map_err(err)
}

/// How many crew workers may work on a goal at once; 0 leaves it to the playbook.
#[tauri::command]
async fn goal_workers(id: i64, workers: i64) -> Result<nitely_core::goals::Goal, String> {
    let change = nitely_core::goals::GoalChange { workers: Some(workers), ..Default::default() };
    db()?.save_goal("", Some(id), &change, &ME).map_err(err)
}

/// The goal rows: every active goal with its issues, of one project or all of them; with
/// `include_closed`, completed and dropped goals too ("Show completed", NIT-611).
#[tauri::command]
async fn goals(project: Option<String>, include_closed: Option<bool>) -> Result<Vec<nitely_core::goals::GoalDetail>, String> {
    tauri::async_runtime::spawn_blocking(move || db()?.goal_rows(project.as_deref(), include_closed.unwrap_or(false)).map_err(err)).await.map_err(|e| e.to_string())?
}

/// The person reopens a completed or dropped goal, or closes one: one click (NIT-611).
#[tauri::command]
async fn goal_state(id: i64, state: String) -> Result<nitely_core::goals::Goal, String> {
    let change = nitely_core::goals::GoalChange { state: Some(state), ..Default::default() };
    db()?.save_goal("", Some(id), &change, &ME).map_err(err)
}

#[tauri::command]
async fn project_brief(project: String) -> Result<nitely_core::brief::ProjectBrief, String> {
    db()?.project_brief(&project).map_err(err)
}

/// Saves the human's edit of a project's brief; refused when an agent wrote it since `revision`.
#[tauri::command]
async fn set_project_brief(project: String, brief: String, revision: i64) -> Result<nitely_core::brief::ProjectBrief, String> {
    db()?.set_project_brief(&project, &brief, revision, &ME).map_err(err)
}

/// The project's crew (docs/crew.md): running workers, cap, today's count, paused, and what the
/// supervisor last said. None when the project has no crew.
#[tauri::command]
async fn crew_status(project: String) -> Result<Option<nitely_crew::crew::CrewStatus>, String> {
    let config = Config::load().map_err(err)?;
    nitely_crew::crew::project_status(&db()?, &config, &project, &nitely_crew::crew::Moment::now()).map_err(err)
}

/// Pauses the project's crew (running workers finish, none start) or resumes it.
#[tauri::command]
async fn crew_pause(project: String, paused: bool) -> Result<Option<nitely_crew::crew::CrewStatus>, String> {
    let db = db()?;
    db.crew_pause(&project, paused, &ME).map_err(err)?;
    nitely_crew::crew::project_status(&db, &Config::load().map_err(err)?, &project, &nitely_crew::crew::Moment::now()).map_err(err)
}

/// The crew settings of each repo in a project (the Crew section): workers, agents, limits.
#[tauri::command]
async fn crew_settings(project: String) -> Result<Vec<nitely_crew::crew_settings::CrewView>, String> {
    nitely_crew::crew_settings::crew_for_project(&Config::load().map_err(err)?, &db()?, &project).map_err(err)
}

/// Saves a repo's crew settings. Only the app saves them: a person, never an agent.
#[tauri::command]
async fn crew_save(repo: String, edit: nitely_crew::crew_settings::CrewEdit) -> Result<nitely_crew::crew_settings::CrewSaved, String> {
    nitely_crew::crew_settings::save_crew(&Config::load().map_err(err)?, &db()?, &repo, &edit).map_err(err)
}

#[tauri::command]
async fn issues(project: String, include_closed: bool) -> Result<Vec<IssueRecord>, String> {
    db()?.list_issues(Some(&project), include_closed).map_err(err)
}

/// Tokens an issue's agents used (crates/nitely-crew/src/usage.rs): counts only.
#[tauri::command]
async fn issue_spend(key: String) -> Result<nitely_crew::usage::IssueSpend, String> {
    nitely_crew::usage::spend_of_issue(&Config::load().map_err(err)?, &db()?, &key).map_err(err)
}

/// The Leaderboard's numbers over `projects` (empty: every project), read from this machine only.
#[tauri::command]
async fn leaderboard_stats(projects: Vec<String>) -> Result<nitely_crew::leaderboard::Leaderboard, String> {
    tauri::async_runtime::spawn_blocking(move || nitely_crew::leaderboard::stats_now(&Config::load().map_err(err)?, &db()?, &projects).map_err(err)).await.map_err(|e| e.to_string())?
}

/// The weekly report exactly as it would be shared, for the opt-in page's preview. Sends nothing.
#[tauri::command]
async fn leaderboard_report(projects: Vec<String>, profile: nitely_crew::leaderboard_report::Profile) -> Result<nitely_crew::leaderboard_report::Report, String> {
    nitely_crew::leaderboard_report::report_now(&Config::load().map_err(err)?, &db()?, &projects, &profile).map_err(err)
}

/// Whether this install shares on the Leaderboard, its id and the last send. No network.
#[tauri::command]
async fn leaderboard_status() -> Result<nitely_crew::leaderboard_send::Status, String> {
    nitely_crew::leaderboard_send::Sharing::default_dir().status(nitely_core::db::now()).map_err(err)
}

/// Joins, or saves a changed profile or project choice. Sends nothing.
#[tauri::command]
async fn leaderboard_join(profile: nitely_crew::leaderboard_report::Profile, projects: Vec<String>) -> Result<nitely_crew::leaderboard_send::Status, String> {
    nitely_crew::leaderboard_send::Sharing::default_dir().join(&profile, &projects, None).map_err(err)
}

/// Sends this week's report now, by the person's own hand.
#[tauri::command]
async fn leaderboard_send() -> Result<nitely_crew::leaderboard_report::Report, String> {
    nitely_crew::leaderboard_send::Sharing::default_dir().send(&Config::load().map_err(err)?, &db()?).map_err(err)
}

/// The public board for `period`, fetched only when the person opens it.
#[tauri::command]
async fn leaderboard_board(period: String) -> Result<serde_json::Value, String> {
    nitely_crew::leaderboard_send::Sharing::default_dir().board(&period).map_err(err)
}

/// Deletes what this install sent; `leave` also stops sharing and forgets the key.
#[tauri::command]
async fn leaderboard_delete(leave: bool) -> Result<u64, String> {
    let sharing = nitely_crew::leaderboard_send::Sharing::default_dir();
    if leave { sharing.leave("app") } else { sharing.delete("app") }.map_err(err)
}

/// A project's tokens per day and by agent and model, and what landed, over `days` days.
#[tauri::command]
async fn project_spend(project: String, day_start: i64, days: usize) -> Result<nitely_crew::usage::ProjectSpend, String> {
    nitely_crew::usage::spend_of_project(&Config::load().map_err(err)?, &db()?, &project, day_start, days).map_err(err)
}

#[tauri::command]
async fn issue_detail(key: String) -> Result<nitely_core::issues::IssueDetail, String> {
    db()?.issue_detail(&key).map_err(err)?.ok_or_else(|| format!("no issue {key}"))
}

/// One ask as its link shows it (NIT-546): its question, options, answer and issue, answered or not.
#[tauri::command]
async fn ask_detail(id: i64) -> Result<nitely_core::issues::Ask, String> {
    db()?.ask(id).map_err(err)?.ok_or_else(|| format!("no ask {id}"))
}

/// Adds (`blocked` true) or removes a blocker of an issue.
#[tauri::command]
async fn set_blocker(key: String, blocker: String, blocked: bool) -> Result<Relations, String> {
    let mut db = db()?;
    // A panel opened from an old link (before its project changed key) still names the old key.
    let key = db.current_key(&key).map_err(err)?;
    if blocked {
        db.add_blocker(&key, &blocker, &ME).map_err(err)?;
    } else {
        db.remove_blocker(&key, &blocker, &ME).map_err(err)?;
    }
    db.relations(&key).map_err(err)
}

#[tauri::command]
async fn move_issue(key: String, state: String, evidence: Option<String>, reason: Option<String>) -> Result<IssueRecord, String> {
    let mut db = db()?;
    let key = db.current_key(&key).map_err(err)?;
    let approving = nitely_core::issues::parse_state(&state).map_err(err)?.0 == "Approved";
    // The key first: an approval the app cannot sign would not let anything land.
    let secret = if approving { Some(approval_key(&db)?) } else { None };
    db.check_open_asks(&key, &state).map_err(err)?;
    // Recorded as landed when it reached the integration branch another way (NIT-361).
    if let Ok(("Done", _)) = nitely_core::issues::parse_state(&state) {
        let project = db.issue(&key).map_err(err)?.and_then(|i| i.project);
        if let Ok(root) = nitely_core::playbook::repo_for_issue(&nitely_core::Config::load().unwrap_or_default(), project.as_deref(), ".") {
            nitely_core::landed_elsewhere::record(&mut db, &root, &key, evidence.as_deref(), &ME).map_err(err)?;
        }
    }
    let moved = db.move_issue(&key, &state, evidence.as_deref(), reason.as_deref(), &ME).map_err(err)?;
    // Approving a playbook proposal applies it and closes its issue at once: nothing to land.
    if let Some(secret) = secret.filter(|_| moved.state == "Approved") {
        db.sign_approval(&key, &secret).map_err(err)?;
    }
    Ok(moved)
}

/// The app's approval signing secret, kept in the OS keychain and made the first time, with its
/// public half beside it for `land` to check with (docs/approvals.md).
static NEW_KEY_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the app says when it made this data folder its own approval key: the text and when, so the
/// person can dismiss that one notice. Kept across restarts until then.
#[derive(serde::Serialize)]
struct ApprovalNotice {
    text: &'static str,
    at: i64,
}

#[tauri::command]
fn approval_notice() -> Result<Option<ApprovalNotice>, String> {
    let at = db()?.new_folder_key_at().map_err(err)?;
    Ok(at.map(|at| ApprovalNotice { text: nitely_core::approval::NEW_FOLDER_KEY_NOTICE, at }))
}

#[tauri::command]
fn dismiss_approval_notice() -> Result<(), String> {
    db()?.dismiss_new_folder_key().map_err(err)
}

fn approval_key(db: &Db) -> Result<[u8; 32], String> {
    let secret = nitely_core::approval::keychain_secret().map_err(err)?;
    // Made just now for a profile that is not the person's own folder: said once, and kept.
    if nitely_core::approval::new_folder_key_notice().is_some() && !NEW_KEY_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        db.log_new_folder_key().map_err(err)?;
    }
    // A public half someone changed is put back and logged.
    if nitely_core::approval::publish_public_key(&secret).map_err(err)? {
        db.log_approval_key_restored().map_err(err)?;
    }
    Ok(secret)
}

/// Makes an issue the human's own work (`human`) or hands it back to agents (`agent`).
#[tauri::command]
async fn set_owner(key: String, owner: String) -> Result<IssueRecord, String> {
    let mut db = db()?;
    let key = db.current_key(&key).map_err(err)?;
    db.set_owner(&key, &owner, &ME).map_err(err)
}

/// The person dismisses a notice in the Inbox (NIT-570): nothing waited on it.
#[tauri::command]
async fn dismiss_notice(id: i64) -> Result<(), String> {
    db()?.dismiss_notice(id).map_err(err)
}

#[tauri::command]
async fn answer_ask(id: i64, answer: String) -> Result<Ask, String> {
    db()?.answer_ask(id, &answer, &ME).map_err(err)
}

/// The person accepts the review finding an agent asked about (NIT-191), signed with the app's key.
#[tauri::command]
async fn accept_finding(id: i64, reason: String) -> Result<nitely_core::accept::Decided, String> {
    let mut db = db()?;
    let secret = approval_key(&db)?;
    db.accept_finding(id, &reason, &secret).map_err(err)
}

/// The person sends the request back: the finding stays a defect to fix.
#[tauri::command]
async fn decline_finding(id: i64, why: String) -> Result<nitely_core::accept::Decided, String> {
    db()?.decline_finding(id, &why, "").map_err(err)
}

/// What adding a repo would do: its project, playbook, and every agent config change.
#[tauri::command]
async fn connect_plan(path: String, key: Option<String>, name: Option<String>) -> Result<nitely_core::connect::ConnectPlan, String> {
    let config = Config::load().map_err(err)?;
    nitely_core::connect::plan(&db()?, &config, &path, key.as_deref(), name.as_deref()).map_err(err)
}

/// Makes a folder that is in no repo a git repo on main, so Add a project can go on (NIT-652).
#[tauri::command]
async fn connect_init(path: String) -> Result<String, String> {
    nitely_core::connect::init_repo(&path).map_err(err)
}

/// What a repo with no commits yet would put in its first commit (NIT-685).
#[tauri::command]
async fn first_commit_preview(path: String) -> Result<nitely_core::first_commit::Preview, String> {
    nitely_core::first_commit::preview(std::path::Path::new(&path)).map_err(err)
}

/// Makes that first commit: the .gitignore lines the person kept, the flagged paths they chose to
/// commit after all, and a
/// git identity for this repo when git has none. Never pushed.
#[tauri::command]
async fn first_commit(path: String, gitignore: Option<Vec<String>>, include: Vec<String>, name: Option<String>, email: Option<String>) -> Result<String, String> {
    let identity = name.as_deref().zip(email.as_deref());
    nitely_core::first_commit::make(std::path::Path::new(&path), gitignore.as_deref(), &include, identity).map_err(err)
}

/// Adds the repo. The plan is recomputed here rather than taken from the page, so what is written
/// is what Nitely decides now.
#[tauri::command]
async fn connect_apply(path: String, key: Option<String>, name: Option<String>) -> Result<nitely_core::connect::Applied, String> {
    let mut config = Config::load().map_err(err)?;
    let mut db = db()?;
    let plan = nitely_core::connect::plan(&db, &config, &path, key.as_deref(), name.as_deref()).map_err(err)?;
    nitely_core::connect::apply(&mut db, &mut config, &plan, &ME).map_err(err)
}

/// What it takes to bring the agents' Nitely setup up to this version; empty when nothing watches a repo.
#[derive(Serialize)]
struct NetworkInfo {
    tokens: Vec<nitely_core::relay::Token>,
    urls: Vec<String>,
    port: u16,
}

/// Machines allowed through `nitely serve`, and where they would connect.
#[tauri::command]
async fn network() -> Result<NetworkInfo, String> {
    use nitely_core::relay;
    Ok(NetworkInfo { tokens: relay::list_tokens().map_err(err)?, urls: relay::lan_urls(), port: relay::serving_port().unwrap_or(relay::DEFAULT_PORT) })
}

#[derive(Serialize)]
struct NewMachine {
    name: String,
    token: String,
    commands: Vec<String>,
}

/// Adds a machine. Its token exists only here, shown once to the person at the app: no command an
/// agent can run prints or stores one, and Nitely keeps only its hash.
#[tauri::command]
async fn network_add(name: String, project: Option<String>) -> Result<NewMachine, String> {
    use nitely_core::relay;
    let token = relay::add_token(&name, project.as_deref()).map_err(err)?;
    let urls = relay::lan_urls();
    let commands = if urls.is_empty() {
        vec![relay::claude_command(&format!("http://<this machine>:{}{}", relay::serving_port().unwrap_or(relay::DEFAULT_PORT), relay::MCP_PATH), &token)]
    } else {
        urls.iter().map(|u| relay::claude_command(u, &token)).collect()
    };
    Ok(NewMachine { name, token, commands })
}

#[tauri::command]
async fn network_remove(name: String) -> Result<bool, String> {
    nitely_core::relay::remove_token(&name).map_err(err)
}

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

/// The review and landing rules of each repo in a project (the project's Rules tab).
#[tauri::command]
async fn rules(project: String) -> Result<Vec<rules::RulesView>, String> {
    rules::rules_for_project(&Config::load().map_err(err)?, &project).map_err(err)
}

/// What saving these rules would loosen of what sends changes to a person; empty when nothing.
#[tauri::command]
async fn rules_check(repo: String, edit: rules::RulesEdit) -> Result<Vec<String>, String> {
    rules::check_rules(&Config::load().map_err(err)?, &repo, &edit).map_err(err)
}

/// Saves a repo's rules. Only the app saves them: a person, never an agent.
#[tauri::command]
async fn rules_save(repo: String, edit: rules::RulesEdit, confirm: bool) -> Result<rules::RulesView, String> {
    rules::save_rules(&Config::load().map_err(err)?, &repo, &edit, confirm).map_err(err)
}

#[tauri::command]
async fn reviewer_settings_preview(repo: String, edit: rules::ReviewerEdit) -> Result<rules::ReviewerPreview, String> {
    rules::preview_reviewer(&Config::load().map_err(err)?, &repo, &edit).map_err(err)
}

/// Human-only settings surface; deliberately absent from CLI, MCP and browser live writes.
#[tauri::command]
async fn reviewer_settings_save(repo: String, edit: rules::ReviewerEdit, preview: rules::ReviewerPreview) -> Result<rules::ReviewerSaved, String> {
    rules::save_reviewer(&Config::load().map_err(err)?, &db()?, &repo, &edit, &preview).map_err(err)
}

#[tauri::command]
async fn rules_branches(repo: String) -> Result<Vec<rules::DryRunBranch>, String> {
    rules::recent_branches(&Config::load().map_err(err)?, &repo).map_err(err)
}

#[tauri::command]
async fn rules_dry_run(repo: String, branch: String) -> Result<rules::DryRun, String> {
    rules::dry_run(&Config::load().map_err(err)?, &repo, &branch).map_err(err)
}

#[tauri::command]
async fn rule_defaults() -> Result<rules::DefaultsView, String> {
    Ok(rules::defaults_view(&Config::load().map_err(err)?))
}

#[tauri::command]
async fn rule_defaults_check(defaults: rules::RuleDefaults) -> Result<Vec<String>, String> {
    Ok(rules::defaults_loosening(&Config::load().map_err(err)?.rule_defaults, &defaults))
}

/// Saves the app-wide defaults. Only the app saves them: a person, never an agent.
#[tauri::command]
async fn rule_defaults_save(defaults: rules::RuleDefaults, confirm: bool) -> Result<rules::DefaultsView, String> {
    let mut config = Config::load().map_err(err)?;
    rules::save_defaults(&mut config, defaults, confirm).map_err(err)?;
    Ok(rules::defaults_view(&config))
}

fn run_sync() -> anyhow::Result<remote::SyncReport> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    let report = remote::sync(&config, &mut db);
    // Sessions whose agent stopped without a session end (a crashed terminal, an exited `codex
    // exec`) end, so nothing shows working or waiting on you for an agent that is gone (NIT-189).
    if let Err(e) = nitely_core::sessions::end_gone_now(&db) {
        eprintln!("sessions: {e:#}");
    }
    // The issue mirror, for projects the person turned it on for (docs/mirror.md).
    for e in nitely_core::mirror::sync(&config, &db).errors {
        eprintln!("mirror: {e}");
    }
    Ok(report)
}

/// What mirroring a project's issues to its repo's tracker would publish, and whether it can.
#[tauri::command]
async fn mirror_preview(project: String) -> Result<nitely_core::mirror::Preview, String> {
    nitely_core::mirror::preview(&Config::load().map_err(err)?, &db()?, &project, true).map_err(err)
}

/// Turns the mirror on, with a backfill (`none`, `open` or `all`), or off (no backfill).
#[tauri::command]
async fn set_mirror(project: String, backfill: Option<String>) -> Result<nitely_core::mirror::Preview, String> {
    let db = db()?;
    db.set_mirror(&project, backfill.as_deref(), &ME).map_err(err)?;
    let config = Config::load().map_err(err)?;
    // Publish right away rather than at the next sync. A failure is shown on the card, which
    // still shows the mirror as on, so it can be turned off again.
    let errors = if backfill.is_some() { nitely_core::mirror::sync(&config, &db).errors } else { vec![] };
    let mut preview = nitely_core::mirror::preview(&config, &db, &project, false).map_err(err)?;
    preview.last_error = errors.first().cloned();
    Ok(preview)
}

/// An issue's place in the mirror: mirrored (with its link), private, held back, or withdrawn.
#[tauri::command]
async fn issue_mirror(key: String) -> Result<Option<nitely_core::mirror::IssueMirror>, String> {
    let db = db()?;
    let key = db.current_key(&key).map_err(err)?;
    nitely_core::mirror::issue_state(&Config::load().map_err(err)?, &db, &key).map_err(err)
}

/// Keeps an issue off the host (withdrawn if it was published), or lets it be mirrored again.
#[tauri::command]
async fn set_issue_private(key: String, private: bool) -> Result<Option<nitely_core::mirror::IssueMirror>, String> {
    let db = db()?;
    let key = db.current_key(&key).map_err(err)?;
    db.set_issue_private(&key, private, &ME).map_err(err)?;
    let config = Config::load().map_err(err)?;
    // Taken down now, even when the project no longer mirrors.
    if private {
        nitely_core::mirror::withdraw_now(&config, &db, &key).map_err(err)?;
    }
    let report = nitely_core::mirror::sync(&config, &db);
    if let Some(e) = report.errors.first() {
        return Err(e.clone());
    }
    nitely_core::mirror::issue_state(&config, &db, &key).map_err(err)
}

/// Keeps a supervisor running for every project whose playbook has a crew (docs/crew.md): it starts
/// `nitely crew run --managed` apart from the app, so the crew outlives the app, and starts it again
/// when it dies or leaves for a newer build.
fn keep_crews() {
    let mut started: std::collections::HashMap<String, std::time::Instant> = Default::default();
    let mut children: Vec<std::process::Child> = Vec::new();
    loop {
        // Reaps the supervisors that exited, so none lingers as a zombie on Unix.
        children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        if let Err(e) = keep_crews_once(&mut started, &mut children) {
            eprintln!("crew: {e:#}");
        }
        std::thread::sleep(KEEP_CREWS_EVERY);
    }
}

fn keep_crews_once(started: &mut std::collections::HashMap<String, std::time::Instant>, children: &mut Vec<std::process::Child>) -> anyhow::Result<()> {
    use nitely_crew::crew;
    let wanted = crew::supervisors_wanted(&Config::load()?);
    if wanted.is_empty() {
        return Ok(());
    }
    let db = Db::open_default()?;
    for (project, root) in wanted {
        if !crew::unsupervised(&db, &project)? || started.get(&project).is_some_and(|t| t.elapsed() < CREW_START_GRACE) {
            continue;
        }
        started.insert(project.clone(), std::time::Instant::now());
        // Under the harness, its own nitely beside the app, never the person's installed one.
        let found = if harness::on() { harness::nitely() } else { crew::nitely_on_path() };
        let Some(nitely) = found else {
            db.crew_set_status(&project, "the app could not start the crew: no nitely on PATH (run `nitely install`)")?;
            continue;
        };
        match crew::start_supervisor(&nitely, &project, &root, &nitely_core::paths::data_dir().join("crew"), None) {
            Ok(child) => {
                eprintln!("crew: started the {project} supervisor (pid {})", child.id());
                children.push(child);
            }
            Err(e) => db.crew_set_status(&project, &format!("the app could not start the crew: {e:#}"))?,
        }
    }
    Ok(())
}

/// Opens a crew worker's log (docs/crew.md), named in its issue's timeline. Only `.log` files in
/// Nitely's crew folder open: the path comes from the page.
#[tauri::command]
async fn open_crew_log(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let crew_dir = nitely_core::paths::data_dir().join("crew").canonicalize().map_err(|e| format!("no crew logs yet: {e}"))?;
    let log = std::path::Path::new(&path).canonicalize().map_err(|e| format!("{path}: {e}"))?;
    if !log.starts_with(&crew_dir) || log.extension().is_none_or(|e| e != "log") {
        return Err(format!("{path} is not a crew log"));
    }
    app.opener().open_path(log.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

/// Reads cached recorder data; this command never starts a sampler.
#[tauri::command]
async fn performance_query(query: nitely_crew::performance_query::Query) -> Result<nitely_crew::performance_query::Report, String> {
    tauri::async_runtime::spawn_blocking(move || nitely_crew::performance_query::query(&nitely_core::performance::directory(), &query).map_err(err)).await.map_err(|e| e.to_string())?
}

/// Human controls from the desktop app, separate from read-only agent query APIs.
#[tauri::command]
async fn set_performance_settings(settings: nitely_core::performance::Settings) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = nitely_core::performance::Store::open(&nitely_core::performance::directory()).map_err(err)?;
        store.configure(&settings).map_err(err)
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn clear_performance_history() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| nitely_core::performance::clear_history(&nitely_core::performance::directory()).map_err(err)).await.map_err(|e| e.to_string())?
}

/// What became of each P0/P1 the project's reviews raised over the last `days`, per area (NIT-316).
#[tauri::command]
async fn review_precision(project: String, days: u32) -> Result<nitely_core::review_precision::Report, String> {
    nitely_core::review_precision::read(&db()?, &project, days).map_err(err)
}

/// The build lanes of every connected repo whose playbook has `[lanes]` (docs/build-lanes.md).
#[tauri::command]
async fn lanes_overview() -> Result<nitely_core::lanes::Overview, String> {
    nitely_core::lanes::overview(&Config::load().map_err(err)?, &db()?).map_err(err)
}

/// Sets the machine's memory floor and run cap, in GB (Settings, NIT-266): None puts back the
/// default; a floor of 0 turns the wait for memory off.
#[tauri::command]
async fn set_machine_limits(floor_gb: Option<f64>, cap_gb: Option<f64>) -> Result<nitely_core::machine::Now, String> {
    if floor_gb.is_some_and(|f| !f.is_finite() || f < 0.0) {
        return Err("the floor is a number of GB, 0 or more".into());
    }
    if cap_gb.is_some_and(|c| !c.is_finite() || c < 0.25) {
        return Err("the cap is a number of GB, at least 0.25".into());
    }
    let mut config = Config::load().map_err(err)?;
    config.memory_floor_gb = floor_gb;
    config.memory_cap_gb = cap_gb;
    config.save().map_err(err)?;
    Ok(nitely_core::machine::now())
}

/// When Nitely may empty idle programs' working memory while a run waits for memory, and whether
/// this system can (NIT-657).
#[tauri::command]
async fn memory_trim_setting() -> Result<nitely_core::memory_trim::Setting, String> {
    Ok(nitely_core::memory_trim::setting())
}

/// Sets it: None puts back the default, only while the person is away.
#[tauri::command]
async fn set_memory_trim(mode: Option<nitely_core::memory_trim::Mode>) -> Result<nitely_core::memory_trim::Setting, String> {
    let mut config = Config::load().map_err(err)?;
    config.memory_trim = mode;
    config.save().map_err(err)?;
    Ok(nitely_core::memory_trim::setting())
}

/// Git, Claude Code, Codex and the GitHub CLI: where each is, its version and whether it is signed
/// in (NIT-687). Runs each found for a few seconds at most, so off the main thread.
#[tauri::command]
async fn tools_check() -> Result<Vec<nitely_core::tools::Tool>, String> {
    tauri::async_runtime::spawn_blocking(nitely_core::tools::check).await.map_err(|e| e.to_string())
}

/// How each missing tool would be installed: the exact command, shown before anything runs
/// (NIT-688).
#[tauri::command]
async fn tool_install_plans() -> Result<Vec<nitely_core::tools_install::Plan>, String> {
    tauri::async_runtime::spawn_blocking(nitely_core::tools_install::plans).await.map_err(|e| e.to_string())
}

/// Installs `tool` with the plan the person was shown and confirmed, sending each line it prints
/// as a `tool-install` event, and returns the tool as it is afterwards (NIT-688).
#[tauri::command]
async fn install_tool(app: tauri::AppHandle, tool: String) -> Result<nitely_core::tools::Tool, String> {
    use tauri::Emitter;
    tauri::async_runtime::spawn_blocking(move || {
        nitely_core::tools_install::install(&tool, &mut |line| {
            app.emit("tool-install", serde_json::json!({ "tool": tool, "line": line })).ok();
        })
        .map_err(err)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Opens a terminal window running a tool's own sign-in (NIT-689): `gh auth login`, `codex login`,
/// or Claude Code itself. Nitely only starts it and never sees what is typed there; the setup's
/// Check again runs `tools_check` once the person is done.
#[tauri::command]
async fn tool_sign_in(command: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || nitely_core::tools::open_sign_in(&command).map_err(err)).await.map_err(|e| e.to_string())?
}

/// Every request waiting now, why, and what it waits behind (NIT-322).
#[tauri::command]
async fn waits_overview() -> Result<nitely_crew::waits::Waits, String> {
    nitely_crew::waits::overview(&Config::load().map_err(err)?, &db()?).map_err(err)
}

/// Review batches and land queues going now, for the board (docs/batching.md, NIT-218).
#[tauri::command]
async fn batches_overview() -> Result<nitely_core::batches::Overview, String> {
    nitely_core::batches::overview(&Config::load().map_err(err)?, &db()?).map_err(err)
}

/// Opens a build lane run's log, named by the run's id in its issue's timeline. The path comes
/// from the run's record, and only a `.log` file in Nitely's lanes folder opens.
#[tauri::command]
async fn open_lane_log(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let db = db()?;
    let run = db.lane_run(id).map_err(err)?.ok_or_else(|| format!("no run {id}"))?;
    let path = run.log.ok_or_else(|| format!("run {id} has no log (logs are kept 7 days)"))?;
    let dir = nitely_core::lanes::state_dir(&db).canonicalize().map_err(|e| format!("no lane logs yet: {e}"))?;
    let log = std::path::Path::new(&path).canonicalize().map_err(|e| format!("{path}: {e}"))?;
    if !log.starts_with(&dir) || log.extension().is_none_or(|e| e != "log") {
        return Err(format!("{path} is not a lane log"));
    }
    app.opener().open_path(log.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
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
    // Under the test harness the app must touch nothing of the person's; it does not start at all
    // when the harness forgot its own data folder.
    match harness::mode() {
        Ok(on) => harness::set(on),
        Err(e) => {
            eprintln!("harness: {e}");
            std::process::exit(2);
        }
    }
    let context = tauri::generate_context!();
    // The harness build (scripts/harness/tauri.harness.conf.json) runs only under the harness.
    if context.config().identifier.ends_with(".harness") && !harness::on() {
        eprintln!("harness: this is the test harness build; it runs only under scripts/harness/app.mjs (NITELY_HARNESS=1 and a throwaway NITELY_HOME)");
        std::process::exit(2);
    }
    if harness::on() {
        if let Err(e) = nitely_core::approval::use_ephemeral_key() {
            eprintln!("harness: {e}");
            std::process::exit(2);
        }
    }

    std::thread::spawn(|| loop {
        if let Err(e) = run_sync() {
            eprintln!("sync: {e:#}");
        }
        std::thread::sleep(SYNC_EVERY);
    });

    std::thread::spawn(keep_crews);
    // The Leaderboard's weekly send: only once the person joined and sent by hand (leaderboard_send).
    std::thread::spawn(|| loop {
        std::thread::sleep(LEADERBOARD_EVERY);
        let sent = Config::load().and_then(|config| nitely_crew::leaderboard_send::Sharing::default_dir().send_if_due(&config, &nitely_core::db::Db::open_default()?));
        if let Err(e) = sent {
            eprintln!("leaderboard: {e:#}");
        }
    });
    // Tauri exits the process without unwinding this stack. The Exit callback must stop and
    // join the collector explicitly so ordinary quits leave a durable clean-stop marker.
    let mut performance_recording = Some(nitely_core::performance::background(nitely_core::performance::directory()));

    // The app made the keychain entries, so it reads them without the OS asking (macOS).
    nitely_core::approval::keychain_is_silent();
    // Make the approval key, or put its public half back if it was changed.
    match db().and_then(|mut db| {
        let secret = approval_key(&db)?;
        // Approvals from before they were signed go back to the person, to approve again.
        db.unsigned_approvals_back_to_needs_you(&nitely_core::approval::public_of(&secret)).map_err(err)
    }) {
        Ok(moved) if !moved.is_empty() => eprintln!("approval key: {} sent back to Needs You to approve again", moved.join(", ")),
        Ok(_) => {}
        Err(e) => eprintln!("approval key: {e}"),
    }

    tauri::Builder::default()
        // First, so a second launch (a link clicked while Nitely runs) is handed to this one; with
        // its deep-link feature the link itself reaches on_open_url below.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| focus_main(app)))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingLink::default())
        .manage(update::UpdateState::default())
        .setup(|app| {
            use tauri_plugin_deep_link::DeepLinkExt;
            // Before anything is shown: a click on a notification that started the app finds its handler.
            notify::init(app.handle());
            // Installers register nitely:// at install; this registers it for the current user each
            // time the app starts, so a dev build opens links too (Windows and Linux; macOS needs the
            // bundled app).
            #[cfg(any(windows, target_os = "linux"))]
            if harness::on() {
                // The harness build must not take over the person's nitely:// links.
            } else if let Err(e) = app.deep_link().register_all() {
                eprintln!("deep link: {e}");
            } else {
                quiet_dev_links();
            }
            let links = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    open_link(&links, url.as_str());
                }
            });
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    open_link(app.handle(), url.as_str());
                }
            }
            // The same links as web links on loopback (http://127.0.0.1:7423/i/KEY), which chats and
            // GitHub open where they drop nitely:// (NIT-228). Only ever opens a view. Not in the
            // harness, whose app must not answer the person's links.
            if !harness::on() {
                let links = app.handle().clone();
                std::thread::spawn(move || answer_links(links));
            }
            let handle = app.handle().clone();
            std::thread::spawn(move || watch(handle));
            // The phone server stays off until the person turns it on (docs/phone.md).
            tauri::Manager::manage(app, phone::PhoneApp::new(app.handle().clone()));
            update::start(app.handle());
            // Durable, opt-in post-merge checks use the same admitted lanes as agent builds.
            std::thread::spawn(|| loop {
                let result=Config::load_read_only().and_then(|config| nitely_core::verification_backstop::tick_all(&mut Db::open_default()?, &config));
                if let Err(e)=result { eprintln!("verification backstop: {e:#}"); }
                std::thread::sleep(std::time::Duration::from_secs(60));
            });
            // Review work has its own durable cursor and queue; a long Full-suite run must
            // not prevent an opted-in project's periodic review from becoming due.
            std::thread::spawn(|| loop {
                let result=Config::load_read_only().and_then(|config| nitely_core::periodic_review::tick_all(&mut Db::open_default()?, &config));
                if let Err(e)=result { eprintln!("periodic review: {e:#}"); }
                // Quick changes, reviewed after landing in batches (NIT-625).
                let result=Config::load_read_only().and_then(|config| nitely_core::quick_review::tick_all(&mut Db::open_default()?, &config));
                if let Err(e)=result { eprintln!("quick review: {e:#}"); }
                std::thread::sleep(std::time::Duration::from_secs(60));
            });
            // A playbook that lands by Nitely lands an approved change without an agent coming back
            // for it. Each round lands what is approved and ready; a land that holds the lock, or an
            // app that was closed, only delays it to a later round. Started here, in the one
            // instance that runs, never in a second launch that only hands over a link.
            std::thread::spawn(|| {
                let mut reported = std::collections::HashMap::new();
                loop {
                    let round = Config::load().and_then(|config| nitely_core::land::land_approved(&mut Db::open_default()?, &config, &mut reported));
                    if let Err(e) = round {
                        eprintln!("land approved: {e:#}");
                    }
                    std::thread::sleep(LAND_APPROVED_EVERY);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            agent_icons::agent_icons,
            notify::notify,
            notify::set_badge,
            snapshot,
            opened_link,
            links_service,
            set_links_service,
            pulse,
            activity,
            sync_now,
            projects,
            archived_projects,
            set_project_archived,
            create_project,
            rename_project,
            rekey_project,
            remove_project,
            private_terms,
            set_private_terms,
            project_brief,
            goals,
            goal_report,
            goal_order,
            project_focus,
            project_review_mode,
            project_review_mode_effective,
            lands_pipeline,
            lands_dry_run,
            set_project_review_mode,
            periodic_review_status,
            retry_periodic_review,
            confirm_verification_escape,
            retry_verification_audit,
            set_project_focus,
            goal_workers,
            goal_choices,
            set_issue_goal,
            set_review_level,
            approve_goal_plan,
            start_goal,
            set_project_brief,
            held_files,
            crew_status,
            issue_spend,
            leaderboard_stats,
            leaderboard_report,
            leaderboard_status,
            leaderboard_join,
            leaderboard_send,
            leaderboard_board,
            leaderboard_delete,
            project_spend,
            crew_pause,
            crew_settings,
            crew_save,
            open_crew_log,
            lanes_overview,
            performance_query,
            set_performance_settings,
            clear_performance_history,
            set_machine_limits,
            tools_check,
            tool_install_plans,
            install_tool,
            tool_sign_in,
            memory_trim_setting,
            set_memory_trim,
            batches_overview,
            waits_overview,
            open_lane_log,
            issues,
            issue_detail,
            ask_detail,
            set_blocker,
            move_issue,
            phone::phone_status,
            phone::phone_start,
            phone::phone_tailscale_name,
            phone::phone_tailscale,
            phone::phone_stop,
            phone::phone_pair,
            phone::phone_decide,
            phone::phone_revoke,
            mirror_preview,
            set_mirror,
            issue_mirror,
            set_issue_private,
            set_owner,
            answer_ask,
            dismiss_notice,
            goal_state,
            accept_finding,
            decline_finding,
            connect_plan,
            connect_init,
            first_commit_preview,
            first_commit,
            connect_apply,
            agent_setup,
            agent_setup_apply,
            rules,
            rules_check,
            rules_save,
            reviewer_settings_preview,
            reviewer_settings_save,
            rules_branches,
            rules_dry_run,
            review_precision,
            rule_defaults,
            rule_defaults_check,
            rule_defaults_save,
            network,
            network_add,
            network_remove,
            update::update_status,
            approval_notice,
            dismiss_approval_notice,
            update::check_for_update,
            update::set_update_channel,
            update::restart_to_update
        ])
        .build(context)
        .expect("error while building Nitely")
        .run(move |app, event| {
            // A downloaded update installs on quit, unless a land is running (then the next quit).
            if let tauri::RunEvent::Exit = event {
                drop(performance_recording.take());
                update::install_on_quit(app);
            }
        });
}
