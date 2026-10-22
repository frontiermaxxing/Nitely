//! Goal branches and checkpoint lands (docs/verification-settings.md, "The landing model"; NIT-495).
//!
//! A goal can have a branch, `goal-<id>`, made from the integration branch when its first item
//! starts. Its items branch from it (`playbook::start_task`) and land into it (`land::land`, the
//! same `run` as a land on main) with item checks only: compile, lint and the affected tests, no
//! review and no pull request. The goal's other items get each one at once.
//!
//! After every land on main, main is merged into each open goal branch (`flow_main_in`): a merge,
//! never a rebase, since the branch is shared. A conflict, or item checks that fail after the merge,
//! turns the branch red and files an issue on the goal naming the item that last touched the files;
//! a red branch holds only its own goal's item lands, until one that contains that main lands.
//!
//! A checkpoint lands the goal branch into main as one change: an ordinary issue on the goal whose
//! branch starts at the goal branch's head, so submit reviews the whole slice once and opens one
//! pull request, and land runs the full gate once (in CI when the playbook sends it there) and
//! merges main in rather than rebasing. When that gate fails, `bisect` gates the items' commits on
//! the goal branch to find the one at fault, reverts it there, sends it back, and opens the next
//! checkpoint without it.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord, NewIssue};
use crate::playbook::{self, git, git_remote, GateResult, Playbook};

/// Items since the last checkpoint after which the next one is due, unless the goal says otherwise.
pub const CHECKPOINT_ITEMS: i64 = 10;
/// A checkpoint is due once its oldest waiting item is this old, however few there are.
pub const CHECKPOINT_AGE_SECS: i64 = 24 * 60 * 60;

/// A goal's branch: on (`created_at` None until its first item starts), green or red, and where
/// its last checkpoint left it.
#[derive(Debug, Clone, Serialize)]
pub struct GoalBranch {
    pub goal: i64,
    /// The repo it lives in, as `paths::normalize` spells it.
    pub repo: String,
    pub branch: String,
    pub created_at: Option<i64>,
    /// The commit it was made from (or, when adopted, where it left main).
    pub created_from: Option<String>,
    /// green or red.
    pub state: String,
    pub red_reason: Option<String>,
    /// The main commit whose merge turned it red: an item land that contains it turns it green.
    pub red_main: Option<String>,
    /// The issue filed on the goal to fix it.
    pub red_issue: Option<String>,
    pub checkpoint_items: i64,
    /// Its head at the last checkpoint that landed.
    pub checkpoint_head: Option<String>,
    pub checkpoint_at: Option<i64>,
    pub closed_at: Option<i64>,
}

const COLUMNS: &str = "goal_id, repo, branch, created_at, created_from, state, red_reason, red_main, red_issue, checkpoint_items, checkpoint_head, checkpoint_at, closed_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<GoalBranch> {
    Ok(GoalBranch {
        goal: r.get(0)?,
        repo: r.get(1)?,
        branch: r.get(2)?,
        created_at: r.get(3)?,
        created_from: r.get(4)?,
        state: r.get(5)?,
        red_reason: r.get(6)?,
        red_main: r.get(7)?,
        red_issue: r.get(8)?,
        checkpoint_items: r.get(9)?,
        checkpoint_head: r.get(10)?,
        checkpoint_at: r.get(11)?,
        closed_at: r.get(12)?,
    })
}

/// One item that landed on a goal branch: the commits it added there.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub issue: String,
    pub from: String,
    pub to: String,
    pub landed_at: i64,
    pub checkpoint: Option<i64>,
    pub reverted_at: Option<i64>,
}

/// A checkpoint: the issue that lands the goal branch at `head` into main.
#[derive(Debug, Clone, Serialize)]
pub struct Checkpoint {
    pub id: i64,
    pub goal: i64,
    pub issue: String,
    pub head: String,
    /// open, landed or sent_back.
    pub state: String,
    pub opened_at: i64,
    pub ended_at: Option<i64>,
    /// Full gates its bisect ran.
    pub gates: i64,
    pub note: Option<String>,
}

const CHECKPOINT_COLUMNS: &str = "id, goal_id, issue_key, head, state, opened_at, ended_at, gates, note";

fn checkpoint_row(r: &rusqlite::Row) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        id: r.get(0)?,
        goal: r.get(1)?,
        issue: r.get(2)?,
        head: r.get(3)?,
        state: r.get(4)?,
        opened_at: r.get(5)?,
        ended_at: r.get(6)?,
        gates: r.get(7)?,
        note: r.get(8)?,
    })
}

/// What the goal row shows about the branch.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub branch: String,
    /// Made yet (its first item started).
    pub created: bool,
    /// green or red.
    pub state: String,
    pub red_reason: Option<String>,
    pub red_issue: Option<String>,
    /// Items landed on it since the last checkpoint, not reverted.
    pub items: i64,
    /// When the next checkpoint is due, in words: "at 10 items (4 to go)", "due now", or the open
    /// checkpoint's issue.
    pub next_checkpoint: String,
    pub checkpoint_issue: Option<String>,
    pub checkpoint_at: Option<i64>,
    /// What landing through it cost: items landed into it, checkpoints landed, and the full runs
    /// and reviews those checkpoints took (NIT-495's before-and-after measure).
    pub landed_items: i64,
    pub checkpoints: i64,
    pub full_runs: i64,
    pub reviews: i64,
}

impl Db {
    /// The goal's branch, while it is open.
    pub fn goal_branch(&self, goal: i64) -> Result<Option<GoalBranch>> {
        Ok(self.conn.query_row(&format!("SELECT {COLUMNS} FROM goal_branch WHERE goal_id = ?1 AND closed_at IS NULL"), [goal], row).optional()?)
    }

