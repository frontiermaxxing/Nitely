//! Build lanes (docs/build-lanes.md): warm, full checkouts of a repo that keep their build output,
//! where builds and tests run as requests instead of in every agent's worktree. A request is a
//! snapshot commit and a command. A lane serves it in the same four steps whatever the last request
//! left: a forced checkout of the snapshot, a clean of everything untracked except the playbook's
//! build directories, setup again when a dependency file changed, then the command. A lane holds
//! nobody's work, so the worst a crash can do is leave it to be cleaned by the next request.
//!
//! Nothing here runs for a repo whose playbook has no `[lanes]`.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::machine;
use crate::playbook::{self, git, Playbook};

/// A playbook's `[lanes]`. Without the table a repo has no lanes and everything works as before.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneRules {
    /// Lanes for this repo on this machine.
    pub count: u32,
    /// Build directories (repo-relative) a lane keeps between requests; `target` and `node_modules`
    /// unless the playbook says (the same list `nitely lanes slim` reclaims).
    pub keep: Vec<String>,
    /// A command running longer is stopped, with everything it started.
    pub timeout_minutes: u64,
    /// Commands (regexes, matched against each command of a shell line) that build or test: run in
    /// a light worktree, the hook reminds the agent to use `run` instead (it never blocks them).
    pub heavy: Vec<String>,
    /// How big a lane's build directories may grow, in GB, before `hygiene` trims them; unset, twice
    /// the size of its first build.
    pub size_cap_gb: Option<f64>,
    /// The person's own main checkout is the repo's one lane (a repo too big for a second
    /// checkout). A request runs there only while it is clean and on the integration branch, and
    /// puts it back on that branch after; nothing there is ever stashed, committed or cleaned.
    pub large_repo: bool,
    /// When the project's base commit moves: `tip` (the integration branch's tip, always) or
    /// `every <n>h`, so new worktrees and the reference tree start from a commit that moves a few
    /// times a day and a dependency sync runs that often, not for every agent.
    pub base: String,
}

/// The commands that build or test when the playbook names none.
pub const DEFAULT_HEAVY: &[&str] = &["^cargo (build|test|clippy|check|run)\\b", "^npm (ci|install|test|run build)\\b"];

impl Default for LaneRules {
    fn default() -> Self {
        LaneRules {
            count: 1,
            keep: crate::slim::DEFAULT_KEEP.iter().map(|s| s.to_string()).collect(),
            timeout_minutes: 30,
            heavy: DEFAULT_HEAVY.iter().map(|s| s.to_string()).collect(),
            size_cap_gb: None,
            large_repo: false,
            base: "tip".into(),
        }
    }
}

impl LaneRules {
    /// Lanes the repo has: one in large-repo mode, the person's checkout.
    pub fn lanes(&self) -> u32 {
        if self.large_repo {
            1
        } else {
            self.count.max(1)
        }
    }

    /// `every <n>h` as hours; None for `tip` or anything else.
    pub fn base_every_hours(&self) -> Option<u64> {
        let b = self.base.trim();
        let n = b.strip_prefix("every").map(str::trim).unwrap_or(b).strip_suffix('h')?;
        n.trim().parse().ok().filter(|h: &u64| *h > 0)
    }
}

/// The folder a repo's lanes live in, beside its reference tree: `.nitely-lanes/<repo>` next to its
/// worktrees (light.rs), which no listing of agents' worktrees shows.
pub fn dir(repo_root: &Path, pb: &Playbook) -> PathBuf {
    crate::light::lanes_dir(pb, repo_root)
}

fn lane_path(repo_root: &Path, pb: &Playbook, n: u32) -> PathBuf {
    if pb.lanes.as_ref().is_some_and(|l| l.large_repo) {
        return repo_root.to_path_buf();
    }
    dir(repo_root, pb).join(format!("lane-{n}"))
}

// ----- the base commit (NIT-201) -----

/// The base commit when the playbook moves it on a schedule (`[lanes] base = "every <n>h"`): the
/// integration branch's tip as it was when the base last moved, moved to the tip once `n` hours
/// have passed. None when the base is the tip (the default). Kept in the lanes folder.
pub fn scheduled_base(pb: &Playbook, repo_root: &Path, tip: &str) -> Result<Option<String>> {
    let Some(hours) = pb.lanes.as_ref().and_then(|l| l.base_every_hours()) else { return Ok(None) };
    let file = dir(repo_root, pb).join("base");
    if let Ok(text) = std::fs::read_to_string(&file) {
        let mut lines = text.lines();
        let (commit, at) = (lines.next().unwrap_or("").trim().to_string(), lines.next().and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or(0));
        let fresh = now() - at < (hours * 3600) as i64;
        if fresh && !commit.is_empty() && git(repo_root, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok() {
            return Ok(Some(commit));
        }
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, format!("{tip}\n{}\n", now())).with_context(|| format!("writing {}", file.display()))?;
    Ok(Some(tip.to_string()))
}

// ----- large-repo mode: the person's checkout is the lane (NIT-201) -----

/// Why the person's checkout cannot take a request now, in words, or None when it can: it is on
/// the integration branch, nothing is in the middle of a merge or rebase, and nothing tracked has
/// changed and nothing is untracked outside the build directories. Ignored files are theirs and
/// are never touched, so they do not count.
pub fn checkout_busy(root: &Path, pb: &Playbook, keep: &[String]) -> Result<Option<String>> {
    let branch = pb.integration_branch();
    for (file, what) in [("MERGE_HEAD", "a merge"), ("CHERRY_PICK_HEAD", "a cherry-pick"), ("REVERT_HEAD", "a revert"), ("rebase-merge", "a rebase"), ("rebase-apply", "a rebase")] {
        if git_path(root, file).is_some_and(|p| p.exists()) {
            return Ok(Some(format!("your checkout is in the middle of {what}")));
        }
    }
    let on = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default();
    if on != branch {
        let at = if on.is_empty() { "a detached HEAD".to_string() } else { on };
        return Ok(Some(format!("your checkout is on {at}, not {branch}")));
    }
    let status = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let changed: Vec<&str> = status.split('\0').filter(|e| e.len() > 3).map(|e| &e[3..]).filter(|p| !in_keep(keep, p)).collect();
    Ok(match changed.as_slice() {
        [] => None,
        [one] => Some(format!("your checkout has a change: {one}")),
        [first, rest @ ..] => Some(format!("your checkout has {} changes, {first} and more", rest.len() + 1)),
    })
}

/// A test drives large-repo mode only on a throwaway repo in the temp folder: anywhere else it would
/// be a person's checkout, so the test stops before it is touched.
#[cfg(test)]
fn guard_throwaway(root: &Path) {
    let temp = crate::paths::normalize(&std::env::temp_dir().to_string_lossy());
    let at = crate::paths::normalize(&root.to_string_lossy());
    assert!(at.starts_with(&format!("{temp}/")), "a test may use large-repo mode only on a throwaway repo under {temp}, not {at}");
}

#[cfg(not(test))]
fn guard_throwaway(_: &Path) {}

/// A repo-relative path inside one of the build directories.
fn in_keep(keep: &[String], path: &str) -> bool {
    keep.iter().any(|k| {
        let k = k.trim_matches('/');
        !k.is_empty() && (path == k || path.starts_with(&format!("{k}/")))
    })
}

/// In large-repo mode, the person's checkout held for something other than a request (land's
/// fast-forward, its after steps): waits for the request using it to finish, up to the lane's
/// timeout, and puts back a checkout a dead request left detached. None without large-repo mode.
pub fn hold_checkout(repo_root: &Path, pb: &Playbook) -> Result<Option<LaneLock>> {
    let Some(rules) = pb.lanes.as_ref().filter(|l| l.large_repo) else { return Ok(None) };
    guard_throwaway(repo_root);
    let until = Instant::now() + Duration::from_secs(rules.timeout_minutes.max(1) * 60);
    loop {
        if let Some(lock) = try_lock(repo_root, pb, 1)? {
            let said = recover_large(repo_root, &rules.keep);
            // Still at a request's commit (it changed since, or going back would overwrite a
            // file): not the checkout the caller expects, so it is not handed over.
            if lent_to_request(repo_root).is_some() && git(repo_root, &["symbolic-ref", "--quiet", "HEAD"]).is_err() {
                bail!("{}", said.unwrap_or_else(|| "your main checkout is still at a build request's commit".into()));
            }
            return Ok(Some(lock));
        }
        if Instant::now() > until {
            bail!("a build request has had the main checkout for over {} minutes", rules.timeout_minutes);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// What a checkout of `to` would overwrite without asking: git treats ignored files as expendable,
/// so a path `to` tracks and HEAD does not, already on disk (as an ignored file, or one made since
/// the last checkout), or under such a file, which git would delete to make the directory. Paths in
/// `keep` (build output) do not count. Returns the first such path.
fn would_overwrite(root: &Path, to: &str, keep: &[String]) -> Result<Option<String>> {
    let added = git(root, &["diff", "--name-only", "-z", "--no-renames", "--diff-filter=A", "HEAD", to])?;
    let mut leaves = Vec::new();
    let mut parents = std::collections::BTreeSet::new();
    for p in added.split('\0').filter(|p| !p.is_empty() && !in_keep(keep, p)) {
        leaves.push(p.to_string());
        let mut at = p;
        while let Some((parent, _)) = at.rsplit_once('/') {
            parents.insert(parent.to_string());
            at = parent;
        }
    }
    for p in &leaves {
        let Ok(meta) = root.join(p).symlink_metadata() else { continue };
        if !meta.is_dir() {
            return Ok(Some(p.clone()));
        }
        // A folder where `to` has a file: git removes it rightly when all it holds is tracked.
        if !git(root, &["ls-files", "-z", "--others", "--", p])?.trim_matches('\0').is_empty() {
            return Ok(Some(p.clone()));
        }
    }
    // A file where `to` has a directory, unless HEAD tracks it (then git replaces it rightly).
    let files: Vec<&String> = parents.iter().filter(|p| root.join(p).symlink_metadata().is_ok_and(|m| !m.is_dir())).collect();
    if files.is_empty() {
        return Ok(None);
    }
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(files.iter().map(|p| p.as_str()));
    let tracked = git(root, &args)?;
    let tracked: Vec<&str> = tracked.split('\0').collect();
    Ok(files.into_iter().find(|p| !tracked.contains(&p.as_str())).cloned())
}

/// The branch the main checkout goes back to while a build request has it (large-repo mode), from
/// the note the request left: the files in it are then a snapshot of someone's work, so the
/// project's rules are read from that branch instead (`Playbook::load`). Read without running git.
pub fn lent_to_request(repo_root: &Path) -> Option<String> {
    lends(repo_root).lent
}

/// How often the main checkout has been lent to a request or given back, the branch it comes back
/// to, and whether it is lent now, in one file replaced whole (so one read sees one state):
/// `Playbook::load` reads it before and after reading the playbook. It moves to "lent" before the
/// checkout goes to a snapshot and to "back", with the count moved again, once it is back. So a read
/// of the playbook that could have seen a snapshot's files either began while it said "lent", or
/// sees the count moved when it looks again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lends {
    pub count: u64,
    /// The branch of the last lend.
    pub last: Option<String>,
    /// The branch it goes back to, while it is lent now.
    pub lent: Option<String>,
    /// The commit it was lent at, while it is lent now.
    pub commit: Option<String>,
}

pub fn lends(repo_root: &Path) -> Lends {
    let text = std::fs::read_to_string(repo_root.join(".git").join(LENDS)).unwrap_or_default();
    let mut lines = text.lines();
    let count = lines.next().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    let last = lines.next().map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
    let lent = (lines.next().map(str::trim) == Some("lent")).then(|| last.clone()).flatten();
    let commit = lines.next().map(|c| c.trim().to_string()).filter(|c| lent.is_some() && !c.is_empty());
    Lends { count, last, lent, commit }
}

/// Whether the main checkout's own playbook file can be trusted while the checkout is as `l` says:
/// always while it is not lent; while it is, only when the request's commit has no playbook, so
/// the file there is the person's own (untracked) one, which the lend left alone.
pub fn playbook_on_disk_is_theirs(repo_root: &Path, l: &Lends) -> bool {
    match (&l.lent, &l.commit) {
        (None, _) => true,
        (Some(_), Some(commit)) => git(repo_root, &["cat-file", "-e", &format!("{commit}:{}", playbook::PLAYBOOK_PATH)]).is_err(),
        (Some(_), None) => false,
    }
}

const LENDS: &str = "nitely-lane-lends";

/// Moves the lend count on, noting the branch and whether the checkout is now lent. Written to a
/// file beside it and renamed over it, so a reader sees the old state or the new one, never half.
fn count_lend(root: &Path, branch: &str, lent_at: Option<&str>) -> Result<()> {
    if let Some(file) = git_path(root, LENDS) {
        let count = lends(root).count + 1;
        let next = file.with_extension("next");
        let state = match lent_at {
            Some(commit) => format!("lent\n{commit}"),
            None => "back".into(),
        };
        std::fs::write(&next, format!("{count}\n{branch}\n{state}\n")).with_context(|| format!("writing {}", next.display()))?;
        std::fs::rename(&next, &file).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}

/// Where the checkout notes the branch a request will put it back on, while the request has it
/// detached; found after a crash, it says Nitely left it there.
fn return_mark(root: &Path) -> Option<PathBuf> {
    git_path(root, "nitely-lane-return")
}

/// Puts the person's checkout at `commit` for a request: a plain detached checkout, which git
/// refuses rather than overwrite anything, after noting the branch to come back to.
fn switch_large(root: &Path, branch: &str, commit: &str, keep: &[String]) -> Result<()> {
    guard_throwaway(root);
    if let Some(p) = would_overwrite(root, commit, keep)? {
        bail!("the request's commit tracks {p}, which your checkout has as an ignored file; it was left alone and the request not run");
    }
    // Counted before anything changes, so a playbook read meanwhile knows to read the branch's.
    count_lend(root, branch, Some(commit))?;
    if let Some(mark) = return_mark(root) {
        std::fs::write(&mark, branch).with_context(|| format!("writing {}", mark.display()))?;
    }
    git(root, &["checkout", "--quiet", "--detach", commit]).context("putting your checkout at the request's commit")?;
    Ok(())
}

/// Back on `branch` after a request: a plain checkout (never forced), so a file the build changed
/// stops it rather than being thrown away.
fn return_large(root: &Path, branch: &str) -> Result<()> {
    guard_throwaway(root);
    if let Some(p) = would_overwrite(root, branch, &[])? {
        bail!(
            "putting your checkout back on {branch} would overwrite {p}, which appeared while the request ran; it was left alone, and your checkout stays at the \
             request's commit: move {p} away, then run `nitely lanes reset`"
        );
    }
    git(root, &["checkout", "--quiet", branch]).with_context(|| format!("putting your checkout back on {branch}"))?;
    // Counted again once it is back (see `Lends`).
    count_lend(root, branch, None)?;
    if let Some(mark) = return_mark(root) {
        std::fs::remove_file(mark).ok();
    }
    Ok(())
}

/// After a request that did not finish (its process died): the checkout Nitely left detached goes
/// back to its branch, when nothing in it changed since. Returns what it did, or why it left the
/// checkout as it is, in words.
fn recover_large(root: &Path, keep: &[String]) -> Option<String> {
    let mark = return_mark(root)?;
    let branch = std::fs::read_to_string(&mark).ok()?.trim().to_string();
    if branch.is_empty() {
        std::fs::remove_file(&mark).ok();
        return None;
    }
    let detached = git(root, &["symbolic-ref", "--quiet", "HEAD"]).is_err();
    if !detached {
        // Put back by hand: no longer lent.
        count_lend(root, &branch, None).ok();
        std::fs::remove_file(&mark).ok();
        return None;
    }
    let status = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).unwrap_or_default();
    if let Some(changed) = status.split('\0').filter(|e| e.len() > 3).map(|e| &e[3..]).find(|p| !in_keep(keep, p)) {
        return Some(format!("your checkout stays detached at the request's commit: {changed} changed since; put it back on {branch} once that is safe"));
    }
    match return_large(root, &branch) {
        Ok(()) => Some(format!("put your checkout back on {branch}, where a request that did not finish had left it detached")),
        Err(e) => Some(format!("your checkout stays detached: {e:#}")),
    }
}

/// Stops only the command the last request recorded, when it still runs that command. In the
/// person's checkout, unlike a lane, every other process using the folder is theirs.
fn stop_recorded(lane: &Path) {
    let Some(file) = pid_file(lane) else { return };
    stop_recorded_command(&file);
    std::fs::remove_file(&file).ok();
}

/// Stops the command a pid file names, when that process still runs it and started when recorded.
fn stop_recorded_command(file: &Path) {
    let Ok(text) = std::fs::read_to_string(file) else { return };
    // "<pid>\n<command>": the pid is stopped only while it still runs that command, since the
    // system may have given the number to another process once the command ended.
    // "<pid> <start time>\n<command>": the same command at the same number with the same start is
    // the process the request left; one that got the number since (running the same command in
    // another lane, say) has another start time.
    let (head, command) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    let mut head = head.split_whitespace();
    if let Some(Ok(pid)) = head.next().map(str::parse::<u32>) {
        let recorded = head.next();
        let same_command = !command.trim().is_empty() && crate::procs::command_line(pid).is_some_and(|line| line.contains(command.trim()));
        let same_start = recorded.is_some_and(|t| crate::procs::started(pid).as_deref() == Some(t));
        if same_command && same_start {
            stop_tree(pid);
        }
    }
}

// ----- snapshots -----

/// What a request builds: a commit in the repo's object store.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    /// The commit the worktree's work sits on (its HEAD).
    pub base: String,
    /// The worktree had uncommitted work, so `commit` was made for this request.
    pub made: bool,
}

/// `worktree` as a commit: its HEAD when nothing is uncommitted, else a commit built in a scratch
/// index from HEAD plus every path `git status` lists. The agent's files, index and branch are
/// never touched. Paths outside a sparse checkout are not listed, so they stay as in HEAD.
pub fn snapshot(worktree: &Path) -> Result<Snapshot> {
    let top = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"])?.trim());
    let head = git(&top, &["rev-parse", "HEAD"])?.trim().to_string();
    let status = git(&top, &["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=all"])?;
    let paths = changed_paths(&status);
    if paths.is_empty() {
        let tree = git(&top, &["rev-parse", "HEAD^{tree}"])?.trim().to_string();
        return Ok(Snapshot { commit: head.clone(), tree, base: head, made: false });
    }
    let scratch = crate::paths::unique_temp("nitely-snapshot", "");
    std::fs::create_dir_all(&scratch)?;
    let index = scratch.join("index");
    let result = (|| -> Result<(String, String)> {
        let with_index = |args: &[&str], input: Option<&[u8]>| -> Result<String> {
            use std::io::Write;
            let mut cmd = playbook::shell_free_command("git");
            cmd.arg("-C").arg(&top).env("GIT_INDEX_FILE", &index).args(args);
            cmd.env("GIT_AUTHOR_NAME", "Nitely").env("GIT_AUTHOR_EMAIL", "nitely@localhost");
            cmd.env("GIT_COMMITTER_NAME", "Nitely").env("GIT_COMMITTER_EMAIL", "nitely@localhost");
            cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn()?;
            if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
                stdin.write_all(bytes)?;
            }
            let out = child.wait_with_output()?;
            if !out.status.success() {
                bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        with_index(&["read-tree", "HEAD"], None)?;
        let mut list = Vec::new();
        for p in &paths {
            list.extend_from_slice(p.as_bytes());
            list.push(0);
        }
        // Each path as the worktree has it now: added or updated when present, removed when gone.
        // --replace: a folder that became a file (or the other way) replaces what the index had.
        with_index(&["update-index", "--add", "--remove", "--replace", "-z", "--stdin"], Some(&list))?;
        let tree = with_index(&["write-tree"], None)?;
        let commit = with_index(&["commit-tree", &tree, "-p", &head, "-m", "nitely run: uncommitted work"], None)?;
        Ok((commit, tree))
    })();
    std::fs::remove_dir_all(&scratch).ok();
    let (commit, tree) = result?;
    Ok(Snapshot { commit, tree, base: head, made: true })
}

/// The paths `git status --porcelain=v2 -z` names: changed, renamed (both sides), unmerged and
/// untracked.
fn changed_paths(status: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut entries = status.split('\0');
    while let Some(e) = entries.next() {
        let mut fields = e.splitn(2, ' ');
        let kind = fields.next().unwrap_or("");
        let path = match kind {
            "1" => e.splitn(9, ' ').nth(8),
            "2" => {
                let p = e.splitn(10, ' ').nth(9);
                // The original path follows as its own entry.
                if let Some(orig) = entries.next() {
                    out.push(orig.to_string());
                }
                p
            }
            "u" => e.splitn(11, ' ').nth(10),
            "?" => fields.next(),
            _ => None,
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            out.push(p.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

// ----- the request -----

/// What a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An agent's or a person's `run`.
    Run,
    /// submit's gate (NIT-198).
    Gate,
    /// land's gate after its rebase (NIT-198).
    Land,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Run => "run",
            Kind::Gate => "gate",
            Kind::Land => "land",
        }
    }
}

pub struct Request<'a> {
    pub project: &'a str,
    pub issue: Option<&'a str>,
    pub kind: Kind,
    pub snapshot: &'a Snapshot,
    pub command: &'a str,
}

/// A request and what came of it: the `lane_run` row. NIT-205 builds the app's view on this.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: i64,
    pub project: String,
    pub repo: String,
    pub issue: Option<String>,
    pub by: String,
    pub kind: String,
    pub snapshot: String,
    pub tree: String,
    pub base: String,
    pub command: String,
    /// queued | running | done | lane_error | abandoned
    pub state: String,
    /// The lane's number (1..count), once it has one.
    pub lane: Option<u32>,
    pub executor: String,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    /// The command's exit code; None until it ends, or when it was stopped or never ran.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Setup ran again first, because a dependency file differed from the lane's last setup.
    pub setup_ran: bool,
    /// Files the checkout wrote to switch the lane to this snapshot.
    pub files_switched: Option<i64>,
    pub log: Option<String>,
    /// The end of the output.
    pub output: String,
    /// Why the lane could not run it (state lane_error), in words.
    pub error: Option<String>,
    /// Large-repo mode: why it waits for the person's checkout (a change there, another branch).
    pub waiting_for: Option<String>,
    /// Why it waits for the machine's memory (NIT-266): "waiting for memory: 6.2 GB free, …".
    pub waiting_memory: Option<String>,
    /// The most memory the run's processes held at once, in bytes, where the system tells it.
    pub peak_memory: Option<i64>,
}

impl Run {
    /// The command ran and exited 0.
    pub fn passed(&self) -> bool {
        self.state == "done" && self.exit_code == Some(0)
    }
}

const COLUMNS: &str = "id, project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, lane, executor, \
queued_at, started_at, ended_at, exit_code, timed_out, setup_ran, files_switched, log_path, output, error, waiting_for, waiting_memory, peak_memory, pid";

fn row(r: &rusqlite::Row) -> rusqlite::Result<(Run, Option<i64>)> {
    Ok((
        Run {
            id: r.get(0)?,
            project: r.get(1)?,
            repo: r.get(2)?,
            issue: r.get(3)?,
            by: r.get(4)?,
            kind: r.get(5)?,
            snapshot: r.get(6)?,
            tree: r.get(7)?,
            base: r.get(8)?,
            command: r.get(9)?,
            state: r.get(10)?,
            lane: r.get::<_, Option<i64>>(11)?.map(|n| n as u32),
            executor: r.get(12)?,
            queued_at: r.get(13)?,
            started_at: r.get(14)?,
            ended_at: r.get(15)?,
            exit_code: r.get(16)?,
            timed_out: r.get::<_, i64>(17)? != 0,
            setup_ran: r.get::<_, i64>(18)? != 0,
            files_switched: r.get(19)?,
            log: r.get(20)?,
            output: r.get(21)?,
            error: r.get(22)?,
            waiting_for: r.get(23)?,
            waiting_memory: r.get(24)?,
            peak_memory: r.get(25)?,
        },
        r.get(26)?,
    ))
}

impl Db {
    /// One run by id. A queued or running run whose process is gone reads as abandoned.
    pub fn lane_run(&self, id: i64) -> Result<Option<Run>> {
        let found = self.conn.query_row(&format!("SELECT {COLUMNS} FROM lane_run WHERE id = ?1"), [id], row).optional()?;
        Ok(found.map(|(run, pid)| settle(self, run, pid)))
    }

    /// A project's runs, newest first; queued and running ones whose process is gone are marked
    /// abandoned on the way.
    pub fn lane_runs(&self, project: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![project, limit as i64], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(run, pid)| settle(self, run, pid)).collect())
    }

    /// The runs one actor asked for, newest first.
    /// A repo's queued and running runs in a project, oldest first, however many finished since:
    /// what its queue shows. Read by repo, so another repo of the project cannot crowd it out.
    pub fn open_lane_runs(&self, project: &str, repo: &str) -> Result<Vec<Run>> {
        let mine = crate::paths::normalize(repo);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE AND state IN ('queued', 'running') ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![project], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|(r, _)| crate::paths::normalize(&r.repo) == mine)
            .map(|(run, pid)| settle(self, run, pid))
            .filter(|r| r.state == "queued" || r.state == "running")
            .collect())
    }

    /// A repo's last `limit` finished runs in a project, newest first.
    pub fn finished_lane_runs(&self, project: &str, repo: &str, limit: usize) -> Result<Vec<Run>> {
        let mine = crate::paths::normalize(repo);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE AND state NOT IN ('queued', 'running') ORDER BY id DESC"
        ))?;
        let mut rows = stmt.query(params![project])?;
        let mut out = Vec::new();
        while out.len() < limit {
            let Some(r) = rows.next()? else { break };
            let (run, _) = row(r)?;
            if crate::paths::normalize(&run.repo) == mine {
                out.push(run);
            }
        }
        Ok(out)
    }

    pub fn lane_runs_by(&self, by: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM lane_run WHERE requested_by = ?1 ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![by, limit as i64], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(run, pid)| settle(self, run, pid)).collect())
    }
}

