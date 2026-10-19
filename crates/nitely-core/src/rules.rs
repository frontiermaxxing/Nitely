//! Review and landing rules as the app shows and edits them: who reviews whose work, whether the
//! review must come from another vendor, who lands a reviewed change, and what sends a change to a
//! human. Two levels: app-wide defaults kept in Nitely's config (`RuleDefaults`), which fill what
//! a project's playbook leaves unset, and the project's own playbook. A repo that commits its own
//! `.nitely/playbook.toml` keeps its approval rules read-only; reviewer routing can be previewed
//! and committed by the person, using the same protected commit path as Crew settings.
//!
//! Only a human changes these: the app's commands call `save_rules` and `save_defaults`; the CLI
//! and the MCP tools never do, so an agent cannot loosen its own approval rules. A save that
//! loosens what sends changes to a human (`loosening`) needs the person to confirm it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::playbook::{git, LandBy, PathRule, Playbook};
use crate::proposals::Target;
use crate::review::Flag;

/// App-wide defaults for what a playbook leaves unset. None keeps Nitely's built-in default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleDefaults {
    /// Who reviews whose branches, as `review.reviewer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_cross_vendor: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land_by: Option<LandBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

impl RuleDefaults {
    pub fn is_empty(&self) -> bool {
        *self == RuleDefaults::default()
    }
}

/// The keys a playbook's text sets itself, of those the defaults can fill.
struct Sets {
    enabled: bool,
    reviewer: bool,
    require_cross_vendor: bool,
    land_by: bool,
    focus: bool,
}

fn sets(text: &str) -> Sets {
    let doc: Option<toml_edit::DocumentMut> = text.parse().ok();
    let has = |table: &str, key: &str| {
        doc.as_ref().and_then(|d| d.get(table)).and_then(|t| t.as_table_like()).is_some_and(|t| t.contains_key(key))
    };
    Sets { enabled: has("review", "enabled"), reviewer: has("review", "reviewer"), require_cross_vendor: has("review", "require_cross_vendor"), land_by: has("land", "by"), focus: has("review", "focus") }
}

/// Fills what `text` (the playbook `pb` was parsed from) does not set from the app-wide defaults.
pub fn apply_defaults(pb: &mut Playbook, text: &str, defaults: &RuleDefaults) {
    if defaults.is_empty() {
        return;
    }
    let set = sets(text);
    if let (false, Some(r)) = (set.reviewer, &defaults.reviewer) {
        pb.review.reviewer = r.clone();
    }
    if let (false, Some(v)) = (set.require_cross_vendor, defaults.require_cross_vendor) {
        pb.review.require_cross_vendor = v;
    }
    if let (false, Some(v)) = (set.land_by, defaults.land_by) {
        pb.land.by = v;
    }
    if let (false, Some(v)) = (set.focus, &defaults.focus) {
        pb.review.focus = v.clone();
    }
}

/// A value and where it comes from: `project` (the playbook sets it), `app` (the app-wide
/// default), or `built-in` (Nitely's own default).
/// `inherited` is what it would be if the project did not set it, from `inherited_from`.
#[derive(Debug, Clone, Serialize)]
pub struct Field<T> {
    pub value: T,
    pub from: &'static str,
    pub inherited: T,
    pub inherited_from: &'static str,
}

fn field<T: Clone>(project: bool, default: Option<&T>, value: T, inherited: T) -> Field<T> {
    let inherited_from = if default.is_some() { "app" } else { "built-in" };
    Field { value, from: if project { "project" } else { inherited_from }, inherited, inherited_from }
}

/// A reviewer the playbook can run, and whether its CLI is on this machine.
#[derive(Debug, Clone, Serialize)]
pub struct Reviewer {
    pub name: String,
    pub installed: bool,
}

/// One repo's review and landing rules, as the project's Rules view shows them.
#[derive(Debug, Clone, Serialize)]
pub struct RulesView {
    pub repo: String,
    pub root: String,
    /// `repo` (the repo commits `.nitely/playbook.toml`), `stored` (the copy Nitely keeps) or
    /// `none` (no playbook yet; saving writes the stored copy).
    pub source: String,
    pub path: String,
    pub editable: bool,
    pub reviewer_editable: bool,
    pub review_enabled: Field<bool>,
    /// Why it cannot be edited here, when it cannot.
    pub read_only: Option<String>,
    pub integration: String,
    // What sends a change to a human; per project only.
    pub always_human: bool,
    pub human_paths: Vec<PathRule>,
    pub blocked_paths: Vec<PathRule>,
    pub flags: Vec<Flag>,
    // What the app-wide defaults can fill.
    pub reviewer: Field<BTreeMap<String, String>>,
    pub require_cross_vendor: Field<bool>,
    pub land_by: Field<LandBy>,
    pub focus: Field<String>,
    pub reviewers: Vec<Reviewer>,
    /// How it batches reviews and lands; shown here, set in the playbook.
    pub batching: Batching,
}