    /// The open goal branches of a repo.
    pub fn goal_branches_in(&self, root: &Path) -> Result<Vec<GoalBranch>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM goal_branch WHERE repo = ?1 AND closed_at IS NULL ORDER BY goal_id"))?;
        let rows = stmt.query_map([repo_key(root)], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Turns the goal's branch on, in `root`. `branch` adopts a branch that already exists (made by
    /// hand, say), or names the new one; the default is `goal-<id>`. Turning it on again changes
    /// only the checkpoint size.
    pub fn enable_goal_branch(&self, goal: i64, root: &Path, branch: Option<&str>, checkpoint_items: Option<i64>, actor: &Actor) -> Result<GoalBranch> {
        let g = self.goal(goal)?.with_context(|| format!("no goal {goal}"))?;
        if g.state != "active" {
            bail!("goal {goal} is {}; only an active goal gets a branch", g.state);
        }
        if checkpoint_items.is_some_and(|n| n < 1) {
            bail!("a checkpoint takes at least one item");
        }
        if let Some(open) = self.goal_branch(goal)? {
            if branch.is_some_and(|b| b != open.branch) {
                bail!("goal {goal} already has the branch {}", open.branch);
            }
            if let Some(n) = checkpoint_items {
                self.conn.execute("UPDATE goal_branch SET checkpoint_items = ?2 WHERE goal_id = ?1", params![goal, n])?;
            }
            return Ok(self.goal_branch(goal)?.expect("open"));
        }
        let name = branch.map(str::to_string).unwrap_or_else(|| format!("goal-{goal}"));
        git(root, &["check-ref-format", "--branch", &name]).with_context(|| format!("{name} is not a branch name git accepts"))?;
        let pb = Playbook::load(root)?;
        if name == pb.integration_branch() {
            bail!("the goal branch cannot be the integration branch {name}");
        }
        // A branch that is there already is adopted: its fork from main is where the first
        // checkpoint starts counting.
        if playbook::has_origin(root) {
            git_remote(root, &["fetch", "--quiet", "origin", &name]).ok();
        }
        let exists = rev(root, &format!("refs/heads/{name}")).ok().or_else(|| rev(root, &format!("refs/remotes/origin/{name}")).ok());
        let (created_at, created_from) = match &exists {
            Some(head) => {
                let fork = git(root, &["merge-base", pb.integration_branch(), head])?.trim().to_string();
                if git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_err() {
                    git(root, &["branch", &name, head])?;
                }
                (Some(now()), Some(fork))
            }
            None => (None, None),
        };
        // A goal turned off and on again starts a new row; the old one stays as history.
        self.conn.execute("DELETE FROM goal_branch WHERE goal_id = ?1 AND closed_at IS NOT NULL", [goal])?;
        self.conn.execute(
            "INSERT INTO goal_branch (goal_id, repo, branch, created_at, created_from, checkpoint_items, checkpoint_head, enabled_by, enabled_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5, ?7, ?8)",
            params![goal, repo_key(root), name, created_at, created_from, checkpoint_items.unwrap_or(CHECKPOINT_ITEMS), actor.id(), now()],
        )?;
        if let (Some(head), Some(fork)) = (&exists, &created_from) {
            adopt_items(self, root, goal, fork, head)?;
        }
        let said = match &exists {
            Some(_) => format!("Goal {goal} lands its items into {name}, which is there already; checkpoints land it into {}", pb.integration_branch()),
            None => format!("Goal {goal} lands its items into {name}, made from {} when its first item starts; checkpoints land it into {}", pb.integration_branch(), pb.integration_branch()),
        };
        self.log_event(actor, None, Some(&root.to_string_lossy()), "goal_branch", &said)?;
        Ok(self.goal_branch(goal)?.expect("just made"))
    }

    /// Turns the goal's branch off. Refused while items wait on it for a checkpoint: they would
    /// never reach main.
    pub fn disable_goal_branch(&self, goal: i64, actor: &Actor) -> Result<()> {
        let gb = self.goal_branch(goal)?.with_context(|| format!("goal {goal} has no branch"))?;
        let waiting = self.goal_items_waiting(goal)?;
        if !waiting.is_empty() {
            bail!(
                "{} holds {} item{} no checkpoint has landed yet ({}); land a checkpoint first",
                gb.branch,
                waiting.len(),
                if waiting.len() == 1 { "" } else { "s" },
                waiting.iter().map(|i| i.issue.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        self.conn.execute("UPDATE goal_branch SET closed_at = ?2, turned_off = 1 WHERE goal_id = ?1 AND closed_at IS NULL", params![goal, now()])?;
        self.log_event(actor, None, None, "goal_branch", &format!("Goal {goal}'s items land on main again; {} is closed", gb.branch))?;
        Ok(())
    }

    /// How many of the goal's issues are open, besides `except` and the goal's checkpoints.
    pub fn goal_open_items(&self, goal: i64, except: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM issue WHERE goal = ?1 AND state_type NOT IN ('completed', 'canceled') AND key != ?2 COLLATE NOCASE
                AND key NOT IN (SELECT issue_key FROM goal_checkpoint WHERE goal_id = ?1)",
            params![goal, except],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Whether the goal's branch was turned off by hand (disable_goal_branch), not closed by its
    /// goal's last checkpoint: the closed row stays as its history.
    pub fn goal_branch_turned_off(&self, goal: i64) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM goal_branch WHERE goal_id = ?1 AND closed_at IS NOT NULL AND turned_off = 1)", [goal], |r| r.get(0))?)
    }

    /// The items on the goal's branch that no checkpoint has landed yet, oldest first.
    pub fn goal_items_waiting(&self, goal: i64) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, issue_key, from_sha, to_sha, landed_at, checkpoint_id, reverted_at FROM goal_branch_item
              WHERE goal_id = ?1 AND checkpoint_id IS NULL AND reverted_at IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([goal], item_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The checkpoint whose issue is `key`.
    pub fn checkpoint_of_issue(&self, key: &str) -> Result<Option<Checkpoint>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {CHECKPOINT_COLUMNS} FROM goal_checkpoint WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 1"), [key], checkpoint_row)
            .optional()?)
    }

    /// The goal's checkpoint that is open (not landed or sent back), when its issue is too.
    pub fn open_checkpoint(&self, goal: i64) -> Result<Option<Checkpoint>> {
        let cp = self.conn.query_row(&format!("SELECT {CHECKPOINT_COLUMNS} FROM goal_checkpoint WHERE goal_id = ?1 AND state = 'open' ORDER BY id DESC LIMIT 1"), [goal], checkpoint_row).optional()?;
        match cp {
            Some(cp) if self.issue(&cp.issue)?.is_some_and(|i| i.is_open()) => Ok(Some(cp)),
            _ => Ok(None),
        }
    }

    /// The goal branch an issue lands into: its goal's open branch, unless it is that goal's
    /// checkpoint (which lands into main).
    pub fn goal_branch_of_issue(&self, key: &str) -> Result<Option<GoalBranch>> {
        let Some(issue) = self.issue(key)? else { return Ok(None) };
        let Some(goal) = issue.goal else { return Ok(None) };
        if self.checkpoint_of_issue(&issue.key)?.is_some() {
            return Ok(None);
        }
        self.goal_branch(goal)
    }

    /// The goal row's view of the branch.
    pub fn goal_branch_summary(&self, goal: i64) -> Result<Option<Summary>> {
        let Some(gb) = self.goal_branch(goal)? else { return Ok(None) };
        let waiting = self.goal_items_waiting(goal)?;
        let open = self.open_checkpoint(goal)?;
        let items = waiting.len() as i64;
        let next_checkpoint = match &open {
            Some(cp) => format!("{} is landing it", cp.issue),
            None if items == 0 => format!("at {} items", gb.checkpoint_items),
            None if due(&gb, &waiting, now(), self.goal_open_items(goal, "")?).is_some() => "due now".into(),
            None => format!("at {} items ({} to go)", gb.checkpoint_items, gb.checkpoint_items - items),
        };
        let (landed_items, checkpoints, full_runs): (i64, i64, i64) = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM goal_branch_item WHERE goal_id = ?1 AND reverted_at IS NULL),
                    (SELECT COUNT(*) FROM goal_checkpoint WHERE goal_id = ?1 AND state = 'landed'),
                    (SELECT COUNT(*) + COALESCE(SUM(gates), 0) FROM goal_checkpoint WHERE goal_id = ?1 AND state != 'open')",
            [goal],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let reviews: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM review WHERE issue_key IN (SELECT issue_key FROM goal_checkpoint WHERE goal_id = ?1)",
            [goal],
            |r| r.get(0),
        )?;
        Ok(Some(Summary {
            branch: gb.branch,
            created: gb.created_at.is_some(),
            state: gb.state,
            red_reason: gb.red_reason,
            red_issue: gb.red_issue,
            items,
            next_checkpoint,
            checkpoint_issue: open.map(|c| c.issue),
            checkpoint_at: gb.checkpoint_at,
            landed_items,
            checkpoints,
            full_runs,
            reviews,
        }))
    }

    fn set_green(&self, goal: i64) -> Result<()> {
        self.conn.execute("UPDATE goal_branch SET state = 'green', red_reason = NULL, red_main = NULL, red_issue = NULL WHERE goal_id = ?1", [goal])?;
        Ok(())
    }
}

/// The items of a branch adopted with work on it already (landed by hand): its first-parent
/// commits since it left main, one item per run of commits whose subjects name the same issue
/// ("... (NIT-12)"). A commit that names none (a merge of main, say) goes with the item before it;
/// those before any named one are recorded under the branch's name. So the first checkpoint holds
/// them, and its bisect can find one of them.
fn adopt_items(db: &Db, root: &Path, goal: i64, fork: &str, head: &str) -> Result<()> {
    let project = db.goal(goal)?.map(|g| g.project).context("the goal went missing")?;
    let named = regex::Regex::new(&format!(r"\b({}-[0-9]+)\b", regex::escape(&project)))?;
    let log = git(root, &["log", "--first-parent", "--reverse", "--format=%H %P%x09%s", &format!("{fork}..{head}")])?;
    let mut items: Vec<(String, String, String)> = Vec::new(); // (issue, from, to)
    for line in log.lines().filter(|l| !l.trim().is_empty()) {
        let (ids, subject) = line.split_once('\t').unwrap_or((line, ""));
        let mut ids = ids.split_whitespace();
        let (Some(sha), Some(parent)) = (ids.next(), ids.next()) else { continue };
        let key = named.captures_iter(subject).last().map(|c| c[1].to_string());
        match (items.last_mut(), key) {
            (Some(last), Some(k)) if last.0 == k => last.2 = sha.to_string(),
            (Some(last), None) => last.2 = sha.to_string(),
            (_, key) => items.push((key.unwrap_or_else(|| "adopted".into()), parent.to_string(), sha.to_string())),
        }
    }
    let at = now();
    for (issue, from, to) in items {
        db.conn.execute(
            "INSERT INTO goal_branch_item (goal_id, issue_key, from_sha, to_sha, landed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![goal, issue, from, to, at],
        )?;
    }
    Ok(())
}

fn item_row(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    Ok(Item { id: r.get(0)?, issue: r.get(1)?, from: r.get(2)?, to: r.get(3)?, landed_at: r.get(4)?, checkpoint: r.get(5)?, reverted_at: r.get(6)? })
}

/// Why a checkpoint is due now, or None: the goal's item count reached, none of its items left
/// open (`open_items`, the goal's open issues other than its checkpoint and the one landing), or
/// its oldest waiting item a day old.
pub fn due(gb: &GoalBranch, waiting: &[Item], at: i64, open_items: usize) -> Option<String> {
    let first = waiting.first()?;
    if waiting.len() as i64 >= gb.checkpoint_items {
        return Some(format!("{} items landed on {} since its last checkpoint", waiting.len(), gb.branch));
    }
    if open_items == 0 {
        return Some(format!("goal {} has no open items left, and {} holds {} of them", gb.goal, gb.branch, waiting.len()));
    }
    (at - first.landed_at >= CHECKPOINT_AGE_SECS).then(|| format!("{} has waited on {} for a day", first.issue, gb.branch))
}

/// Who an issue Nitely files for a land is from: the vendor of the agent whose land it was, as
/// propose_playbook_change says.
fn source_of(actor: &Actor) -> String {
    match actor.id().split(':').next() {
        Some(v) if crate::issues::SOURCES.contains(&v) => v.to_string(),
        _ => "claude".into(),
    }
}

fn repo_key(root: &Path) -> String {
    crate::paths::normalize(&root.to_string_lossy())
}

fn rev(dir: &Path, name: &str) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")])?.trim().to_string())
}

