//! Playbook changes agents propose and only a human puts in effect. An agent that finds a playbook
//! wrong (a stale integration branch, missing gate commands, review rules for a new project) calls
//! `propose_playbook_change`; Nitely checks the result still parses, files it as an issue in Needs
//! You with the diff and the agent's reasons, and the Inbox shows it as a decision. Approving the
//! issue applies the change to the playbook Nitely keeps; for a repo that commits its own
//! `.nitely/playbook.toml` it commits the change on the integration branch (an agent could not
//! carry it when the broken rule is what stops agents). Canceling it rejects the change and records why. A second proposal for the same repo
//! while one is pending replaces it with a new issue carrying both changes, so an issue's change
//! never moves under the human who is reading it.

use crate::db::{now, Db};
use crate::issues::{Actor, NewIssue};
use crate::playbook::{Playbook, PLAYBOOK_PATH};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Lines of unchanged context around each change in a diff.
const CONTEXT: usize = 3;

/// The playbook a proposal changes.
#[derive(Debug, Clone)]
pub struct Target {
    /// The repo's name in Nitely's config.
    pub repo: String,
    pub root: PathBuf,
    /// The repo commits its own `.nitely/playbook.toml`, so a change lands on a branch.
    pub in_repo: bool,
    /// The file that holds the playbook (it may not exist yet).
    pub path: PathBuf,
}

impl Target {
    /// The playbook a connected repo reads: its own file when it commits one, else the one Nitely
    /// keeps for it.
    pub fn of(repo: &str, root: &Path) -> Target {
        let in_repo = root.join(PLAYBOOK_PATH);
        if in_repo.exists() {
            return Target { repo: repo.into(), root: root.into(), in_repo: true, path: in_repo };
        }
        Target { repo: repo.into(), root: root.into(), in_repo: false, path: Playbook::stored_path(repo) }
    }

    fn text(&self) -> Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => Ok(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// The playbook `steps` apply to: the file Nitely keeps, or, for a repo that commits its own,
    /// the version committed on the integration branch the changed playbook names, which is
    /// where approving commits it. The diff the human reads and the commit start from the same.
    fn base(&self, steps: &[Step]) -> Result<String> {
        let text = self.text()?;
        if !self.in_repo {
            return Ok(text);
        }
        let branch = committed_branch(&text, steps)?;
        let committed = crate::playbook::git(&self.root, &["show", &format!("refs/heads/{branch}:{PLAYBOOK_PATH}")]).unwrap_or_default();
        Ok(committed)
    }

    fn describe(&self) -> String {
        if self.in_repo {
            format!("{} in {}", PLAYBOOK_PATH, self.repo)
        } else {
            format!("the playbook Nitely keeps for {} ({})", self.repo, self.path.display())
        }
    }
}

/// How a change is given: a TOML fragment merged into the playbook (tables merge key by key; any
/// other value, arrays included, is replaced), or the whole new playbook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Merge,
    Replace,
}