/// The playbook's `[review] batch` and `[land] batch` settings (docs/batching.md, NIT-218).
#[derive(Debug, Clone, Serialize)]
pub struct Batching {
    /// off | small | all | shadow
    pub review: crate::review::BatchMode,
    pub review_window: String,
    pub review_max: usize,
    pub small_lines: usize,
    pub small_files: usize,
    pub max_lines: usize,
    /// off | waiting | all
    pub land: crate::playbook::LandBatch,
    pub land_window_seconds: u64,
    pub land_max: usize,
    /// Why changes land alone all the same, when `land` batches but the repo cannot.
    pub land_alone: Option<String>,
}

fn batching(pb: &Playbook) -> Batching {
    use crate::playbook::LandBatch;
    let land_alone = match (&pb.land.batch, &pb.lanes) {
        (LandBatch::Off, _) => None,
        (_, None) => Some("a batch is gated in a build lane, and this repo has no [lanes]".into()),
        (_, Some(l)) if l.large_repo => Some("large-repo mode lands each change alone".into()),
        _ => None,
    };
    Batching {
        review: pb.review.batch,
        review_window: pb.review.batch_window.clone(),
        review_max: pb.review.batch_max.max(2),
        small_lines: pb.review.small_lines,
        small_files: pb.review.small_files,
        max_lines: pb.review.max_lines,
        land: pb.land.batch,
        land_window_seconds: pb.land.batch_window().as_secs(),
        land_max: pb.land.batch_max(),
        land_alone,
    }
}

/// A save from the Rules view. The per-project lists replace what the playbook has; a None
/// default-able value removes the key, so the project inherits again.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RulesEdit {
    pub always_human: bool,
    pub human_paths: Vec<PathRule>,
    pub blocked_paths: Vec<PathRule>,
    pub flags: Vec<Flag>,
    pub review_enabled: Option<bool>,
    pub reviewer: Option<BTreeMap<String, String>>,
    pub require_cross_vendor: Option<bool>,
    pub land_by: Option<LandBy>,
    pub focus: Option<String>,
}

/// The repos linked to `project`, as Nitely's config lists them.
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

/// The playbook `text` makes, with the defaults filling what it leaves unset.
fn effective(text: &str, defaults: &RuleDefaults) -> Result<Playbook> {
    let mut pb = Playbook::parse(text)?;
    apply_defaults(&mut pb, text, defaults);
    Ok(pb)
}

/// The rules of every repo linked to `project`.
pub fn rules_for_project(config: &crate::Config, project: &str) -> Result<Vec<RulesView>> {
    repos(config, project).into_iter().map(|r| view(config, r)).collect()
}

fn view(config: &crate::Config, repo: &crate::RepoConfig) -> Result<RulesView> {
    let root = PathBuf::from(&repo.root);
    let target = Target::of(&repo.name, &root);
    let text = read(&target.path)?;
    let pb = effective(&text, &config.rule_defaults).with_context(|| format!("in {}", target.path.display()))?;
    let set = sets(&text);
    let d = &config.rule_defaults;
    let base = effective("", d)?;
    let source = if target.in_repo {
        "repo"
    } else if target.path.exists() {
        "stored"
    } else {
        "none"
    };
    let reviewers = pb.review.commands.keys().map(|name| Reviewer { name: name.clone(), installed: pb.review.is_installed(name, &root) }).collect();
    Ok(RulesView {
        repo: repo.name.clone(),
        root: repo.root.clone(),
        source: source.into(),
        path: target.path.to_string_lossy().into_owned(),
        editable: !target.in_repo,
        reviewer_editable: true,
        review_enabled: field(set.enabled, None, pb.review.enabled, true),
        read_only: target.in_repo.then(|| {
            format!(
                "{} commits its own {}. You can change review settings below. Other rules are set in that file; an agent can propose those changes for you to approve.",
                repo.name,
                crate::playbook::PLAYBOOK_PATH
            )
        }),
        integration: pb.integration_branch().to_string(),
        always_human: pb.approval.always_human,
        human_paths: pb.approval.human_paths.clone(),
        blocked_paths: pb.approval.blocked_paths.clone(),
        flags: pb.review.flags.clone(),
        reviewer: field(set.reviewer, d.reviewer.as_ref(), pb.review.reviewer.clone(), base.review.reviewer.clone()),
        require_cross_vendor: field(set.require_cross_vendor, d.require_cross_vendor.as_ref(), pb.review.require_cross_vendor, base.review.require_cross_vendor),
        land_by: field(set.land_by, d.land_by.as_ref(), pb.land.by, base.land.by),
        focus: field(set.focus, d.focus.as_ref(), pb.review.focus.clone(), base.review.focus.clone()),
        reviewers,
        batching: batching(&pb),
    })
}

