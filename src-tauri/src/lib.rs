mod agent_icons;
mod harness;
mod notify;
mod update;

mod phone;
use nitely_core::issues::{Ask, Project, Relations};
use nitely_core::{remote, rules, Actor, Board, Config, Db, IssueRecord};
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
async fn pulse(since: i64, day_start: i64) -> Result<nitely_core::Pulse, String> {
    nitely_core::pulse(&db()?, since, day_start).map_err(err)
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

/// Opens what a `nitely://issue/KEY` link names: keeps the key for the page, tells it at once if
/// it is listening, and brings the window forward. Anything else is ignored.
fn open_link(app: &tauri::AppHandle, url: &str) {
    use tauri::{Emitter, Manager};
    let Some(key) = nitely_core::issues::key_from_link(url) else { return };
    if let Some(pending) = app.try_state::<PendingLink>() {
        if let Ok(mut slot) = pending.0.lock() {
            *slot = Some(key.clone());
        }
    }
    app.emit(OPEN_ISSUE_EVENT, &key).ok();
    focus_main(app);
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

/// The goal rows: every active goal with its issues, of one project or all of them.
#[tauri::command]
async fn goals(project: Option<String>) -> Result<Vec<nitely_core::goals::GoalDetail>, String> {
    db()?.active_goals(project.as_deref()).map_err(err)
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
async fn crew_status(project: String) -> Result<Option<nitely_core::crew::CrewStatus>, String> {
    let config = Config::load().map_err(err)?;
    nitely_core::crew::project_status(&db()?, &config, &project, &nitely_core::crew::Moment::now()).map_err(err)
}

/// Pauses the project's crew (running workers finish, none start) or resumes it.
#[tauri::command]
async fn crew_pause(project: String, paused: bool) -> Result<Option<nitely_core::crew::CrewStatus>, String> {
    let db = db()?;
    db.crew_pause(&project, paused, &ME).map_err(err)?;
    nitely_core::crew::project_status(&db, &Config::load().map_err(err)?, &project, &nitely_core::crew::Moment::now()).map_err(err)
}

#[tauri::command]
async fn issues(project: String, include_closed: bool) -> Result<Vec<IssueRecord>, String> {
    db()?.list_issues(Some(&project), include_closed).map_err(err)
}

/// Tokens an issue's agents used (crates/nitely-core/src/usage.rs): counts only.
#[tauri::command]
async fn issue_spend(key: String) -> Result<nitely_core::usage::IssueSpend, String> {
    nitely_core::usage::spend_of_issue(&Config::load().map_err(err)?, &db()?, &key).map_err(err)
}

/// A project's tokens per day and by agent and model, and what landed, over `days` days.
#[tauri::command]
async fn project_spend(project: String, day_start: i64, days: usize) -> Result<nitely_core::usage::ProjectSpend, String> {
    nitely_core::usage::spend_of_project(&Config::load().map_err(err)?, &db()?, &project, day_start, days).map_err(err)
}

#[tauri::command]
async fn issue_detail(key: String) -> Result<nitely_core::issues::IssueDetail, String> {
    db()?.issue_detail(&key).map_err(err)?.ok_or_else(|| format!("no issue {key}"))
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
    let moved = db.move_issue(&key, &state, evidence.as_deref(), reason.as_deref(), &ME).map_err(err)?;
    // Approving a playbook proposal applies it and closes its issue at once: nothing to land.
    if let Some(secret) = secret.filter(|_| moved.state == "Approved") {
        db.sign_approval(&key, &secret).map_err(err)?;
    }
    Ok(moved)
}

/// The app's approval signing secret, kept in the OS keychain and made the first time, with its
/// public half beside it for `land` to check with (docs/approvals.md).
fn approval_key(db: &Db) -> Result<[u8; 32], String> {
    let secret = nitely_core::approval::keychain_secret().map_err(err)?;
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
async fn rules_branches(repo: String) -> Result<Vec<String>, String> {
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
    use nitely_core::crew;
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
        let Some(nitely) = crew::nitely_on_path() else {
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
            let handle = app.handle().clone();
            std::thread::spawn(move || watch(handle));
            // The phone server stays off until the person turns it on (docs/phone.md).
            tauri::Manager::manage(app, phone::PhoneApp::new(app.handle().clone()));
            update::start(app.handle());
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
            pulse,
            activity,
            sync_now,
            projects,
            create_project,
            rename_project,
            rekey_project,
            remove_project,
            private_terms,
            set_private_terms,
            project_brief,
            goals,
            goal_choices,
            set_issue_goal,
            approve_goal_plan,
            start_goal,
            set_project_brief,
            held_files,
            crew_status,
            issue_spend,
            project_spend,
            crew_pause,
            open_crew_log,
            issues,
            issue_detail,
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
            connect_plan,
            connect_apply,
            agent_setup,
            agent_setup_apply,
            rules,
            rules_check,
            rules_save,
            rules_branches,
            rules_dry_run,
            rule_defaults,
            rule_defaults_check,
            rule_defaults_save,
            network,
            network_add,
            network_remove,
            update::update_status,
            update::check_for_update,
            update::set_update_channel,
            update::restart_to_update
        ])
        .build(context)
        .expect("error while building Nitely")
        .run(|app, event| {
            // A downloaded update installs on quit, unless a land is running (then the next quit).
            if let tauri::RunEvent::Exit = event {
                update::install_on_quit(app);
            }
        });
}