fn is_ancestor(root: &Path, a: &str, b: &str) -> bool {
    git(root, &["merge-base", "--is-ancestor", a, b]).is_ok()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

/// The lock that makes item lands, main merges and reverts on one goal branch go one at a time,
/// beside the repo's land lock (which a goal branch never needs).
pub(crate) fn lock_path(root: &Path, goal: i64) -> Result<PathBuf> {
    Ok(crate::land::lock_path(root)?.with_file_name(format!("nitely-land-goal-{goal}.lock")))
}

/// Where a new item of the issue's goal starts: the goal branch (made now from main when this is
/// its first item), or, for a checkpoint, the goal branch's head it lands. None: from main.
pub(crate) fn start_point(db: &Db, root: &Path, pb: &Playbook, issue: &IssueRecord, actor: &Actor) -> Result<Option<(String, String)>> {
    if let Some(cp) = db.checkpoint_of_issue(&issue.key)? {
        // A checkpoint started later takes in what landed on the goal branch meanwhile.
        if let Some(gb) = db.goal_branch(cp.goal)?.filter(|g| g.state == "green" && g.created_at.is_some()) {
            if cp.state == "open" {
                let gb = prepare(db, root, pb, &gb, actor)?;
                let head = rev(root, &format!("refs/heads/{}", gb.branch))?;
                if head != cp.head {
                    db.conn.execute("UPDATE goal_checkpoint SET head = ?2 WHERE id = ?1", params![cp.id, head])?;
                }
                return Ok(Some((head.clone(), format!("{} at {}", gb.branch, short(&head)))));
            }
        }
        return Ok(Some((cp.head.clone(), format!("the checkpoint's head {}", short(&cp.head)))));
    }
    // The project's mode lands goal items through goal branches: a goal's first item turns its
    // branch on (NIT-561); before, only the goal_branch tool did, so no mode ever engaged one.
    if let Some(goal) = issue.goal {
        let options = crate::project_modes::for_issue(db, &issue.key)?.map(|m| m.options()).unwrap_or_default();
        let active = db.goal(goal)?.is_some_and(|g| g.state == "active");
        let open = db.goal_branch(goal)?.is_some();
        let routes = goal_items_route(options.goal_branches, !pb.submodules.is_empty()).is_ok();
        if let Some(items) = engage(routes, options.checkpoint.as_ref().map(|c| c.items), active, open, db.goal_branch_turned_off(goal)?) {
            db.enable_goal_branch(goal, root, None, Some(items), actor)?;
        }
    }
    let Some(gb) = route_of_issue(db, root, &issue.key, None)? else { return Ok(None) };
    let gb = prepare(db, root, pb, &gb, actor)?;
    Ok(Some((gb.branch.clone(), gb.branch)))
}

/// Whether a goal item lands through its goal's branch (NIT-563), decided the same way when it
/// starts, is submitted, lands and says where it lands: the branch is open, and either the item
/// already started from it (its branch holds the goal branch's commits, which must not reach main
/// with it) or the mode routes goal items through goal branches here.
pub fn item_route(open: bool, mode_routes: bool, started_from_goal: bool) -> bool {
    open && (started_from_goal || mode_routes)
}

/// The goal branch `key`'s change lands into from `root` (`item_route`), or None for main.
/// `branch` is the item's own branch, once it has one. A project whose mode the person never chose
/// keeps a goal branch opened by hand.
pub fn route_of_issue(db: &Db, root: &Path, key: &str, branch: Option<&str>) -> Result<Option<GoalBranch>> {
    let Some(gb) = db.goal_branch_of_issue(key)?.filter(|g| same_repo(g, root)) else { return Ok(None) };
    let pb = Playbook::load(root)?;
    let mode_routes = match crate::project_modes::for_issue(db, key)? {
        Some(m) if m.explicit => goal_items_route(m.options().goal_branches, !pb.submodules.is_empty()).is_ok(),
        _ => true,
    };
    let started = branch.is_some_and(|b| started_from(root, b, &gb.branch, pb.integration_branch()));
    Ok(item_route(true, mode_routes, started).then_some(gb))
}

/// Whether `item` started from `goal_branch`: where they meet holds commits `main` does not.
fn started_from(root: &Path, item: &str, goal_branch: &str, main: &str) -> bool {
    let Ok(met) = crate::playbook::git(root, &["merge-base", item, goal_branch]) else { return false };
    crate::playbook::git(root, &["rev-list", "--count", &format!("{main}..{}", met.trim())]).is_ok_and(|n| n.trim() != "0")
}

/// Whether a mode's goal items go through goal branches in a repo, or why they land on main: the
/// mode must say so (an item gets the item review it asks for on the way in: submit.rs,
/// `submit_into_goal`, NIT-563); a repo with managed submodules lands its goal items on main for
/// now (land.rs, item_hop).
pub fn goal_items_route(goal_branches: Option<bool>, submodules: bool) -> Result<(), &'static str> {
    if goal_branches != Some(true) {
        return Err("Each goal item lands on main like any other change");
    }
    if submodules {
        return Err("A repo with managed submodules lands goal items on main for now");
    }
    Ok(())
}

/// Whether a new item of a goal turns the goal's branch on, and with what checkpoint size: when
/// the mode's goal items go through goal branches here (`routes`), the goal is active, it has no
/// branch open, and nobody turned its branch off by hand. None: it lands as it would otherwise.
pub(crate) fn engage(routes: bool, checkpoint_items: Option<u32>, active: bool, open: bool, turned_off: bool) -> Option<i64> {
    (routes && active && !open && !turned_off).then(|| checkpoint_items.map_or(CHECKPOINT_ITEMS, i64::from))
}

/// The branch `issue`'s change lands into from `root`: its goal's branch, or the integration
/// branch (a checkpoint, an item of a goal without one, any other issue).
pub(crate) fn lands_into(db: &Db, root: &Path, pb: &Playbook, issue: &str) -> Result<String> {
    let branch = crate::land::branch_of(root, issue).ok().map(|(b, _)| b);
    Ok(route_of_issue(db, root, issue, branch.as_deref())?.map_or_else(|| pb.integration_branch().to_string(), |g| g.branch))
}

fn same_repo(gb: &GoalBranch, root: &Path) -> bool {
    gb.repo == repo_key(root)
}

/// The goal branch made (from the integration branch) when it is not yet, and the local branch
/// brought up to origin's when origin is ahead. Refused when the two have parted.
pub(crate) fn prepare(db: &Db, root: &Path, pb: &Playbook, gb: &GoalBranch, actor: &Actor) -> Result<GoalBranch> {
    let local = format!("refs/heads/{}", gb.branch);
    let remote = format!("refs/remotes/origin/{}", gb.branch);
    let origin = playbook::has_origin(root);
    if origin {
        git_remote(root, &["fetch", "--quiet", "origin", &gb.branch]).ok();
    }
    let have_local = rev(root, &local).ok();
    let have_remote = if origin { rev(root, &remote).ok() } else { None };
    match (have_local, have_remote) {
        (None, None) => {
            let base = pb.integration_branch();
            let from = rev(root, base)?;
            git(root, &["branch", &gb.branch, &from])?;
            if origin {
                git_remote(root, &["push", "--quiet", "origin", &format!("{from}:{local}")]).with_context(|| format!("pushing the new {}", gb.branch))?;
            }
            db.conn.execute("UPDATE goal_branch SET created_at = ?2, created_from = ?3, checkpoint_head = COALESCE(checkpoint_head, ?3) WHERE goal_id = ?1", params![gb.goal, now(), from])?;
            db.log_event(actor, None, Some(&root.to_string_lossy()), "goal_branch", &format!("Made {} from {base} at {} for goal {}'s first item", gb.branch, short(&from), gb.goal))?;
        }
        (None, Some(remote_head)) => {
            git(root, &["branch", &gb.branch, &remote_head])?;
        }
        (Some(l), Some(r)) if l != r => {
            if is_ancestor(root, &l, &r) {
                git(root, &["update-ref", &local, &r, &l])?;
            } else if !is_ancestor(root, &r, &l) {
                bail!("{} here and on origin have parted ({} and {}); bring them together by hand before landing into it", gb.branch, short(&l), short(&r));
            }
        }
        _ => {}
    }
    if gb.created_at.is_none() {
        db.conn.execute("UPDATE goal_branch SET created_at = COALESCE(created_at, ?2) WHERE goal_id = ?1", params![gb.goal, now()])?;
    }
    Ok(db.goal_branch(gb.goal)?.unwrap_or_else(|| gb.clone()))
}

/// Moves the goal branch from `from` to `to` (refused when it moved meanwhile) and pushes it.
/// A push that fails is a warning: the branch moved here, and the next push takes it along.
pub(crate) fn advance(root: &Path, gb: &GoalBranch, from: &str, to: &str) -> Result<Option<String>> {
    let local = format!("refs/heads/{}", gb.branch);
    git(root, &["update-ref", &local, to, from]).with_context(|| format!("{} moved while this land ran; land again", gb.branch))?;
    if !playbook::has_origin(root) {
        return Ok(None);
    }
    Ok(git_remote(root, &["push", "--quiet", "origin", &format!("{to}:{local}")]).err().map(|e| format!("{} moved here but the push failed: {}", gb.branch, crate::land::first_line(&e))))
}

/// The detached worktree the goal's merges, reverts and bisect gates run in, at `at`.
fn work_tree(root: &Path, pb: &Playbook, goal: i64, at: &str) -> Result<PathBuf> {
    let path = crate::light::lanes_dir(pb, root).join(format!("goal-{goal}"));
    if !path.join(".git").exists() {
        std::fs::create_dir_all(path.parent().expect("lanes dir"))?;
        git(root, &["worktree", "prune"]).ok();
        if path.exists() {
            crate::unlink::remove_tree(&path).with_context(|| format!("clearing {}", path.display()))?;
        }
        git(root, &["worktree", "add", "--detach", &path.to_string_lossy(), at]).context("making the goal branch's worktree")?;
        return Ok(path);
    }
    git(&path, &["merge", "--abort"]).ok();
    git(&path, &["revert", "--abort"]).ok();
    git(&path, &["checkout", "--quiet", "--force", "--detach", at]).context("moving the goal branch's worktree")?;
    git(&path, &["clean", "-ffdq"]).ok();
    Ok(path)
}