impl Mode {
    pub fn parse(s: Option<&str>) -> Result<Mode> {
        match s.unwrap_or("merge") {
            "merge" => Ok(Mode::Merge),
            "replace" => Ok(Mode::Replace),
            other => bail!("mode is merge (a TOML fragment merged into the playbook) or replace (the whole playbook), not {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Step {
    mode: Mode,
    change: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    /// ' ' unchanged, '+' added, '-' removed, '@' a gap of unchanged lines left out.
    pub op: char,
    pub text: String,
}

/// A proposal as the issue panel and the Inbox show it.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub id: i64,
    pub issue_key: String,
    pub repo: String,
    pub path: String,
    pub in_repo: bool,
    pub why: String,
    /// pending, applied, rejected or superseded.
    pub status: String,
    pub created_by: String,
    pub created_at: i64,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
    /// What applying did, or why it was rejected.
    pub outcome: Option<String>,
    /// While pending: what approving would change in the playbook as it is now.
    pub diff: Vec<DiffLine>,
    /// While pending: why the change no longer applies to the playbook as it is now.
    pub stale: Option<String>,
}

/// What `propose` returns to the agent.
#[derive(Debug, Clone, Serialize)]
pub struct Proposed {
    pub issue: crate::IssueRecord,
    pub url: String,
    /// The proposal it joined, when one was already pending for the repo.
    pub combined_with: Option<i64>,
    pub diff: String,
    pub next: String,
}

/// Merges a TOML fragment into a playbook's text, keeping the text's comments and layout.
fn merge(base: &str, change: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = base.parse().context("the current playbook is not valid TOML")?;
    let mut patch: toml_edit::DocumentMut = change.parse().context("the change is not valid TOML")?;
    // Tables print in position order, and the fragment's start at 0: new ones go after every
    // table the playbook has, in the fragment's own order.
    let after = last_position(doc.as_table()) + 1;
    shift_positions(patch.as_table_mut(), after);
    merge_table(doc.as_table_mut(), patch.as_table());
    drop_stale_approval_note(&mut doc);
    Ok(doc.to_string())
}

fn last_position(table: &toml_edit::Table) -> isize {
    let mut last = table.position().unwrap_or(0);
    for (_, item) in table.iter() {
        match item {
            toml_edit::Item::Table(t) => last = last.max(last_position(t)),
            toml_edit::Item::ArrayOfTables(a) => last = a.iter().map(last_position).fold(last, isize::max),
            _ => {}
        }
    }
    last
}

/// Moves the fragment's tables to `by` and after, each with a blank line above it.
fn shift_positions(table: &mut toml_edit::Table, by: isize) {
    let place = |t: &mut toml_edit::Table| {
        if let Some(p) = t.position() {
            t.set_position(p + by);
        }
        if t.decor().prefix().and_then(|p| p.as_str()).is_none_or(|p| !p.starts_with('\n')) {
            t.decor_mut().set_prefix("\n");
        }
    };
    for (_, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Table(t) => {
                place(t);
                shift_positions(t, by);
            }
            toml_edit::Item::ArrayOfTables(a) => {
                for t in a.iter_mut() {
                    place(t);
                    shift_positions(t, by);
                }
            }
            _ => {}
        }
    }
}

fn merge_table(into: &mut dyn toml_edit::TableLike, from: &dyn toml_edit::TableLike) {
    for (key, item) in from.iter() {
        if let Some(existing) = into.get_mut(key) {
            match (existing.as_table_like_mut(), item.as_table_like()) {
                (Some(existing), Some(patch)) => merge_table(existing, patch),
                // Replacing the value in place keeps the key, and with it the comments above it.
                _ => *existing = item.clone(),
            }
            continue;
        }
        into.insert(key, item.clone());
    }
}

/// The note `connect` writes above `always_human = true`, which stops being true once a change
/// turns it off.
const ALWAYS_HUMAN_NOTE: &str = "Every change waits for you until";

/// Drops `connect`'s note above `approval.always_human` once it is false, and with it the comment
/// lines that continue it.
fn drop_stale_approval_note(doc: &mut toml_edit::DocumentMut) {
    let Some(approval) = doc.get_mut("approval").and_then(|i| i.as_table_like_mut()) else { return };
    if approval.get("always_human").and_then(|v| v.as_bool()) != Some(false) {
        return;
    }
    let Some((mut key, _)) = approval.get_key_value_mut("always_human") else { return };
    let prefix = key.leaf_decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
    let lines: Vec<&str> = prefix.split_inclusive('\n').collect();
    let Some(start) = lines.iter().position(|l| l.contains(ALWAYS_HUMAN_NOTE)) else { return };
    let end = lines[start + 1..].iter().position(|l| !l.trim_start().starts_with('#')).map_or(lines.len(), |n| start + 1 + n);
    let kept: String = lines[..start].iter().chain(&lines[end..]).copied().collect();
    key.leaf_decor_mut().set_prefix(kept);
}

/// The playbook after `steps`, starting from `base`.
fn replay(base: &str, steps: &[Step]) -> Result<String> {
    let mut text = base.to_string();
    for s in steps {
        text = match s.mode {
            Mode::Merge => merge(&text, &s.change)?,
            Mode::Replace => s.change.clone(),
        };
    }
    Playbook::parse(&text).context("the playbook would no longer parse")?;
    Ok(text)
}

/// A line diff of `a` to `b`, with `CONTEXT` lines around each change and the rest left out.
pub fn diff(a: &str, b: &str) -> Vec<DiffLine> {
    let (x, y): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    // Longest common subsequence, from the end; playbooks are short.
    let mut lcs = vec![vec![0u32; y.len() + 1]; x.len() + 1];
    for i in (0..x.len()).rev() {
        for j in (0..y.len()).rev() {
            lcs[i][j] = if x[i] == y[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let mut all = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < x.len() || j < y.len() {
        if i < x.len() && j < y.len() && x[i] == y[j] {
            all.push((' ', x[i]));
            i += 1;
            j += 1;
        } else if i < x.len() && (j == y.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // Removals before additions, as diffs read.
            all.push(('-', x[i]));
            i += 1;
        } else {
            all.push(('+', y[j]));
            j += 1;
        }
    }
    let near = |k: usize| all.iter().enumerate().any(|(m, (op, _))| *op != ' ' && m.abs_diff(k) <= CONTEXT);
    let mut out = Vec::new();
    let mut gap = false;
    for (k, (op, text)) in all.iter().enumerate() {
        if *op != ' ' || near(k) {
            if gap && !out.is_empty() {
                out.push(DiffLine { op: '@', text: String::new() });
            }
            gap = false;
            out.push(DiffLine { op: *op, text: text.to_string() });
        } else {
            gap = true;
        }
    }
    out
}

/// The diff as text, for an issue body or a note.
pub fn diff_text(lines: &[DiffLine]) -> String {
    if lines.is_empty() {
        return "(no change)".into();
    }
    lines.iter().map(|l| if l.op == '@' { "@@".to_string() } else { format!("{}{}", l.op, l.text) }).collect::<Vec<_>>().join("\n")
}

struct Row {
    id: i64,
    issue_key: String,
    repo: String,
    root: String,
    in_repo: bool,
    path: String,
    steps: Vec<Step>,
    base: String,
    proposed: String,
    why: String,
    status: String,
    created_by: String,
    created_at: i64,
    decided_by: Option<String>,
    decided_at: Option<i64>,
    outcome: Option<String>,
}

const COLUMNS: &str = "id, issue_key, repo, root, in_repo, path, steps, base, proposed, why, status, created_by, created_at, decided_by, decided_at, outcome";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    let steps: String = r.get(6)?;
    Ok(Row {
        id: r.get(0)?,
        issue_key: r.get(1)?,
        repo: r.get(2)?,
        root: r.get(3)?,
        in_repo: r.get(4)?,
        path: r.get(5)?,
        steps: serde_json::from_str(&steps).unwrap_or_default(),
        base: r.get(7)?,
        proposed: r.get(8)?,
        why: r.get(9)?,
        status: r.get(10)?,
        created_by: r.get(11)?,
        created_at: r.get(12)?,
        decided_by: r.get(13)?,
        decided_at: r.get(14)?,
        outcome: r.get(15)?,
    })
}

impl Row {
    fn target(&self) -> Target {
        Target { repo: self.repo.clone(), root: PathBuf::from(&self.root), in_repo: self.in_repo, path: PathBuf::from(&self.path) }
    }

    /// Where the playbook is now: a repo can start committing its own after the proposal, and a
    /// repo renamed in Nitely takes the playbook Nitely keeps for it to the new name.
    fn current_target(&self) -> Target {
        let root = PathBuf::from(&self.root);
        let committed = root.join(PLAYBOOK_PATH);
        if committed.exists() {
            return Target { in_repo: true, path: committed, ..self.target() };
        }
        let scope = crate::paths::normalize(&self.root);
        // Read-only: this runs on reads, and the read-only preview among them.
        let renamed = crate::Config::load_read_only()
            .ok()
            .and_then(|c| c.repos.into_iter().find(|r| crate::paths::normalize(&r.root) == scope))
            .map(|r| r.name)
            .filter(|name| *name != self.repo);
        match renamed {
            Some(name) if !self.in_repo => Target { path: Playbook::stored_path(&name), repo: name, root, in_repo: false },
            _ => self.target(),
        }
    }

    fn view(self) -> Proposal {
        let now = self.current_target();
        let (diff, stale) = if self.status == "pending" {
            match now.base(&self.steps).and_then(|now| replay(&now, &self.steps).map(|after| diff(&now, &after))) {
                Ok(d) => (d, None),
                Err(e) => (diff(&self.base, &self.proposed), Some(format!("{e:#}"))),
            }
        } else {
            (diff(&self.base, &self.proposed), None)
        };
        Proposal {
            id: self.id,
            issue_key: self.issue_key,
            repo: now.repo,
            path: now.path.to_string_lossy().into_owned(),
            in_repo: now.in_repo,
            why: self.why,
            status: self.status,
            created_by: self.created_by,
            created_at: self.created_at,
            decided_by: self.decided_by,
            decided_at: self.decided_at,
            outcome: self.outcome,
            diff,
            stale,
        }
    }
}

/// The integration branch a committed playbook's change lands on: the one the changed playbook
/// names (a change can be the fix of a stale one), read off the checkout's copy.
fn committed_branch(checkout: &str, steps: &[Step]) -> Result<String> {
    Ok(Playbook::parse(&replay(checkout, steps)?)?.integration_branch().to_string())
}

/// Commits the approved change to the committed playbook on `branch`, built in a scratch index so
/// no checkout's index or files are touched; the branch only moves if it is still where it was.
/// When the main checkout has the branch checked out, its playbook file follows the commit, and
/// a local edit of that file stops it. Returns the new commit.
fn commit_playbook(root: &Path, branch: &str, steps: &[Step], message: &str) -> Result<String> {
    commit_playbook_edit(root, branch, &|at_tip| replay(at_tip, steps), message)
}

/// `commit_playbook` with the change as a function of the playbook committed at the branch's tip.
pub(crate) fn commit_playbook_edit(root: &Path, branch: &str, edit: &dyn Fn(&str) -> Result<String>, message: &str) -> Result<String> {
    let git = |args: &[&str]| crate::playbook::git(root, args).map(|s| s.trim().to_string());
    let tip = git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .map_err(|_| anyhow::anyhow!("{branch}, the integration branch the changed playbook names, is not a branch in {}", root.display()))?;
    let at_tip = crate::playbook::git(root, &["show", &format!("{tip}:{PLAYBOOK_PATH}")]).unwrap_or_default();
    let text = edit(&at_tip)?;

    // Where the branch is checked out, if anywhere.
    let listing = git(&["worktree", "list", "--porcelain"])?;
    let mut at = None;
    let mut holder = None;
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            at = Some(path.to_string());
        } else if line == format!("branch refs/heads/{branch}") {
            holder = at.clone();
        }
    }
    let in_main = match holder {
        Some(h) if crate::paths::normalize(&h) == crate::paths::normalize(&root.to_string_lossy()) => true,
        Some(h) => bail!("{branch} is checked out in {h}; finish or remove that worktree, then approve again"),
        None => false,
    };
    if in_main {
        let local = std::fs::read_to_string(root.join(PLAYBOOK_PATH)).unwrap_or_default();
        let staged = git(&["diff", "--cached", "--quiet", "--", PLAYBOOK_PATH]).is_err();
        if staged || local.replace("\r\n", "\n") != at_tip.replace("\r\n", "\n") {
            bail!("{PLAYBOOK_PATH} has uncommitted changes in {}; commit or discard them, then approve again", root.display());
        }
    }

    let scratch = crate::paths::unique_temp("nitely-playbook", "");
    std::fs::create_dir_all(&scratch)?;
    let result = (|| -> Result<String> {
        let index = scratch.join("index");
        let file = scratch.join("playbook.toml");
        std::fs::write(&file, &text)?;
        let with_index = |args: &[&str]| -> Result<String> {
            let out = crate::playbook::shell_free_command("git").arg("-C").arg(root).env("GIT_INDEX_FILE", &index).args(args).output()?;
            if !out.status.success() {
                bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        with_index(&["read-tree", &tip])?;
        let blob = git(&["hash-object", "-w", &file.to_string_lossy()])?;
        with_index(&["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{PLAYBOOK_PATH}")])?;
        let tree = with_index(&["write-tree"])?;
        let commit = git(&["commit-tree", &tree, "-p", &tip, "-m", message])?;
        let rollback = || -> Result<()> {
            // A post-checkout hook can fail after restore has written both the file and index.
            // Recovery must not run that failing hook again. Only this invocation ignores hooks;
            // the repo's configuration and unrelated staged paths are left alone.
            let hooks = scratch.join("recovery-hooks");
            std::fs::create_dir_all(&hooks)?;
            git(&["-c", &format!("core.hooksPath={}", hooks.display()), "restore", "--source",
                &format!("refs/heads/{branch}"), "--staged", "--worktree", "--", PLAYBOOK_PATH])?;
            Ok(())
        };
        if in_main {
            // Git takes the real index lock before changing either the file or the index. Do this
            // before moving the branch: a leftover index.lock must not leave a committed policy
            // that the checkout (and therefore Nitely) still reads as the old policy.
            if let Err(e) = git(&["restore", "--source", &commit, "--staged", "--worktree", "--", PLAYBOOK_PATH]) {
                if let Err(recovery) = rollback() {
                    return Err(e).context(format!("the integration branch has not changed; restoring its playbook also failed: {recovery:#}; inspect the playbook before retrying"));
                }
                return Err(e).context("the playbook checkout could not be updated; its previous contents were restored and the integration branch has not changed");
            }
        }
        if let Err(e) = git(&["update-ref", &format!("refs/heads/{branch}"), &commit, &tip]) {
            if in_main {
                // Catch up with the actual branch if another writer moved it, or put the old
                // playbook back if the ref write was refused. Never undo a concurrent commit.
                // Only this path changes; unrelated index entries stay untouched.
                rollback()
                    .context("the branch moved meanwhile and restoring the previous playbook failed; inspect the playbook before retrying")?;
            }
            return Err(e).context("the branch moved meanwhile; approve again");
        }
        Ok(commit)
    })();
    std::fs::remove_dir_all(&scratch).ok();
    result
}

impl Db {
    /// The proposal waiting on the human for the repo at `scope` (its normalized root).
    fn pending_in(&self, scope: &str) -> Result<Option<Row>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLUMNS} FROM playbook_proposal WHERE scope = ?1 AND status = 'pending'"), [scope], row)
            .optional()?)
    }

    fn latest_for_issue(&self, key: &str) -> Result<Option<Row>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLUMNS} FROM playbook_proposal WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 1"), [key], row)
            .optional()?)
    }

    /// The issue's proposal, if it carries one.
    pub fn proposal_for_issue(&self, key: &str) -> Result<Option<Proposal>> {
        Ok(self.latest_for_issue(key)?.map(Row::view))
    }

    /// Files a playbook change for a human to approve. Nothing is filed when the result would not
    /// parse. Each issue carries one proposal that never changes, so what the human approves is
    /// what they read. A proposal for a repo with one already pending replaces it with a new
    /// issue that carries both changes, and closes the old one as its duplicate.
    pub fn propose_playbook_change(&mut self, project: &str, target: &Target, mode: Mode, change: &str, why: &str, actor: &Actor) -> Result<Proposed> {
        if *actor == Actor::Human {
            bail!("a human edits the playbook directly; proposals are for agents");
        }
        if change.trim().is_empty() || why.trim().is_empty() {
            bail!("a proposal needs the change and why");
        }
        if mode == Mode::Merge {
            change.parse::<toml_edit::DocumentMut>().context("the change is not valid TOML")?;
        }
        let scope = crate::paths::normalize(&target.root.to_string_lossy());
        let earlier = self.pending_in(&scope)?;
        let mut steps = earlier.as_ref().map(|e| e.steps.clone()).unwrap_or_default();
        steps.push(Step { mode, change: change.to_string() });
        let base = target.base(&steps)?;
        let proposed = replay(&base, &steps)?;
        if proposed == base {
            bail!("the change leaves the playbook as it is");
        }
        let lines = diff(&base, &proposed);
        let why = why.trim();
        let why_all = match &earlier {
            Some(e) => format!("{}\n\n{}: {why}", e.why, actor.id()),
            None => why.to_string(),
        };

        // Take the repo's one pending slot: supersede the earlier proposal only if it is still
        // pending, and insert only if no other proposal took the slot meanwhile (a unique index).
        let again = || anyhow::anyhow!("another proposal for {} was filed or decided at the same moment; propose again", target.repo);
        if let Some(e) = &earlier {
            if self.conn.execute("UPDATE playbook_proposal SET status = 'superseded' WHERE id = ?1 AND status = 'pending'", [e.id])? == 0 {
                return Err(again());
            }
        }
        let restore = |db: &Db| {
            if let Some(e) = &earlier {
                db.conn.execute("UPDATE playbook_proposal SET status = 'pending' WHERE id = ?1 AND status = 'superseded'", [e.id]).ok();
            }
        };
        let inserted = self.conn.execute(
            "INSERT INTO playbook_proposal (issue_key, repo, root, scope, in_repo, path, steps, base, proposed, why, status, created_by, created_at)
             VALUES ('', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?11)",
            params![
                target.repo,
                target.root.to_string_lossy(),
                scope,
                target.in_repo,
                target.path.to_string_lossy(),
                serde_json::to_string(&steps)?,
                base,
                proposed,
                why_all,
                actor.id(),
                now()
            ],
        );
        if inserted.is_err() {
            restore(self);
            return Err(again());
        }
        let id = self.conn.last_insert_rowid();

        // Titled after the first sentence of the first change's reason (agents are told to open
        // `why` with a summary), with a count when later ones joined it.
        let mut summary = crate::issues::headline(&why_all, 70);
        if steps.len() > 1 {
            summary.push_str(&format!(" (and {} more)", steps.len() - 1));
        }
        let applies = if target.in_repo {
            format!("{} commits its own {PLAYBOOK_PATH}, so approving commits the change to it on the integration branch the changed playbook names.", target.repo)
        } else {
            format!("Approving applies it to {}.", target.path.display())
        };
        let joins = earlier.as_ref().map(|e| format!("\n\nIt includes the change proposed in {}, which it replaces.", e.issue_key)).unwrap_or_default();
        let body = format!(
            "{who} proposes a change to {where_}.{joins}\n\nWhy: {why_all}\n\n{applies} Canceling it with a reason rejects it and records why.\n\n```diff\n{diff}\n```",
            who = crate::issues::agent_name(actor.id()),
            where_ = target.describe(),
            diff = diff_text(&lines),
        );
        let filed = self.create_issue(
            &NewIssue {
                project: project.to_string(),
                title: format!("Playbook change for {}: {summary}", target.repo),
                body,
                kind: "improvement".into(),
                source: match actor.id().split(':').next() {
                    Some(v) if crate::issues::SOURCES.contains(&v) => v.to_string(),
                    _ => "claude".into(),
                },
                evidence: "Proposed with propose_playbook_change; the changed playbook parses.".into(),
                priority: 2,
                allow_similar: true,
                state: Some("Needs You".into()),
                blocked_by: vec![],
                labels: vec!["playbook".into()],
                // Only the human decides it, so no agent is ever handed it.
                owner: Some("human".into()),
                goal: None,
            },
            actor,
        );
        let issue = match filed {
            Ok(issue) => issue,
            Err(e) => {
                self.conn.execute("DELETE FROM playbook_proposal WHERE id = ?1", [id]).ok();
                restore(self);
                return Err(e);
            }
        };
        self.conn.execute("UPDATE playbook_proposal SET issue_key = ?2 WHERE id = ?1", params![id, issue.key])?;
        if let Some(e) = &earlier {
            let replaced = format!("Replaced by {}, which includes this change", issue.key);
            self.conn.execute("UPDATE playbook_proposal SET outcome = ?2 WHERE id = ?1", params![e.id, replaced])?;
            // The old issue's change lives on in the new one; a human may have moved it already.
            if let Err(err) = self.move_issue(&e.issue_key, "Duplicate", None, Some(&replaced), actor) {
                self.add_note(&e.issue_key, &format!("{replaced}. ({err:#})"), "note", actor).ok();
            }
        }
        Ok(Proposed {
            url: issue.url.clone(),
            combined_with: earlier.map(|e| e.id),
            diff: diff_text(&lines),
            next: format!(
                "Filed as {} in Needs You. Only the human approves it; keep working on what does not depend on it, and check it with get_issue.",
                issue.key
            ),
            issue,
        })
    }

    /// Runs before an issue moves. A human approving an issue that carries a pending proposal
    /// applies the change and closes it. Approving one another proposal replaced is refused. Canceling a
    /// pending one rejects the change with the reason. Anything else is left to the move.
    pub(crate) fn before_move(&mut self, key: &str, state: &str, reason: Option<&str>, actor: &Actor) -> Result<Option<crate::IssueRecord>> {
        let Some(p) = self.latest_for_issue(key)? else { return Ok(None) };
        match (p.status.as_str(), state) {
            ("pending", "Approved") if *actor == Actor::Human => Ok(Some(self.apply(p, actor)?)),
            ("superseded", "Approved") => {
                bail!("{}; decide that one instead", p.outcome.as_deref().unwrap_or("another proposal replaced this playbook change"))
            }
            ("pending", "Done") => bail!("{key} carries a playbook change: apply it (Approve) or reject it (Cancel with a reason)"),
            ("pending", "Canceled" | "Duplicate") => {
                let reason = reason.map(str::trim).filter(|r| !r.is_empty()).unwrap_or("no reason given");
                self.decide(p.id, "rejected", reason, actor)?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Refuses handing an issue that carries a pending playbook change to agents: only the human
    /// decides it.
    pub(crate) fn check_owner_change(&self, key: &str, owner: &str) -> Result<()> {
        if owner != "human" && self.latest_for_issue(key)?.is_some_and(|p| p.status == "pending") {
            bail!("{key} carries a playbook change only you can decide: apply it (Approve) or reject it (Cancel with a reason)");
        }
        Ok(())
    }

    fn decide(&self, id: i64, status: &str, outcome: &str, actor: &Actor) -> Result<()> {
        self.conn.execute(
            "UPDATE playbook_proposal SET status = ?2, outcome = ?3, decided_by = ?4, decided_at = ?5 WHERE id = ?1",
            params![id, status, outcome, actor.id(), now()],
        )?;
        Ok(())
    }

    fn apply(&mut self, p: Row, actor: &Actor) -> Result<crate::IssueRecord> {
        let target = p.current_target();
        let current = target.base(&p.steps)?;
        let text = replay(&current, &p.steps).context("the change no longer applies to the playbook as it is now; reject it and ask for a new proposal")?;
        if target.in_repo {
            // The human approved this exact change, so it goes in as it is. An agent could not
            // carry it: a broken integration branch or gate is what would stop that agent.
            let branch = committed_branch(&target.text()?, &p.steps)?;
            let first = p.why.lines().next().unwrap_or_default();
            let message = format!("Apply the playbook change approved in {}\n\n{first}\n\nProposed by {}.", p.issue_key, p.created_by);
            let commit = commit_playbook(&target.root, &branch, &p.steps, &message)?;
            let mut outcome = format!("Committed {} to {PLAYBOOK_PATH} on {branch} in {}; it is pushed with the next land", &commit[..commit.len().min(9)], target.repo);
            // Nitely reads the playbook from the main checkout. One on another branch (the stale
            // one, typically) keeps its old copy until the human switches it; Nitely never does.
            let checked_out = crate::playbook::git(&target.root, &["branch", "--show-current"]).map(|b| b.trim().to_string()).unwrap_or_default();
            if checked_out != branch {
                let on = if checked_out.is_empty() { "a detached HEAD".to_string() } else { checked_out };
                outcome.push_str(&format!(". The main checkout ({}) is on {on}, so Nitely reads that branch's playbook until you switch it to {branch}", target.root.display()));
            }
            self.decide(p.id, "applied", &outcome, actor)?;
            return self.move_issue(&p.issue_key, "Done", Some(&format!("{outcome}; approved by the human.")), None, actor);
        }
        if let Some(dir) = target.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&target.path, &text).with_context(|| format!("writing {}", target.path.display()))?;
        let outcome = format!("Applied to {}", target.path.display());
        self.decide(p.id, "applied", &outcome, actor)?;
        self.move_issue(&p.issue_key, "Done", Some(&format!("{outcome}; approved by the human.")), None, actor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Db, PathBuf) {
        let dir = crate::paths::unique_temp("nitely-proposals", "");
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        (db, dir)
    }

    fn stored(dir: &Path) -> Target {
        Target { repo: "shop".into(), root: dir.join("repo"), in_repo: false, path: dir.join("playbooks").join("shop.toml") }
    }

    const BASE: &str = "# kept by Nitely\n[branch]\nintegration = \"master\"\n\n[gate]\n# must pass\ncommands = [\"npm test\"]\n";
    fn agent() -> Actor {
        Actor::Agent("claude:/w".into())
    }

    #[test]
    fn titles_read_as_a_sentence_and_the_body_names_the_agent() {
        use crate::issues::{agent_name, headline};
        assert_eq!(
            headline("Setup sweep of main ee30a597f3c89767432861e8015fdbb570664304. main is the integration branch.", 70),
            "Setup sweep of main ee30a597f3c89767432861e8015fdbb570664304"
        );
        assert_eq!(headline("Review rules for the billing code, so money changes wait for a person and the rest lands alone", 70), "Review rules for the billing code, so money changes wait for a…");
        assert_eq!(headline("\nGate on npm test, e.g. the unit tests. More later.", 70), "Gate on npm test, e.g. the unit tests");
        assert_eq!(agent_name("codex:c:/users/someone/appdata/local/demo"), "Codex");
        assert_eq!(agent_name("claude:x"), "Claude");
    }

    #[test]
    fn approving_review_rules_drops_the_note_that_every_change_waits() {
        let empty = crate::paths::unique_temp("nitely-note", "");
        std::fs::create_dir_all(&empty).unwrap();
        let base = crate::connect::generated_playbook(&empty);
        assert!(base.contains("Every change waits for you"), "{base}");
        let out = merge(&base, "[approval]\nalways_human = false\n").unwrap();
        assert!(!out.contains("Every change waits") && !out.contains("sweep drafts them"), "{out}");
        assert!(out.contains("[approval]\nalways_human = false"), "{out}");
        // Left true, the note still holds.
        assert!(merge(&base, "[review]\nfocus = \"money\"\n").unwrap().contains("Every change waits for you"));
    }

    #[test]
    fn merge_keeps_comments_and_replaces_values() {
        let out = merge(BASE, "[branch]\nintegration = \"main\"\n[review]\nfocus = \"money\"\n").unwrap();
        assert!(out.contains("# kept by Nitely") && out.contains("# must pass"), "{out}");
        let pb = Playbook::parse(&out).unwrap();
        assert_eq!(pb.branch.integration, "main");
        assert_eq!(pb.gate.commands, vec!["npm test".to_string()]);
        assert_eq!(pb.review.focus, "money");
        // New tables go after the existing ones, in the change's order, each table whole.
        let out = merge(BASE, "[review]\nfocus = \"money\"\n\n[[review.flags]]\nname = \"MONEY\"\nquestion = \"q?\"\n\n[[approval.human_paths]]\npattern = \"^billing/\"\nreason = \"money\"\n").unwrap();
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s} in {out}"));
        assert!(at("[gate]") < at("[review]") && at("[review]") < at("[[review.flags]]") && at("[[review.flags]]") < at("[[approval.human_paths]]"), "{out}");
        assert!(out.contains("integration = \"master\"\n\n[gate]"), "{out}");
        // A replaced value keeps the comment written above its key.
        let out = merge(BASE, "[gate]\ncommands = [\"npm test\", \"npm run build\"]\n").unwrap();
        assert!(out.contains("# must pass\ncommands = [\"npm test\", \"npm run build\"]"), "{out}");
        // Nested tables merge rather than replace each other.
        let out = merge("[a.b]\nx = 1\ny = 2\n", "[a.b]\ny = 3\n").unwrap();
        assert!(out.contains("x = 1") && out.contains("y = 3"), "{out}");
    }

    #[test]
    fn diff_shows_changes_with_context() {
        let a = (1..=20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let b = a.replace("line 3", "line three").replace("line 18", "line eighteen");
        let d = diff_text(&diff(&a, &b));
        assert!(d.contains("-line 3\n+line three") && d.contains("-line 18\n+line eighteen"), "{d}");
        assert!(d.contains("@@") && !d.contains("line 10"), "unchanged middle is left out: {d}");
        assert_eq!(diff_text(&diff("x", "x")), "(no change)");
    }

    #[test]
    fn proposes_files_in_needs_you_and_applies_only_when_a_human_approves() {
        let (mut db, dir) = setup();
        let t = stored(&dir);
        std::fs::create_dir_all(t.path.parent().unwrap()).unwrap();
        std::fs::write(&t.path, BASE).unwrap();

        // A change that would not parse files nothing.
        assert!(db.propose_playbook_change("NIT", &t, Mode::Merge, "[gate]\ncommands = 3\n", "why", &agent()).is_err());
        assert!(db.propose_playbook_change("NIT", &t, Mode::Merge, "not toml [", "why", &agent()).is_err());
        assert!(db.list_issues(Some("NIT"), true).unwrap().is_empty());

        let p = db.propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"main\"\n", "master is three years old", &agent()).unwrap();
        assert_eq!(p.issue.state, "Needs You");
        assert!(p.diff.contains("-integration = \"master\"\n+integration = \"main\""), "{}", p.diff);
        assert!(p.issue.body.contains("```diff"));
        let key = p.issue.key.clone();

        // Only the human decides it: no agent is handed it, and it cannot just be closed.
        assert_eq!(p.issue.owner, "human");
        assert!(db.set_owner(&key, "agent", &Actor::Human).is_err());
        assert!(db.move_issue(&key, "Done", Some("did it"), None, &Actor::Human).is_err());
        // An agent cannot approve it, and the file is untouched until a human does.
        assert!(db.move_issue(&key, "Approved", None, None, &agent()).is_err());
        assert_eq!(std::fs::read_to_string(&t.path).unwrap(), BASE);
        let shown = db.proposal_for_issue(&key).unwrap().unwrap();
        assert_eq!((shown.status.as_str(), shown.stale.is_none()), ("pending", true));

        let done = db.move_issue(&key, "Approved", None, None, &Actor::Human).unwrap();
        assert_eq!(done.state, "Done");
        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(text.contains("integration = \"main\"") && text.contains("# must pass"), "{text}");
        assert_eq!(db.proposal_for_issue(&key).unwrap().unwrap().status, "applied");
    }

    #[test]
    fn a_second_proposal_replaces_the_pending_one_with_both_changes() {
        let (mut db, dir) = setup();
        let t = stored(&dir);
        let first = db.propose_playbook_change("NIT", &t, Mode::Merge, "[gate]\ncommands = [\"npm test\"]\n", "no gate", &agent()).unwrap();
        let second = db
            .propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"trunk\"\n", "trunk is current", &Actor::Agent("codex:/x".into()))
            .unwrap();
        // Each issue's change stays what the human read: the combined one gets its own issue.
        assert_ne!(first.issue.key, second.issue.key);
        assert!(second.combined_with.is_some());
        assert!(second.diff.contains("+commands") && second.diff.contains("+integration = \"trunk\""), "{}", second.diff);
        let p = db.proposal_for_issue(&second.issue.key).unwrap().unwrap();
        assert!(p.why.contains("no gate") && p.why.contains("trunk is current"));
        assert_eq!(db.issue(&first.issue.key).unwrap().unwrap().state, "Duplicate");
        let old = db.proposal_for_issue(&first.issue.key).unwrap().unwrap();
        assert_eq!(old.status, "superseded");
        // Approving the replaced one is refused and changes nothing.
        let err = db.move_issue(&first.issue.key, "Approved", None, None, &Actor::Human).unwrap_err().to_string();
        assert!(err.contains(&second.issue.key), "{err}");
        assert!(!t.path.exists());

        // The human edits the stored playbook meanwhile: approving applies the change to that.
        std::fs::create_dir_all(t.path.parent().unwrap()).unwrap();
        std::fs::write(&t.path, "# edited by hand\n[worktree]\nsetup = [\"npm ci\"]\n").unwrap();
        db.move_issue(&second.issue.key, "Approved", None, None, &Actor::Human).unwrap();
        let pb = Playbook::parse(&std::fs::read_to_string(&t.path).unwrap()).unwrap();
        assert_eq!((pb.branch.integration.as_str(), pb.worktree.setup.len(), pb.gate.commands.len()), ("trunk", 1, 1));
    }

    #[test]
    fn one_pending_proposal_per_repo_and_proposals_follow_a_new_key() {
        let (mut db, dir) = setup();
        let t = stored(&dir);
        let p = db.propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"main\"\n", "stale", &agent()).unwrap();
        // A second pending row for the same repo cannot exist, whoever writes it.
        let dup = db.conn.execute(
            "INSERT INTO playbook_proposal (issue_key, repo, root, scope, in_repo, path, steps, base, proposed, why, status, created_by, created_at)
             SELECT 'NIT-99', repo, root, scope, in_repo, path, steps, base, proposed, why, 'pending', created_by, created_at FROM playbook_proposal",
            [],
        );
        assert!(dup.is_err());
        db.rekey_project("NIT", "NX").unwrap();
        let key = p.issue.key.replace("NIT-", "NX-");
        assert_eq!(db.proposal_for_issue(&key).unwrap().unwrap().status, "pending");
        assert_eq!(db.move_issue(&key, "Approved", None, None, &Actor::Human).unwrap().state, "Done");
        assert!(std::fs::read_to_string(&t.path).unwrap().contains("integration = \"main\""));
        // Removing the project takes its proposals with it, so the repo can be proposed for again.
        db.propose_playbook_change("NX", &t, Mode::Merge, "[branch]\nintegration = \"dev\"\n", "dev", &agent()).unwrap();
        db.remove_project("NX", true).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        assert!(db.propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"dev\"\n", "dev", &agent()).is_ok());
    }

    #[test]
    fn canceling_rejects_with_the_reason() {
        let (mut db, dir) = setup();
        let t = stored(&dir);
        let p = db.propose_playbook_change("NIT", &t, Mode::Replace, "[branch]\nintegration = \"dev\"\n", "dev", &agent()).unwrap();
        db.move_issue(&p.issue.key, "Canceled", None, Some("dev is not ours"), &Actor::Human).unwrap();
        let shown = db.proposal_for_issue(&p.issue.key).unwrap().unwrap();
        assert_eq!((shown.status.as_str(), shown.outcome.as_deref()), ("rejected", Some("dev is not ours")));
        assert!(!t.path.exists(), "nothing written");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        crate::playbook::git(dir, args).unwrap().trim().to_string()
    }

    /// A repo that commits its playbook on `main`, with `main` checked out.
    fn committed_repo(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        std::fs::write(repo.join(PLAYBOOK_PATH), "[branch]\nintegration = \"main\"\n\n[gate]\ncommands = [\"npm test\"]\n").unwrap();
        std::fs::write(repo.join("app.txt"), "app\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "start"]);
        repo
    }

    #[test]
    fn approving_a_committed_playbook_commits_it_on_the_integration_branch() {
        let (mut db, dir) = setup();
        let repo = committed_repo(&dir);
        // Someone's unrelated work in the main checkout stays as it is.
        std::fs::write(repo.join("app.txt"), "app, edited\n").unwrap();
        let t = Target::of("shop", &repo);
        assert!(t.in_repo);
        let p = db.propose_playbook_change("NIT", &t, Mode::Merge, "[gate]\ncommands = [\"npm test\", \"npm run build\"]\n", "no build", &agent()).unwrap();
        assert!(db.proposal_for_issue(&p.issue.key).unwrap().unwrap().in_repo);
        let done = db.move_issue(&p.issue.key, "Approved", None, None, &Actor::Human).unwrap();
        assert_eq!(done.state, "Done");
        let committed = git(&repo, &["show", "main:.nitely/playbook.toml"]);
        assert!(committed.contains("npm run build"), "{committed}");
        assert_eq!(git(&repo, &["log", "--format=%s", "-1", "main"]), format!("Apply the playbook change approved in {}", p.issue.key));
        assert!(std::fs::read_to_string(repo.join(PLAYBOOK_PATH)).unwrap().contains("npm run build"), "the checkout's file follows");
        assert_eq!(git(&repo, &["status", "--porcelain"]), "M app.txt");
    }

    #[test]
    fn a_stale_integration_branch_is_fixed_on_the_branch_the_change_names() {
        let (mut db, dir) = setup();
        let repo = committed_repo(&dir);
        // The playbook names a branch that is three years behind; work happens on trunk, whose
        // playbook already differs from the checkout's (it gates on `make`).
        git(&repo, &["checkout", "-q", "-b", "trunk"]);
        std::fs::write(repo.join(PLAYBOOK_PATH), "[branch]\nintegration = \"main\"\n\n[gate]\ncommands = [\"make\"]\n").unwrap();
        git(&repo, &["commit", "-q", "-am", "gate on make"]);
        git(&repo, &["checkout", "-q", "main"]);
        let t = Target::of("shop", &repo);
        let p = db.propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"trunk\"\n", "main is stale", &agent()).unwrap();
        // The diff is against trunk's playbook, where the commit goes: only the branch changes.
        let shown = db.proposal_for_issue(&p.issue.key).unwrap().unwrap();
        let changed: Vec<String> = shown.diff.iter().filter(|l| l.op != ' ').map(|l| format!("{}{}", l.op, l.text)).collect();
        assert_eq!(changed, vec!["-integration = \"main\"".to_string(), "+integration = \"trunk\"".to_string()]);
        db.move_issue(&p.issue.key, "Approved", None, None, &Actor::Human).unwrap();
        let on_trunk = git(&repo, &["show", "trunk:.nitely/playbook.toml"]);
        assert!(on_trunk.contains("integration = \"trunk\"") && on_trunk.contains("make"), "{on_trunk}");
        assert!(!git(&repo, &["show", "main:.nitely/playbook.toml"]).contains("trunk"), "main is left alone");
        // The checkout is still on main, so the human is told to switch it.
        let outcome = db.proposal_for_issue(&p.issue.key).unwrap().unwrap().outcome.unwrap();
        assert!(outcome.contains("is on main") && outcome.contains("switch it to trunk"), "{outcome}");
        assert_eq!(git(&repo, &["status", "--porcelain"]), "", "the checkout (on main) is untouched");
    }

    #[test]
    fn a_local_edit_of_the_committed_playbook_stops_the_commit() {
        let (mut db, dir) = setup();
        let repo = committed_repo(&dir);
        let t = Target::of("shop", &repo);
        let p = db.propose_playbook_change("NIT", &t, Mode::Merge, "[gate]\ncommands = []\n", "gate hangs", &agent()).unwrap();
        std::fs::write(repo.join(PLAYBOOK_PATH), "[branch]\nintegration = \"main\"\n# mine\n").unwrap();
        let err = db.move_issue(&p.issue.key, "Approved", None, None, &Actor::Human).unwrap_err().to_string();
        assert!(err.contains("uncommitted changes"), "{err}");
        assert_eq!(db.issue(&p.issue.key).unwrap().unwrap().state, "Needs You");
        assert_eq!(db.proposal_for_issue(&p.issue.key).unwrap().unwrap().status, "pending");
        assert_eq!(git(&repo, &["rev-list", "--count", "main"]), "1");
    }

    #[test]
    fn humans_do_not_propose_and_empty_changes_are_refused() {
        let (mut db, dir) = setup();
        let t = stored(&dir);
        assert!(db.propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"x\"\n", "why", &Actor::Human).is_err());
        std::fs::create_dir_all(t.path.parent().unwrap()).unwrap();
        std::fs::write(&t.path, BASE).unwrap();
        let err = db.propose_playbook_change("NIT", &t, Mode::Merge, "[branch]\nintegration = \"master\"\n", "why", &agent()).unwrap_err();
        assert!(err.to_string().contains("leaves the playbook as it is"), "{err}");
    }
}
