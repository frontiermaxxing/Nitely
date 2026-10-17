//! A project's crew as the app edits it (NIT-173): the playbook's `[crew]` workers, agents, daily
//! cap, token budget and quiet hours (docs/crew.md). Only the app calls the save, as the person;
//! agents propose playbook changes instead (proposals.rs).
//!
//! The playbook Nitely keeps for a repo is written in place, keeping its comments. A repo that
//! commits its own `.nitely/playbook.toml` gets the change as a commit on its integration branch,
//! built the way an approved proposal is, so no checkout's other files or index are touched.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::crew::{CrewRules, DEFAULT_MAX_PER_DAY};
use crate::db::Db;
use crate::issues::Actor;
use crate::playbook::{Playbook, PLAYBOOK_PATH};
use crate::proposals::Target;

/// Most workers the app lets a project run at once.
pub const MAX_WORKERS: u32 = 8;

/// An agent a crew can run, and whether this machine has it.
#[derive(Debug, Clone, Serialize)]
pub struct AgentChoice {
    pub name: String,
    pub installed: bool,
}

/// One repo's crew, as the Crew section shows it.
#[derive(Debug, Clone, Serialize)]
pub struct CrewView {
    pub repo: String,
    pub root: String,
    /// `repo` (saving commits to its `.nitely/playbook.toml`), `stored` (the copy Nitely keeps) or
    /// `none` (no playbook yet; saving writes the stored copy).
    pub source: String,
    pub path: String,
    /// The branch a save commits to, for a repo that commits its playbook.
    pub integration: String,
    pub workers: u32,
    /// In preference order.
    pub agents: Vec<String>,
    pub max_per_day: Option<u32>,
    pub default_max_per_day: u32,
    pub max_tokens_per_day: Option<u64>,
    pub quiet_hours: Option<String>,
    /// Every agent the crew knows how to start, the chosen ones first.
    pub choices: Vec<AgentChoice>,
    /// The playbook has gate commands; without them the crew never starts.
    pub has_gate: bool,
    /// A playbook change an agent proposed for this repo that waits for the person: approving it
    /// later replays it onto what this saves.
    pub pending_proposal: Option<String>,
}

/// A save from the Crew section. None removes the key, so the default applies.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CrewEdit {
    pub workers: u32,
    pub agents: Vec<String>,
    pub max_per_day: Option<u32>,
    pub max_tokens_per_day: Option<u64>,
    pub quiet_hours: Option<String>,
}

/// What a save did.
#[derive(Debug, Clone, Serialize)]
pub struct CrewSaved {
    pub view: CrewView,
    /// In words: the file written, or the commit made and where.
    pub done: String,
}

fn repos<'a>(config: &'a crate::Config, project: &str) -> Vec<&'a crate::RepoConfig> {
    config.repos.iter().filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))).collect()
}

fn repo<'a>(config: &'a crate::Config, name: &str) -> Result<&'a crate::RepoConfig> {
    config.repos.iter().find(|r| r.name == name).with_context(|| format!("no repo {name} in Nitely's config"))
}

fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The crew of every repo linked to `project`.
pub fn crew_for_project(config: &crate::Config, db: &Db, project: &str) -> Result<Vec<CrewView>> {
    repos(config, project).into_iter().map(|r| view(db, r, &installed)).collect()
}

/// Whether this machine has `program`, as the supervisor would find it.
fn installed(program: &str) -> bool {
    use crate::crew::Agents;
    crate::crew::SystemAgents.resolve(program).is_some()
}

fn view(db: &Db, repo: &crate::RepoConfig, installed: &dyn Fn(&str) -> bool) -> Result<CrewView> {
    let root = Path::new(&repo.root);
    let target = Target::of(&repo.name, root);
    let text = read(&target.path)?;
    let pb = Playbook::parse(&text).with_context(|| format!("in {}", target.path.display()))?;
    let source = if target.in_repo {
        "repo"
    } else if target.path.exists() {
        "stored"
    } else {
        "none"
    };
    let crew = &pb.crew;
    let mut names: Vec<String> = crew.agents.clone();
    for known in CrewRules::default().agents.into_iter().chain(crew.commands.keys().cloned()) {
        if !names.contains(&known) {
            names.push(known);
        }
    }
    let choices = names
        .into_iter()
        .map(|name| {
            let program = crew.commands.get(&name).and_then(|c| c.first().cloned()).unwrap_or_else(|| name.clone());
            AgentChoice { installed: installed(&program), name }
        })
        .collect();
    let scope = crate::paths::normalize(&repo.root);
    let pending_proposal = db
        .conn
        .query_row("SELECT issue_key FROM playbook_proposal WHERE scope = ?1 AND status = 'pending'", [&scope], |r| r.get::<_, String>(0))
        .ok();
    Ok(CrewView {
        repo: repo.name.clone(),
        root: repo.root.clone(),
        source: source.into(),
        path: target.path.to_string_lossy().into_owned(),
        integration: pb.integration_branch().to_string(),
        workers: crew.workers,
        agents: crew.agents.clone(),
        max_per_day: crew.max_per_day,
        default_max_per_day: DEFAULT_MAX_PER_DAY,
        max_tokens_per_day: crew.max_tokens_per_day,
        quiet_hours: crew.quiet_hours.clone(),
        choices,
        has_gate: !(pb.gate.commands.is_empty() && pb.gate.steps.is_empty()),
        pending_proposal,
    })
}