/// The item checks of the change from `base` to the worktree's HEAD: `git diff --check`, the
/// playbook's quick commands (`[verification] basic`) and the tests the affected-test map selects.
/// A change the map cannot place, or a playbook with no quick commands, runs the full gate: the
/// risk floors hold on every hop. The plan is the item's verification evidence; it never counts as
/// full evidence for main.
pub(crate) fn item_gate(db: &Db, root: &Path, worktree: &Path, key: Option<&str>, base: &str, branch: &str, actor: &Actor) -> Result<GateResult> {
    let head = rev(worktree, "HEAD")?;
    let base = rev(worktree, base)?;
    let pb = Playbook::load(root)?;
    let selection = crate::affected_tests::select_with_policy(worktree, &base, &head, &base)?;
    if selection.full || pb.verification.basic.is_empty() {
        let mut gate = playbook::run_gate_as(db, root, worktree, key, actor, crate::lanes::Kind::Land)?;
        let why = if pb.verification.basic.is_empty() { "the playbook has no quick commands ([verification] basic)".to_string() } else { selection.reasons.first().cloned().unwrap_or_else(|| "the affected-test map cannot place every file".into()) };
        gate.warnings.push(format!("Item checks into {branch} ran the full gate: {why}"));
        return Ok(gate);
    }
    let mut commands = vec![format!("git diff --check {base} {head}")];
    commands.extend(pb.verification.basic.iter().cloned());
    for check in &selection.checks {
        if let Some(d) = &check.discovery {
            commands.extend([d.command.clone(), d.ignored_command.clone()]);
        }
        commands.push(check.command.clone());
    }
    let tree = git(worktree, &["rev-parse", "HEAD^{tree}"])?.trim().to_string();
    let effective = if selection.components.len() == 1 { "contained" } else { "standard" };
    let plan = crate::verification::Plan {
        commands,
        audit_percent: 0,
        affected: Some(selection),
        audit_sample: None,
        policy_fingerprint: None,
        rule_ids: vec![],
        head: head.clone(),
        tree,
        base: base.clone(),
        policy_commit: base.clone(),
        classifier: "goal-item-v1".into(),
        declared: None,
        declared_by: None,
        declared_source: None,
        declared_at: None,
        effective: effective.into(),
        depth: "Item".into(),
        shadow: false,
        reasons: vec![format!("An item of a goal lands into {branch} with item checks; the goal's checkpoint runs the full gate and the review before main.")],
        files: vec![],
        enforcement: format!("Item checks into {branch}: compile, lint and the affected tests. Not evidence for main."),
    };
    let mut gate = playbook::run_planned_gate(db, root, worktree, key, actor, crate::lanes::Kind::Land, &plan)?;
    gate.plan = Some(plan);
    Ok(gate)
}

/// Records what an item landed on its goal branch, and opens the goal's checkpoint when one is due
/// and none is open.
pub(crate) fn item_landed(db: &mut Db, gb: &GoalBranch, key: &str, from: &str, to: &str, actor: &Actor) -> Result<Option<String>> {
    db.conn.execute(
        "INSERT INTO goal_branch_item (goal_id, issue_key, from_sha, to_sha, landed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![gb.goal, key, from, to, now()],
    )?;
    if gb.state == "red" && gb.red_main.as_deref().is_some_and(|m| !m.is_empty()) {
        // It got here only because it contains the main that turned the branch red.
        db.set_green(gb.goal)?;
        db.log_event(actor, Some(key), None, "goal_branch", &format!("{} is green again: {key} brought main in", gb.branch))?;
    }
    if db.open_checkpoint(gb.goal)?.is_some() {
        return Ok(None);
    }
    let waiting = db.goal_items_waiting(gb.goal)?;
    let Some(why) = due(gb, &waiting, now(), db.goal_open_items(gb.goal, key)?) else { return Ok(None) };
    let cp = open_checkpoint(db, gb, to, actor)?;
    Ok(Some(format!("{why}, so {} is filed to land it into main: start_task, submit and land it", cp.issue)))
}

/// Files the issue that lands the goal branch at `head` into main, and records it as the goal's
/// open checkpoint.
fn open_checkpoint(db: &mut Db, gb: &GoalBranch, head: &str, actor: &Actor) -> Result<Checkpoint> {
    let goal = db.goal(gb.goal)?.with_context(|| format!("no goal {}", gb.goal))?;
    let waiting = db.goal_items_waiting(gb.goal)?;
    let mut list = String::new();
    for item in &waiting {
        let title = db.issue(&item.issue)?.map(|i| i.title).unwrap_or_default();
        list.push_str(&format!("- {}: {title}\n", item.issue));
    }
    let n = waiting.len();
    let issue = db.create_issue(
        &NewIssue {
            project: goal.project.clone(),
            title: format!("Checkpoint: {} into main ({n} item{})", gb.branch, if n == 1 { "" } else { "s" }),
            body: format!(
                "Lands {} into main as one change: one full verification, one review of the slice, one pull request. \
                 start_task branches it from the goal branch's head; submit reviews the slice; land merges main in, runs the full gate, \
                 and, when that fails, finds the item at fault, reverts it on {} and sends it back.\n\nItems in this slice:\n{list}",
                gb.branch, gb.branch
            ),
            kind: "improvement".into(),
            source: source_of(actor),
            evidence: format!("{n} item{} landed on {} with item checks since its last checkpoint.", if n == 1 { "" } else { "s" }, gb.branch),
            priority: 2,
            allow_similar: true,
            labels: vec!["checkpoint".into()],
            goal: Some(gb.goal),
            review_level: Some("sensitive".into()),
            review_source: Some("planner".into()),
            ..Default::default()
        },
        actor,
    )?;
    db.conn.execute(
        "INSERT INTO goal_checkpoint (goal_id, issue_key, head, opened_at) VALUES (?1, ?2, ?3, ?4)",
        params![gb.goal, issue.key, head, now()],
    )?;
    Ok(db.checkpoint_of_issue(&issue.key)?.expect("just recorded"))
}

/// What `checkpoint` opened: the issue, and its worktree when it was started.
#[derive(Debug, Clone, Serialize)]
pub struct Opened {
    pub checkpoint: Checkpoint,
    pub items: Vec<String>,
    pub started: Option<playbook::Started>,
}

/// Opens the goal's checkpoint now (the coordinator calls a coherent slice), with main merged into
/// the goal branch first, and starts its worktree for `vendor` when given. An open one is returned
/// as it is.
pub fn checkpoint(db: &mut Db, config: &crate::Config, goal: i64, dir: &str, vendor: Option<&str>, actor: &Actor) -> Result<Opened> {
    let gb = db.goal_branch(goal)?.with_context(|| format!("goal {goal} has no branch: its items land on main"))?;
    let project = db.goal(goal)?.map(|g| g.project);
    let root = playbook::repo_for_issue(config, project.as_deref(), dir)?;
    if !same_repo(&gb, &root) {
        bail!("goal {goal}'s branch lives in {}, not {}", gb.repo, root.display());
    }
    let pb = Playbook::load(&root)?;
    let cp = match db.open_checkpoint(goal)? {
        Some(cp) => cp,
        None => {
            if gb.created_at.is_none() {
                bail!("{} is not made yet: no item of goal {goal} has started", gb.branch);
            }
            let lock = crate::land::Lock::try_take(&lock_path(&root, goal)?, &format!("goal {goal} checkpoint"), false)?
                .with_context(|| format!("another land into {} is running; open the checkpoint once it is done", gb.branch))?;
            let gb = prepare(db, &root, &pb, &gb, actor)?;
            if let Flow::Red(why) = merge_main(db, &root, &pb, &gb, actor)? {
                bail!("{} cannot take main: {why}", gb.branch);
            }
            let gb = db.goal_branch(goal)?.expect("open");
            if gb.state == "red" {
                bail!("{} is red ({}); fix it before a checkpoint", gb.branch, gb.red_reason.unwrap_or_default());
            }
            if db.goal_items_waiting(goal)?.is_empty() {
                bail!("nothing landed on {} since its last checkpoint", gb.branch);
            }
            let head = rev(&root, &format!("refs/heads/{}", gb.branch))?;
            let cp = open_checkpoint(db, &gb, &head, actor)?;
            drop(lock);
            cp
        }
    };
    let items = db.goal_items_waiting(goal)?.into_iter().filter(|i| is_ancestor(&root, &i.to, &cp.head)).map(|i| i.issue).collect();
    let started = match vendor {
        Some(v) => {
            let issue = db.issue(&cp.issue)?.context("the checkpoint's issue went missing")?;
            Some(playbook::start_task(db, &root, &issue, v, actor)?)
        }
        None => None,
    };
    let cp = db.checkpoint_of_issue(&cp.issue)?.expect("recorded");
    Ok(Opened { checkpoint: cp, items, started })
}

/// What merging main into a goal branch did.
#[derive(Debug, Clone, PartialEq)]
pub enum Flow {
    /// It holds main already.
    Current,
    /// It moved to main: nothing of its own since main last took it in.
    FastForwarded(String),
    /// Main merged in and the item checks passed: the new head.
    Merged(String),
    /// The merge conflicted or its checks failed; the branch is red now.
    Red(String),
}

/// Merges the integration branch into the goal branch, checks the merge at item level, and moves
/// the branch; on a conflict or failed checks, turns it red instead and files the fix on the goal.
/// The caller holds the goal's lock.
pub(crate) fn merge_main(db: &mut Db, root: &Path, pb: &Playbook, gb: &GoalBranch, actor: &Actor) -> Result<Flow> {
    let base = pb.integration_branch();
    let main = rev(root, base)?;
    let local = format!("refs/heads/{}", gb.branch);
    let head = rev(root, &local)?;
    if is_ancestor(root, &main, &head) {
        return Ok(Flow::Current);
    }
    if gb.state == "red" && gb.red_main.as_deref() == Some(main.as_str()) {
        return Ok(Flow::Red(gb.red_reason.clone().unwrap_or_default()));
    }
    if is_ancestor(root, &head, &main) {
        if let Some(warning) = advance(root, gb, &head, &main)? {
            db.log_event(actor, None, None, "goal_branch", &warning)?;
        }
        if gb.state == "red" {
            db.set_green(gb.goal)?;
        }
        return Ok(Flow::FastForwarded(main));
    }
    let wt = work_tree(root, pb, gb.goal, &head)?;
    let message = format!("Merge {base} into {}", gb.branch);
    if git(&wt, &["merge", "--no-ff", "--no-edit", "-m", &message, &main]).is_err() {
        let conflicts: Vec<String> = git(&wt, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default().lines().map(str::to_string).collect();
        git(&wt, &["merge", "--abort"]).ok();
        let why = if conflicts.is_empty() { format!("merging {base} at {} failed", short(&main)) } else { format!("{base} at {} conflicts in {}", short(&main), conflicts.join(", ")) };
        turn_red(db, root, gb, &main, &why, &conflicts, actor)?;
        return Ok(Flow::Red(why));
    }
    let gate = item_gate(db, root, &wt, None, &head, &gb.branch, actor)?;
    if !(gate.passed || gate.skipped.is_some()) {
        let failed = gate.results.iter().find(|r| !r.ok).map(|r| format!(" at `{}`", r.command)).unwrap_or_default();
        let why = format!("item checks failed{failed} after merging {base} at {}", short(&main));
        let named = files_named(&gate);
        turn_red(db, root, gb, &main, &why, &named, actor)?;
        return Ok(Flow::Red(why));
    }
    let merged = rev(&wt, "HEAD")?;
    if let Some(warning) = advance(root, gb, &head, &merged)? {
        db.log_event(actor, None, None, "goal_branch", &warning)?;
    }
    if gb.state == "red" {
        db.set_green(gb.goal)?;
    }
    db.log_event(actor, None, None, "goal_branch", &format!("Merged {base} at {} into {}; item checks passed", short(&main), gb.branch))?;
    Ok(Flow::Merged(merged))
}

/// Paths a failed gate's output names, for finding the item that touched them.
fn files_named(gate: &GateResult) -> Vec<String> {
    gate.results
        .iter()
        .filter(|r| !r.ok)
        .flat_map(|r| r.output.lines().filter(|l| !l.starts_with("$ ")).map(|l| l.replace('\\', "/")).collect::<Vec<_>>())
        .flat_map(|l| l.split(|c: char| c.is_whitespace() || c == ':' || c == '(' || c == ')' || c == '\'' || c == '"').map(str::to_string).collect::<Vec<_>>())
        .filter(|w| w.contains('/') && w.contains('.'))
        .collect()
}

/// The newest waiting item whose commits touch any of `files`.
fn last_to_touch(db: &Db, root: &Path, goal: i64, files: &[String]) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
    }
    for item in db.goal_items_waiting(goal)?.iter().rev() {
        let touched = git(root, &["diff", "--name-only", "--no-renames", &item.from, &item.to]).unwrap_or_default();
        if touched.lines().any(|t| files.iter().any(|f| f == t || f.ends_with(&format!("/{t}")) || t.ends_with(f.as_str()))) {
            return Ok(Some(item.issue.clone()));
        }
    }
    Ok(None)
}