/// A queued or running run outlives the process serving it only by a crash: it is abandoned.
fn settle(db: &Db, mut run: Run, pid: Option<i64>) -> Run {
    let open = run.state == "queued" || run.state == "running";
    if open && pid.is_some_and(|p| p as u32 != std::process::id() && !crate::crew::pid_alive(p as u32)) {
        db.conn
            .execute("UPDATE lane_run SET state = 'abandoned', ended_at = COALESCE(ended_at, ?2) WHERE id = ?1 AND state IN ('queued', 'running')", params![run.id, now()])
            .ok();
        run.state = "abandoned".into();
    }
    run
}

/// The folder for what lanes keep outside the repo (run logs): `lanes` beside the database the runs
/// are recorded in, so a database and its runs' logs stay together (the data folder, or a test's
/// own folder).
pub fn state_dir(db: &Db) -> PathBuf {
    let beside = db.conn.path().filter(|p| !p.is_empty()).and_then(|p| Path::new(p).parent().map(Path::to_path_buf));
    beside.unwrap_or_else(crate::paths::data_dir).join("lanes")
}

/// Where a project's run logs go.
fn logs_dir(db: &Db, project: &str) -> PathBuf {
    let project = if project.is_empty() { "_" } else { project };
    state_dir(db).join(project.to_uppercase())
}

/// How long a snapshot's ref and a run's log are kept.
const KEEP_SECS: i64 = 7 * 24 * 3600;

/// `run` as the MCP tool and the CLI ask for it: the worktree `dir` is in, as a snapshot, in a lane
/// of the repo its issue belongs to. The issue is `issue`, else the one the branch names.
pub fn run_in(config: &crate::Config, db: &Db, dir: &str, issue: Option<&str>, command: &str, actor: &Actor, on_event: &mut dyn FnMut(&Run)) -> Result<Run> {
    if command.trim().is_empty() {
        bail!("say which command to run");
    }
    let (worktree, branch_key) = playbook::current_worktree(dir)?;
    let key = issue.map(str::to_string).or(branch_key);
    let record = match key.as_deref() {
        Some(k) => Some(db.issue(k)?.with_context(|| format!("no issue {k}"))?),
        None => None,
    };
    let root = match &record {
        Some(i) => playbook::repo_for_issue(config, i.project.as_deref(), dir)?,
        None => playbook::repo_root(config, dir).with_context(|| format!("{dir} is not inside a git repo"))?,
    };
    let project = record
        .as_ref()
        .and_then(|i| i.project.clone())
        .or_else(|| config.repo_for_dir(&root.to_string_lossy()).and_then(|r| r.project.clone()))
        .unwrap_or_default();
    let pb = Playbook::load_with(&root, config)?;
    let snap = snapshot(&worktree)?;
    let req = Request { project: &project, issue: record.as_ref().map(|i| i.key.as_str()), kind: Kind::Run, snapshot: &snap, command };
    run(db, &root, &pb, &req, actor, on_event)
}

/// Runs `req` in one of the repo's lanes, waiting for a free one, and returns its record. The record
/// is written as the request moves (queued, running, done), so another process can follow it with
/// `Db::lane_run`. `on_event` is called with the run when it is queued and when it starts.
pub fn run(db: &Db, repo_root: &Path, pb: &Playbook, req: &Request, actor: &Actor, on_event: &mut dyn FnMut(&Run)) -> Result<Run> {
    let share = share();
    let command = req.command.replace("{jobs}", &share.jobs.to_string());
    run_with(db, repo_root, pb, req, actor, on_event, &mut |lane, log, limit| run_logged(&command, lane, log, limit, share))
}

/// What runs in a lane once it holds the request's snapshot: given the lane, the log and the time
/// limit, it returns how the command ended.
type Exec<'a> = dyn FnMut(&Path, &Path, Duration) -> Result<Ended> + 'a;

/// How a lane command ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ended {
    /// Its exit code; None when it was stopped.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// The most memory it and what it started held at once, in bytes, where the system tells it.
    pub peak: Option<u64>,
}

/// `run`, with what happens in the lane given by `exec` (one command, or a gate's commands).
fn run_with(db: &Db, repo_root: &Path, pb: &Playbook, req: &Request, actor: &Actor, on_event: &mut dyn FnMut(&Run), exec: &mut Exec) -> Result<Run> {
    let Some(rules) = pb.lanes.clone() else {
        bail!("{} has no [lanes] in its playbook, so it has no build lanes; run the command in your worktree, or add [lanes] (docs/build-lanes.md)", repo_root.display());
    };
    let root = repo_root.to_string_lossy().into_owned();
    // Land first, then submit's gate, then everything else (the scheduler, NIT-199, orders by it).
    let priority = match req.kind {
        Kind::Land => 1,
        Kind::Gate => 2,
        Kind::Run => 3,
    };
    db.conn.execute(
        "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'local', ?10, ?11, ?12)",
        params![req.project.to_uppercase(), root, req.issue, actor.id(), req.kind.as_str(), req.snapshot.commit, req.snapshot.tree, req.snapshot.base, req.command, now(), std::process::id(), priority],
    )?;
    let id = db.conn.last_insert_rowid();
    // A made snapshot exists only as an object: a ref keeps git's gc from taking it while it waits.
    if req.snapshot.made {
        git(repo_root, &["update-ref", &format!("refs/nitely/runs/{id}"), &req.snapshot.commit]).ok();
    }
    expire(db, repo_root, req.project);
    if let Some(r) = db.lane_run(id)? {
        on_event(&r);
    }

    // Until it runs, a failure here (a lock that cannot be opened, a log folder that cannot be made)
    // closes the request's row: a queued row left behind would hold up the requests after it.
    let started = (|| -> Result<_> {
        let (n, lock, slot) = take_turn(db, id, repo_root, pb, &rules, machine_budget().slots, &mut read_memory, &mut memory_limits)?;
        let logs = logs_dir(db, req.project);
        std::fs::create_dir_all(&logs)?;
        let log = logs.join(format!("{id}.log"));
        db.conn.execute(
            "UPDATE lane_run SET state = 'running', lane = ?2, started_at = ?3, log_path = ?4 WHERE id = ?1",
            params![id, n, now(), log.to_string_lossy()],
        )?;
        Ok((n, lock, slot, log))
    })();
    let (n, _lock, _slot, log) = match started {
        Ok(s) => s,
        Err(e) => {
            db.conn
                .execute(
                    "UPDATE lane_run SET state = 'lane_error', ended_at = ?2, error = ?3 WHERE id = ?1 AND state IN ('queued', 'running')",
                    params![id, now(), format!("{e:#}")],
                )
                .ok();
            return Err(e);
        }
    };
    let lane = lane_path(repo_root, pb, n);
    if let Some(r) = db.lane_run(id)? {
        on_event(&r);
    }

    let outcome = serve(repo_root, pb, &rules, &lane, &log, req, exec);
    let ended = now();
    match outcome {
        Ok(o) => {
            db.conn.execute(
                "UPDATE lane_run SET state = 'done', ended_at = ?2, exit_code = ?3, timed_out = ?4, setup_ran = ?5, files_switched = ?6, output = ?7, peak_memory = ?8 WHERE id = ?1",
                params![id, ended, o.exit_code, o.timed_out, o.setup_ran, o.files_switched, o.output, o.peak.map(|p| p as i64)],
            )?;
        }
        Err(e) => {
            let why = format!("{e:#}");
            let output = std::fs::read_to_string(&log).map(|t| playbook::tail(&t)).unwrap_or_default();
            db.conn.execute(
                "UPDATE lane_run SET state = 'lane_error', ended_at = ?2, error = ?3, output = ?4 WHERE id = ?1",
                params![id, ended, why, output],
            )?;
        }
    }
    let done = db.lane_run(id)?.context("the run's record went missing")?;
    let at = if rules.large_repo { "the main checkout".to_string() } else { format!("lane {n}") };
    let said = match (&done.error, done.exit_code, done.timed_out) {
        (Some(e), _, _) if rules.large_repo => format!("The main checkout could not run `{}`: {}", done.command, e.lines().next().unwrap_or("")),
        (Some(e), _, _) => format!("Lane {n} could not run `{}`: {}", done.command, e.lines().next().unwrap_or("")),
        (None, _, true) => format!("`{}` ran past {} minutes in {at} and was stopped", done.command, rules.timeout_minutes),
        (None, Some(code), _) => format!("Ran `{}` in {at}: exit {code} after {} s", done.command, ended - done.started_at.unwrap_or(ended)),
        (None, None, _) => format!("Ran `{}` in {at}", done.command),
    };
    if let Some(key) = req.issue {
        db.log_event(actor, Some(key), Some(&lane.to_string_lossy()), "lane_run", &format!("{said} (run {id})")).ok();
    }
    Ok(done)
}