fn check(edit: &CrewEdit) -> Result<CrewEdit> {
    if edit.workers > MAX_WORKERS {
        bail!("at most {MAX_WORKERS} workers run at once");
    }
    let mut agents: Vec<String> = Vec::new();
    for a in &edit.agents {
        let a = a.trim();
        if a.is_empty() || !a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("`{a}` is not an agent's name");
        }
        if !agents.iter().any(|b| b == a) {
            agents.push(a.to_string());
        }
    }
    if edit.workers > 0 && agents.is_empty() {
        bail!("pick at least one agent for the crew to start");
    }
    if edit.max_per_day == Some(0) {
        bail!("the daily cap is at least 1; set workers to 0 to turn the crew off");
    }
    if edit.max_tokens_per_day == Some(0) {
        bail!("the token budget is at least 1; leave it empty for none");
    }
    let quiet_hours = edit.quiet_hours.as_deref().map(str::trim).filter(|q| !q.is_empty()).map(str::to_string);
    if let Some(q) = &quiet_hours {
        crate::crew::quiet_until(q, 0)?;
        if q.split_once('-').is_some_and(|(a, b)| a.trim() == b.trim()) {
            bail!("quiet hours `{q}` start and end at the same time");
        }
    }
    Ok(CrewEdit { workers: edit.workers, agents, max_per_day: edit.max_per_day, max_tokens_per_day: edit.max_tokens_per_day, quiet_hours })
}

/// `text` with `edit` written into its `[crew]`, keeping its comments and every other key.
pub fn edited(text: &str, edit: &CrewEdit) -> Result<String> {
    use toml_edit::{value, Array, Item, Table};
    let mut doc: toml_edit::DocumentMut = text.parse().context("the playbook is not valid TOML")?;
    if !doc.contains_key("crew") {
        doc.insert("crew", Item::Table(Table::new()));
    }
    let crew = doc["crew"].as_table_like_mut().context("the playbook's crew is not a table")?;
    let mut set = |key: &str, item: Option<Item>| match item {
        Some(item) => {
            // In place, so the comment above a key stays with it.
            match crew.get_mut(key) {
                Some(slot) if slot.is_value() && item.is_value() => {
                    let decor = slot.as_value().map(|v| v.decor().clone());
                    *slot = item;
                    if let (Some(decor), Some(v)) = (decor, slot.as_value_mut()) {
                        *v.decor_mut() = decor;
                    }
                }
                _ => {
                    crew.insert(key, item);
                }
            }
        }
        None => {
            crew.remove(key);
        }
    };
    set("workers", Some(value(i64::from(edit.workers))));
    let agents: Array = edit.agents.iter().map(String::as_str).collect();
    let default = CrewRules::default().agents;
    set("agents", (edit.agents != default || edit.workers > 0).then(|| value(agents)));
    set("max_per_day", edit.max_per_day.map(|n| value(i64::from(n))));
    set("max_tokens_per_day", edit.max_tokens_per_day.map(|n| value(n.min(i64::MAX as u64) as i64)));
    set("quiet_hours", edit.quiet_hours.as_deref().map(value));
    let out = doc.to_string();
    let pb = Playbook::parse(&out).context("the playbook would no longer parse")?;
    if pb.crew.workers != edit.workers {
        bail!("the playbook would not keep the change");
    }
    Ok(out)
}

/// The words for what a crew edit sets, for the timeline and a commit message.
fn describe(edit: &CrewEdit) -> String {
    if edit.workers == 0 {
        return "off".to_string();
    }
    let mut out = format!("{} worker{} ({})", edit.workers, if edit.workers == 1 { "" } else { "s" }, edit.agents.join(", "));
    if let Some(n) = edit.max_per_day {
        out.push_str(&format!(", {n} a day"));
    }
    if let Some(n) = edit.max_tokens_per_day {
        out.push_str(&format!(", {} tokens a day", crate::usage::short(n)));
    }
    if let Some(q) = &edit.quiet_hours {
        out.push_str(&format!(", quiet {q}"));
    }
    out
}

/// Saves `edit` as `repo`'s crew, as the person. Only the app calls this.
pub fn save_crew(config: &crate::Config, db: &Db, repo_name: &str, edit: &CrewEdit) -> Result<CrewSaved> {
    save_with(config, db, repo_name, edit, &installed)
}