/// Turns the goal branch red over `main`, says so on the item that last touched the files, and
/// files the fix on the goal (once: a red branch already has its issue).
fn turn_red(db: &mut Db, root: &Path, gb: &GoalBranch, main: &str, why: &str, files: &[String], actor: &Actor) -> Result<()> {
    let culprit = last_to_touch(db, root, gb.goal, files)?;
    let open_fix = gb.red_issue.as_deref().map(|k| db.issue(k)).transpose()?.flatten().filter(|i| i.is_open());
    let fix = match open_fix {
        Some(i) => i.key,
        None => {
            let project = db.goal(gb.goal)?.map(|g| g.project).context("the goal went missing")?;
            let by = culprit.as_deref().map(|k| format!(" {k} last changed those files on the goal branch.")).unwrap_or_default();
            let filed = db.create_issue(
                &NewIssue {
                    project,
                    title: format!("{} cannot take main: {why}", gb.branch),
                    body: format!(
                        "Main moved, and merging it into {} {why}.{by} Until this lands, the goal's other items wait.\n\n\
                         To fix it: start_task this issue (it branches from {}), merge origin/main into it, resolve, commit, and land it. \
                         An item land that contains main at {} turns {} green again.",
                        gb.branch,
                        gb.branch,
                        short(main),
                        gb.branch
                    ),
                    kind: "bug".into(),
                    source: source_of(actor),
                    evidence: why.to_string(),
                    priority: 2,
                    allow_similar: true,
                    goal: Some(gb.goal),
                    review_level: Some("standard".into()),
                    review_source: Some("planner".into()),
                    ..Default::default()
                },
                actor,
            )?;
            filed.key
        }
    };
    db.conn.execute(
        "UPDATE goal_branch SET state = 'red', red_reason = ?2, red_main = ?3, red_issue = ?4 WHERE goal_id = ?1",
        params![gb.goal, why, main, fix],
    )?;
    if let Some(k) = culprit.as_ref().filter(|k| db.issue(k).ok().flatten().is_some()) {
        db.add_note(k, &format!("{} cannot take main: {why}. Your change last touched those files there; {fix} is filed to fix it.", gb.branch), "goal_branch", actor)?;
    }
    db.log_event(actor, Some(&fix), None, "goal_branch", &format!("{} is red: {why}", gb.branch))?;
    Ok(())
}

/// After a land on main: main merged into each open goal branch of the repo that does not hold it,
/// each under its goal's lock (a goal whose lock is held is skipped: its item land merges main in
/// itself). What happened, one line per goal branch that moved or turned red.
pub fn flow_main_in(db: &mut Db, root: &Path, actor: &Actor) -> Vec<String> {
    let Ok(branches) = db.goal_branches_in(root) else { return vec![] };
    let Ok(pb) = Playbook::load(root) else { return vec![] };
    let mut out = Vec::new();
    for gb in branches.into_iter().filter(|g| g.created_at.is_some()) {
        let Ok(Some(_lock)) = lock_path(root, gb.goal).and_then(|p| crate::land::Lock::try_take(&p, &format!("goal {} main merge", gb.goal), false)) else { continue };
        let flowed = prepare(db, root, &pb, &gb, actor).and_then(|gb| merge_main(db, root, &pb, &gb, actor));
        match flowed {
            Ok(Flow::Current) => {}
            Ok(Flow::FastForwarded(to)) => out.push(format!("{} moved to main at {}", gb.branch, short(&to))),
            Ok(Flow::Merged(to)) => out.push(format!("main merged into {} at {}", gb.branch, short(&to))),
            Ok(Flow::Red(why)) => out.push(format!("{} is red: {why}", gb.branch)),
            Err(e) => out.push(format!("main was not merged into {}: {e:#}", gb.branch)),
        }
    }
    out
}

/// Before an item lands into its goal branch: main merged in when it moved, and the red branch's
/// rule. A red branch takes only the item that contains the main that turned it red.
pub(crate) fn before_item_land(db: &mut Db, root: &Path, pb: &Playbook, gb: &GoalBranch, key: &str, item_head: &str, actor: &Actor) -> Result<GoalBranch> {
    let gb = prepare(db, root, pb, gb, actor)?;
    if gb.state == "green" {
        if let Flow::Red(why) = merge_main(db, root, pb, &gb, actor)? {
            let gb = db.goal_branch(gb.goal)?.expect("open");
            bail!("{} cannot take main ({why}), so it is red and its item lands wait; {} is filed to fix it", gb.branch, gb.red_issue.unwrap_or_default());
        }
        return Ok(db.goal_branch(gb.goal)?.expect("open"));
    }
    let fixes = gb.red_main.as_deref().is_some_and(|m| is_ancestor(root, m, item_head));
    if !fixes {
        bail!(
            "{} is red ({}), so its item lands wait until {} lands with main merged in; {key} lands after that",
            gb.branch,
            gb.red_reason.clone().unwrap_or_default(),
            gb.red_issue.clone().unwrap_or_else(|| "the fix".into())
        );
    }
    Ok(gb)
}

/// After a checkpoint landed on main: the goal branch's checkpoint moves to its head, its items
/// are marked landed, and a closed goal with nothing left waiting closes its branch.
pub(crate) fn checkpoint_landed(db: &mut Db, root: &Path, key: &str, actor: &Actor) -> Result<Option<String>> {
    let Some(cp) = db.checkpoint_of_issue(key)? else { return Ok(None) };
    if cp.state != "open" {
        return Ok(None);
    }
    // What landed is the checkpoint's branch, which start_task may have moved past the head first
    // recorded: the items it holds are those reachable from the branch's head.
    let landed = crate::land::branch_of(root, key).ok().and_then(|(b, _)| rev(root, &b).ok());
    let head = match landed {
        Some(h) if is_ancestor(root, &cp.head, &h) => h,
        _ => cp.head.clone(),
    };
    let waiting = db.goal_items_waiting(cp.goal)?;
    let mut n = 0;
    for item in waiting.iter().filter(|i| is_ancestor(root, &i.to, &head)) {
        db.conn.execute("UPDATE goal_branch_item SET checkpoint_id = ?2 WHERE id = ?1", params![item.id, cp.id])?;
        n += 1;
    }
    db.conn.execute("UPDATE goal_checkpoint SET state = 'landed', ended_at = ?2, head = ?3 WHERE id = ?1", params![cp.id, now(), head])?;
    db.conn.execute("UPDATE goal_branch SET checkpoint_head = ?2, checkpoint_at = ?3 WHERE goal_id = ?1", params![cp.goal, head, now()])?;
    let gb = db.goal_branch(cp.goal)?;
    let mut said = format!("Checkpoint landed {n} item{} of goal {} on main", if n == 1 { "" } else { "s" }, cp.goal);
    if let Some(gb) = gb {
        let goal_open = db.goal(cp.goal)?.is_some_and(|g| g.state == "active");
        let waiting = db.goal_items_waiting(cp.goal)?;
        if !goal_open && waiting.is_empty() {
            db.conn.execute("UPDATE goal_branch SET closed_at = ?2 WHERE goal_id = ?1", params![cp.goal, now()])?;
            said.push_str(&format!("; the goal is closed, so {} is too", gb.branch));
        } else if let Some(why) = due(&gb, &waiting, now(), db.goal_open_items(cp.goal, key)?) {
            // Items that landed on the branch while this checkpoint was open (the goal's last, say)
            // get the next one now: no later item would file it.
            let head = rev(root, &format!("refs/heads/{}", gb.branch))?;
            let next = open_checkpoint(db, &gb, &head, actor)?;
            said.push_str(&format!("; {why}, so {} is filed to land the rest", next.issue));
        }
    }
    db.log_event(actor, Some(key), None, "goal_branch", &said)?;
    Ok(Some(said))
}

/// After a gate of `key` failed (its submit's or its land's): when `key` is a checkpoint and the
/// gate did fail (not ran out of time), the bisect, and what it found or why it stopped.
pub(crate) fn after_failed_gate(db: &mut Db, root: &Path, key: &str, gate: &GateResult, actor: &Actor) -> Option<String> {
    if gate.passed || gate.skipped.is_some() || gate.timed_out().is_some() {
        return None;
    }
    if db.checkpoint_of_issue(key).ok()?.is_none_or(|cp| cp.state != "open") {
        return None;
    }
    let pb = Playbook::load(root).ok()?;
    Some(match bisect(db, root, &pb, key, gate, actor) {
        Ok(said) => said,
        Err(e) => format!("the checkpoint's bisect stopped: {e:#}"),
    })
}