/// A gate that ran in a lane (or was answered from one), for `playbook::run_gate` to record the way
/// it records a gate in a worktree.
pub struct GateRun {
    /// The snapshot's short commit.
    pub commit: String,
    pub results: Vec<playbook::CommandResult>,
    pub passed: bool,
    pub flaked: Vec<playbook::Flake>,
    /// The lane run it was, or the earlier one it was answered from.
    pub run: Option<i64>,
    /// What the caller should know: uncommitted work left out of the gate.
    pub warnings: Vec<String>,
}

/// The gate's `steps` (command, why it is skipped) for `worktree`, as one lane request: one switch
/// to the worktree's snapshot, then each command that is not skipped, in order, each run again up to
/// `[gate] retries` times when it fails (a pass after a failure is a flake), stopping at the first
/// that keeps failing. A tree whose same commands already passed in a lane is answered from that
/// run's record without running anything.
#[allow(clippy::too_many_arguments)]
pub fn gate(db: &Db, repo_root: &Path, pb: &Playbook, worktree: &Path, issue: Option<&str>, kind: Kind, steps: &[(String, Option<String>)], actor: &Actor) -> Result<GateRun> {
    // What is committed, as land will build it: the branch head, never a commit made for the
    // request, so the gate reports the commit the review sees.
    let top = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"])?.trim());
    let head = git(&top, &["rev-parse", "HEAD"])?.trim().to_string();
    let snap = Snapshot { tree: git(&top, &["rev-parse", "HEAD^{tree}"])?.trim().to_string(), commit: head.clone(), base: head, made: false };
    let mut warnings = Vec::new();
    if snapshot(worktree)?.made {
        warnings.push("the worktree has uncommitted work, which this gate leaves out: it builds what is committed, as land will; `run` builds the worktree as it is".to_string());
    }
    let short = snap.commit[..snap.commit.len().min(7)].to_string();
    let project = issue.and_then(|k| db.issue(k).ok().flatten()).and_then(|i| i.project).unwrap_or_default();
    let base = pb.integration_branch().to_string();
    let skipped = |why: &str, command: &str| playbook::CommandResult { command: command.to_string(), ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true };
    // The commands as the playbook writes them: the record's command, and what an earlier pass of
    // the same tree must have run.
    let to_run: Vec<&str> = steps.iter().filter(|(_, skip)| skip.is_none()).map(|(c, _)| c.as_str()).collect();
    let key = to_run.join("\n");
    let root = repo_root.to_string_lossy().into_owned();
    // One command per line is the key, so a command that holds a line break of its own could
    // stand for two: such a gate is never answered from a record.
    let keyed = !to_run.iter().any(|c| c.contains('\n'));
    let passed_before: Option<i64> = if !keyed {
        None
    } else {
        db.conn
            .query_row(
                "SELECT id FROM lane_run WHERE repo = ?1 AND tree = ?2 AND command = ?3 AND kind IN ('gate', 'land') AND state = 'done' AND exit_code = 0 AND executor = 'local' ORDER BY id DESC LIMIT 1",
                params![root, snap.tree, key],
                |r| r.get(0),
            )
            .optional()?
    };
    if let Some(id) = passed_before {
        let results = steps
            .iter()
            .map(|(command, skip)| match skip {
                Some(why) => skipped(why, command),
                None => skipped(&format!("this tree passed it in lane run {id}; not run again"), command),
            })
            .collect();
        if let Some(key) = issue {
            db.log_event(actor, Some(key), None, "lane_run", &format!("The gate for {short} passed in lane run {id} already; not run again")).ok();
        }
        return Ok(GateRun { commit: short, results, passed: true, flaked: vec![], run: Some(id), warnings });
    }

    let mut results: Vec<playbook::CommandResult> = Vec::new();
    let mut flaked: Vec<playbook::Flake> = Vec::new();
    let retries = pb.gate.retries;
    let share = share();
    let req = Request { project: &project, issue, kind, snapshot: &snap, command: &key };
    let run = run_with(db, repo_root, pb, &req, actor, &mut |_| {}, &mut |lane: &Path, log: &Path, limit: Duration| {
        let mut peak: Option<u64> = None;
        for (command, skip) in steps {
            if let Some(why) = skip {
                results.push(skipped(why, command));
                continue;
            }
            let command = playbook::expand(command, repo_root, lane, &base).replace("{jobs}", &share.jobs.to_string());
            let started = Instant::now();
            let mut failures: Vec<String> = Vec::new();
            loop {
                let from = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
                let ended = run_logged(&command, lane, log, limit, share)?;
                let (code, timed_out) = (ended.code, ended.timed_out);
                peak = peak.max(ended.peak);
                let output = log_since(log, from);
                let seconds = started.elapsed().as_secs();
                if timed_out {
                    results.push(playbook::CommandResult { command: command.clone(), ok: false, seconds, output, skipped: false });
                    return Ok(Ended { code: None, timed_out: true, peak });
                }
                if code == Some(0) {
                    if !failures.is_empty() {
                        let tests = failures.iter().flat_map(|o| playbook::failed_tests(o)).fold(Vec::<String>::new(), |mut all, t| {
                            if !all.contains(&t) {
                                all.push(t);
                            }
                            all
                        });
                        flaked.push(playbook::Flake { command: command.clone(), tests, output: failures.swap_remove(0) });
                    }
                    results.push(playbook::CommandResult { command: command.clone(), ok: true, seconds, output, skipped: false });
                    break;
                }
                if failures.len() as u32 >= retries {
                    results.push(playbook::CommandResult { command: command.clone(), ok: false, seconds, output, skipped: false });
                    return Ok(Ended { code, timed_out: false, peak });
                }
                failures.push(output);
            }
        }
        Ok(Ended { code: Some(0), timed_out: false, peak })
    })?;
    if let Some(why) = &run.error {
        // The setup's own output too, so the caller sees why it failed.
        let output = if run.output.trim().is_empty() { why.clone() } else { format!("{why}\n{}", run.output) };
        results.push(playbook::CommandResult { command: format!("lane {}", run.lane.unwrap_or(0)), ok: false, seconds: 0, output, skipped: false });
    }
    Ok(GateRun { commit: short, passed: run.passed(), results, flaked, run: Some(run.id), warnings })
}

/// What `log` gained after byte `from`, its end kept as a command's output is.
fn log_since(log: &Path, from: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut text = Vec::new();
    if let Ok(mut f) = std::fs::File::open(log) {
        if f.seek(SeekFrom::Start(from)).is_ok() {
            f.read_to_end(&mut text).ok();
        }
    }
    playbook::tail(&String::from_utf8_lossy(&text))
}