/// What a change from `old` to `new` loosens among the rules that send changes to a human. Empty
/// when it loosens nothing.
pub fn loosening(old: &Playbook, new: &Playbook) -> Vec<String> {
    let mut out = Vec::new();
    if old.approval.always_human && !new.approval.always_human {
        out.push("Every change no longer waits for a person.".to_string());
    }
    let gone = |a: &[PathRule], b: &[PathRule]| -> Vec<String> {
        a.iter().filter(|r| !b.iter().any(|n| n.pattern == r.pattern)).map(|r| format!("`{}` ({})", r.pattern, r.reason)).collect()
    };
    for r in gone(&old.approval.human_paths, &new.approval.human_paths) {
        out.push(format!("Changes to {r} no longer wait for a person."));
    }
    for r in gone(&old.approval.blocked_paths, &new.approval.blocked_paths) {
        out.push(format!("Task branches may now change {r}."));
    }
    for f in &old.review.flags {
        match new.review.flags.iter().find(|n| n.name == f.name) {
            None => out.push(format!("The reviewer no longer answers {} ({}).", f.name, f.question)),
            // A reworded question can ask about less; only the person can tell.
            Some(n) if n.question.trim() != f.question.trim() => {
                out.push(format!("{} now asks \"{}\" instead of \"{}\".", f.name, n.question.trim(), f.question.trim()))
            }
            Some(_) => {}
        }
    }
    if old.review.require_cross_vendor && !new.review.require_cross_vendor {
        out.push("A review by the author's own vendor no longer waits for a person.".to_string());
    }
    if old.land.by == LandBy::Human && new.land.by != LandBy::Human {
        out.push("Changes no longer all wait for a person before they land.".to_string());
    }
    out
}

/// `text` with `edit` written in, keeping its comments and everything else it sets.
fn edited(text: &str, edit: &RulesEdit) -> Result<String> {
    use toml_edit::{value, ArrayOfTables, Item, Table};
    let mut doc: toml_edit::DocumentMut = text.parse().context("the playbook is not valid TOML")?;
    let table = |doc: &mut toml_edit::DocumentMut, name: &str| -> Result<()> {
        if !doc.contains_key(name) {
            doc.insert(name, Item::Table(Table::new()));
        }
        if doc[name].as_table_like().is_none() {
            bail!("the playbook's {name} is not a table");
        }
        Ok(())
    };
    table(&mut doc, "approval")?;
    table(&mut doc, "review")?;
    table(&mut doc, "land")?;
    let rules = |rules: &[PathRule]| {
        let mut a = ArrayOfTables::new();
        for r in rules {
            let mut t = Table::new();
            t.insert("pattern", value(r.pattern.trim()));
            t.insert("reason", value(r.reason.trim()));
            a.push(t);
        }
        Item::ArrayOfTables(a)
    };
    let set = |doc: &mut toml_edit::DocumentMut, t: &str, key: &str, item: Option<Item>| {
        let table = doc[t].as_table_like_mut().expect("checked above");
        match item {
            Some(item) => {
                table.insert(key, item);
            }
            None => {
                table.remove(key);
            }
        }
    };
    set(&mut doc, "approval", "always_human", edit.always_human.then(|| value(true)));
    set(&mut doc, "approval", "human_paths", (!edit.human_paths.is_empty()).then(|| rules(&edit.human_paths)));
    set(&mut doc, "approval", "blocked_paths", (!edit.blocked_paths.is_empty()).then(|| rules(&edit.blocked_paths)));
    let flags = (!edit.flags.is_empty()).then(|| {
        let mut a = ArrayOfTables::new();
        for f in &edit.flags {
            let mut t = Table::new();
            t.insert("name", value(f.name.trim()));
            t.insert("question", value(f.question.trim()));
            if !f.reason.trim().is_empty() {
                t.insert("reason", value(f.reason.trim()));
            }
            a.push(t);
        }
        Item::ArrayOfTables(a)
    });
    set(&mut doc, "review", "flags", flags);
    if let Some(enabled) = edit.review_enabled { set(&mut doc, "review", "enabled", Some(value(enabled))); }
    let reviewer = edit.reviewer.as_ref().map(|m| {
        let mut t = toml_edit::InlineTable::new();
        for (k, v) in m {
            t.insert(k, v.as_str().into());
        }
        value(t)
    });
    set(&mut doc, "review", "reviewer", reviewer);
    set(&mut doc, "review", "require_cross_vendor", edit.require_cross_vendor.map(value));
    let by = edit.land_by.map(|b| value(serde_json::to_value(b).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()));
    set(&mut doc, "land", "by", by);
    set(&mut doc, "review", "focus", edit.focus.as_deref().map(|f| value(f.trim())));
    for t in ["approval", "review", "land"] {
        if doc[t].as_table_like().is_some_and(|t| t.is_empty()) {
            doc.remove(t);
        }
    }
    Ok(doc.to_string())
}

fn check_edit(edit: &RulesEdit) -> Result<()> {
    for r in edit.human_paths.iter().chain(&edit.blocked_paths) {
        if r.pattern.trim().is_empty() {
            bail!("a path rule has no pattern");
        }
        regex::Regex::new(r.pattern.trim()).with_context(|| format!("`{}` is not a regular expression", r.pattern))?;
    }
    for (i, f) in edit.flags.iter().enumerate() {
        let name = f.name.trim();
        // The reviewer's last answer for a name wins, so a second question could cancel the first.
        if edit.flags[..i].iter().any(|g| g.name.trim() == name) {
            bail!("two questions are named {name}; give each its own name");
        }
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') || name == "VERDICT" {
            bail!("a flag's name is uppercase letters, digits and _, and not VERDICT: `{name}`");
        }
        if f.question.trim().is_empty() {
            bail!("the flag {name} has no question");
        }
    }
    Ok(())
}