/// When a checkpoint's full gate failed: gates the items' commits on the goal branch to find the
/// first that fails, reverts it there, sends it back to its agent, closes this checkpoint and opens
/// the next without it. One bad item in k costs at most ceil(log2(k + 1)) more full runs. When every
/// item's commit passes, only the merge with main fails, and nothing is reverted.
pub(crate) fn bisect(db: &mut Db, root: &Path, pb: &Playbook, key: &str, failed: &GateResult, actor: &Actor) -> Result<String> {
    let cp = db.checkpoint_of_issue(key)?.context("not a checkpoint")?;
    let gb = db.goal_branch(cp.goal)?.context("the goal's branch is closed")?;
    let items: Vec<Item> = db.goal_items_waiting(cp.goal)?.into_iter().filter(|i| is_ancestor(root, &i.to, &cp.head)).collect();
    if items.is_empty() {
        bail!("the checkpoint holds no item to bisect over");
    }
    let k = items.len();
    let wt = work_tree(root, pb, cp.goal, &items[k - 1].to)?;
    // Prefixes 1..=k are the items' commits; k + 1 is the checkpoint as it landed, which failed.
    let (mut lo, mut hi) = (0usize, k + 1);
    let mut failing = failed.clone();
    let mut gates = 0i64;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        git(&wt, &["checkout", "--quiet", "--force", "--detach", &items[mid - 1].to])?;
        db.log_event(actor, Some(key), None, "status", &format!("Bisecting the checkpoint: the full gate at {} ({mid} of {k} items)", items[mid - 1].issue))?;
        let g = playbook::run_gate_as(db, root, &wt, Some(key), actor, crate::lanes::Kind::Land)?;
        gates += 1;
        if g.timed_out().is_some() {
            bail!("the full gate at {} ran past the lane's time limit, which says nothing about it; land the checkpoint again", items[mid - 1].issue);
        }
        if g.passed || g.skipped.is_some() {
            lo = mid;
        } else {
            hi = mid;
            failing = g;
        }
    }
    db.conn.execute("UPDATE goal_checkpoint SET gates = gates + ?2 WHERE id = ?1", params![cp.id, gates])?;
    if hi == k + 1 {
        let said = format!("every item's commit on {} passes the full gate; only the merge with main fails, so nothing is reverted ({gates} full run{})", gb.branch, if gates == 1 { "" } else { "s" });
        db.conn.execute("UPDATE goal_checkpoint SET note = ?2 WHERE id = ?1", params![cp.id, said])?;
        return Ok(said);
    }
    let bad = &items[hi - 1];
    let output = failing.results.iter().find(|r| !r.ok).map(|r| format!("`{}`:\n{}", r.command, tail(&r.output, 30))).unwrap_or_default();
    // The revert, under the goal's lock: nothing else moves the goal branch meanwhile.
    let lock = crate::land::Lock::try_take(&lock_path(root, cp.goal)?, &format!("{key} bisect"), false)?
        .with_context(|| format!("another land into {} is running; the bisect found {} but could not revert it", gb.branch, bad.issue))?;
    let gb = prepare(db, root, pb, &gb, actor)?;
    let head = rev(root, &format!("refs/heads/{}", gb.branch))?;
    let wt = work_tree(root, pb, cp.goal, &head)?;
    let message = format!("Revert {} on {}: it fails the full gate at checkpoint {key}", bad.issue, gb.branch);
    let reverted = git(&wt, &["revert", "--no-commit", &format!("{}..{}", bad.from, bad.to)]).and_then(|_| git(&wt, &["commit", "--quiet", "-m", &message]));
    if let Err(e) = reverted {
        git(&wt, &["revert", "--abort"]).ok();
        bail!("the bisect found {}, but reverting it on {} did not apply cleanly ({}); revert it there by hand", bad.issue, gb.branch, crate::land::first_line(&e));
    }
    let to = rev(&wt, "HEAD")?;
    let warning = advance(root, &gb, &head, &to)?;
    drop(lock);
    db.conn.execute("UPDATE goal_branch_item SET reverted_at = ?2 WHERE id = ?1", params![bad.id, now()])?;
    let why = format!(
        "Checkpoint {key} found this item breaks the full gate (bisected over {k} item{}, {gates} more full run{}), so it was reverted on {}. \
         Start it again (it branches from {}, which no longer has it: revert the revert first), fix it, and land it.\n{output}",
        if k == 1 { "" } else { "s" },
        if gates == 1 { "" } else { "s" },
        gb.branch,
        gb.branch
    );
    // An item adopted with its branch may name no issue here.
    if let Some(issue) = db.issue(&bad.issue)? {
        if issue.origin == "nitely" {
            db.move_issue(&issue.key, "Todo", None, Some("its change fails the full gate at the goal's checkpoint"), actor).ok();
        }
        db.add_note(&issue.key, &why, "goal_branch", actor)?;
    }
    db.conn.execute(
        "UPDATE goal_checkpoint SET state = 'sent_back', ended_at = ?2, note = ?3 WHERE id = ?1",
        params![cp.id, now(), format!("{} broke the full gate; reverted on {}", bad.issue, gb.branch)],
    )?;
    let mut said = format!("{} broke the full gate ({gates} full run{} to find it); it was reverted on {} and sent back", bad.issue, if gates == 1 { "" } else { "s" }, gb.branch);
    if let Some(w) = warning {
        said.push_str(&format!("; {w}"));
    }
    if db.issue(key)?.is_some_and(|i| i.is_open()) {
        db.move_issue(key, "Canceled", None, Some(&format!("{} was reverted on {}; the next checkpoint lands the rest", bad.issue, gb.branch)), actor).ok();
    }
    let gb = db.goal_branch(cp.goal)?.expect("open");
    if !db.goal_items_waiting(cp.goal)?.is_empty() {
        let next = open_checkpoint(db, &gb, &to, actor)?;
        said.push_str(&format!("; {} lands the rest: start_task, submit and land it", next.issue));
    }
    Ok(said)
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::land::{land, LandOptions};
    use crate::submit::{submit, SubmitOptions};
    use crate::test_repo::git as sh;

    // The gate fails while bad.txt is there; the fake reviewer passes everything but risky.txt.
    const PLAYBOOK: &str = r#"