/// Deletes snapshot refs and logs older than `KEEP_SECS`. Best effort.
fn expire(db: &Db, repo_root: &Path, project: &str) {
    let cutoff = now() - KEEP_SECS;
    // This repo's runs only: its refs are the ones this can delete.
    let Ok(mut stmt) = db.conn.prepare("SELECT id, log_path FROM lane_run WHERE project = ?1 COLLATE NOCASE AND repo = ?2 AND queued_at < ?3 AND log_path IS NOT NULL") else { return };
    let repo = repo_root.to_string_lossy().into_owned();
    let old: Vec<(i64, String)> = stmt.query_map(params![project, repo, cutoff], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default();
    for (id, log) in old {
        std::fs::remove_file(&log).ok();
        git(repo_root, &["update-ref", "-d", &format!("refs/nitely/runs/{id}")]).ok();
        db.conn.execute("UPDATE lane_run SET log_path = NULL WHERE id = ?1", [id]).ok();
    }
}

/// A lane's lock: an OS file lock, so a process that dies lets go of it.
pub struct LaneLock(#[allow(dead_code)] std::fs::File);

fn lock_path(repo_root: &Path, pb: &Playbook, n: u32) -> PathBuf {
    dir(repo_root, pb).join(format!("lane-{n}.lock"))
}

fn try_lock(repo_root: &Path, pb: &Playbook, n: u32) -> Result<Option<LaneLock>> {
    let path = lock_path(repo_root, pb, n);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LaneLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

// ----- the machine's build budget and its queue (NIT-199) -----

/// How many lane requests this machine runs at once across every project, and the jobs each gets.
#[derive(Debug, Clone, Serialize)]
pub struct Budget {
    pub slots: u32,
    /// Cores for one request: the cores left for builds (all but the headroom) shared by its slots.
    pub jobs: u32,
    pub cores: u32,
    pub memory_gb: Option<u64>,
    /// Where `slots` comes from: `machine` (cores and memory), `settings` (`build_slots` in
    /// Nitely's config) or `NITELY_BUILD_SLOTS`.
    pub from: String,
}

/// Cores per slot, when the machine decides.
const CORES_PER_SLOT: u32 = 8;

/// The cores builds may use between them: all but 2, or 75%, whichever is more, so the person's
/// own apps always have some (NIT-266).
pub fn build_cores(cores: u32) -> u32 {
    let cores = cores.max(1);
    cores.saturating_sub(2).max(cores * 3 / 4).max(1)
}

/// The budget for `cores` and the machine's memory `limits`, unless `set` says how many slots.
/// When the machine decides, a slot per 8 cores, and no more slots than runs at their memory cap
/// fit above the floor.
pub fn budget_for(cores: u32, limits: Option<&machine::Limits>, set: Option<(u32, &str)>) -> Budget {
    let cores = cores.max(1);
    let (slots, from) = match set.filter(|(n, _)| *n > 0) {
        Some((n, from)) => (n, from.to_string()),
        None => {
            let by_cores = cores / CORES_PER_SLOT;
            let by_memory = limits.map(|l| (l.total.saturating_sub(l.floor) / l.cap.max(1)) as u32).unwrap_or(by_cores);
            (by_cores.min(by_memory).max(1), "machine".to_string())
        }
    };
    // Rounded, so 63.9 GB of usable memory counts as the 64 it is sold as.
    let memory_gb = limits.map(|l| (l.total + (1 << 29)) >> 30);
    Budget { slots, jobs: (build_cores(cores) / slots).max(1), cores, memory_gb, from }
}

/// This machine's budget: `NITELY_BUILD_SLOTS`, else `build_slots` in Nitely's config, else from
/// its cores and memory.
pub fn machine_budget() -> Budget {
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let env = std::env::var("NITELY_BUILD_SLOTS").ok().and_then(|v| v.trim().parse::<u32>().ok());
    let settings = crate::Config::load_read_only().ok().and_then(|c| c.build_slots);
    let set = env.map(|n| (n, "NITELY_BUILD_SLOTS")).or(settings.map(|n| (n, "settings")));
    budget_for(cores, machine::limits().as_ref(), set)
}

/// What one request gets of the machine: cores for its build tools, and the memory it and all it
/// starts may hold (NIT-266).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Share {
    pub jobs: u32,
    pub cap: Option<u64>,
}

/// This machine's share for one request.
pub fn share() -> Share {
    Share { jobs: machine_budget().jobs, cap: machine::limits().map(|l| l.cap) }
}

/// Holds a build command to its share of cores: the build tools that read one get it from the
/// environment (`NITELY_JOBS`, `CARGO_BUILD_JOBS`, `MAKEFLAGS`, and `RUST_TEST_THREADS` for the
/// tests cargo runs).
pub fn share_env(cmd: &mut std::process::Command, jobs: u32) {
    let jobs = jobs.to_string();
    cmd.env("NITELY_JOBS", &jobs).env("CARGO_BUILD_JOBS", &jobs).env("RUST_TEST_THREADS", &jobs).env("MAKEFLAGS", format!("-j{jobs}"));
}

/// `command` with `--test-concurrency=<jobs>` after a `node --test` that sets none: node runs a
/// test file per core otherwise, and takes no such setting from the environment.
pub fn node_tests_share(command: &str, jobs: u32) -> String {
    static NODE_TEST: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if command.contains("--test-concurrency") {
        return command.to_string();
    }
    let re = NODE_TEST.get_or_init(|| regex::Regex::new(r"(^|[\s&|;(])(node(?:\.exe)?(?:\s+-[^\s]+)*?\s+--test)(\s|$)").unwrap());
    re.replace_all(command, |c: &regex::Captures| format!("{}{} --test-concurrency={jobs}{}", &c[1], &c[2], &c[3])).into_owned()
}

/// The machine's memory now, as the scheduler reads it (a test's own readings in tests).
#[cfg(not(test))]
fn read_memory() -> Option<machine::Memory> {
    machine::memory()
}

/// The limits admission holds requests to (None in tests that give no readings: never waits).
#[cfg(not(test))]
fn memory_limits() -> Option<machine::Limits> {
    machine::limits()
}

#[cfg(test)]
fn read_memory() -> Option<machine::Memory> {
    tests::FAKE_MEMORY.with(|f| f.borrow_mut().as_mut().and_then(|next| next()))
}

#[cfg(test)]
fn memory_limits() -> Option<machine::Limits> {
    let floor = tests::FAKE_FLOOR.with(|f| f.get()).map(|gb| (gb, "settings"));
    tests::FAKE_MEMORY.with(|f| f.borrow().is_some()).then(|| machine::limits_for(64 * machine::GIB, floor, None))
}

/// A machine slot's lock: an OS file lock beside the database, so a process that dies frees it.
pub struct SlotLock(#[allow(dead_code)] std::fs::File);

fn try_slot(db: &Db, k: u32) -> Result<Option<SlotLock>> {
    let dir = state_dir(db);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("slot-{k}.lock"));
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(SlotLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// A queued request as the queue orders them.
#[derive(Debug, Clone)]
struct Waiting {
    id: i64,
    project: String,
    repo: String,
    priority: i64,
}

/// The order requests start in: priority first (land, then submit's gate, then the rest), then
/// each project's first waiting request before any project's second, then the oldest.
/// Among requests at the same place in their projects' queues, the project served longest ago
/// goes first (`last_started`, by upper-case project; never served counts as longest ago), so a
/// project whose request just started or ran waits behind the others' next, whenever its own was
/// queued.
fn in_order(mut waiting: Vec<Waiting>, last_started: &std::collections::HashMap<String, i64>) -> Vec<Waiting> {
    waiting.sort_by_key(|w| (w.priority, w.id));
    let mut seen: std::collections::HashMap<(i64, String), usize> = std::collections::HashMap::new();
    let mut ranked: Vec<(i64, usize, i64, i64, Waiting)> = waiting
        .into_iter()
        .map(|w| {
            let project = w.project.to_uppercase();
            let n = seen.entry((w.priority, project.clone())).or_default();
            *n += 1;
            let last = last_started.get(&project).copied().unwrap_or(i64::MIN);
            (w.priority, *n, last, w.id, w)
        })
        .collect();
    ranked.sort_by_key(|(p, n, last, id, _)| (*p, *n, *last, *id));
    ranked.into_iter().map(|(_, _, _, _, w)| w).collect()
}

/// Whether request `id` may try for a slot and a lane now: no request of its own repo is ahead of
/// it, and fewer requests ahead of it, of repos with a free lane, than there are free slots. A
/// repo whose lanes are all busy does not hold up the others.
fn my_turn(db: &Db, id: i64, slots: u32, alive: &mut dyn FnMut(i64) -> bool) -> Result<bool> {
    let mut stmt = db.conn.prepare("SELECT id, project, repo, priority, state, pid, lane_count, waiting_for IS NOT NULL FROM lane_run WHERE state IN ('queued', 'running')")?;
    let rows: Vec<(Waiting, String, Option<i64>, i64, bool)> = stmt
        .query_map([], |r| Ok((Waiting { id: r.get(0)?, project: r.get(1)?, repo: r.get(2)?, priority: r.get(3)? }, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut held_by_checkout: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut running_by_repo: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut lanes_by_repo: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut running = 0i64;
    let mut waiting = Vec::new();
    for (w, state, pid, lane_count, waits_for_checkout) in rows {
        let repo = crate::paths::normalize(&w.repo);
        // Waiting for the person's checkout (large-repo mode), neither it nor its repo's requests
        // behind it can start, so they hold up no other project's request.
        if waits_for_checkout && w.id != id {
            held_by_checkout.insert(repo.clone());
        }
        lanes_by_repo.insert(repo.clone(), lane_count.max(1));
        // A request whose process is gone holds nothing and waits for nothing.
        if w.id != id && pid.is_some_and(|p| !alive(p)) {
            continue;
        }
        if state == "running" {
            running += 1;
            *running_by_repo.entry(repo).or_default() += 1;
        } else {
            waiting.push(w);
        }
    }
    // When each project last had a turn (a request of it started), so the one served longest ago
    // goes first and one project's backlog never holds up the rest.
    let mut served: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut stmt = db.conn.prepare("SELECT project, MAX(started_at) FROM lane_run WHERE started_at IS NOT NULL GROUP BY project")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (project, at) = row?;
        let slot = served.entry(project.to_uppercase()).or_insert(at);
        *slot = (*slot).max(at);
    }
    let free_slots = (slots as i64 - running).max(0);
    let mine = crate::paths::normalize(&waiting.iter().find(|w| w.id == id).map(|w| w.repo.clone()).unwrap_or_default());
    let mut ahead_that_can_start = 0;
    for w in in_order(waiting, &served) {
        if w.id == id {
            return Ok(ahead_that_can_start < free_slots);
        }
        let repo = crate::paths::normalize(&w.repo);
        if repo == mine {
            return Ok(false);
        }
        let free_lanes = lanes_by_repo.get(&repo).copied().unwrap_or(1) - running_by_repo.get(&repo).copied().unwrap_or(0);
        if free_lanes > 0 && !held_by_checkout.contains(&repo) {
            ahead_that_can_start += 1;
        }
    }
    // Not queued (any more): nothing to wait for.
    Ok(true)
}

/// Waits for request `id`'s turn, then holds a machine slot and one of the repo's lanes, and starts
/// only once the machine's memory admits it (`machine::admit_once` with `limits`, read again on
/// every look so a change in Settings counts at once, reading `memory`): until then it lets both go
/// again and says why it waits (`waiting_memory`).
#[allow(clippy::too_many_arguments)]
fn take_turn(
    db: &Db,
    id: i64,
    repo_root: &Path,
    pb: &Playbook,
    rules: &LaneRules,
    slots: u32,
    memory: &mut dyn FnMut() -> Option<machine::Memory>,
    limits: &mut dyn FnMut() -> Option<machine::Limits>,
) -> Result<(u32, LaneLock, SlotLock)> {
    db.conn.execute("UPDATE lane_run SET lane_count = ?2 WHERE id = ?1", params![id, rules.lanes()])?;
    // Whether a requester's process still runs, asked again every few seconds at most.
    let mut known: std::collections::HashMap<i64, (bool, Instant)> = std::collections::HashMap::new();
    let mut alive = |pid: i64| -> bool {
        if pid as u32 == std::process::id() {
            return true;
        }
        match known.get(&pid) {
            Some((is, at)) if at.elapsed() < Duration::from_secs(5) => *is,
            _ => {
                let is = crate::crew::pid_alive(pid as u32);
                known.insert(pid, (is, Instant::now()));
                is
            }
        }
    };
    let mut busy_since = None;
    let mut waits_for_memory = false;
    'turn: loop {
        if my_turn(db, id, slots, &mut alive)? {
            // A checkout a request has (or a dead one left) is looked at under the lock, where one
            // left behind is put back first.
            if rules.large_repo && lent_to_request(repo_root).is_none() {
                if let Some(why) = checkout_busy(repo_root, pb, &rules.keep)? {
                    wait_for_checkout(db, id, &why, &mut busy_since, rules)?;
                    continue 'turn;
                }
            }
            for k in 1..=slots.max(1) {
                let Some(slot) = try_slot(db, k)? else { continue };
                for n in 1..=rules.lanes() {
                    if let Some(lock) = try_lock(repo_root, pb, n)? {
                        // The person's checkout: checked again under the lock, right before use.
                        if rules.large_repo {
                            recover_large(repo_root, &rules.keep);
                            if let Some(why) = checkout_busy(repo_root, pb, &rules.keep)? {
                                drop(lock);
                                drop(slot);
                                wait_for_checkout(db, id, &why, &mut busy_since, rules)?;
                                continue 'turn;
                            }
                            db.conn.execute("UPDATE lane_run SET waiting_for = NULL WHERE id = ?1", [id])?;
                        }
                        if let Some(why) = machine::admit_once(&state_dir(db), limits().as_ref(), memory)? {
                            // Held while it waits, the slot and lane would keep the rest waiting too.
                            drop(lock);
                            drop(slot);
                            db.conn.execute("UPDATE lane_run SET waiting_memory = ?2 WHERE id = ?1", params![id, why])?;
                            waits_for_memory = true;
                            std::thread::sleep(Duration::from_secs(2));
                            continue 'turn;
                        }
                        if waits_for_memory {
                            db.conn.execute("UPDATE lane_run SET waiting_memory = NULL WHERE id = ?1", [id])?;
                        }
                        return Ok((n, lock, slot));
                    }
                }
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A gate that runs in a worktree, outside lanes (a repo without `[lanes]`), holding one of the
/// machine's build slots as a lane request does: a row of its own (executor `worktree`) that the
/// scheduler counts and orders like the rest, the same wait for memory, and `running_on_machine`
/// counts it (NIT-266). `finish` ends the row; dropped first, it ends as a lane problem.
pub struct MachineTurn<'a> {
    db: &'a Db,
    id: i64,
    _slot: SlotLock,
    finished: bool,
}

impl MachineTurn<'_> {
    /// The row's id.
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn finish(mut self, passed: bool) {
        self.db
            .conn
            .execute("UPDATE lane_run SET state = 'done', ended_at = ?2, exit_code = ?3 WHERE id = ?1", params![self.id, now(), if passed { 0 } else { 1 }])
            .ok();
        self.finished = true;
    }
}

impl Drop for MachineTurn<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.db
                .conn
                .execute(
                    "UPDATE lane_run SET state = 'lane_error', ended_at = ?2, error = 'the gate stopped before it finished' WHERE id = ?1 AND state IN ('queued', 'running')",
                    params![self.id, now()],
                )
                .ok();
        }
    }
}

/// Waits for a machine slot and the machine's memory for a gate in `worktree` outside lanes, and
/// holds the slot until the turn ends (`MachineTurn`).
#[allow(clippy::too_many_arguments)]
pub fn hold_machine<'a>(db: &'a Db, repo_root: &Path, worktree: &Path, project: &str, issue: Option<&str>, kind: Kind, command: &str, actor: &Actor) -> Result<MachineTurn<'a>> {
    let rev = |what: &str| git(worktree, &["rev-parse", what]).map(|s| s.trim().to_string()).unwrap_or_default();
    let (head, tree) = (rev("HEAD"), rev("HEAD^{tree}"));
    let priority = match kind {
        Kind::Land => 1,
        Kind::Gate => 2,
        Kind::Run => 3,
    };
    let slots = machine_budget().slots;
    db.conn.execute(
        "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8, 'queued', 'worktree', ?9, ?10, ?11, ?12)",
        params![project.to_uppercase(), repo_root.to_string_lossy(), issue, actor.id(), kind.as_str(), head, tree, command, now(), std::process::id(), priority, slots],
    )?;
    let id = db.conn.last_insert_rowid();
    let mut waits_for_memory = false;
    let taken = (|| -> Result<SlotLock> {
        let mut alive = |pid: i64| pid as u32 == std::process::id() || crate::crew::pid_alive(pid as u32);
        loop {
            if my_turn(db, id, slots, &mut alive)? {
                if let Some(slot) = (1..=slots.max(1)).find_map(|k| try_slot(db, k).ok().flatten()) {
                    // The limits read again on every look, so a change in Settings counts at once.
                    match machine::admit_once(&state_dir(db), memory_limits().as_ref(), &mut read_memory)? {
                        None => return Ok(slot),
                        Some(why) => {
                            drop(slot);
                            db.conn.execute("UPDATE lane_run SET waiting_memory = ?2 WHERE id = ?1", params![id, why])?;
                            waits_for_memory = true;
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    })();
    let slot = match taken {
        Ok(slot) => slot,
        Err(e) => {
            db.conn
                .execute("UPDATE lane_run SET state = 'lane_error', ended_at = ?2, error = ?3 WHERE id = ?1", params![id, now(), format!("{e:#}")])
                .ok();
            return Err(e);
        }
    };
    let clear = if waits_for_memory { ", waiting_memory = NULL" } else { "" };
    db.conn.execute(&format!("UPDATE lane_run SET state = 'running', started_at = ?2{clear} WHERE id = ?1"), params![id, now()])?;
    Ok(MachineTurn { db, id, _slot: slot, finished: false })
}

/// Records why request `id` waits for the person's checkout, and gives up after the lane's
/// timeout: a checkout left dirty all afternoon is not one to build in.
fn wait_for_checkout(db: &Db, id: i64, why: &str, since: &mut Option<Instant>, rules: &LaneRules) -> Result<()> {
    db.conn.execute("UPDATE lane_run SET waiting_for = ?2 WHERE id = ?1", params![id, why])?;
    let minutes = rules.timeout_minutes.max(1);
    if since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(minutes * 60) {
        let long = if minutes == 1 { "a minute".to_string() } else { format!("{minutes} minutes") };
        bail!("{why}, for over {long}, so the request was not run; it runs only while your checkout is clean and on its integration branch");
    }
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}

struct Outcome {
    exit_code: Option<i32>,
    timed_out: bool,
    peak: Option<u64>,
    setup_ran: bool,
    files_switched: i64,
    output: String,
}

/// The four steps, in a lane this process holds.
fn serve(repo_root: &Path, pb: &Playbook, rules: &LaneRules, lane: &Path, log: &Path, req: &Request, exec: &mut Exec) -> Result<Outcome> {
    let base = pb.integration_branch();
    if git(repo_root, &["cat-file", "-e", &format!("{}^{{commit}}", req.snapshot.commit)]).is_err() {
        bail!("snapshot {} is missing from the repo (expired or cleaned up); make a new one", &req.snapshot.commit[..req.snapshot.commit.len().min(9)]);
    }
    let limit = Duration::from_secs(rules.timeout_minutes.max(1) * 60);
    if rules.large_repo {
        // The person's checkout: no forced checkout, no clean, nothing else stopped; and back on
        // its branch afterwards, whatever came of the command.
        stop_recorded(lane);
        let switched = git(lane, &["diff", "--name-only", "HEAD", &req.snapshot.commit]).map(|o| o.lines().count() as i64).unwrap_or(0);
        switch_large(lane, base, &req.snapshot.commit, &rules.keep)?;
        let ran = (|| -> Result<(bool, Ended)> {
            let setup_ran = ensure_setup(repo_root, pb, lane, base, log, limit)?;
            Ok((setup_ran, exec(lane, log, limit)?))
        })();
        let back = return_large(lane, base);
        let (setup_ran, ended) = ran?;
        back.with_context(|| format!("the command ran (exit {}), but", ended.code.map_or("none".to_string(), |c| c.to_string())))?;
        let output = std::fs::read_to_string(log).map(|t| playbook::tail(&t)).unwrap_or_default();
        return Ok(Outcome { exit_code: ended.code, timed_out: ended.timed_out, peak: ended.peak, setup_ran, files_switched: switched, output });
    }
    let files_switched = if !lane.join(".git").exists() {
        create_lane(repo_root, lane, &req.snapshot.commit)?;
        git(lane, &["ls-files"]).map(|o| o.lines().count() as i64).unwrap_or(0)
    } else {
        stop_leftovers(lane);
        let switched = git(lane, &["diff", "--name-only", "HEAD", &req.snapshot.commit]).map(|o| o.lines().count() as i64).unwrap_or(0);
        switch(lane, &req.snapshot.commit, &rules.keep)?;
        switched
    };
    crate::submodules::sync_lane(repo_root, pb, lane, &req.snapshot.commit, &rules.keep)?;
    let setup_ran = ensure_setup(repo_root, pb, lane, base, log, limit)?;
    let Ended { code: exit_code, timed_out, peak } = exec(lane, log, limit)?;
    let output = std::fs::read_to_string(log).map(|t| playbook::tail(&t)).unwrap_or_default();
    // Between requests, still holding the lane: best effort, it never fails the request.
    if let Some(record) = git_path(lane, SIZE_RECORD) {
        hygiene(lane, rules, &record, log);
    }
    Ok(Outcome { exit_code, timed_out, peak, setup_ran, files_switched, output })
}

/// The file in a lane's git folder that keeps the size of its build directories after its last
/// request and after its first one: "<bytes> <first bytes>".
const SIZE_RECORD: &str = "nitely-lane-size";

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A lane's size cap in bytes: the playbook's, else twice its first build.
fn cap_bytes(rules: &LaneRules, first: u64) -> u64 {
    match rules.size_cap_gb {
        Some(gb) if gb > 0.0 => (gb * GB) as u64,
        _ => first.saturating_mul(2),
    }
}

/// What a lane's size record says: (size after the last request, size after the first), if any.
fn read_size(record: &Path) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(record).ok()?;
    let mut parts = text.split_whitespace().map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??))
}

/// Keeps a lane within its size cap (docs/build-lanes.md, "Lane hygiene"): over the cap, the
/// incremental caches in its build directories (`*/incremental`, cargo's) are emptied; over twice
/// the cap, its build directories go and the next request builds cold. Records the size after, and
/// the first build's size the first time. Nothing it removes is anyone's work. Says what it did in
/// the log.
fn hygiene(lane: &Path, rules: &LaneRules, record: &Path, log: &Path) {
    use std::io::Write;
    let measured = keep_size(lane, &rules.keep);
    let first = read_size(record).map(|(_, f)| f).filter(|f| *f > 0).unwrap_or(measured);
    let cap = cap_bytes(rules, first);
    let mut size = measured;
    let mut said = None;
    if cap > 0 && size > cap.saturating_mul(2) {
        for d in rules.keep.iter().filter_map(|k| keep_dir(lane, k)) {
            std::fs::remove_dir_all(&d).ok();
        }
        size = keep_size(lane, &rules.keep);
        said = Some(format!("over twice its size cap ({} of {}): removed its build directories; the next request builds cold", human(measured), human(cap)));
    } else if cap > 0 && size > cap {
        for d in rules.keep.iter().filter_map(|k| keep_dir(lane, k)) {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let incremental = e.path().join("incremental");
                if !is_link(&e.path()) && incremental.is_dir() && !is_link(&incremental) {
                    std::fs::remove_dir_all(incremental).ok();
                }
            }
        }
        size = keep_size(lane, &rules.keep);
        said = Some(format!("over its size cap ({} of {}): emptied the incremental caches, now {}", human(measured), human(cap), human(size)));
    }
    std::fs::write(record, format!("{size} {first}")).ok();
    if let (Some(said), Ok(mut f)) = (said, std::fs::OpenOptions::new().append(true).open(log)) {
        writeln!(f, "nitely: lane {said}").ok();
    }
}

/// The bytes in a lane's build directories, links (symlinks, junctions) not followed.
fn keep_size(lane: &Path, keep: &[String]) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                _ if is_link(&e.path()) => 0,
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    keep.iter().filter_map(|k| keep_dir(lane, k)).map(|d| walk(&d)).sum()
}

/// A `keep` entry as a folder inside `lane`, or None when it is not a plain relative path (`..`,
/// an absolute or drive path) or any folder on the way to it is a link: hygiene never touches
/// anything outside the lane.
fn keep_dir(lane: &Path, keep: &str) -> Option<PathBuf> {
    let rel = Path::new(keep.trim_matches('/'));
    if rel.components().count() == 0 || !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
        return None;
    }
    let mut dir = lane.to_path_buf();
    for c in rel.components() {
        dir.push(c);
        if is_link(&dir) {
            return None;
        }
    }
    Some(dir)
}

/// Whether `path` is a link to somewhere else: a symlink, or on Windows any reparse point (a
/// junction). Hygiene never deletes through one.
fn is_link(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return false };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    false
}

/// "8.3 GB", "640 MB".
fn human(bytes: u64) -> String {
    let gb = bytes as f64 / GB;
    if gb >= 1.0 { format!("{gb:.1} GB") } else { format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0)) }
}

/// Makes lane `lane` as a worktree at `commit`; `ensure_setup` sets it up.
fn create_lane(repo_root: &Path, lane: &Path, commit: &str) -> Result<()> {
    std::fs::create_dir_all(lane.parent().unwrap())?;
    // A folder left by a lane whose registration is gone (a crash during creation) goes first.
    git(repo_root, &["worktree", "prune"]).ok();
    if lane.exists() {
        std::fs::remove_dir_all(lane).with_context(|| format!("removing the half-made lane {}", lane.display()))?;
    }
    git(repo_root, &["worktree", "add", "--detach", &lane.to_string_lossy(), commit])?;
    Ok(())
}

/// Runs the playbook's setup in `lane` when it has never passed there, or a dependency file differs
/// from the last time it passed; each command bounded by `limit`. The record of a passed setup is
/// written only after every command passed, so a setup that failed or was stopped runs again next
/// time. Returns whether setup ran.
fn ensure_setup(repo_root: &Path, pb: &Playbook, lane: &Path, base: &str, log: &Path, limit: Duration) -> Result<bool> {
    if pb.worktree.setup.is_empty() {
        return Ok(false);
    }
    let record = git_path(lane, "nitely-lane-setup").context("the lane has no git folder for its setup record")?;
    let now = playbook::setup_fingerprint(&pb.worktree, lane);
    // The record: the fingerprint, then the untracked paths setup made. Setup ran and still holds
    // only while both match: a clean that took what setup made (not in `keep`) means it runs again.
    if let Ok(last) = std::fs::read_to_string(&record) {
        let mut lines = last.lines();
        let same = lines.next().is_some_and(|f| f.trim() == now);
        if same && lines.filter(|p| !p.trim().is_empty()).all(|p| lane.join(p.trim()).exists()) {
            return Ok(false);
        }
    }
    std::fs::remove_file(&record).ok();
    let share = share();
    for command in &pb.worktree.setup {
        let command = playbook::expand(command, repo_root, lane, base).replace("{jobs}", &share.jobs.to_string());
        // Run again on failure as the gate's commands are (`[gate] retries`): a setup that fails
        // once, on a busy machine, then passes is not the change's fault.
        let mut tries = 0;
        loop {
            let ended = run_logged(&command, lane, log, limit, share)?;
            match (ended.code, ended.timed_out) {
                (Some(0), _) => break,
                (_, true) => bail!("setup ran past {} minutes in the lane and was stopped: {command}", limit.as_secs() / 60),
                (code, false) if tries >= pb.gate.retries => {
                    bail!("setup failed in the lane (exit {}): {command}", code.map_or("none".to_string(), |c| c.to_string()))
                }
                _ => tries += 1,
            }
        }
    }
    // What setup left that git does not track, by its top-level path: `node_modules`, `.venv`.
    let status = git(lane, &["status", "--porcelain=v1", "-z", "--ignored=matching", "--untracked-files=normal"]).unwrap_or_default();
    let mut made: Vec<String> = status
        .split('\0')
        .filter_map(|e| e.strip_prefix("!! ").or_else(|| e.strip_prefix("?? ")))
        // The whole path git names (`packages/app/node_modules`), not its first part, which tracked
        // files would keep present after a clean took what setup made.
        .map(|p| p.trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    made.sort();
    made.dedup();
    std::fs::write(&record, format!("{now}\n{}", made.join("\n"))).with_context(|| format!("writing {}", record.display()))?;
    Ok(true)
}

/// A file in `dir`'s own git folder (per worktree), named `name`.
fn git_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = git(dir, &["rev-parse", "--git-path", name]).ok()?;
    let path = PathBuf::from(path.trim());
    Some(if path.is_absolute() { path } else { dir.join(path) })
}