fn save_with(config: &crate::Config, db: &Db, repo_name: &str, edit: &CrewEdit, installed: &dyn Fn(&str) -> bool) -> Result<CrewSaved> {
    let edit = check(edit)?;
    let repo = repo(config, repo_name)?;
    let root = Path::new(&repo.root);
    let target = Target::of(&repo.name, root);
    // A project runs one crew, from the first of its repos whose playbook has one (crew.rs,
    // supervisors_wanted): a second would be saved and never run.
    if edit.workers > 0 {
        let project = repo.project.as_deref().unwrap_or_default();
        for other in repos(config, project).into_iter().filter(|r| r.name != repo.name) {
            if Playbook::load_with(Path::new(&other.root), config).is_ok_and(|p| p.crew.workers > 0) {
                bail!("{project} already runs its crew from {}; a project runs one crew, so turn that one off first", other.name);
            }
        }
    }
    let what = describe(&edit);
    let done = if target.in_repo {
        let branch = Playbook::parse(&read(&target.path)?)?.integration_branch().to_string();
        // Nitely reads the playbook from the main checkout: committed to a branch it is not on, the
        // change would be saved and never followed.
        let on = crate::playbook::git(root, &["branch", "--show-current"]).map(|b| b.trim().to_string()).unwrap_or_default();
        if on != branch {
            let on = if on.is_empty() { "a detached HEAD".to_string() } else { on };
            bail!("{} is on {on}, and Nitely follows the playbook there; switch it to {branch} first, or change the crew on {on}'s playbook", root.display());
        }
        let message = format!("Crew: {what}\n\nSet in the Nitely app.");
        let commit = crate::proposals::commit_playbook_edit(root, &branch, &|at_tip| edited(at_tip, &edit), &message)?;
        format!("Committed {} to {PLAYBOOK_PATH} on {branch}; it is pushed with the next land", &commit[..commit.len().min(9)])
    } else {
        let new_text = edited(&read(&target.path)?, &edit)?;
        if let Some(dir) = target.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&target.path, new_text).with_context(|| format!("writing {}", target.path.display()))?;
        format!("Saved to {}", target.path.display())
    };
    let project = repo.project.clone().unwrap_or_default();
    db.log_event(&Actor::Human, None, Some(&repo.root), "crew_changed", &format!("Crew for {project} ({}): {what}", repo.name))?;
    Ok(CrewSaved { view: view(db, repo, installed)?, done })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn turn_on() -> CrewEdit {
        CrewEdit { workers: 2, agents: vec!["codex".into(), "claude".into()], max_per_day: Some(10), max_tokens_per_day: Some(5_000_000), quiet_hours: Some("01:00-07:00".into()) }
    }

    #[test]
    fn edits_crew_keeping_comments_and_other_keys() {
        let base = "[gate]\ncommands = [\"npm test\"]\n\n# Off until you turn it on.\n[crew]\n# How many at once.\nworkers = 0\nshared_files = [\"src/App.tsx\"]\n";
        let out = edited(base, &turn_on()).unwrap();
        assert!(out.contains("# Off until you turn it on.") && out.contains("# How many at once.\nworkers = 2"), "{out}");
        let pb = Playbook::parse(&out).unwrap();
        assert_eq!(pb.crew.workers, 2);
        assert_eq!(pb.crew.agents, ["codex", "claude"]);
        assert_eq!((pb.crew.max_per_day, pb.crew.max_tokens_per_day, pb.crew.quiet_hours.as_deref()), (Some(10), Some(5_000_000), Some("01:00-07:00")));
        assert_eq!(pb.crew.shared_files, ["src/App.tsx"], "keys the section does not edit stay");
        assert_eq!(pb.gate.commands, ["npm test"]);

        // Turning it off again clears the limits; a playbook with no [crew] gets one.
        let off = edited(&out, &CrewEdit { agents: vec!["claude".into(), "codex".into()], ..Default::default() }).unwrap();
        let pb = Playbook::parse(&off).unwrap();
        assert_eq!((pb.crew.workers, pb.crew.max_per_day, pb.crew.quiet_hours), (0, None, None));
        assert!(!off.contains("agents"), "the default agents with the crew off need no key: {off}");
        assert_eq!(Playbook::parse(&edited("", &turn_on()).unwrap()).unwrap().crew.workers, 2);
    }

    #[test]
    fn refuses_what_the_crew_could_not_run() {
        let bad = |e: CrewEdit| check(&e).unwrap_err().to_string();
        assert!(bad(CrewEdit { workers: 9, ..turn_on() }).contains("at most 8"));
        assert!(bad(CrewEdit { agents: vec![], ..turn_on() }).contains("at least one agent"));
        assert!(bad(CrewEdit { agents: vec!["rm -rf".into()], ..turn_on() }).contains("not an agent"));
        assert!(bad(CrewEdit { quiet_hours: Some("1am-7am".into()), ..turn_on() }).contains("HH:MM-HH:MM"));
        assert!(bad(CrewEdit { quiet_hours: Some("07:00-07:00".into()), ..turn_on() }).contains("same time"));
        assert!(bad(CrewEdit { max_per_day: Some(0), ..turn_on() }).contains("at least 1"));
        let ok = check(&CrewEdit { quiet_hours: Some("  ".into()), agents: vec!["codex".into(), "codex".into()], ..turn_on() }).unwrap();
        assert_eq!((ok.quiet_hours, ok.agents), (None, vec!["codex".to_string()]));
    }

    #[test]
    fn saves_a_stored_playbook_in_place_and_commits_a_repos_own() {
        let base = crate::paths::unique_temp("nitely-crew-settings", "");
        let db = Db::open(&base.join("t.db")).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = crate::playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        // A repo that commits its playbook: the change is a commit on its integration branch.
        let shop = base.join("shop");
        std::fs::create_dir_all(shop.join(".nitely")).unwrap();
        git(&shop, &["init", "-q", "-b", "main"]);
        std::fs::write(shop.join(PLAYBOOK_PATH), "[gate]\n# must pass\ncommands = [\"npm test\"]\n").unwrap();
        git(&shop, &["add", "."]);
        git(&shop, &["commit", "-q", "-m", "init"]);
        let mut config = crate::Config::default();
        config.repos.push(crate::RepoConfig { name: "shop".into(), root: shop.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some("SHOP".into()) });
        let agents = |p: &str| p == "codex";

        // Not while the checkout is on another branch: the crew would never see it.
        git(&shop, &["checkout", "-q", "-b", "side"]);
        assert!(save_with(&config, &db, "shop", &turn_on(), &agents).unwrap_err().to_string().contains("is on side"));
        git(&shop, &["checkout", "-q", "main"]);

        let saved = save_with(&config, &db, "shop", &turn_on(), &agents).unwrap();
        assert!(saved.done.starts_with("Committed ") && saved.done.contains("on main"), "{}", saved.done);
        assert_eq!(git(&shop, &["log", "-1", "--format=%s"]), "Crew: 2 workers (codex, claude), 10 a day, 5.0M tokens a day, quiet 01:00-07:00");
        assert_eq!(git(&shop, &["status", "--porcelain"]), "", "the checkout follows the commit");
        let committed = git(&shop, &["show", "HEAD:.nitely/playbook.toml"]);
        assert!(committed.contains("# must pass") && committed.contains("workers = 2"), "{committed}");
        let v = &saved.view;
        assert_eq!((v.source.as_str(), v.workers, v.has_gate), ("repo", 2, true));
        let installed: Vec<(&str, bool)> = v.choices.iter().map(|c| (c.name.as_str(), c.installed)).collect();
        assert_eq!(installed, [("codex", true), ("claude", false)]);

        // A second repo of the project cannot run a crew of its own.
        let site = base.join("site");
        std::fs::create_dir_all(site.join(".nitely")).unwrap();
        git(&site, &["init", "-q", "-b", "main"]);
        std::fs::write(site.join(PLAYBOOK_PATH), "[gate]
commands = [\"npm test\"]
").unwrap();
        git(&site, &["add", "."]);
        git(&site, &["commit", "-q", "-m", "init"]);
        config.repos.push(crate::RepoConfig { name: "site".into(), root: site.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some("SHOP".into()) });
        assert!(save_with(&config, &db, "site", &turn_on(), &agents).unwrap_err().to_string().contains("already runs its crew from shop"));
        let off = CrewEdit { agents: vec!["codex".into()], ..Default::default() };
        assert!(save_with(&config, &db, "site", &off, &agents).is_ok(), "settings with the crew off are fine");

        // A repo whose playbook Nitely keeps: written in place.
        let kept = base.join("kept.toml");
        let target = Target { repo: "notes".into(), root: base.join("notes"), in_repo: false, path: kept.clone() };
        std::fs::write(&kept, "# kept by Nitely\n").unwrap();
        let text = edited(&read(&target.path).unwrap(), &turn_on()).unwrap();
        assert!(text.contains("# kept by Nitely") && Playbook::parse(&text).unwrap().crew.workers == 2, "{text}");

        let events: Vec<String> = db.conn.prepare("SELECT summary FROM event WHERE type = 'crew_changed'").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(events, ["Crew for SHOP (shop): 2 workers (codex, claude), 10 a day, 5.0M tokens a day, quiet 01:00-07:00", "Crew for SHOP (site): off"]);
        std::fs::remove_dir_all(base).ok();
    }
}