[worktree]
dir = "../wt/{key}"
[gate]
commands = ["{gate}"]
[review]
reviewer = { "*" = "fake" }
commands = { fake = ["{sh}", "-c", "cat > /dev/null; case \"$(git diff --name-only --diff-filter=AM goal-1...HEAD 2>/dev/null)\" in *risky.txt*) echo '- [P1] Risky default: risky.txt, the greeting'; echo 'VERDICT: findings';; *) echo 'Fine.'; echo 'VERDICT: clean';; esac"] }
"#;

    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        db: Db,
        actor: Actor,
        goal: i64,
        /// An item of the goal that stays open, so a checkpoint waits for its count or a day.
        rest: String,
    }

    fn fixture(name: &str, remote: bool) -> Fixture {
        let base = crate::paths::unique_temp(name, "");
        let repo = base.join("repo");
        let gate = if cfg!(windows) { "if exist bad.txt exit 1" } else { "test ! -f bad.txt" };
        let playbook = PLAYBOOK.replace("{sh}", &playbook::posix_sh().replace('\\', "/")).replace("{gate}", gate);
        crate::test_repo::init(&repo, &[(playbook::PLAYBOOK_PATH, &playbook), ("shared.txt", "one\ntwo\nthree\n")]);
        if remote {
            crate::test_repo::add_remote(&repo, &base.join("remote.git"));
        }
        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let change = crate::goals::GoalChange { title: Some("Slice".into()), outcome: Some("It works.".into()), ..Default::default() };
        let goal = db.save_goal("NIT", None, &change, &Actor::Human).unwrap().id;
        let actor = Actor::Agent("claude:test".into());
        let rest = NewIssue { project: "NIT".into(), title: "The rest of the slice".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), goal: Some(goal), ..Default::default() };
        let rest = db.create_issue(&rest, &actor).unwrap().key;
        Fixture { base, repo, db, actor, goal, rest }
    }

    impl Fixture {
        fn issue(&mut self, title: &str, goal: Option<i64>) -> String {
            let new = NewIssue { project: "NIT".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, goal, ..Default::default() };
            self.db.create_issue(&new, &self.actor).unwrap().key
        }

        /// Starts `key`, commits `file`, and returns its worktree.
        fn work(&mut self, key: &str, file: &str, content: &str) -> PathBuf {
            let issue = self.db.issue(key).unwrap().unwrap();
            let wt = PathBuf::from(playbook::start_task(&self.db, &self.repo, &issue, "claude", &self.actor).unwrap().worktree);
            std::fs::write(wt.join(file), content).unwrap();
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", key]);
            wt
        }

        fn submit(&mut self, wt: &Path) -> crate::submit::Submitted {
            let dir = wt.to_string_lossy().into_owned();
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: false, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
            submit(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap()
        }

        fn land(&mut self, key: &str) -> crate::land::Landed {
            let dir = self.repo.to_string_lossy().into_owned();
            land(&mut self.db, &crate::Config::default(), &LandOptions { dir: &dir, issue_key: key, push: true, keep: false, break_lock: false }, &self.actor).unwrap()
        }

        fn head(&self, name: &str) -> String {
            sh(&self.repo, &["rev-parse", name])
        }

        fn has(&self, rev: &str, file: &str) -> bool {
            git(&self.repo, &["cat-file", "-e", &format!("{rev}:{file}")]).is_ok()
        }

        fn told(&self, key: &str, text: &str) -> bool {
            let n: i64 = self.db.conn.query_row("SELECT COUNT(*) FROM event WHERE issue_key = ?1 AND summary LIKE ?2", params![key, format!("%{text}%")], |r| r.get(0)).unwrap();
            n > 0
        }
    }

    #[test]
    fn a_mode_with_goal_branches_turns_a_goals_branch_on_unless_it_was_turned_off() {
        use crate::project_modes::{preset, Mode};
        let route = |m: Mode, submodules: bool| goal_items_route(preset(m).goal_branches, submodules);
        // Fast and Careful land goal items through goal branches, Careful with its item review on
        // the way in (NIT-563); Release has none.
        assert_eq!(route(Mode::Fast, false), Ok(()));
        assert_eq!(route(Mode::Careful, false), Ok(()));
        assert!(route(Mode::Release, false).is_err());
        assert!(route(Mode::Fast, true).unwrap_err().contains("managed submodules"));
        assert_eq!(goal_items_route(None, false).is_err(), true, "no mode set: as before");
        // NIT-563: an open branch takes an item the mode routes, or one that started from it.
        assert!(item_route(true, true, false));
        assert!(item_route(true, false, true), "started from the goal branch: never onto main with its commits");
        assert!(!item_route(true, false, false), "the mode moved to main: a new item lands there");
        assert!(!item_route(false, true, true), "no open branch");

        assert_eq!(engage(true, Some(10), true, false, false), Some(10));
        assert_eq!(engage(true, None, true, false, false), Some(CHECKPOINT_ITEMS));
        assert_eq!(engage(false, Some(10), true, false, false), None);
        assert_eq!(engage(true, Some(10), true, true, false), None, "it has one already");
        assert_eq!(engage(true, Some(10), true, false, true), None, "a person turned it off");
        assert_eq!(engage(true, Some(10), false, false, false), None, "a goal done or dropped");
    }

    /// Under Fast, with no goal_branch call: the goal's first item makes its branch and lands into
    /// it, the next starts from it, and once the goal has nothing left open the checkpoint is filed
    /// without waiting for a day or the item count.
    #[test]
    fn a_goal_under_fast_lands_its_items_through_its_branch_and_checkpoints_when_all_are_in() {
        let mut f = fixture("nitely-goal-branch-mode", false);
        crate::project_modes::set_for_test(&mut f.db, "NIT", crate::project_modes::Mode::Fast, Default::default());
        f.db.move_issue(&f.rest, "Canceled", None, Some("not in this test"), &Actor::Human).unwrap();
        let main_before = f.head("main");
        let (a, b) = (f.issue("A", Some(f.goal)), f.issue("B", Some(f.goal)));
        let none = f.issue("No goal", None);

        let issue = f.db.issue(&a).unwrap().unwrap();
        let started = playbook::start_task(&f.db, &f.repo, &issue, "claude", &f.actor).unwrap();
        assert_eq!(started.lands_into, "goal-1", "start_task says where it lands");
        let gb = f.db.goal_branch(f.goal).unwrap().expect("the mode turned the goal's branch on");
        assert_eq!((gb.branch.as_str(), gb.checkpoint_items), ("goal-1", 10), "with the mode's checkpoint size");
        let wt = PathBuf::from(started.worktree);
        std::fs::write(wt.join("a.txt"), "a\n").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "A"]);
        assert_eq!(f.land(&a).goal_branch.as_deref(), Some("goal-1"));
        assert_eq!(f.head("main"), main_before, "main is untouched by an item");
        assert!(f.db.open_checkpoint(f.goal).unwrap().is_none(), "B is still open");

        let wt = f.work(&b, "b.txt", "b\n");
        assert!(wt.join("a.txt").exists(), "the next item starts from the goal branch");
        let landed = f.land(&b);
        assert!(landed.landed && f.has("goal-1", "b.txt"), "{landed:?}");
        let cp = f.db.open_checkpoint(f.goal).unwrap().expect("nothing left open: the checkpoint is filed");
        assert!(landed.goals.iter().any(|g| g.contains("no open items left")), "and says why: {:?}", landed.goals);
        assert_eq!(f.db.issue(&cp.issue).unwrap().unwrap().goal, Some(f.goal));

        // A last item that lands while that checkpoint is under way is not stranded: when the
        // checkpoint lands, the next one is filed for it.
        let opened = checkpoint(&mut f.db, &crate::Config::default(), f.goal, &f.repo.to_string_lossy(), Some("claude"), &f.actor).unwrap();
        let cp_wt = PathBuf::from(&opened.started.as_ref().unwrap().worktree);
        let c = f.issue("C", Some(f.goal));
        f.work(&c, "c.txt", "c\n");
        assert!(f.land(&c).landed);
        assert_eq!(f.db.open_checkpoint(f.goal).unwrap().map(|o| o.issue), Some(cp.issue.clone()), "one checkpoint at a time");
        f.submit(&cp_wt);
        let landed = f.land(&cp.issue);
        assert!(landed.landed && f.has("main", "b.txt") && !f.has("main", "c.txt"), "{landed:?}");
        let next = f.db.open_checkpoint(f.goal).unwrap().expect("C waits on the branch: the next checkpoint is filed");
        assert_ne!(next.issue, cp.issue);

        // An issue with no goal still starts from and lands on main.
        let issue = f.db.issue(&none).unwrap().unwrap();
        assert_eq!(playbook::start_task(&f.db, &f.repo, &issue, "claude", &f.actor).unwrap().lands_into, "main");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// NIT-563: Careful's goal items go through the goal branch with an advisory item review, and a
    /// mode whose item review blocks sends an item back for its P1 until it is fixed.
    #[test]
    fn goal_items_get_the_item_review_their_mode_asks_for_on_the_way_into_the_branch() {
        use crate::project_modes::{ItemReview, Mode, Options};
        let mut f = fixture("nitely-goal-item-review", false);
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Careful, Default::default());
        let a = f.issue("A", Some(f.goal));
        let wt = f.work(&a, "risky.txt", "hello
");
        assert!(f.db.goal_branch(f.goal).unwrap().is_some(), "Careful routes goal items through the goal branch");
        let s = f.submit(&wt);
        assert!(s.review.as_ref().is_some_and(|r| r.verdict == "changes"), "{s:?}");
        assert!(s.land.as_ref().is_some_and(|l| l.landed) && f.has("goal-1", "risky.txt"), "advisory: it lands as it is: {:?}", s.next);
        assert!(f.told(&a, "advisory, found: [P1] Risky default"), "its findings are on the issue");

        // Custom with the pre-merge review off and a blocking item review: the item review still
        // runs, guarded by the item review (NIT-563's rework).
        let blocking = Options { goal_branches: Some(true), pre_review: Some(crate::project_modes::PreReview::Off), item_review: Some(ItemReview::Blocking), ..Default::default() };
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Custom, blocking);
        let b = f.issue("B", Some(f.goal));
        let wt = f.work(&b, "risky.txt", "hello again
");
        let s = f.submit(&wt);
        assert!(s.land.is_none() && s.state == "In Progress", "blocking: sent back for its P1: {s:?}");
        assert!(f.told(&b, "Item review by fake requested changes: [P1] Risky default"));
        // A direct land meets the same review, and does not land.
        let direct = f.land(&b);
        assert!(!direct.landed && direct.error.as_deref().is_some_and(|e| e.contains("blocking item review")), "{direct:?}");
        // Fixed, it goes in.
        sh(&wt, &["rm", "-q", "risky.txt"]);
        std::fs::write(wt.join("safe.txt"), "hello
").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "safe"]);
        let s = f.submit(&wt);
        assert!(s.land.as_ref().is_some_and(|l| l.landed) && f.has("goal-1", "safe.txt"), "{:?}", s.next);
        // A goal item that lands on main (the mode no longer routes through the branch) still gets
        // its blocking item review with the pre-merge review off (NIT-597 review).
        let to_main = Options { goal_branches: Some(false), pre_review: Some(crate::project_modes::PreReview::Off), item_review: Some(ItemReview::Blocking), ..Default::default() };
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Custom, to_main);
        let c = f.issue("C", Some(f.goal));
        let wt = f.work(&c, "risky.txt", "on main
");
        let s = f.submit(&wt);
        assert_eq!(s.base, "main", "{s:?}");
        assert!(s.review.as_ref().is_some_and(|r| r.verdict == "changes") && s.state == "In Progress", "reviewed and sent back: {s:?}");
        // Tests off as well (what YOLO is): the blocking item review still runs.
        let yolo = Options { goal_branches: Some(false), pre_review: Some(crate::project_modes::PreReview::Off), test_depth: Some(crate::project_modes::TestDepth::Off), item_review: Some(ItemReview::Blocking), ..Default::default() };
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Custom, yolo);
        let d = f.issue("D", Some(f.goal));
        let wt = f.work(&d, "risky.txt", "no tests
");
        let s = f.submit(&wt);
        assert!(!s.yolo && s.review.as_ref().is_some_and(|r| r.verdict == "changes") && s.state == "In Progress", "reviewed, not YOLO: {s:?}");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// The whole goal-branch hop, as goal-19 itself runs by hand: items start from the goal branch
    /// and land into it with item checks and no review, main flows in after a land on main, a
    /// conflict turns the branch red and files the fix while the goal's other items wait, and a
    /// checkpoint lands the slice into main with one review and one full gate.
    #[test]
    fn items_land_into_the_goal_branch_and_a_checkpoint_lands_the_slice_on_main() {
        let mut f = fixture("nitely-goal-branch", true);
        let gb = f.db.enable_goal_branch(f.goal, &f.repo, None, Some(4), &f.actor).unwrap();
        assert_eq!((gb.branch.as_str(), gb.created_at), ("goal-1", None), "made when the first item starts");

        // The first item makes the branch, from main, and lands into it: main does not move.
        let main_before = f.head("main");
        let a = f.issue("A", Some(f.goal));
        let wt = f.work(&a, "a.txt", "a\n");
        assert_eq!(f.head("goal-1"), main_before);
        let submitted = f.submit(&wt);
        let landed = submitted.land.clone().expect("submit of an item lands it");
        assert!(landed.landed && submitted.review.is_none() && submitted.pr_url.is_none(), "{landed:?}");
        assert_eq!(landed.goal_branch.as_deref(), Some("goal-1"));
        assert_eq!(f.head("main"), main_before, "main is untouched by an item");
        assert!(f.has("goal-1", "a.txt") && f.has("origin/goal-1", "a.txt"), "the goal branch moved and was pushed");
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "Done");

        // The next item starts from the goal branch, so it has the first.
        let b = f.issue("B", Some(f.goal));
        let wt = f.work(&b, "b.txt", "b\n");
        assert!(wt.join("a.txt").exists());
        assert!(f.land(&b).landed);
        let summary = f.db.goal_branch_summary(f.goal).unwrap().unwrap();
        assert_eq!((summary.items, summary.state.as_str()), (2, "green"));
        assert_eq!(f.db.goal(f.goal).unwrap().unwrap().branch.unwrap().next_checkpoint, "at 4 items (2 to go)");

        // A change with no goal lands on main; main flows into the goal branch.
        let c = f.issue("C", None);
        let wt = f.work(&c, "c.txt", "c\n");
        f.submit(&wt);
        let landed = f.land(&c);
        assert!(landed.landed && landed.goals.iter().any(|g| g.contains("main merged into goal-1")), "{:?}", landed.goals);
        assert!(f.has("goal-1", "c.txt") && f.has("goal-1", "a.txt"));

        // Main and an item change the same line: the merge conflicts, the branch turns red, the fix
        // is filed on the goal naming the item, and the goal's other items wait.
        let d = f.issue("D", Some(f.goal));
        f.work(&d, "shared.txt", "one\nTWO from the goal\nthree\n");
        assert!(f.land(&d).landed);
        let e = f.issue("E", None);
        let wt = f.work(&e, "shared.txt", "one\nTWO from main\nthree\n");
        f.submit(&wt);
        let landed = f.land(&e);
        assert!(landed.goals.iter().any(|g| g.contains("goal-1 is red")), "{:?}", landed.goals);
        let gb = f.db.goal_branch(f.goal).unwrap().unwrap();
        let fix = gb.red_issue.clone().expect("a fix is filed");
        assert_eq!(gb.state, "red");
        assert_eq!(f.db.issue(&fix).unwrap().unwrap().goal, Some(f.goal));
        assert!(f.told(&d, "cannot take main"), "the item that touched the file is told");
        let g = f.issue("G", Some(f.goal));
        f.work(&g, "g.txt", "g\n");
        let refused = f.land(&g);
        assert!(!refused.landed && refused.error.as_deref().unwrap_or("").contains("is red"), "{refused:?}");

        // The fix merges main in and resolves it: the branch is green again, and G lands.
        let wt = f.work(&fix, "fix.txt", "x\n");
        assert!(git(&wt, &["merge", "--no-edit", "main"]).is_err(), "it conflicts");
        std::fs::write(wt.join("shared.txt"), "one\nTWO from both\nthree\n").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "--no-edit"]);
        assert!(f.land(&fix).landed);
        assert_eq!(f.db.goal_branch(f.goal).unwrap().unwrap().state, "green");
        assert!(f.land(&g).landed);

        // Enough items: the checkpoint is open. Started, it holds the goal branch's head; submitted,
        // the slice is reviewed once; landed, main has every item and the goal branch is current.
        let cp = f.db.open_checkpoint(f.goal).unwrap().expect("due after 4 items");
        let opened = checkpoint(&mut f.db, &crate::Config::default(), f.goal, &f.repo.to_string_lossy(), Some("claude"), &f.actor).unwrap();
        assert_eq!(opened.checkpoint.issue, cp.issue, "the open one is returned");
        let wt = PathBuf::from(&opened.started.as_ref().unwrap().worktree);
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), f.head("goal-1"));
        let submitted = f.submit(&wt);
        assert!(submitted.review.is_some() && submitted.land.is_none(), "a checkpoint is reviewed like any change");
        let landed = f.land(&cp.issue);
        assert!(landed.landed, "{landed:?}");
        for file in ["a.txt", "b.txt", "g.txt", "fix.txt"] {
            assert!(f.has("main", file), "{file} reached main");
        }
        assert_eq!(f.head("goal-1"), f.head("main"), "the goal branch moves on to main");
        let summary = f.db.goal_branch_summary(f.goal).unwrap().unwrap();
        assert_eq!((summary.items, summary.checkpoints, summary.full_runs, summary.reviews), (0, 1, 1, 1));
        assert!(f.db.open_checkpoint(f.goal).unwrap().is_none());
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A goal run by hand on a branch of its own (as goal-19 was) becomes a goal branch with its
    /// work as the first checkpoint's items: one per issue its commit subjects name, a commit that
    /// names none going with the item before it.
    #[test]
    fn adopting_a_branch_with_work_on_it_makes_its_commits_the_first_checkpoints_items() {
        let f = fixture("nitely-goal-adopt", false);
        let fork = f.head("main");
        sh(&f.repo, &["checkout", "-q", "-b", "by-hand"]);
        let commit = |file: &str, subject: &str| {
            std::fs::write(f.repo.join(file), subject).unwrap();
            sh(&f.repo, &["add", file]);
            sh(&f.repo, &["commit", "-q", "-m", subject]);
            f.head("HEAD")
        };
        commit("a.txt", "First item (NIT-7)");
        commit("a.txt", "Merge main into by-hand");
        let seven = commit("a.txt", "Fix the first item's test (NIT-7)");
        let nine = commit("b.txt", "Second item, UTF-8 safe (NIT-9)");
        sh(&f.repo, &["checkout", "-q", "main"]);
        let gb = f.db.enable_goal_branch(f.goal, &f.repo, Some("by-hand"), None, &f.actor).unwrap();
        assert!(gb.created_at.is_some() && gb.created_from.as_deref() == Some(fork.as_str()));
        let items: Vec<(String, String, String)> = f.db.goal_items_waiting(f.goal).unwrap().into_iter().map(|i| (i.issue, i.from, i.to)).collect();
        assert_eq!(items, [("NIT-7".to_string(), fork, seven.clone()), ("NIT-9".to_string(), seven, nine)]);
        assert_eq!(f.db.goal_branch_summary(f.goal).unwrap().unwrap().next_checkpoint, "at 10 items (8 to go)");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A checkpoint whose full gate fails bisects over the goal's items: the one at fault is
    /// reverted on the goal branch and sent back, and the next checkpoint holds the rest.
    #[test]
    fn a_failing_checkpoint_finds_the_item_at_fault_reverts_it_and_opens_the_next() {
        let mut f = fixture("nitely-goal-bisect", false);
        f.db.enable_goal_branch(f.goal, &f.repo, None, Some(10), &f.actor).unwrap();
        for name in ["one", "two", "three"] {
            let key = f.issue(name, Some(f.goal));
            f.work(&key, &format!("{name}.txt"), "x\n");
            assert!(f.land(&key).landed);
        }
        // An item whose fault its item checks missed (they ran the tests it affects, not the one it
        // breaks): put on the goal branch as its land would, so only the full gate finds it.
        let gb = f.db.goal_branch(f.goal).unwrap().unwrap();
        let wt = work_tree(&f.repo, &Playbook::load(&f.repo).unwrap(), f.goal, "goal-1").unwrap();
        let from = rev(&wt, "HEAD").unwrap();
        std::fs::write(wt.join("bad.txt"), "x\n").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "bad"]);
        let to = rev(&wt, "HEAD").unwrap();
        advance(&f.repo, &gb, &from, &to).unwrap();
        let bad = f.issue("bad", Some(f.goal));
        item_landed(&mut f.db, &gb, &bad, &from, &to, &f.actor).unwrap();
        // One more good item after it (its item checks would trip on bad.txt, so it goes on the
        // same way).
        let four = f.issue("four", Some(f.goal));
        let from = rev(&f.repo, "goal-1").unwrap();
        let gb = f.db.goal_branch(f.goal).unwrap().unwrap();
        let wt_goal = work_tree(&f.repo, &Playbook::load(&f.repo).unwrap(), f.goal, &from).unwrap();
        std::fs::write(wt_goal.join("four.txt"), "x\n").unwrap();
        sh(&wt_goal, &["add", "."]);
        sh(&wt_goal, &["commit", "-q", "-m", "four"]);
        let to = rev(&wt_goal, "HEAD").unwrap();
        advance(&f.repo, &gb, &from, &to).unwrap();
        item_landed(&mut f.db, &gb, &four, &from, &to, &f.actor).unwrap();

        let opened = checkpoint(&mut f.db, &crate::Config::default(), f.goal, &f.repo.to_string_lossy(), Some("claude"), &f.actor).unwrap();
        assert_eq!(opened.items.len(), 5);
        let wt = PathBuf::from(&opened.started.as_ref().unwrap().worktree);
        // The checkpoint's full gate fails (at submit here; at land it bisects the same way).
        let submitted = f.submit(&wt);
        assert!(submitted.next.contains(&bad) && submitted.next.contains("reverted"), "{}", submitted.next);
        assert!(!f.has("goal-1", "bad.txt") && f.has("goal-1", "four.txt"), "only the bad item is reverted");
        assert!(!f.has("main", "one.txt"), "nothing reached main");
        assert_eq!(f.db.issue(&bad).unwrap().unwrap().state, "Todo", "sent back");
        let cp = f.db.checkpoint_of_issue(&opened.checkpoint.issue).unwrap().unwrap();
        assert_eq!(cp.state, "sent_back");
        assert!(cp.gates <= 3, "ceil(log2(5 + 1)) = 3 at most: {}", cp.gates);
        let next = f.db.open_checkpoint(f.goal).unwrap().expect("the next checkpoint holds the rest");
        assert_ne!(next.issue, cp.issue);
        assert_eq!(f.db.goal_items_waiting(f.goal).unwrap().len(), 4);
        std::fs::remove_dir_all(&f.base).ok();
    }
}