/// Where a lane keeps the pid of the command running in it: in its own git folder, gone once the
/// command ends. Found at the start of a request, it means the last one's process died with its
/// command still running.
fn pid_file(lane: &Path) -> Option<PathBuf> {
    git_path(lane, "nitely-lane-pid")
}

/// Stops what a request that died left running in `lane`: the command it recorded, with what that
/// started, and any process whose executable or command line is inside the lane (test binaries
/// under its build directory). Only when the last request did not end normally.
fn stop_leftovers(lane: &Path) {
    let Some(file) = pid_file(lane) else { return };
    // No record: the last request ended normally, and nothing it started is left.
    if !file.exists() {
        return;
    }
    stop_recorded_command(&file);
    for h in crate::procs::holders(lane) {
        stop_tree(h.pid);
    }
    std::fs::remove_file(&file).ok();
}

/// Puts `lane` at `commit`: a forced checkout, then everything untracked or ignored removed except
/// the `keep` directories. One retry after a short wait, for a file a finished process had open.
fn switch(lane: &Path, commit: &str, keep: &[String]) -> Result<()> {
    let attempt = || -> Result<()> {
        git(lane, &["checkout", "--quiet", "--force", "--detach", commit])?;
        let mut args: Vec<String> = vec!["clean".into(), "-ffdxq".into()];
        for k in keep {
            args.push("-e".into());
            args.push(format!("/{}", k.trim_matches('/')));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(lane, &args)?;
        Ok(())
    };
    attempt().or_else(|first| {
        std::thread::sleep(Duration::from_secs(2));
        attempt().map_err(|e| e.context(format!("switching the lane failed twice (first: {first:#})")))
    })
}

/// Runs `command` in `dir` with its output appended to `log`, stopping it and everything it started
/// after `limit`, held to the machine's `share` for one request: its cores, its memory cap and a
/// priority below the person's own apps (NIT-266). A run over its cap is stopped and fails as out
/// of memory in the lane, a lane problem rather than the command's. Returns how it ended.
fn run_logged(command: &str, dir: &Path, log: &Path, limit: Duration, share: Share) -> Result<Ended> {
    use std::io::Write;
    let command = node_tests_share(command, share.jobs);
    let command = command.as_str();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "$ {command}")?;
    let mut cmd = playbook::shell_command(command);
    cmd.current_dir(dir).stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file.try_clone()?);
    // The machine's share for one request (NIT-199), for the build tools that read it.
    share_env(&mut cmd, share.jobs);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // In a lane, the pid is kept while it runs, so a request after a crash can stop it. The file is
    // written before the start too, without a pid, so a crash in between still leaves the mark that
    // makes the next request look for what runs in the lane.
    let pids = pid_file(dir);
    if let Some(p) = &pids {
        std::fs::write(p, format!("starting\n{command}")).ok();
    }
    // On Windows the command and all it starts go in a job that ends with this process: a crash of
    // the requester (even before the pid is written) leaves nothing running in the lane, and what
    // the command left running ends when it does (NIT-223). The job also holds it to its memory cap
    // and below-normal priority (NIT-266).
    let guard = crate::procs::TreeGuard::with(crate::procs::Contain { memory_cap: share.cap, low_priority: true, end_with_guard: true });
    let spawned = match &guard {
        Some(g) => g.spawn(&mut cmd),
        None => cmd.spawn(),
    };
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            if let Some(p) = &pids {
                std::fs::remove_file(p).ok();
            }
            return Err(e).with_context(|| format!("starting {command}"));
        }
    };
    if let Some(p) = &pids {
        let started = crate::procs::started(child.id()).unwrap_or_default();
        std::fs::write(p, format!("{} {started}\n{command}", child.id())).ok();
    }
    let started = Instant::now();
    let over_memory = || guard.as_ref().is_some_and(|g| g.over_memory());
    let (code, timed_out, out_of_memory) = loop {
        if let Some(status) = child.try_wait()? {
            // A process the cap stopped may have ended before it was seen.
            break (status.code(), false, over_memory());
        }
        let oom = over_memory();
        if oom || started.elapsed() > limit {
            if let Some(g) = &guard {
                g.stop();
            }
            stop_tree(child.id());
            child.kill().ok();
            child.wait().ok();
            break (None, !oom, oom);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if let Some(p) = &pids {
        std::fs::remove_file(p).ok();
    }
    let peak = guard.as_ref().and_then(|g| g.peak_memory());
    drop(guard);
    let cap = share.cap.map(machine::gb).unwrap_or_default();
    if out_of_memory {
        let why = format!("out of memory in the lane: `{command}` and what it started went over its {cap} memory cap and were stopped. This is a lane problem, not a test failure; Settings > Build lanes sets the cap");
        writeln!(file, "{why}").ok();
        bail!(why);
    }
    if let Some(peak) = peak {
        let of = if cap.is_empty() { String::new() } else { format!(" of its {cap} cap") };
        writeln!(file, "(peak memory {}{of})", machine::gb(peak)).ok();
    }
    Ok(Ended { code, timed_out, peak })
}

/// Stops `pid` and everything it started.
fn stop_tree(pid: u32) {
    let pid = pid.to_string();
    #[cfg(windows)]
    {
        playbook::shell_free_command("taskkill").args(["/T", "/F", "/PID", &pid]).output().ok();
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).output().ok();
    }
}

// ----- what `nitely lanes` shows and does -----

#[derive(Debug, Clone, Serialize)]
pub struct LaneState {
    pub lane: u32,
    pub path: String,
    /// The lane's checkout exists (it is made on its first request).
    pub made: bool,
    /// A request holds it now.
    pub busy: bool,
    /// The commit it holds.
    pub at: Option<String>,
    /// The size of its build directories after its last request, and the cap `hygiene` keeps it
    /// under, once it has run one.
    pub size_bytes: Option<u64>,
    pub cap_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lanes {
    pub project: String,
    pub repo: String,
    pub dir: String,
    pub rules: LaneRules,
    pub lanes: Vec<LaneState>,
    /// Waiting and running requests, oldest first.
    pub queue: Vec<Run>,
    /// The last finished runs, newest first.
    pub recent: Vec<Run>,
    /// The machine's budget, shared by every project's lanes (NIT-199).
    pub budget: Budget,
    /// Requests running on this machine now, across every project.
    pub running_on_machine: i64,
    /// The machine's memory, its floor and cap, and the priority builds run at (NIT-266).
    pub machine: machine::Now,
    /// Why the first request waiting on the machine's memory waits, when one does.
    pub waiting_for_memory: Option<String>,
}

/// One repo's lanes for the app (NIT-205): `status`, plus the last finished run in each lane,
/// however long ago, so a lane whose last request ended in a lane problem shows it until it runs
/// again (`recent` is only the last few runs of the repo).
#[derive(Debug, Clone, Serialize)]
pub struct ShownLanes {
    #[serde(flatten)]
    pub lanes: Lanes,
    /// The last finished run of each lane that has one, in lane order.
    pub last: Vec<Run>,
}

/// The machine's build budget once the scheduler keeps one: "builds 3 of 4".
#[derive(Debug, Clone, Serialize)]
pub struct Slots {
    pub used: u32,
    pub total: u32,
}

/// Every connected repo's lanes, for the app's lanes strip and Settings.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub projects: Vec<ShownLanes>,
    /// Absent until the scheduler keeps a machine budget.
    pub slots: Option<Slots>,
    /// The machine's memory, its floor and cap, and the priority builds run at (NIT-266).
    pub machine: machine::Now,
    /// Why the first request waiting on the machine's memory waits, when one does.
    pub waiting_for_memory: Option<String>,
}

/// The lanes of every connected repo whose playbook has `[lanes]`. A repo whose playbook does not
/// read is left out; its own views say why. It writes nothing (no lock files, no playbook copies),
/// so the read-only live preview can show it too: a lane is busy when a request runs in it.
pub fn overview(config: &crate::Config, db: &Db) -> Result<Overview> {
    let mut projects = Vec::new();
    for repo in &config.repos {
        let Some(project) = repo.project.as_deref() else { continue };
        let root = Path::new(&repo.root);
        let Ok(pb) = Playbook::load_with(root, config) else { continue };
        let Some(lanes) = status_with(db, root, &pb, project, false)? else { continue };
        let mut last = Vec::new();
        for l in &lanes.lanes {
            if let Some(r) = db.last_finished_lane_run(project, &repo.root, l.lane)? {
                last.push(r);
            }
        }
        projects.push(ShownLanes { lanes, last });
    }
    // The machine's budget and what runs in it, the same for every repo ("builds 1 of 4").
    let slots = projects.first().map(|p| Slots { used: p.lanes.running_on_machine.max(0) as u32, total: p.lanes.budget.slots });
    Ok(Overview { projects, slots, machine: machine::now(), waiting_for_memory: waiting_for_memory(db)? })
}

/// Why the oldest request still waiting on the machine's memory waits, if one does.
fn waiting_for_memory(db: &Db) -> Result<Option<String>> {
    let ids: Vec<i64> =
        db.conn.prepare("SELECT id FROM lane_run WHERE state = 'queued' AND waiting_memory IS NOT NULL ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        if let Some(r) = db.lane_run(id)?.filter(|r| r.state == "queued") {
            return Ok(r.waiting_memory);
        }
    }
    Ok(None)
}

impl Db {
    /// The newest finished run of lane `lane` of `repo`'s lanes in `project`, however long ago.
    pub fn last_finished_lane_run(&self, project: &str, repo: &str, lane: u32) -> Result<Option<Run>> {
        let mine = crate::paths::normalize(repo);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE AND lane = ?2 ORDER BY id DESC"
        ))?;
        let mut rows = stmt.query(params![project, lane as i64])?;
        while let Some(r) = rows.next()? {
            let (run, pid) = row(r)?;
            if crate::paths::normalize(&run.repo) != mine {
                continue;
            }
            // A run whose process died counts as finished (abandoned), even where that cannot be
            // written back (the read-only preview); one still going is not the lane's last.
            let run = settle(self, run, pid);
            if run.state != "queued" && run.state != "running" {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }
}

/// The repo's lanes, its queue and its last runs. None when its playbook has no `[lanes]`.
pub fn status(db: &Db, repo_root: &Path, pb: &Playbook, project: &str) -> Result<Option<Lanes>> {
    status_with(db, repo_root, pb, project, true)
}

/// `status`; with `probe_locks`, a lane is busy when its lock is held (which opens the lock file),
/// otherwise when a running request names it.
fn status_with(db: &Db, repo_root: &Path, pb: &Playbook, project: &str, probe_locks: bool) -> Result<Option<Lanes>> {
    let Some(rules) = pb.lanes.clone() else { return Ok(None) };
    let root = repo_root.to_string_lossy();
    // The queue by this repo directly: a long build stays in it whatever finished since (NIT-223).
    let queue = db.open_lane_runs(project, &root)?;
    let mut lanes = Vec::new();
    for n in 1..=rules.lanes() {
        let path = lane_path(repo_root, pb, n);
        let made = path.join(".git").exists();
        let busy = if probe_locks { try_lock(repo_root, pb, n)?.is_none() } else { queue.iter().any(|r| r.state == "running" && r.lane == Some(n)) };
        let at = made.then(|| git(&path, &["rev-parse", "--short", "HEAD"]).ok().map(|s| s.trim().to_string())).flatten();
        let size = made.then(|| git_path(&path, SIZE_RECORD).and_then(|r| read_size(&r))).flatten();
        let (size_bytes, cap_bytes) = match size {
            Some((size, first)) => (Some(size), Some(cap_bytes(&rules, first))),
            None => (None, None),
        };
        lanes.push(LaneState { lane: n, path: path.to_string_lossy().into_owned(), made, busy, at, size_bytes, cap_bytes });
    }
    let recent = db.finished_lane_runs(project, &root, 10)?;
    // Every project's running requests, each read through `lane_run` so one whose process is gone
    // reads as abandoned and is not counted.
    let running_ids: Vec<i64> = db.conn.prepare("SELECT id FROM lane_run WHERE state = 'running'")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut running_on_machine = 0i64;
    for id in running_ids {
        if db.lane_run(id)?.is_some_and(|r| r.state == "running") {
            running_on_machine += 1;
        }
    }
    Ok(Some(Lanes {
        project: project.to_uppercase(),
        repo: root.into_owned(),
        dir: dir(repo_root, pb).to_string_lossy().into_owned(),
        rules,
        lanes,
        queue,
        recent,
        budget: machine_budget(),
        running_on_machine,
        machine: machine::now(),
        waiting_for_memory: waiting_for_memory(db)?,
    }))
}