/// What saving `edit` to `repo` would loosen; empty when nothing.
pub fn check_rules(config: &crate::Config, repo_name: &str, edit: &RulesEdit) -> Result<Vec<String>> {
    let (_, text, new_text) = prepare(config, repo_name, edit)?;
    Ok(loosening(&effective(&text, &config.rule_defaults)?, &effective(&new_text, &config.rule_defaults)?))
}

fn prepare(config: &crate::Config, repo_name: &str, edit: &RulesEdit) -> Result<(Target, String, String)> {
    let repo = repo(config, repo_name)?;
    let target = Target::of(&repo.name, Path::new(&repo.root));
    if target.in_repo {
        bail!("{} commits its own {}; change it there", repo.name, crate::playbook::PLAYBOOK_PATH);
    }
    check_edit(edit)?;
    let text = read(&target.path)?;
    let new_text = edited(&text, edit)?;
    Playbook::parse(&new_text).context("the playbook would no longer parse")?;
    Ok((target, text, new_text))
}

/// Saves `edit` as the playbook Nitely keeps for `repo`. Refused when it loosens what sends
/// changes to a human and `confirm` is not set. Only the app (a person) calls this.
pub fn save_rules(config: &crate::Config, repo_name: &str, edit: &RulesEdit, confirm: bool) -> Result<RulesView> {
    let root = Path::new(&repo(config, repo_name)?.root);
    let _lock = crate::land::Lock::try_take(&crate::land::lock_path(root)?, "review settings", false)?
        .context("another land or settings save is running; try again after it finishes")?;
    let (target, text, new_text) = prepare(config, repo_name, edit)?;
    let loose = loosening(&effective(&text, &config.rule_defaults)?, &effective(&new_text, &config.rule_defaults)?);
    if !loose.is_empty() && !confirm {
        bail!("this loosens what sends changes to a person; confirm it first: {}", loose.join(" "));
    }
    if let Some(dir) = target.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&target.path, new_text).with_context(|| format!("writing {}", target.path.display()))?;
    view(config, repo(config, repo_name)?)
}

/// Narrow human edit: it cannot change approval rules or reviewer commands.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerEdit {
    /// Omitted by older clients: retain the existing policy.
    pub enabled: Option<bool>,
    pub reviewer: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerPreview {
    pub before: String,
    pub after: String,
    pub diff: String,
    pub destination: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewerSaved {
    pub view: RulesView,
    pub done: String,
}

fn edited_reviewer(text: &str, edit: &ReviewerEdit) -> Result<String> {
    use toml_edit::{value, Item, Table};
    let pb = Playbook::parse(text)?;
    if let Some(map) = &edit.reviewer {
        for reviewer in map.values() {
            if !pb.review.commands.contains_key(reviewer) {
                bail!("no reviewer command named {reviewer}");
            }
        }
    }
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    if !doc.contains_key("review") {
        doc.insert("review", Item::Table(Table::new()));
    }
    let review = doc["review"].as_table_like_mut().context("review is not a table")?;
    if let Some(enabled) = edit.enabled { review.insert("enabled", value(enabled)); }
    if let Some(map) = &edit.reviewer {
        let mut table = toml_edit::InlineTable::new();
        for (author, reviewer) in map {
            table.insert(author, reviewer.as_str().into());
        }
        review.insert("reviewer", value(table));
    } else {
        review.remove("reviewer");
    }
    if review.is_empty() {
        doc.remove("review");
    }
    let out = doc.to_string();
    Playbook::parse(&out)?;
    Ok(out)
}

/// The exact file change the person will review before saving; no mutation.
pub fn preview_reviewer(config: &crate::Config, repo_name: &str, edit: &ReviewerEdit) -> Result<ReviewerPreview> {
    let repo = repo(config, repo_name)?;
    let target = Target::of(&repo.name, Path::new(&repo.root));
    let before = read(&target.path)?.replace("\r\n", "\n");
    let after = edited_reviewer(&before, edit)?;
    let destination = if target.in_repo {
        format!("Commit to {} on {}; pushed with the next land.", target.path.display(), Playbook::parse(&before)?.integration_branch())
    } else {
        format!("Save to {}.", target.path.display())
    };
    Ok(ReviewerPreview { diff: crate::proposals::diff_text(&crate::proposals::diff(&before, &after)), before, after, destination })
}

/// Human app command only. Recomputes the diff, refuses stale previews, and serializes with land.
pub fn save_reviewer(config: &crate::Config, db: &crate::Db, repo_name: &str, edit: &ReviewerEdit, preview: &ReviewerPreview) -> Result<ReviewerSaved> {
    let repo = repo(config, repo_name)?;
    let root = Path::new(&repo.root);
    let target = Target::of(&repo.name, root);
    let _lock = crate::land::Lock::try_take(&crate::land::lock_path(root)?, "reviewer settings", false)?
        .context("another land or settings save is running; try again after it finishes")?;
    let current = preview_reviewer(config, repo_name, edit)?;
    if current.before != preview.before || current.after != preview.after || current.destination != preview.destination {
        bail!("the playbook changed since this preview; review the new change before saving");
    }
    if current.before == current.after {
        return Ok(ReviewerSaved { view: view(config, repo)?, done: "Review settings are already saved.".into() });
    }
    let done = if target.in_repo {
        let branch = Playbook::parse(&current.before)?.integration_branch().to_string();
        let on = git(root, &["branch", "--show-current"])?;
        if on.trim() != branch {
            bail!("{} is on {}; switch it to {branch} before saving review settings", root.display(), if on.trim().is_empty() { "a detached HEAD" } else { on.trim() });
        }
        let commit = crate::proposals::commit_playbook_edit(root, &branch, &|at_tip| {
            if at_tip.replace("\r\n", "\n") != current.before {
                bail!("the committed playbook differs from this preview; reload and review the change again");
            }
            Ok(current.after.clone())
        }, "Review: update reviewer settings\n\nSet in the Nitely app.")?;
        format!("Committed {} on {branch}; pushed with the next land. New review launches use these settings; running reviewers finish.", &commit[..9])
    } else {
        if let Some(dir) = target.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&target.path, &current.after)?;
        format!("Saved to {}. New review launches use these settings; running reviewers finish.", target.path.display())
    };
    db.log_event(&crate::Actor::Human, None, Some(&repo.root), "reviewer_settings_changed", &format!("Review settings for {}: {done}", repo.name))?;
    Ok(ReviewerSaved { view: view(config, repo)?, done })
}

/// The app-wide defaults as the Agents view shows them: what is set, what Nitely uses when
/// nothing is, and the reviewers Nitely knows how to run here.
#[derive(Debug, Clone, Serialize)]
pub struct DefaultsView {
    pub defaults: RuleDefaults,
    pub built_in_reviewer: BTreeMap<String, String>,
    pub reviewers: Vec<Reviewer>,
}

pub fn defaults_view(config: &crate::Config) -> DefaultsView {
    let built_in = crate::review::ReviewRules::default();
    let here = std::env::temp_dir();
    let reviewers = built_in.commands.keys().map(|name| Reviewer { name: name.clone(), installed: built_in.is_installed(name, &here) }).collect();
    DefaultsView { defaults: config.rule_defaults.clone(), built_in_reviewer: built_in.reviewer, reviewers }
}

/// What changing the app-wide defaults from `old` to `new` loosens, for a project that sets
/// none of them.
pub fn defaults_loosening(old: &RuleDefaults, new: &RuleDefaults) -> Vec<String> {
    loosening(&effective("", old).unwrap_or_default(), &effective("", new).unwrap_or_default())
}

/// Saves the app-wide defaults. Refused when they loosen what sends changes to a human and
/// `confirm` is not set. Only the app (a person) calls this.
pub fn save_defaults(config: &mut crate::Config, defaults: RuleDefaults, confirm: bool) -> Result<()> {
    if let Some(r) = &defaults.reviewer {
        if r.is_empty() {
            bail!("a reviewer map needs at least one entry; leave it unset to use Nitely's");
        }
    }
    let loose = defaults_loosening(&config.rule_defaults, &defaults);
    if !loose.is_empty() && !confirm {
        bail!("this loosens what sends changes to a person; confirm it first: {}", loose.join(" "));
    }
    config.rule_defaults = defaults;
    config.save()
}

/// What Nitely would do with a branch under the rules as they are now.
#[derive(Debug, Clone, Serialize)]
pub struct DryRun {
    pub branch: String,
    pub base: String,
    pub files: Vec<String>,
    /// Who would review it, and how independent that is; None when no reviewer can run.
    pub reviewer: Option<String>,
    pub reviewer_note: String,
    pub reviewer_error: Option<String>,
    /// Why it would wait for a person, before the reviewer answers any flag.
    pub reasons: Vec<String>,
    /// Questions the reviewer answers; a yes also sends it to a person.
    pub flags: Vec<String>,
    /// Submit refuses it: it changes a blocked path.
    pub blocked: Option<String>,
    /// What happens once the review passes, in a sentence.
    pub outcome: String,
}

/// The branches of `repo` a dry run can try, most recently committed first.
pub fn recent_branches(config: &crate::Config, repo_name: &str) -> Result<Vec<String>> {
    let repo = repo(config, repo_name)?;
    let root = Path::new(&repo.root);
    let base = Playbook::load(root)?.integration_branch().to_string();
    let out = git(root, &["branch", "--sort=-committerdate", "--format=%(refname:short)"])?;
    Ok(out.lines().map(str::trim).filter(|b| !b.is_empty() && *b != base).take(20).map(str::to_string).collect())
}