/// Puts every lane that is free back to a clean checkout of what it holds; its build output stays.
/// Returns what it did per lane, in words.
pub fn reset(repo_root: &Path, pb: &Playbook) -> Result<Vec<String>> {
    let Some(rules) = pb.lanes.clone() else { bail!("{} has no [lanes] in its playbook", repo_root.display()) };
    let mut said = Vec::new();
    if rules.large_repo {
        // The person's checkout is never cleaned: only put back on its branch when a request that
        // died left it detached.
        let Some(_lock) = try_lock(repo_root, pb, 1)? else {
            return Ok(vec!["your checkout: busy with a request, left alone".into()]);
        };
        said.push(recover_large(repo_root, &rules.keep).unwrap_or_else(|| "your checkout: nothing to do (it is never cleaned)".into()));
        return Ok(said);
    }
    for n in 1..=rules.count.max(1) {
        let path = lane_path(repo_root, pb, n);
        if !path.join(".git").exists() {
            said.push(format!("lane {n}: not made yet"));
            continue;
        }
        let Some(_lock) = try_lock(repo_root, pb, n)? else {
            said.push(format!("lane {n}: busy with a request, left alone"));
            continue;
        };
        let head = git(&path, &["rev-parse", "HEAD"])?.trim().to_string();
        match switch(&path, &head, &rules.keep) {
            Ok(()) => said.push(format!("lane {n}: clean at {}", &head[..head.len().min(9)])),
            Err(e) => said.push(format!("lane {n}: {e:#}")),
        }
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    type Readings = Box<dyn FnMut() -> Option<machine::Memory>>;

    thread_local! {
        /// The machine's memory as this test's requests read it; none set, they never wait.
        pub(super) static FAKE_MEMORY: std::cell::RefCell<Option<Readings>> = const { std::cell::RefCell::new(None) };
        /// The floor as Settings would set it, in GB.
        pub(super) static FAKE_FLOOR: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
    }

    const NO_CAP: Share = Share { jobs: 1, cap: None };

    fn repo(base: &Path) -> PathBuf {
        let root = base.join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let g = |args: &[&str]| assert!(playbook::run_git(Command::new("git").arg("-C").arg(&root).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("src/a.txt"), "one\n").unwrap();
        std::fs::write(root.join("src/b.txt"), "two\n").unwrap();
        std::fs::write(root.join(".gitignore"), "/out/\n/scratch/\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        root
    }

    fn playbook_with(text: &str) -> Playbook {
        Playbook::parse(text).unwrap()
    }

    #[test]
    fn the_overview_keeps_each_lanes_last_problem_however_many_runs_since() {
        let base = crate::paths::unique_temp("nitely-overview", "");
        let root = repo(&base);
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), "[lanes]
count = 2
").unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let insert = |lane: i64, state: &str, at: i64, error: Option<&str>| {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at, error)
                     VALUES ('APP', ?1, 'claude', 'run', 's', 't', 'b', 'npm test', ?2, ?3, ?4, ?5)",
                    params![root_s, state, lane, at, error],
                )
                .unwrap();
        };
        insert(2, "lane_error", 1, Some("the switch failed twice"));
        // A long build that started before everything below: still in the queue after 600 more.
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at, pid)
                 VALUES ('APP', ?1, 'claude', 'gate', 's', 't', 'b', 'cargo test', 'running', 1, 2, ?2)",
                params![root_s, std::process::id()],
            )
            .unwrap();
        // Another repo of the same project, busier than this one, crowds nothing out.
        for i in 0..80 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at)
                     VALUES ('APP', 'C:/Code/other', 'claude', 'run', 's', 't', 'b', 'npm test', 'done', 1, ?1)",
                    params![700 + i],
                )
                .unwrap();
        }
        for i in 0..600 {
            insert(1, "done", 10 + i, None);
        }
        let config = crate::Config {
            repos: vec![crate::config::RepoConfig { name: "app".into(), root: root_s.clone(), integration_branch: "main".into(), project: Some("APP".into()) }],
            ..Default::default()
        };
        let o = overview(&config, &db).unwrap();
        assert_eq!(o.projects.len(), 1);
        let shown = &o.projects[0];
        assert!(shown.lanes.recent.iter().all(|r| r.lane == Some(1) && r.state == "done"), "the last few runs are all lane 1's");
        assert_eq!(shown.last.iter().map(|r| (r.lane, r.state.as_str())).collect::<Vec<_>>(), [(Some(1), "done"), (Some(2), "lane_error")]);
        assert_eq!(shown.lanes.queue.iter().map(|r| r.command.as_str()).collect::<Vec<_>>(), ["cargo test"], "the long build stays in the queue");
        assert_eq!(shown.lanes.recent.len(), 10);
        assert!(shown.lanes.recent.iter().all(|r| r.repo == root_s), "only this repo's runs");
        let slots = o.slots.as_ref().expect("the machine's budget");
        assert!(slots.total >= 1);
        assert!(!dir(&root, &playbook_with("[lanes]\n")).exists(), "reading the lanes writes nothing: no lane folder, no lock file");
        // A repo without [lanes] is left out.
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), "[gate]
commands = []
").unwrap();
        assert!(overview(&config, &db).unwrap().projects.is_empty());
        drop(db);
        std::fs::remove_dir_all(base).ok();
    }

    /// What a lane command leaves running ends with it on Windows (the job object), so a requester
    /// that crashes, or a command that starts a background process, leaves nothing in the lane.
    #[cfg(windows)]
    #[test]
    fn a_lane_command_takes_what_it_started_with_it() {
        let base = crate::paths::unique_temp("nitely-tree", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        // A marker no other process carries: an address only this test pings.
        let marker = "ping -n 40 127.0.0.97";
        let ended = run_logged(&format!("start \"\" /B {marker} >NUL & exit /b 0"), &base, &log, Duration::from_secs(30), NO_CAP).unwrap();
        assert_eq!((ended.code, ended.timed_out), (Some(0), false));
        let left = (0..25).all(|_| {
            std::thread::sleep(Duration::from_millis(200));
            crate::procs::holders(&base).is_empty() && !crate::procs::running_with(marker)
        });
        assert!(left, "the background ping ended with the command");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn hygiene_trims_a_lane_over_its_cap_and_builds_cold_over_twice_it() {
        let base = crate::paths::unique_temp("nitely-hygiene", "");
        let lane = base.join("lane");
        let record = base.join("size");
        let log = base.join("run.log");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&log, "").unwrap();
        let fill = |rel: &str, bytes: usize| {
            let p = lane.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![0u8; bytes]).unwrap();
        };
        let rules = LaneRules { keep: vec!["target".into()], ..Default::default() };
        // The first build sets the cap at twice its size: 2 000 bytes.
        fill("target/debug/app", 600);
        fill("target/debug/incremental/a", 400);
        fill("src/main.rs", 5_000);
        hygiene(&lane, &rules, &record, &log);
        assert_eq!(read_size(&record), Some((1_000, 1_000)), "only build directories count");

        // Over the cap: the incremental caches go, the rest stays.
        fill("target/debug/incremental/b", 1_500);
        hygiene(&lane, &rules, &record, &log);
        assert!(!lane.join("target/debug/incremental").exists() && lane.join("target/debug/app").exists());
        assert_eq!(read_size(&record), Some((600, 1_000)));

        // Over twice the cap: the build directories go; sources never.
        fill("target/release/big", 5_000);
        hygiene(&lane, &rules, &record, &log);
        assert!(!lane.join("target").exists() && lane.join("src/main.rs").exists());
        assert_eq!(read_size(&record), Some((0, 1_000)), "the first build's size is kept");
        let said = std::fs::read_to_string(&log).unwrap();
        assert!(said.contains("emptied the incremental caches") && said.contains("builds cold"), "{said}");

        // Nothing outside the lane is ever touched: not a `keep` that climbs out, not what a link
        // inside a build directory points at.
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("dep").join("incremental")).unwrap();
        std::fs::write(outside.join("dep").join("incremental").join("keep.me"), vec![0u8; 3_000]).unwrap();
        std::fs::write(outside.join("big"), vec![0u8; 9_000]).unwrap();
        fill("target/debug/app", 2_500);
        let link = lane.join("target").join("linked");
        #[cfg(windows)]
        let linked = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(outside.join("dep")).output().unwrap().status.success();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(outside.join("dep"), &link).is_ok();
        assert!(linked, "made the link");
        let climbing = LaneRules { keep: vec!["target".into(), "../outside".into(), "/abs".into(), "target/linked".into()], ..rules.clone() };
        hygiene(&lane, &climbing, &record, &log);
        assert!(outside.join("dep").join("incremental").join("keep.me").exists() && outside.join("big").exists(), "outside the lane: untouched");
        assert_eq!(keep_dir(&lane, "../outside"), None);
        #[cfg(windows)]
        assert_eq!(keep_dir(&lane, "C:/x"), None);
        assert_eq!(keep_dir(&lane, "target/linked/incremental"), None, "through a link");
        assert_eq!(keep_dir(&lane, "target/"), Some(lane.join("target")));

        // The playbook's cap wins over twice the first build.
        assert_eq!(cap_bytes(&LaneRules { size_cap_gb: Some(1.0), ..Default::default() }, 10), 1 << 30);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn changed_paths_reads_porcelain_v2() {
        let status = ["1 .M N... 100644 100644 100644 abc abc src/a.txt", "? src/new file.txt", "2 R. N... 100644 100644 100644 abc abc R100 src/c.txt", "src/b.txt", ""].join("\0");
        assert_eq!(changed_paths(&status), ["src/a.txt", "src/b.txt", "src/c.txt", "src/new file.txt"]);
    }

    #[test]
    fn a_snapshot_is_head_or_a_commit_of_the_worktree_without_touching_it() {
        let base = crate::paths::unique_temp("nitely-snap", "");
        let root = repo(&base);
        let head = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let clean = snapshot(&root).unwrap();
        assert_eq!((clean.commit.as_str(), clean.made), (head.as_str(), false));

        std::fs::write(root.join("src/a.txt"), "one, changed\n").unwrap();
        std::fs::remove_file(root.join("src/b.txt")).unwrap();
        std::fs::write(root.join("src/c.txt"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/build.bin"), "ignored").unwrap();
        let before = git(&root, &["status", "--porcelain"]).unwrap();
        let snap = snapshot(&root).unwrap();
        assert!(snap.made && snap.base == head);
        assert_eq!(git(&root, &["status", "--porcelain"]).unwrap(), before, "the worktree and its index are as they were");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap().trim(), head, "and its branch");
        let files = git(&root, &["ls-tree", "-r", "--name-only", &snap.commit]).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "src/a.txt", "src/c.txt"], "ignored files stay out");
        assert_eq!(git(&root, &["show", &format!("{}:src/a.txt", snap.commit)]).unwrap(), "one, changed\n");

        // A tracked folder that became a file.
        std::fs::remove_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src"), "now a file\n").unwrap();
        let snap = snapshot(&root).unwrap();
        let files = git(&root, &["ls-tree", "-r", "--name-only", &snap.commit]).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "src"]);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_lane_runs_requests_keeping_only_its_build_dirs() {
        let base = crate::paths::unique_temp("nitely-lane", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        // No [lanes]: no lanes, and the caller is told what to do instead.
        let none = playbook_with("[gate]\ncommands = [\"x\"]\n");
        let snap = snapshot(&root).unwrap();
        let req = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap, command: "echo hi" };
        assert!(run(&db, &root, &none, &req, &actor, &mut |_| {}).unwrap_err().to_string().contains("no [lanes]"));
        assert!(status(&db, &root, &none, "APP").unwrap().is_none());

        let pb = playbook_with("[lanes]\nkeep = [\"out\"]\ntimeout_minutes = 1\n");
        let lister = if cfg!(windows) { "dir /b src && (if not exist out mkdir out) && echo built > out\\done.txt && echo junk > scratch.txt" } else { "ls src && mkdir -p out && echo built > out/done.txt && echo junk > scratch.txt" };
        let req = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap, command: lister };
        let mut seen = Vec::new();
        let first = run(&db, &root, &pb, &req, &actor, &mut |r| seen.push(r.state.clone())).unwrap();
        assert_eq!(seen, ["queued", "running"]);
        assert!(first.passed(), "{first:?}");
        assert_eq!(first.lane, Some(1));
        // Canonical both sides: on macOS the temp folder is /var/..., which resolves to /private/var/...
        let log = std::fs::canonicalize(first.log.as_deref().unwrap()).unwrap();
        assert!(log.starts_with(std::fs::canonicalize(&base).unwrap().join("lanes")), "the log sits beside the database: {:?}", first.log);
        assert!(first.output.contains("a.txt") && first.output.contains("b.txt"), "{}", first.output);
        let lane = lane_path(&root, &pb, 1);
        assert!(lane.join("out/done.txt").exists() && lane.join("scratch.txt").exists());

        // The next request: another snapshot; the build dir stays, the stray file and b.txt go.
        std::fs::remove_file(root.join("src/b.txt")).unwrap();
        let snap2 = snapshot(&root).unwrap();
        let check = if cfg!(windows) { "if exist scratch.txt exit 3 & if not exist out\\done.txt exit 4 & if exist src\\b.txt exit 5" } else { "test ! -e scratch.txt || exit 3; test -e out/done.txt || exit 4; test ! -e src/b.txt || exit 5" };
        let req2 = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap2, command: check };
        let second = run(&db, &root, &pb, &req2, &actor, &mut |_| {}).unwrap();
        assert_eq!(second.exit_code, Some(0), "{second:?}");
        assert_eq!(second.files_switched, Some(1));

        // A failing command is a result, not a lane problem; the timeline names the run.
        let req3 = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap2, command: "exit 7" };
        let third = run(&db, &root, &pb, &req3, &actor, &mut |_| {}).unwrap();
        assert_eq!((third.state.as_str(), third.exit_code, third.passed()), ("done", Some(7), false));
        let said: Vec<String> = db.timeline("APP-1").unwrap().into_iter().filter(|t| t.kind == "lane_run").filter_map(|t| t.summary).collect();
        assert_eq!(said.len(), 2);
        assert!(said[1].contains("exit 7") && said[1].contains(&format!("run {}", third.id)), "{said:?}");

        let lanes = status(&db, &root, &pb, "APP").unwrap().unwrap();
        assert_eq!(lanes.lanes.len(), 1);
        assert!(lanes.lanes[0].made && !lanes.lanes[0].busy);
        assert!(lanes.queue.is_empty() && lanes.recent.len() == 3);
        assert_eq!(reset(&root, &pb).unwrap().len(), 1);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_busy_lane_waits_and_a_dead_requester_is_abandoned() {
        let base = crate::paths::unique_temp("nitely-lane-busy", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]\ncount = 1\n");
        let held = try_lock(&root, &pb, 1).unwrap().expect("free at first");
        assert!(try_lock(&root, &pb, 1).unwrap().is_none(), "held by this handle");
        drop(held);
        assert!(try_lock(&root, &pb, 1).unwrap().is_some(), "free once dropped, as after a crash");

        // A row left queued by a process that is gone reads as abandoned.
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid) VALUES ('APP', ?1, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, 999999)",
                [root.to_string_lossy()],
            )
            .unwrap();
        let id = db.conn.last_insert_rowid();
        assert_eq!(db.lane_run(id).unwrap().unwrap().state, "abandoned");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_setup_that_failed_runs_again_and_a_dead_requests_command_is_stopped() {
        let base = crate::paths::unique_temp("nitely-lane-setup", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        // Fails the first time it runs in a lane (it leaves a mark outside the repo), passes after.
        let mark = base.join("setup-ran-once");
        let mark_s = mark.to_string_lossy().replace('\\', "/");
        let setup = if cfg!(windows) { format!("if exist \"{mark_s}\" (exit 0) else (type nul > \"{mark_s}\" & exit 1)") } else { format!("if [ -e '{mark_s}' ]; then exit 0; else touch '{mark_s}'; exit 1; fi") };
        // No retries, so the first request sees the failure (with the default one retry it passes).
        let pb = playbook_with(&format!("[worktree]\nsetup = [{setup:?}]\n[gate]\nretries = 0\n[lanes]\n"));
        let snap = snapshot(&root).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo built" };
        let first = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert_eq!(first.state, "lane_error", "{first:?}");
        assert!(first.error.as_deref().unwrap_or("").contains("setup failed"), "{first:?}");
        let second = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert!(second.passed() && second.setup_ran, "a failed setup is never taken as done: {second:?}");
        let third = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert!(third.passed() && !third.setup_ran, "and a passed one is not run again");

        // What setup makes and `keep` does not list goes with the clean, so setup runs again; what
        // `keep` lists stays, and setup does not. Inside a tracked folder (`src/deps`), which the
        // clean leaves in place.
        let makes = if cfg!(windows) { "if not exist src\\deps mkdir src\\deps && echo x > src\\deps\\ok.txt" } else { "mkdir -p src/deps && echo x > src/deps/ok.txt" };
        let needs = if cfg!(windows) { "if not exist src\\deps\\ok.txt exit 9" } else { "test -e src/deps/ok.txt || exit 9" };
        let other = crate::paths::unique_temp("nitely-lane-keep", "");
        let root2 = repo(&other);
        let snap2 = snapshot(&root2).unwrap();
        for (keep, again) in [("", true), ("keep = [\"src/deps\"]\n", false)] {
            let pb2 = playbook_with(&format!("[worktree]\nsetup = [{makes:?}]\n[lanes]\n{keep}"));
            let req2 = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap2, command: needs };
            assert!(run(&db, &root2, &pb2, &req2, &actor, &mut |_| {}).unwrap().passed());
            let next = run(&db, &root2, &pb2, &req2, &actor, &mut |_| {}).unwrap();
            assert!(next.passed(), "{next:?}");
            assert_eq!(next.setup_ran, again, "keep {keep:?}");
        }
        std::fs::remove_dir_all(other).ok();

        // A command left running by a request whose process died is stopped before the next one.
        let lane = lane_path(&root, &pb, 1);
        let sleeper = if cfg!(windows) { "ping -n 60 127.0.0.1 >NUL" } else { "sleep 60" };
        let mut left = playbook::shell_command(sleeper);
        // Outside the lane, so only the recorded pid, command and start time can stop it: anything
        // working inside the lane is stopped whatever the record says (`holders`, which on Linux
        // reads every process's folder).
        left.current_dir(&base);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            left.process_group(0);
        }
        // A recorded pid now running something else (the number was reused) is left alone.
        std::fs::write(pid_file(&lane).unwrap(), format!("{}\nsome other command", std::process::id())).unwrap();
        stop_leftovers(&lane);
        assert!(!pid_file(&lane).unwrap().exists(), "this test process is still here to check");
        let mut child = left.spawn().unwrap();
        let started = crate::procs::started(child.id()).expect("the start time of a running child");
        // The same command at the same number but another start time is another process (a reused
        // pid in another lane): left alone.
        std::fs::write(pid_file(&lane).unwrap(), format!("{} 1\n{sleeper}", child.id())).unwrap();
        stop_leftovers(&lane);
        std::thread::sleep(Duration::from_millis(500));
        assert!(matches!(child.try_wait(), Ok(None)), "another start time: not the one the request left");
        std::fs::write(pid_file(&lane).unwrap(), format!("{} {started}\n{sleeper}", child.id())).unwrap();
        stop_leftovers(&lane);
        let stopped = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            matches!(child.try_wait(), Ok(Some(_)))
        });
        assert!(stopped, "the leftover command was stopped");
        assert!(!pid_file(&lane).unwrap().exists());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_gate_runs_in_a_lane_once_per_tree_and_land_goes_first() {
        let base = crate::paths::unique_temp("nitely-lane-gate", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        // Proves where it ran: the lane gets the file, the worktree does not. The second command
        // fails the first time it ever runs (a mark outside the repo), then passes: a flake.
        let mark = base.join("flaked-once").to_string_lossy().replace('\\', "/");
        let (wrote, flaky) = if cfg!(windows) {
            ("echo ran > gate-ran.txt".to_string(), format!("if exist \"{mark}\" (exit 0) else (type nul > \"{mark}\" & exit 1)"))
        } else {
            ("echo ran > gate-ran.txt".to_string(), format!("if [ -e '{mark}' ]; then exit 0; else touch '{mark}'; exit 1; fi"))
        };
        let text = format!("[gate]\ncommands = [{wrote:?}, {flaky:?}]\n[lanes]\n");
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), &text).unwrap();
        let g = |args: &[&str]| assert!(playbook::run_git(Command::new("git").arg("-C").arg(&root).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "playbook"]);
        let pb = Playbook::load(&root).unwrap();
        let rows = |db: &Db| db.lane_runs("", 50).unwrap();

        let gate = playbook::run_gate(&db, &root, &root, Some("APP-1"), &actor).unwrap();
        assert!(gate.passed, "{gate:?}");
        assert_eq!(gate.flaked.len(), 1, "the second command passed on its second run");
        assert!(!root.join("gate-ran.txt").exists(), "nothing ran in the worktree");
        assert!(lane_path(&root, &pb, 1).join("gate-ran.txt").exists(), "it ran in the lane");
        let first = rows(&db);
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].kind.as_str(), first[0].command.lines().count()), ("gate", 2));
        let kinds: Vec<String> = db.timeline("APP-1").unwrap().into_iter().map(|t| t.kind).collect();
        assert!(kinds.ends_with(&["lane_run".to_string(), "gate_flaked".to_string(), "gate_passed".to_string()]), "{kinds:?}");

        // The same tree again: answered from that run, nothing runs.
        let again = playbook::run_gate(&db, &root, &root, Some("APP-1"), &actor).unwrap();
        assert!(again.passed && again.results.iter().all(|r| r.skipped && r.output.contains("not run again")), "{again:?}");
        assert_eq!(rows(&db).len(), 1, "no new run");

        // Uncommitted work is not what a gate builds: it says so, and the record answers it.
        std::fs::write(root.join("src/a.txt"), "changed\n").unwrap();
        let dirty = playbook::run_gate(&db, &root, &root, Some("APP-1"), &actor).unwrap();
        assert!(dirty.passed && dirty.warnings.iter().any(|w| w.contains("uncommitted work")), "{dirty:?}");
        assert_eq!(dirty.commit, gate.commit, "it reports the branch head, the commit the review sees");
        assert_eq!(rows(&db).len(), 1);

        // Another tree, as land asks: a land request, first in line.
        g(&["commit", "-q", "-am", "change"]);
        let landing = playbook::run_gate_as(&db, &root, &root, Some("APP-1"), &actor, Kind::Land).unwrap();
        assert_eq!(landing.lane_run, Some(rows(&db)[0].id));
        assert!(landing.passed, "{landing:?}");
        let now = rows(&db);
        assert_eq!(now.len(), 2);
        assert_eq!(now[0].kind, "land");
        let priority: i64 = db.conn.query_row("SELECT priority FROM lane_run WHERE id = ?1", [now[0].id], |r| r.get(0)).unwrap();
        assert_eq!(priority, 1);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_budget_comes_from_cores_and_memory_unless_set() {
        let gb = |n: u64| machine::limits_for(n * machine::GIB, None, None);
        // 64 GB: a 12.8 GB floor and 16 GB cap leave room for 3 runs at their cap; 30 of 32 cores.
        let b = budget_for(32, Some(&gb(64)), None);
        assert_eq!((b.slots, b.jobs, b.from.as_str(), b.memory_gb), (3, 10, "machine", Some(64)));
        assert_eq!(budget_for(32, Some(&gb(16)), None).slots, 2, "memory caps it");
        assert_eq!((budget_for(4, Some(&gb(8)), None).slots, budget_for(4, Some(&gb(8)), None).jobs), (1, 3), "always one, and a core left over");
        assert_eq!(budget_for(8, None, None).slots, 1, "memory unknown: cores decide");
        let set = budget_for(32, Some(&gb(64)), Some((6, "settings")));
        assert_eq!((set.slots, set.jobs, set.from.as_str()), (6, 5, "settings"));
        assert_eq!(budget_for(32, Some(&gb(64)), Some((0, "settings"))).from, "machine", "0 is not a setting");
    }

    #[test]
    fn builds_leave_cores_for_the_person() {
        assert_eq!([build_cores(32), build_cores(8), build_cores(4), build_cores(2), build_cores(1)], [30, 6, 3, 1, 1]);
    }

    #[test]
    fn node_tests_get_the_share_of_cores() {
        assert_eq!(node_tests_share("node --test dist/", 6), "node --test --test-concurrency=6 dist/");
        assert_eq!(node_tests_share("npm ci && node --enable-source-maps --test a.js", 3), "npm ci && node --enable-source-maps --test --test-concurrency=3 a.js");
        assert_eq!(node_tests_share("node --test-reporter=spec --test", 2), "node --test-reporter=spec --test --test-concurrency=2");
        assert_eq!(node_tests_share("node --test --test-concurrency=1 x", 6), "node --test --test-concurrency=1 x", "one it sets stays");
        assert_eq!(node_tests_share("cargo test --workspace", 6), "cargo test --workspace");
        assert_eq!(node_tests_share("mynode --test", 6), "mynode --test");
    }

    /// A request waits, saying why, while the machine's memory is under its floor, and starts once
    /// it frees (NIT-266).
    #[test]
    fn a_request_waits_for_memory_and_says_so() {
        let base = crate::paths::unique_temp("nitely-lane-memory", "");
        let root = repo(&base);
        let db_path = base.join("t.db");
        let db = Db::open(&db_path).unwrap();
        let pb = playbook_with("[lanes]\n");
        let snap = snapshot(&root).unwrap();
        let seen: std::rc::Rc<std::cell::RefCell<Vec<Option<String>>>> = Default::default();
        let mem = |free_gb: u64| Some(machine::Memory { total: 64 * machine::GIB, available: free_gb * machine::GIB });
        let mut readings = vec![mem(5), mem(6), mem(40)].into_iter();
        let (watch, path) = (seen.clone(), db_path.clone());
        let next: Readings = Box::new(move || {
            // What the queue shows while it waits, read as another process would.
            let other = Db::open(&path).unwrap();
            let why: Option<String> = other.conn.query_row("SELECT waiting_memory FROM lane_run ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
            watch.borrow_mut().push(why);
            readings.next().flatten()
        });
        FAKE_MEMORY.with(|f| *f.borrow_mut() = Some(next));
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo built" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {});
        FAKE_MEMORY.with(|f| *f.borrow_mut() = None);
        let ran = ran.unwrap();
        assert!(ran.passed(), "{ran:?}");
        let seen = seen.borrow();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen[1].as_deref().is_some_and(|w| w.starts_with("waiting for memory: 5.0 GB free")), "{seen:?}");
        assert_eq!(ran.waiting_memory, None, "cleared once it started");
        std::fs::remove_dir_all(base).ok();
    }

    /// A floor lowered while a request waits lets it go at its next look, without a restart.
    #[test]
    fn a_floor_lowered_while_a_request_waits_lets_it_go() {
        let base = crate::paths::unique_temp("nitely-lane-floor", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]
");
        let snap = snapshot(&root).unwrap();
        let mut looks = 0;
        let next: Readings = Box::new(move || {
            looks += 1;
            if looks == 2 {
                // The person sets the floor to 0 in Settings while it waits.
                FAKE_FLOOR.with(|f| f.set(Some(0.0)));
            }
            Some(machine::Memory { total: 64 * machine::GIB, available: 2 * machine::GIB })
        });
        FAKE_MEMORY.with(|f| *f.borrow_mut() = Some(next));
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo built" };
        let started = Instant::now();
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {});
        FAKE_MEMORY.with(|f| *f.borrow_mut() = None);
        FAKE_FLOOR.with(|f| f.set(None));
        assert!(ran.unwrap().passed());
        assert!(started.elapsed() < Duration::from_secs(30), "went at its next look");
        std::fs::remove_dir_all(base).ok();
    }

    /// A run that goes over its memory cap is stopped and fails as a lane problem, never as the
    /// command's failure, and never by taking the machine's memory (NIT-266).
    #[test]
    fn a_run_over_its_memory_cap_is_stopped_as_out_of_memory_in_the_lane() {
        if Command::new("node").arg("--version").output().is_err() {
            return;
        }
        let base = crate::paths::unique_temp("nitely-lane-cap", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        // 800 MB, written to so it is really held, against a 300 MB cap.
        let hog = "node -e \"const b = Buffer.alloc(800 * 1024 * 1024, 1); setTimeout(() => console.log(b.length), 15000)\"";
        let started = Instant::now();
        let cap = Share { jobs: 1, cap: Some(300 << 20) };
        let err = run_logged(hog, &base, &log, Duration::from_secs(60), cap).unwrap_err();
        assert!(format!("{err:#}").starts_with("out of memory in the lane"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(14), "stopped at its cap, not waited out");
        assert!(std::fs::read_to_string(&log).unwrap().contains("out of memory in the lane"));
        // Under its cap, a run ends as usual, with its peak where the system tells it.
        let fine = run_logged("node -e \"console.log(1)\"", &base, &log, Duration::from_secs(60), Share { jobs: 1, cap: Some(2 << 30) }).unwrap();
        assert_eq!(fine.code, Some(0));
        if cfg!(windows) {
            assert!(fine.peak.is_some_and(|p| p > 1 << 20 && p < 2 << 30), "{fine:?}");
        }
        std::fs::remove_dir_all(base).ok();
    }

    /// A lane command and what it starts run below the person's own apps: below-normal priority
    /// and low I/O priority on Windows, nice 10 elsewhere (NIT-266).
    #[test]
    fn a_lane_command_runs_below_normal() {
        let base = crate::paths::unique_temp("nitely-lane-nice", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        let ask = if cfg!(windows) {
            "powershell -NoProfile -Command \"(Get-Process -Id $PID).PriorityClass\""
        } else {
            "sh -c 'ps -o nice= -p $$'"
        };
        let ended = run_logged(ask, &base, &log, Duration::from_secs(60), NO_CAP).unwrap();
        assert_eq!(ended.code, Some(0));
        let said = std::fs::read_to_string(&log).unwrap();
        let want = if cfg!(windows) { "BelowNormal" } else { "10" };
        assert!(said.lines().any(|l| l.trim() == want), "{said}");
        std::fs::remove_dir_all(base).ok();
    }

    /// A gate outside lanes holds a machine slot as a lane request does, and counts as running.
    #[test]
    fn a_gate_outside_lanes_takes_a_machine_slot() {
        let base = crate::paths::unique_temp("nitely-lane-outside", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]\n");
        let turn = hold_machine(&db, &root, &root, "app", Some("APP-1"), Kind::Gate, "cargo test", &Actor::Agent("claude:test".into())).unwrap();
        let row = db.lane_run(turn.id()).unwrap().unwrap();
        assert_eq!((row.state.as_str(), row.executor.as_str(), row.kind.as_str()), ("running", "worktree", "gate"));
        assert_eq!(status(&db, &root, &pb, "APP").unwrap().unwrap().running_on_machine, 1, "it counts against the machine");
        let id = turn.id();
        turn.finish(true);
        let row = db.lane_run(id).unwrap().unwrap();
        assert_eq!((row.state.as_str(), row.exit_code), ("done", Some(0)));
        // One dropped before it finished ends as a lane problem, never left running.
        let turn = hold_machine(&db, &root, &root, "app", None, Kind::Land, "cargo test", &Actor::Agent("claude:test".into())).unwrap();
        let id = turn.id();
        drop(turn);
        assert_eq!(db.lane_run(id).unwrap().unwrap().state, "lane_error");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_queue_goes_by_priority_then_one_per_project_then_age() {
        let w = |id, project: &str, priority| Waiting { id, project: project.into(), repo: format!("C:/Code/{project}"), priority };
        let none = std::collections::HashMap::new();
        let order: Vec<i64> = in_order(vec![w(1, "A", 3), w(2, "A", 3), w(3, "B", 3), w(4, "A", 2), w(5, "C", 1), w(6, "B", 3)], &none).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [5, 4, 1, 3, 2, 6]);
        // A1 started: A2 waits behind B1, though it is older.
        // A was served last (A1 started): A2 waits behind B1, though it is older.
        let a_served = std::collections::HashMap::from([("A".to_string(), 10)]);
        let order: Vec<i64> = in_order(vec![w(2, "A", 3), w(3, "B", 3)], &a_served).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [3, 2]);
        // Both served, B more recently: A's goes first.
        let both = std::collections::HashMap::from([("A".to_string(), 10), ("B".to_string(), 20)]);
        let order: Vec<i64> = in_order(vec![w(2, "A", 3), w(3, "B", 3)], &both).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [2, 3]);
    }

    #[test]
    fn a_request_waits_for_its_repo_and_a_slot_but_not_for_a_busy_repo() {
        let base = crate::paths::unique_temp("nitely-lane-turn", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let row = |project: &str, state: &str, priority: i64, lanes: i64, pid: i64| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, started_at)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', ?3, 'local', 1, ?4, ?5, ?6, CASE WHEN ?3 = 'queued' THEN NULL ELSE 2 END)",
                    params![project, format!("C:/Code/{project}"), state, pid, priority, lanes],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |pid: i64| pid == me;
        // B's one lane is busy; A's request behind B's waiting one still goes, with a slot free.
        row("B", "running", 3, 1, me);
        let b_waits = row("B", "queued", 3, 1, me);
        let a = row("A", "queued", 3, 1, me);
        assert!(!my_turn(&db, b_waits, 2, &mut alive).unwrap(), "B has one running: A's first goes before B's second");
        assert!(my_turn(&db, b_waits, 3, &mut alive).unwrap(), "with a slot for each, B may try; its lane's lock decides");
        assert!(my_turn(&db, a, 2, &mut alive).unwrap(), "a busy repo does not hold A up");
        assert!(!my_turn(&db, a, 1, &mut alive).unwrap(), "no free slot: the one is B's running request");
        // A second request of A waits behind the first.
        let a2 = row("A", "queued", 3, 1, me);
        assert!(!my_turn(&db, a2, 4, &mut alive).unwrap());
        // A land request jumps the line; a dead requester's row holds nothing.
        let land = row("C", "queued", 1, 1, me);
        assert!(my_turn(&db, land, 2, &mut alive).unwrap());
        assert!(!my_turn(&db, a, 2, &mut alive).unwrap(), "the land request takes the one free slot first");
        row("D", "running", 3, 1, 999_999);
        assert!(my_turn(&db, a, 3, &mut alive).unwrap(), "a dead requester's running row holds no slot");
        let mut all_alive = |_: i64| true;
        assert!(!my_turn(&db, a, 3, &mut all_alive).unwrap(), "while it lives, it does");
        std::fs::remove_dir_all(base).ok();

        // A1 has run and finished; A2 and B1 wait for one slot: B1 goes first.
        let base = crate::paths::unique_temp("nitely-lane-turn2", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let add = |project: &str, state: &str, queued: i64, started: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, started_at, pid, priority, lane_count)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', ?3, 'local', ?4, ?5, ?6, 3, 1)",
                    params![project, format!("C:/Code/{project}"), state, queued, started, me],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        // A1 started (10) before A2 (11) and B1 (12) were even queued, and has finished.
        add("A", "done", 9, Some(10));
        let a2 = add("A", "queued", 11, None);
        let b1 = add("B", "queued", 12, None);
        let mut alive = |pid: i64| pid == me;
        assert!(my_turn(&db, b1, 1, &mut alive).unwrap(), "B was never served; A was, last");
        assert!(!my_turn(&db, a2, 1, &mut alive).unwrap());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_command_gets_the_machines_share_of_cores() {
        let base = crate::paths::unique_temp("nitely-lane-jobs", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]\n");
        let snap = snapshot(&root).unwrap();
        let echo = if cfg!(windows) { "echo jobs={jobs} env=%NITELY_JOBS% cargo=%CARGO_BUILD_JOBS%" } else { "echo jobs={jobs} env=$NITELY_JOBS cargo=$CARGO_BUILD_JOBS" };
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: echo };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        let n = machine_budget().jobs;
        assert!(ran.output.contains(&format!("jobs={n} env={n} cargo={n}")), "{}", ran.output);
        let lanes = status(&db, &root, &pb, "APP").unwrap().unwrap();
        assert_eq!(lanes.budget.jobs, n);
        assert_eq!(lanes.running_on_machine, 0);
        std::fs::remove_dir_all(base).ok();
    }

    /// A throwaway repo standing in for a person's large checkout (never a real one: every
    /// checkout-touching step asserts it is under the temp folder), and an agent's worktree of it
    /// with one commit of work.
    fn large(base: &Path, extra: &str) -> (PathBuf, PathBuf, Playbook) {
        let root = repo(base);
        guard_throwaway(&root);
        let agent = base.join("agent");
        git_ok(&root, &["worktree", "add", "-q", "-b", "app-1", &agent.to_string_lossy()]);
        std::fs::write(agent.join("src/a.txt"), "agent's\n").unwrap();
        git_ok(&agent, &["commit", "-qam", "agent work"]);
        let pb = playbook_with(&format!("[lanes]\nlarge_repo = true\nkeep = [\"out\"]\ntimeout_minutes = 1\n{extra}"));
        (root, agent, pb)
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        assert!(playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
    }

    fn on_branch(root: &Path) -> String {
        git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default()
    }

    #[test]
    fn the_test_guard_refuses_a_checkout_outside_the_temp_folder() {
        let caught = std::panic::catch_unwind(|| guard_throwaway(Path::new(env!("CARGO_MANIFEST_DIR"))));
        assert!(caught.is_err(), "the repo's own checkout is not a throwaway");
        guard_throwaway(&crate::paths::unique_temp("nitely-guard", ""));
    }

    #[test]
    fn the_main_checkout_is_ready_only_when_clean_and_on_its_branch() {
        let base = crate::paths::unique_temp("nitely-large-ready", "");
        let (root, _agent, pb) = large(&base, "");
        let keep = pb.lanes.clone().unwrap().keep;
        assert_eq!(checkout_busy(&root, &pb, &keep).unwrap(), None);

        std::fs::write(root.join("src/a.txt"), "half done\n").unwrap();
        assert!(checkout_busy(&root, &pb, &keep).unwrap().unwrap().contains("src/a.txt"));
        std::fs::write(root.join("notes.txt"), "mine\n").unwrap();
        assert!(checkout_busy(&root, &pb, &keep).unwrap().unwrap().contains("2 changes"));
        git_ok(&root, &["checkout", "--", "src/a.txt"]);
        std::fs::remove_file(root.join("notes.txt")).unwrap();

        git_ok(&root, &["switch", "-q", "-c", "spike"]);
        assert!(checkout_busy(&root, &pb, &keep).unwrap().unwrap().contains("on spike, not main"));
        git_ok(&root, &["switch", "-q", "main"]);

        // Ignored files and the build directories are not changes.
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        std::fs::write(root.join("scratch/mine.txt"), "mine\n").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/old.o"), "x").unwrap();
        assert_eq!(checkout_busy(&root, &pb, &keep).unwrap(), None);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_request_runs_in_the_main_checkout_and_puts_it_back_on_its_branch() {
        let base = crate::paths::unique_temp("nitely-large-run", "");
        let (root, agent, pb) = large(&base, "");
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        std::fs::write(root.join("scratch/mine.txt"), "mine\n").unwrap();
        let main_at = git(&root, &["rev-parse", "HEAD"]).unwrap();

        let snap = snapshot(&agent).unwrap();
        let build = if cfg!(windows) { "type src\\a.txt && (if not exist out mkdir out) && echo built > out\\done.txt" } else { "cat src/a.txt && mkdir -p out && echo built > out/done.txt" };
        let req = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap, command: build };
        let ran = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert!(ran.passed(), "{ran:?}");
        assert!(ran.output.contains("agent's"), "it built the agent's commit: {}", ran.output);
        assert_eq!(ran.files_switched, Some(1));
        assert_eq!(ran.waiting_for, None);

        // Back on main at the same commit, the person's files as they were, the build output kept.
        assert_eq!(on_branch(&root), "main");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap(), main_at);
        assert_eq!(std::fs::read_to_string(root.join("src/a.txt")).unwrap().replace('\r', ""), "one\n");
        assert_eq!(std::fs::read_to_string(root.join("scratch/mine.txt")).unwrap(), "mine\n");
        assert!(root.join("out/done.txt").exists());
        assert!(git(&root, &["status", "--porcelain"]).unwrap().trim().is_empty());
        assert!(!return_mark(&root).unwrap().exists());

        let said: Vec<String> = db.timeline("APP-1").unwrap().into_iter().filter(|t| t.kind == "lane_run").filter_map(|t| t.summary).collect();
        assert!(said[0].contains("in the main checkout"), "{said:?}");
        let lanes = status(&db, &root, &pb, "APP").unwrap().unwrap();
        assert_eq!(lanes.lanes.len(), 1);
        assert_eq!(crate::paths::normalize(&lanes.lanes[0].path), crate::paths::normalize(&root.to_string_lossy()));
        assert!(lanes.rules.large_repo);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_request_waits_while_the_main_checkout_has_changes_and_says_why() {
        let base = crate::paths::unique_temp("nitely-large-wait", "");
        let (root, agent, pb) = large(&base, "");
        let db_path = base.join("t.db");
        let db = Db::open(&db_path).unwrap();
        std::fs::write(root.join("notes.txt"), "mine\n").unwrap();

        // The person finishes with their change a moment later.
        let (r, n) = (root.clone(), db_path.clone());
        let person = std::thread::spawn(move || {
            let db = Db::open(&n).unwrap();
            let started = Instant::now();
            loop {
                let why: Option<String> = db.conn.query_row("SELECT waiting_for FROM lane_run ORDER BY id DESC LIMIT 1", [], |row| row.get(0)).optional().unwrap().flatten();
                if let Some(why) = why {
                    std::fs::remove_file(r.join("notes.txt")).unwrap();
                    return why;
                }
                assert!(started.elapsed() < Duration::from_secs(30), "the request never said it was waiting");
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let snap = snapshot(&agent).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo ok" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        let why = person.join().unwrap();
        assert!(why.contains("notes.txt"), "{why}");
        assert!(ran.passed() && ran.waiting_for.is_none(), "{ran:?}");
        assert_eq!(on_branch(&root), "main");

        // A checkout that stays busy past the lane's timeout: the request gives up, saying why.
        let rules = pb.lanes.clone().unwrap();
        let mut since = Instant::now().checked_sub(Duration::from_secs(120));
        let e = wait_for_checkout(&db, ran.id, "your checkout has a change: notes.txt", &mut since, &rules).unwrap_err().to_string();
        assert!(e.contains("notes.txt, for over a minute"), "{e}");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_commit_that_would_overwrite_an_ignored_file_is_refused() {
        let base = crate::paths::unique_temp("nitely-large-ignored", "");
        let (root, agent, pb) = large(&base, "");
        let db = Db::open(&base.join("t.db")).unwrap();
        std::fs::create_dir_all(agent.join("scratch")).unwrap();
        std::fs::write(agent.join("scratch/plan.txt"), "the agent's\n").unwrap();
        git_ok(&agent, &["add", "-f", "scratch/plan.txt"]);
        git_ok(&agent, &["commit", "-qm", "track a scratch file"]);
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        std::fs::write(root.join("scratch/plan.txt"), "the person's\n").unwrap();

        let snap = snapshot(&agent).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo ok" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        assert_eq!(ran.state, "lane_error");
        assert!(ran.error.as_deref().unwrap().contains("scratch/plan.txt"), "{ran:?}");
        assert_eq!(std::fs::read_to_string(root.join("scratch/plan.txt")).unwrap(), "the person's\n");
        assert_eq!(on_branch(&root), "main");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_checkout_a_dead_request_left_detached_goes_back_unless_it_changed() {
        let base = crate::paths::unique_temp("nitely-large-crash", "");
        let (root, agent, pb) = large(&base, "");
        let keep = pb.lanes.clone().unwrap().keep;
        let work = git(&agent, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // As a request leaves it when its process dies mid-command.
        switch_large(&root, "main", &work, &keep).unwrap();
        assert_eq!(on_branch(&root), "");
        assert!(checkout_busy(&root, &pb, &keep).unwrap().unwrap().contains("detached"));
        assert!(reset(&root, &pb).unwrap()[0].contains("back on main"));
        assert_eq!(on_branch(&root), "main");
        assert!(!return_mark(&root).unwrap().exists());

        // Changed while detached (the person started working there): left as it is.
        switch_large(&root, "main", &work, &keep).unwrap();
        std::fs::write(root.join("src/b.txt"), "theirs\n").unwrap();
        assert!(recover_large(&root, &keep).unwrap().contains("stays detached at the request's commit: src/b.txt changed"));
        assert_eq!(on_branch(&root), "");
        assert_eq!(std::fs::read_to_string(root.join("src/b.txt")).unwrap(), "theirs\n");

        // Something other than a request holds the checkout (land): requests wait for it.
        git_ok(&root, &["checkout", "--", "src/b.txt"]);
        let held = hold_checkout(&root, &pb).unwrap().expect("held in large-repo mode");
        assert_eq!(on_branch(&root), "main", "held, and put back first");
        assert!(try_lock(&root, &pb, 1).unwrap().is_none());
        drop(held);
        assert!(hold_checkout(&root, &playbook_with("[lanes]\n")).unwrap().is_none());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn an_ignored_file_where_the_commit_needs_a_folder_is_refused() {
        let base = crate::paths::unique_temp("nitely-large-parent", "");
        let (root, agent, pb) = large(&base, "");
        let db = Db::open(&base.join("t.db")).unwrap();
        // `cache` is ignored here only, and the person has a file by that name.
        std::fs::write(git_path(&root, "info/exclude").unwrap(), "cache\n").unwrap();
        std::fs::write(root.join("cache"), "the person's\n").unwrap();
        std::fs::create_dir_all(agent.join("cache")).unwrap();
        std::fs::write(agent.join("cache/x.txt"), "x\n").unwrap();
        git_ok(&agent, &["add", "-f", "cache/x.txt"]);
        git_ok(&agent, &["commit", "-qm", "a cache folder"]);

        let snap = snapshot(&agent).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo ok" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        assert_eq!(ran.state, "lane_error");
        assert!(ran.error.as_deref().unwrap().contains("tracks cache,"), "{ran:?}");
        assert_eq!(std::fs::read_to_string(root.join("cache")).unwrap(), "the person's\n");
        assert_eq!(on_branch(&root), "main");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_file_that_appeared_during_the_request_keeps_the_checkout_from_going_back_over_it() {
        let base = crate::paths::unique_temp("nitely-large-back", "");
        let (root, agent, pb) = large(&base, "");
        let db = Db::open(&base.join("t.db")).unwrap();
        git_ok(&agent, &["rm", "-q", "src/b.txt"]);
        git_ok(&agent, &["commit", "-qm", "drop b"]);

        // The command makes a file where main tracks one: going back would overwrite it.
        let snap = snapshot(&agent).unwrap();
        let make = if cfg!(windows) { "echo built> src\\b.txt" } else { "echo built > src/b.txt" };
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: make };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        assert_eq!(ran.state, "lane_error");
        assert!(ran.error.as_deref().unwrap().contains("would overwrite src/b.txt"), "{ran:?}");
        assert_eq!(on_branch(&root), "", "left at the request's commit rather than overwrite");
        assert!(std::fs::read_to_string(root.join("src/b.txt")).unwrap().starts_with("built"));
        assert!(reset(&root, &pb).unwrap()[0].contains("stays detached"));

        std::fs::remove_file(root.join("src/b.txt")).unwrap();
        assert!(reset(&root, &pb).unwrap()[0].contains("back on main"));
        assert_eq!(std::fs::read_to_string(root.join("src/b.txt")).unwrap().replace('\r', ""), "two\n");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_file_the_request_made_a_folder_comes_back_as_a_file() {
        let base = crate::paths::unique_temp("nitely-large-dir", "");
        let (root, _agent, pb) = large(&base, "");
        let db = Db::open(&base.join("t.db")).unwrap();
        std::fs::write(root.join("tools"), "a file on main\n").unwrap();
        git_ok(&root, &["add", "tools"]);
        git_ok(&root, &["commit", "-qm", "tools"]);
        let other = base.join("agent3");
        git_ok(&root, &["worktree", "add", "-q", "-b", "app-3", &other.to_string_lossy()]);
        git_ok(&other, &["rm", "-q", "tools"]);
        std::fs::create_dir_all(other.join("tools")).unwrap();
        std::fs::write(other.join("tools/run.txt"), "a folder now\n").unwrap();
        git_ok(&other, &["add", "tools"]);
        git_ok(&other, &["commit", "-qm", "tools becomes a folder"]);

        let snap = snapshot(&other).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo ok" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        assert!(ran.passed(), "{ran:?}");
        assert_eq!(on_branch(&root), "main");
        assert_eq!(std::fs::read_to_string(root.join("tools")).unwrap().replace('\r', ""), "a file on main\n");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn while_a_request_has_the_checkout_the_rules_come_from_its_branch() {
        let base = crate::paths::unique_temp("nitely-large-rules", "");
        let (root, _agent, pb) = large(&base, "");
        let keep = pb.lanes.clone().unwrap().keep;
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), "[gate]\ncommands = [\"main-gate\"]\n").unwrap();
        git_ok(&root, &["add", "."]);
        git_ok(&root, &["commit", "-qm", "playbook"]);
        let other = base.join("agent2");
        git_ok(&root, &["worktree", "add", "-q", "-b", "app-2", &other.to_string_lossy()]);
        std::fs::write(other.join(playbook::PLAYBOOK_PATH), "[gate]\ncommands = [\"agent-gate\"]\n").unwrap();
        git_ok(&other, &["commit", "-qam", "weaker rules"]);
        let work = git(&other, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        let before = lends(&root);
        switch_large(&root, "main", &work, &keep).unwrap();
        assert_eq!(lent_to_request(&root).as_deref(), Some("main"));
        let during = lends(&root);
        assert_eq!((during.count, during.last.as_deref()), (before.count + 1, Some("main")), "each lend counts, so a read across one shows it");
        assert!(std::fs::read_to_string(root.join(playbook::PLAYBOOK_PATH)).unwrap().contains("agent-gate"));
        assert_eq!(Playbook::load(&root).unwrap().gate.commands, ["main-gate"], "the branch's rules, not the snapshot's");
        return_large(&root, "main").unwrap();
        assert_eq!(lent_to_request(&root), None);
        assert_eq!(lends(&root).count, before.count + 2, "and each return, so a read that began during the lend sees the count move");
        assert!(std::fs::read_to_string(git_path(&root, LENDS).unwrap()).unwrap().ends_with("main\nback\n"), "lent and back in the same file, read at once");
        assert_eq!(during.commit.as_deref(), Some(work.as_str()));

        // A checkout put back by hand after a crash is no longer lent once Nitely sees it.
        switch_large(&root, "main", &work, &keep).unwrap();
        git_ok(&root, &["checkout", "-q", "main"]);
        assert_eq!(lent_to_request(&root).as_deref(), Some("main"));
        assert_eq!(recover_large(&root, &keep), None);
        assert_eq!(lent_to_request(&root), None);
        assert_eq!(Playbook::load(&root).unwrap().gate.commands, ["main-gate"]);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_persons_own_untracked_playbook_stays_in_force_while_lent() {
        let base = crate::paths::unique_temp("nitely-large-ownrules", "");
        let (root, agent, pb) = large(&base, "");
        let keep = pb.lanes.clone().unwrap().keep;
        std::fs::write(git_path(&root, "info/exclude").unwrap(), ".nitely/\n").unwrap();
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), "[gate]\ncommands = [\"theirs\"]\n").unwrap();
        let work = git(&agent, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        switch_large(&root, "main", &work, &keep).unwrap();
        assert_eq!(Playbook::load(&root).unwrap().gate.commands, ["theirs"], "the request's commit has no playbook, so the one on disk is theirs");
        return_large(&root, "main").unwrap();
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn after_land_steps_never_run_on_a_requests_commit() {
        let base = crate::paths::unique_temp("nitely-large-hold", "");
        let (root, agent, pb) = large(&base, "");
        let keep = pb.lanes.clone().unwrap().keep;
        let work = git(&agent, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        switch_large(&root, "main", &work, &keep).unwrap();
        std::fs::write(root.join("src/b.txt"), "theirs\n").unwrap();
        let e = hold_checkout(&root, &pb).err().expect("refused").to_string();
        assert!(e.contains("stays detached"), "{e}");
        git_ok(&root, &["checkout", "--", "src/b.txt"]);
        assert!(hold_checkout(&root, &pb).unwrap().is_some());
        assert_eq!(on_branch(&root), "main");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_snapshots_playbook_never_counts_when_the_branch_has_none() {
        let base = crate::paths::unique_temp("nitely-large-norules", "");
        let (root, agent, pb) = large(&base, "");
        let keep = pb.lanes.clone().unwrap().keep;
        std::fs::create_dir_all(agent.join(".nitely")).unwrap();
        std::fs::write(agent.join(playbook::PLAYBOOK_PATH), "[gate]\ncommands = [\"agent-gate\"]\n").unwrap();
        git_ok(&agent, &["add", "."]);
        git_ok(&agent, &["commit", "-qm", "a playbook of its own"]);
        let work = git(&agent, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        switch_large(&root, "main", &work, &keep).unwrap();
        assert!(root.join(playbook::PLAYBOOK_PATH).exists());
        assert!(Playbook::load(&root).unwrap().gate.commands.is_empty(), "main has no playbook, so not the snapshot's either");
        return_large(&root, "main").unwrap();
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_request_waiting_for_its_checkout_holds_up_no_other_project() {
        let base = crate::paths::unique_temp("nitely-large-turn", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let row = |project: &str, waiting_for: Option<&str>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, waiting_for)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, ?3, 3, 1, ?4)",
                    params![project, format!("C:/Code/{project}"), me, waiting_for],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |_: i64| true;
        let big = row("BIG", None);
        let big2 = row("BIG", None);
        let small = row("SML", None);
        assert!(!my_turn(&db, small, 1, &mut alive).unwrap(), "one slot, and BIG's request is first");
        db.conn.execute("UPDATE lane_run SET waiting_for = 'your checkout has a change: notes.txt' WHERE id = ?1", [big]).unwrap();
        assert!(my_turn(&db, small, 1, &mut alive).unwrap(), "BIG waits for its checkout, and its second request behind it, so SML goes");
        assert!(my_turn(&db, big, 1, &mut alive).unwrap(), "and BIG keeps its own place");
        assert!(!my_turn(&db, big2, 1, &mut alive).unwrap(), "behind its own repo's first");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_base_moves_only_on_its_schedule() {
        let every = |b: &str| LaneRules { base: b.into(), ..Default::default() }.base_every_hours();
        assert_eq!((every("every 4h"), every("4h"), every("tip"), every("every 0h"), every("every day")), (Some(4), Some(4), None, None, None));

        let base = crate::paths::unique_temp("nitely-base", "");
        let root = repo(&base);
        let first = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_eq!(crate::light::base_commit(&playbook_with("[lanes]\n"), &root).unwrap(), first);
        let pb = playbook_with("[lanes]\nbase = \"every 4h\"\n");
        assert_eq!(crate::light::base_commit(&pb, &root).unwrap(), first);

        std::fs::write(root.join("src/a.txt"), "two\n").unwrap();
        git_ok(&root, &["commit", "-qam", "next"]);
        let second = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_eq!(crate::light::base_commit(&pb, &root).unwrap(), first, "not yet: the base moved under 4 hours ago");
        assert_eq!(crate::light::base_commit(&playbook_with("[lanes]\n"), &root).unwrap(), second, "the tip, without a schedule");

        std::fs::write(dir(&root, &pb).join("base"), format!("{first}\n{}\n", now() - 5 * 3600)).unwrap();
        assert_eq!(crate::light::base_commit(&pb, &root).unwrap(), second, "moved after 4 hours");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_command_past_its_time_is_stopped() {
        let base = crate::paths::unique_temp("nitely-lane-time", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        let sleeper = if cfg!(windows) { "ping -n 30 127.0.0.1 >NUL" } else { "sleep 30" };
        let started = Instant::now();
        let ended = run_logged(sleeper, &base, &log, Duration::from_secs(1), NO_CAP).unwrap();
        assert!(ended.timed_out && ended.code.is_none());
        assert!(started.elapsed() < Duration::from_secs(20), "stopped, not waited out");
        std::fs::remove_dir_all(base).ok();
    }
}