/// What submit would decide for `branch` of `repo` under the rules as they are now. The review
/// itself does not run, so its findings and flag answers are not known.
pub fn dry_run(config: &crate::Config, repo_name: &str, branch: &str) -> Result<DryRun> {
    let repo = repo(config, repo_name)?;
    let root = Path::new(&repo.root);
    let pb = Playbook::load(root)?;
    // Measured as submit measures it: against origin's base when the local one is behind.
    let base = crate::playbook::diff_base(root, pb.integration_branch());
    let files: Vec<String> = git(root, &["diff", "--name-only", "--no-renames", &format!("{base}...{branch}")])
        .with_context(|| format!("comparing {branch} with {base}"))?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let blocked = pb.approval.check_blocked(&files).err().map(|e| format!("{e:#}"));
    let mut reasons = pb.human_reasons(&files)?;
    let vendor = crate::git::parse_branch(branch).0.unwrap_or_default();
    let (reviewer, reviewer_note, reviewer_error) = if !pb.review.enabled {
        reasons.extend(pb.review.unevaluated_reasons());
        (None, crate::review::DISABLED_NOTE.into(), None)
    } else { match pb.review.choose(&vendor, &|r| pb.review.is_installed(r, root)) {
        Ok(choice) => {
            reasons.extend(pb.review.independence_reason(&choice.reviewer, &vendor, &choice.note));
            (Some(choice.reviewer), choice.note, None)
        }
        Err(e) => (None, String::new(), Some(format!("{e:#}"))),
    } };
    let flags = pb.review.flags.iter().map(|f| format!("{}: {}", f.name, f.question)).collect();
    let outcome = if blocked.is_some() {
        "Submit refuses it: it changes a path task branches may not change.".to_string()
    } else if !pb.review.enabled {
        let next = match (reasons.is_empty(), pb.land.by) {
            (false, LandBy::Author) => "Once the gate passes it waits for a person; after approval, the agent lands it.",
            (false, _) => "Once the gate passes it waits for a person; Nitely lands it after approval.",
            (true, LandBy::Author) => "Once the gate passes the agent can land it.",
            (true, _) => "Once the gate passes Nitely lands it.",
        };
        format!("{}. {next}", crate::review::DISABLED_NOTE)
    } else if reviewer.is_none() {
        "Submit stops: no reviewer can run on this machine.".to_string()
    } else {
        match (reasons.is_empty(), pb.land.by) {
            (false, LandBy::Author) => "Once the review passes it waits for a person in Needs You; after they approve, the agent lands it.".into(),
            (false, _) => "Once the review passes it waits for a person in Needs You; Nitely lands it as soon as they approve.".into(),
            (true, LandBy::Author) => "Once the review passes it moves to In Review, and the agent lands it.".into(),
            (true, _) => "Once the review passes Nitely lands it in the same submit.".into(),
        }
    };
    Ok(DryRun { branch: branch.into(), base, files, reviewer, reviewer_note, reviewer_error, reasons, flags, blocked, outcome })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_only_what_the_playbook_leaves_unset() {
        let d = RuleDefaults { land_by: Some(LandBy::Auto), require_cross_vendor: Some(true), focus: Some("Money first.".into()), ..Default::default() };
        let pb = effective("[review]\nfocus = \"Speed.\"\n", &d).unwrap();
        assert_eq!((pb.land.by, pb.review.require_cross_vendor, pb.review.focus.as_str()), (LandBy::Auto, true, "Speed."));
        let own = effective("[land]\nby = \"author\"\n", &d).unwrap();
        assert_eq!(own.land.by, LandBy::Author, "the project's own value wins");
        assert_eq!(effective("", &RuleDefaults::default()).unwrap().land.by, LandBy::Author);
    }

    #[test]
    fn edits_keep_the_rest_of_the_playbook_and_inherit_again_when_unset() {
        let text = "# Our rules\n[gate]\ncommands = [\"npm test\"]\n[land]\nby = \"auto\"\n";
        let edit = RulesEdit {
            human_paths: vec![PathRule { pattern: "^billing/".into(), reason: "money".into() }],
            flags: vec![Flag { name: "MONEY".into(), question: "the change alters billing".into(), reason: String::new() }],
            require_cross_vendor: Some(true),
            ..Default::default()
        };
        let out = edited(text, &edit).unwrap();
        assert!(out.contains("# Our rules") && out.contains("npm test"), "{out}");
        let pb = Playbook::parse(&out).unwrap();
        assert_eq!(pb.approval.human_paths[0].pattern, "^billing/");
        assert_eq!(pb.review.flags[0].name, "MONEY");
        assert!(pb.review.require_cross_vendor);
        assert_eq!(pb.land.by, LandBy::Author, "land.by unset, so it inherits (built-in author)");
        assert!(!out.contains("[land]"), "an emptied table goes: {out}");
    }

    #[test]
    fn loosening_lists_what_no_longer_goes_to_a_person() {
        let old = Playbook::parse(
            "[approval]\nalways_human = true\n[[approval.human_paths]]\npattern = \"billing\"\nreason = \"money\"\n[review]\nrequire_cross_vendor = true\n[[review.flags]]\nname = \"MONEY\"\nquestion = \"q\"\n[land]\nby = \"human\"\n",
        )
        .unwrap();
        let loose = loosening(&old, &Playbook::default());
        assert_eq!(loose.len(), 5, "{loose:?}");
        assert!(loosening(&Playbook::default(), &old).is_empty(), "tightening needs no confirmation");
        let reworded = Playbook::parse("[[review.flags]]\nname = \"MONEY\"\nquestion = \"only refunds\"\n").unwrap();
        let asked = Playbook::parse("[[review.flags]]\nname = \"MONEY\"\nquestion = \"q\"\n").unwrap();
        assert_eq!(loosening(&asked, &reworded).len(), 1, "a reworded question needs confirming");
        let d = RuleDefaults { land_by: Some(LandBy::Human), ..Default::default() };
        assert_eq!(defaults_loosening(&d, &RuleDefaults::default()).len(), 1);
    }

    #[test]
    fn bad_edits_are_refused() {
        let bad_regex = RulesEdit { human_paths: vec![PathRule { pattern: "(".into(), reason: "x".into() }], ..Default::default() };
        assert!(check_edit(&bad_regex).is_err());
        let bad_flag = RulesEdit { flags: vec![Flag { name: "verdict".into(), question: "q".into(), reason: String::new() }], ..Default::default() };
        assert!(check_edit(&bad_flag).is_err());
        let flag = |q: &str| Flag { name: "MONEY".into(), question: q.into(), reason: String::new() };
        let twice = RulesEdit { flags: vec![flag("billing"), flag("payouts")], ..Default::default() };
        assert!(check_edit(&twice).unwrap_err().to_string().contains("two questions"));
    }

    #[test]
    fn reviewer_edits_only_change_routing_and_can_restore_inheritance() {
        let text = "# policy\n[gate]\ncommands = [\"npm test\"]\n[approval]\nalways_human = true\n[review]\nrequire_cross_vendor = true\n[review.commands]\ncustom = [\"review-tool\"]\n";
        let edit = ReviewerEdit { enabled: None, reviewer: Some(BTreeMap::from([("*".into(), "custom".into()), ("codex".into(), "custom".into())])) };
        let after = edited_reviewer(text, &edit).unwrap();
        let pb = Playbook::parse(&after).unwrap();
        assert!(pb.approval.always_human && pb.review.require_cross_vendor);
        assert_eq!(pb.gate.commands, ["npm test"]);
        assert!(after.contains("# policy") && after.contains("review-tool"));
        let inherited = edited_reviewer(&after, &ReviewerEdit { enabled: None, reviewer: None }).unwrap();
        let defaults = RuleDefaults { reviewer: Some(BTreeMap::from([("*".into(), "codex".into())])), ..Default::default() };
        assert_eq!(effective(&inherited, &defaults).unwrap().review.reviewer["*"], "codex");
        assert!(!sets(&inherited).reviewer);
        let bad = ReviewerEdit { enabled: None, reviewer: Some(BTreeMap::from([("*".into(), "missing".into())])) };
        assert!(edited_reviewer(text, &bad).unwrap_err().to_string().contains("no reviewer command"));
        assert!(serde_json::from_value::<ReviewerEdit>(serde_json::json!({"reviewer":null,"always_human":false})).is_err());
    }

    #[test]
    fn committed_reviewer_save_requires_exact_preview_clean_playbook_and_land_lock() {
        let root = crate::paths::unique_temp("nitely-reviewer-settings", "");
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        let command = |args: &[&str]| {
            let out = crate::playbook::run_git(std::process::Command::new("git").arg("-C").arg(&root).args(args)).unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        command(&["init", "-q", "-b", "main"]);
        command(&["config", "user.name", "Test"]);
        command(&["config", "user.email", "test@example.invalid"]);
        let path = root.join(crate::playbook::PLAYBOOK_PATH);
        let text = "[gate]\ncommands = [\"npm test\"]\n[approval]\nalways_human = true\n";
        std::fs::write(&path, text).unwrap();
        command(&["add", "."]);
        command(&["commit", "-qm", "initial"]);
        let db = crate::Db::open(&root.join("test.db")).unwrap();
        let mut config = crate::Config::default();
        config.repos.push(crate::RepoConfig { name: "sample".into(), root: root.to_string_lossy().into(), project: Some("APP".into()), integration_branch: "main".into() });
        let edit = ReviewerEdit { enabled: Some(false), reviewer: Some(BTreeMap::from([("*".into(), "codex".into())])) };
        let preview = preview_reviewer(&config, "sample", &edit).unwrap();
        let held = crate::land::Lock::try_take(&crate::land::lock_path(&root).unwrap(), "other", false).unwrap().unwrap();
        assert!(save_reviewer(&config, &db, "sample", &edit, &preview).unwrap_err().to_string().contains("another land"));
        drop(held);
        std::fs::write(&path, format!("{text}# local edit\n")).unwrap();
        assert!(save_reviewer(&config, &db, "sample", &edit, &preview).unwrap_err().to_string().contains("changed since"));
        let dirty_preview = preview_reviewer(&config, "sample", &edit).unwrap();
        assert!(save_reviewer(&config, &db, "sample", &edit, &dirty_preview).unwrap_err().to_string().contains("committed playbook differs"));
        std::fs::write(&path, text).unwrap();
        command(&["checkout", "-qb", "topic"]);
        assert!(save_reviewer(&config, &db, "sample", &edit, &preview).unwrap_err().to_string().contains("switch it to main"));
        command(&["checkout", "-q", "main"]);
        std::fs::write(root.join("unrelated.txt"), "keep staged").unwrap();
        command(&["add", "unrelated.txt"]);
        let before_tip = command(&["rev-parse", "HEAD"]);
        let before_index = std::fs::read(root.join(".git/index")).unwrap();
        let index_lock = root.join(".git/index.lock");
        std::fs::write(&index_lock, "someone else's lock").unwrap();
        let locked = save_reviewer(&config, &db, "sample", &edit, &preview).unwrap_err();
        assert!(format!("{locked:#}").contains("index.lock"), "{locked:#}");
        assert_eq!(command(&["rev-parse", "HEAD"]), before_tip, "a failed checkout never advances the branch");
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), before_index);
        assert_eq!(std::fs::read_to_string(&index_lock).unwrap(), "someone else's lock", "another Git's lock is never removed");
        std::fs::remove_file(index_lock).unwrap();
        let ref_lock = root.join(".git/refs/heads/main.lock");
        std::fs::write(&ref_lock, "another ref writer").unwrap();
        assert!(save_reviewer(&config, &db, "sample", &edit, &preview).is_err());
        assert_eq!(command(&["rev-parse", "HEAD"]), before_tip);
        assert_eq!(std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n"), text, "a refused ref update restores the playbook (with Git's checkout line endings)");
        assert_eq!(command(&["diff", "--cached", "--name-only"]), "unrelated.txt", "rollback preserves unrelated staged work");
        assert_eq!(std::fs::read_to_string(&ref_lock).unwrap(), "another ref writer");
        std::fs::remove_file(ref_lock).unwrap();
        let hooks = root.join(".git/settings-hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        command(&["config", "core.hooksPath", &hooks.to_string_lossy()]);
        let hook = hooks.join("post-checkout");
        std::fs::write(&hook, "#!/bin/sh\necho called >> .git/hook-count\ngit diff --cached -- .nitely/playbook.toml > .git/hook-observed\nexit 1\n").unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let staged_before_hook = command(&["diff", "--cached", "--binary"]);
        let failed = save_reviewer(&config, &db, "sample", &edit, &preview).unwrap_err();
        assert!(format!("{failed:#}").contains("previous contents were restored"), "{failed:#}");
        assert!(std::fs::read_to_string(root.join(".git/hook-observed")).unwrap().contains("codex"), "the hook failed after restore had changed the real index");
        assert_eq!(std::fs::read_to_string(root.join(".git/hook-count")).unwrap().lines().count(), 1, "recovery must not rerun the failing hook");
        assert_eq!(command(&["rev-parse", "HEAD"]), before_tip);
        assert_eq!(std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n"), text);
        assert_eq!(command(&["diff", "--cached", "--binary"]), staged_before_hook, "rollback preserves all unrelated staged content");
        assert_eq!(command(&["config", "core.hooksPath"]), hooks.to_string_lossy());
        std::fs::remove_file(hook).unwrap();
        let saved = save_reviewer(&config, &db, "sample", &edit, &preview).unwrap();
        assert!(saved.done.starts_with("Committed"));
        assert!(saved.view.always_human && saved.view.reviewer_editable && !saved.view.editable);
        assert!(!saved.view.review_enabled.value);
        let dry = dry_run(&config, "sample", "main").unwrap();
        assert!(dry.reviewer.is_none() && dry.reviewer_error.is_none());
        assert_eq!(dry.reviewer_note, crate::review::DISABLED_NOTE);
        assert!(dry.outcome.contains("gate passes"));
        assert_eq!(saved.view.reviewer.from, "project");
        assert_eq!(command(&["diff", "--cached", "--name-only"]), "unrelated.txt");
        assert_eq!(command(&["show", "--format=", "--name-only", "HEAD"]), ".nitely/playbook.toml");
        assert!(save_reviewer(&config, &db, "sample", &edit, &preview).unwrap_err().to_string().contains("changed since"));
        let inherit = ReviewerEdit { enabled: None, reviewer: None };
        config.rule_defaults.reviewer = Some(BTreeMap::from([("*".into(), "codex".into())]));
        let preview = preview_reviewer(&config, "sample", &inherit).unwrap();
        let saved = save_reviewer(&config, &db, "sample", &inherit, &preview).unwrap();
        assert_eq!(saved.view.reviewer.from, "app");
        assert_eq!(saved.view.reviewer.value["*"], "codex");
        assert!(!saved.view.review_enabled.value, "an older routing-only client preserves the review policy");
        drop(db);
        std::fs::remove_dir_all(root).ok();
    }
}
