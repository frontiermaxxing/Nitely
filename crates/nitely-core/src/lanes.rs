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
    /// Explicit executor selection; transport permissions live in machine-local executors.json.
    pub executor: String,
    /// Exact GitHub check names and app identities required for the CI executor.
    pub ci_checks: Vec<crate::executor::RequiredCheck>,
    /// Lanes for this repo on this machine.
    pub count: u32,
    /// Build directories a lane keeps between requests; `target` and `node_modules` unless the
    /// playbook says (the same list `nitely lanes slim` reclaims). A name (`node_modules`) is that
    /// directory at any depth, a path (`/node_modules`, `web/dist`) only from the repo's root
    /// (`keep_anywhere`, NIT-268).
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
    /// A quick lane (NIT-347): one more checkout, made on first use, only for quick runs (a short
    /// `run`, `quick_class`), one at a time, so a one-file test never waits out lanes full of
    /// gates. Never in large-repo mode; `quick = false` turns it off.
    pub quick: bool,
}

/// The commands that build or test when the playbook names none.
pub const DEFAULT_HEAVY: &[&str] = &["^cargo (build|test|clippy|check|run)\\b", "^npm (ci|install|test|run build)\\b"];

impl Default for LaneRules {
    fn default() -> Self {
        LaneRules {
            executor: "local".into(),
            ci_checks: vec![],
            count: 1,
            keep: crate::slim::DEFAULT_KEEP.iter().map(|s| s.to_string()).collect(),
            timeout_minutes: 30,
            heavy: DEFAULT_HEAVY.iter().map(|s| s.to_string()).collect(),
            size_cap_gb: None,
            large_repo: false,
            base: "tip".into(),
            quick: true,
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

    /// Whether the repo has a quick lane: on unless `quick = false`, and never in large-repo mode.
    pub fn quick_lane(&self) -> bool {
        self.quick && !self.large_repo
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

/// The quick lane's number (NIT-347): beside lanes 1 to `count`, in `lane-quick`.
pub const QUICK_LANE: u32 = 0;

/// A lane's name in the lanes folder: `lane-<n>`, or `lane-quick` for the quick lane.
fn lane_name(n: u32) -> String {
    if n == QUICK_LANE {
        "lane-quick".to_string()
    } else {
        format!("lane-{n}")
    }
}

/// A repo's lanes by number: 1 to its count, then its quick lane when it has one.
fn lane_numbers(rules: &LaneRules) -> Vec<u32> {
    let mut lanes: Vec<u32> = (1..=rules.lanes()).collect();
    if rules.quick_lane() {
        lanes.push(QUICK_LANE);
    }
    lanes
}

fn lane_path(repo_root: &Path, pb: &Playbook, n: u32) -> PathBuf {
    if pb.lanes.as_ref().is_some_and(|l| l.large_repo) {
        return repo_root.to_path_buf();
    }
    dir(repo_root, pb).join(lane_name(n))
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
    // Only what git does not track can be build output: a changed tracked file counts wherever it
    // is, even in a folder named like one (`src/target/parser.rs`), or the build would take it in.
    let changed: Vec<&str> = status.split('\0').filter(|e| e.len() > 3).filter(|e| !(e.starts_with("??") && in_keep(keep, &e[3..]))).map(|e| &e[3..]).collect();
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

/// A repo-relative path inside one of the build directories: under a path entry, or under a
/// directory a name entry names at any depth.
fn in_keep(keep: &[String], path: &str) -> bool {
    keep.iter().any(|k| match keep_anywhere(k) {
        Some(name) => {
            // The directories on the path: every part but a file's own name.
            let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
            let dirs = if path.ends_with('/') { &parts[..] } else { &parts[..parts.len().saturating_sub(1)] };
            dirs.contains(&name)
        }
        None => {
            let k = k.trim_matches('/');
            !k.is_empty() && (path == k || path.starts_with(&format!("{k}/")))
        }
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
    for p in added.split('\0').filter(|p| !p.is_empty()) {
        if !in_keep(keep, p) {
            leaves.push(p.to_string());
        }
        // Every folder on the way, kept ones too: a path into a kept directory deep down
        // (`assets/target/x.rs`) still needs `assets`, and `assets/target`, to be a folder or free.
        // A kept folder that is a folder is never flagged below; one that is a file is someone's.
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
    /// What admission set aside for it, in bytes (NIT-363): its command's expected peak
    /// (`expected_peak`), which the next runs' admission counts.
    #[serde(default)]
    pub memory_reserved: Option<i64>,
    /// The cap it is held to, in bytes: past it, its processes are stopped (NIT-266).
    #[serde(default)]
    pub memory_cap: Option<i64>,
    /// Why it waits, in words, where the queue was read for it (`nitely lanes`, the app): the
    /// machine's slots, its own repo's queue or lanes, the person's checkout, or memory (NIT-291).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_why: Option<String>,
}

impl Run {
    /// The command ran and exited 0.
    pub fn passed(&self) -> bool {
        self.state == "done" && self.exit_code == Some(0)
    }
}

const COLUMNS: &str = "id, project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, lane, executor, \
queued_at, started_at, ended_at, exit_code, timed_out, setup_ran, files_switched, log_path, output, error, waiting_for, waiting_memory, peak_memory, pid, \
pid_started, memory_reserved, memory_cap";

/// The process that asked for a run (NIT-270): its pid and when it started.
#[derive(Debug, Clone, Default)]
struct Requester {
    pid: Option<i64>,
    started: Option<String>,
}

impl Requester {
    /// Its process has ended: the pid is gone, or now names a process that started at another time.
    fn gone(&self) -> bool {
        self.pid.is_some_and(|p| !requester_alive(p as u32, self.started.as_deref()))
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<(Run, Requester)> {
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
            memory_reserved: r.get(28)?,
            memory_cap: r.get(29)?,
            waiting_why: None,
        },
        Requester { pid: r.get(26)?, started: r.get(27)? },
    ))
}

impl Db {
    /// One run by id. A queued or running run whose process is gone reads as abandoned.
    pub fn lane_run(&self, id: i64) -> Result<Option<Run>> {
        let found = self.conn.query_row(&format!("SELECT {COLUMNS} FROM lane_run WHERE id = ?1"), [id], row).optional()?;
        Ok(found.map(|(run, requester)| settle(self, run, requester)))
    }

    /// A project's runs, newest first; queued and running ones whose process is gone are marked
    /// abandoned on the way.
    pub fn lane_runs(&self, project: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![project, limit as i64], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(run, requester)| settle(self, run, requester)).collect())
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
            .map(|(run, requester)| settle(self, run, requester))
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
        Ok(rows.into_iter().map(|(run, requester)| settle(self, run, requester)).collect())
    }
}

/// Takes a queued run out of the queue (`nitely lanes cancel`): for a request whose requester will
/// not come for it, so the ones behind it go on. A running one is ended by stopping what asked for
/// it, which the refusal names.
pub fn cancel(db: &Db, id: i64, actor: &Actor) -> Result<Run> {
    let run = db.lane_run(id)?.with_context(|| format!("no run {id}"))?;
    match run.state.as_str() {
        "queued" => {
            let why = format!("cancelled by {} while it was queued", actor.id());
            db.conn.execute(
                "UPDATE lane_run SET state = 'abandoned', ended_at = ?2, error = ?3 WHERE id = ?1 AND state = 'queued'",
                params![id, now(), why],
            )?;
            db.lane_run(id)?.context("the run's record went missing")
        }
        "running" => {
            let pid: Option<i64> = db.conn.query_row("SELECT pid FROM lane_run WHERE id = ?1", [id], |r| r.get(0))?;
            bail!(
                "run {id} is running{}; stop what asked for it ({}) and it ends as abandoned",
                run.lane.map(|n| format!(" in lane {n}")).unwrap_or_default(),
                pid.map(|p| format!("process {p}")).unwrap_or_else(|| "its process".into())
            )
        }
        _ => Ok(run),
    }
}

/// A queued or running run outlives the process serving it only by a crash: it is abandoned, and
/// says so.
fn settle(db: &Db, mut run: Run, requester: Requester) -> Run {
    let open = run.state == "queued" || run.state == "running";
    if open && requester.gone() {
        let why = if run.state == "queued" { "its requester exited while it was queued" } else { "its requester exited while it ran" };
        db.conn
            .execute(
                "UPDATE lane_run SET state = 'abandoned', ended_at = COALESCE(ended_at, ?2), error = COALESCE(error, ?3) WHERE id = ?1 AND state IN ('queued', 'running')",
                params![run.id, now(), why],
            )
            .ok();
        run.state = "abandoned".into();
        run.error.get_or_insert(why.to_string());
    }
    run
}

/// Whether a requester's process runs, as `settle` asks it: the app reads every lane's queue on each
/// refresh, so an answer is kept for two seconds (NIT-289). Tests ask the system every time. A
/// process at that number that started at another time is not the requester (NIT-270).
fn requester_alive(pid: u32, started: Option<&str>) -> bool {
    if cfg!(test) {
        return crate::procs::still_runs(pid, started);
    }
    type Seen = std::collections::HashMap<(u32, Option<String>), (bool, Instant)>;
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(Default::default);
    let key = (pid, started.map(str::to_string));
    if let Some((alive, at)) = seen.get(&key) {
        if at.elapsed() < Duration::from_secs(2) {
            return *alive;
        }
    }
    let alive = crate::procs::still_runs(pid, started);
    seen.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(60));
    seen.insert(key, (alive, Instant::now()));
    alive
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
    if let Some(run) = crate::executor::run(db, repo_root, pb, req, actor, on_event)? { return Ok(run); }
    // The machine's share as it is when the command starts, not when it was queued (NIT-291), with
    // the cap it was admitted with (NIT-323).
    run_with(db, repo_root, pb, req, actor, on_event, &[], &mut |lane, log, limit, temp, share| {
        let command = playbook::expand(req.command, repo_root, lane, pb.integration_branch()).replace("{jobs}", &share.jobs.to_string());
        run_logged(&command, lane, log, limit, share, Some(temp))
    })
}

/// What runs in a lane once it holds the request's snapshot: given the lane, the log, the time
/// limit and the request's temporary folder (`lane_temp`), it returns how the command ended.
type Exec<'a> = dyn FnMut(&Path, &Path, Duration, &Path, Share) -> Result<Ended> + 'a;

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
#[allow(clippy::too_many_arguments)]
fn run_with(db: &Db, repo_root: &Path, pb: &Playbook, req: &Request, actor: &Actor, on_event: &mut dyn FnMut(&Run), yolo_keys: &[&str], exec: &mut Exec) -> Result<Run> {
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
        "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, pid_started, polled_at, scheduler)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'local', ?10, ?11, ?12, ?13, ?10, ?14)",
        params![
            req.project.to_uppercase(),
            root,
            req.issue,
            actor.id(),
            req.kind.as_str(),
            req.snapshot.commit,
            req.snapshot.tree,
            req.snapshot.base,
            req.command,
            now(),
            std::process::id(),
            priority,
            crate::procs::me().1,
            SCHEDULER
        ],
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
        let quick = matches!(req.kind, Kind::Run) && quick_class(db, &root, req.command)?.is_some();
        let (n, lock, slot) = take_turn(db, id, repo_root, pb, &rules, quick, &mut slots_now(), &mut read_memory, &mut memory_limits, yolo_keys)?;
        let logs = logs_dir(db, req.project);
        std::fs::create_dir_all(&logs)?;
        let log = logs.join(format!("{id}.log"));
        let started = db.conn.execute(
            "UPDATE lane_run SET state = 'running', lane = ?2, started_at = ?3, log_path = ?4, memory_cap = COALESCE(memory_cap, ?5) WHERE id = ?1 AND state = 'queued'",
            params![id, n, now(), log.to_string_lossy(), run_cap().map(|c| c as i64)],
        )?;
        if started == 0 {
            bail!("run {id} was taken out of the queue before it started");
        }
        crate::performance_events::lane(db, id, crate::performance_events::Kind::Started);
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

    // What it runs with: the machine's cores now, and the cap it was admitted with, so what runs is
    // what admission counted (NIT-323).
    let recorded: Option<i64> = db.conn.query_row("SELECT memory_cap FROM lane_run WHERE id = ?1", [id], |r| r.get(0)).optional()?.flatten();
    let now_share = share();
    let run_share = Share { jobs: now_share.jobs, cap: recorded.map(|c| c.max(0) as u64).or(now_share.cap) };
    let _telemetry = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project: Some(req.project.into()), issue: req.issue.map(str::to_owned), run_id: Some(id), lane: Some(n), ..Default::default() });
    let outcome = serve(repo_root, pb, &rules, &lane, &log, req, run_share, exec);
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
            if why.starts_with("out of memory in the lane:") {
                crate::performance_events::lane(db, id, crate::performance_events::Kind::OutOfMemory);
            }
        }
    }
    crate::performance_events::lane(db, id, crate::performance_events::Kind::Ended);
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
    pub yolo_skipped: bool,
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
pub fn gate(db: &Db, repo_root: &Path, pb: &Playbook, worktree: &Path, issue: Option<&str>, kind: Kind, steps: &[(String, Option<String>)], actor: &Actor, yolo_keys: &[&str]) -> Result<GateRun> {
    gate_inner(db, repo_root, pb, worktree, issue, kind, steps, actor, true, yolo_keys)
}

/// A backstop or reproduction must execute, even when the same tree passed before.
#[allow(clippy::too_many_arguments)]
pub fn gate_fresh(db: &Db, repo_root: &Path, pb: &Playbook, worktree: &Path, issue: Option<&str>, kind: Kind, steps: &[(String, Option<String>)], actor: &Actor) -> Result<GateRun> {
    gate_inner(db, repo_root, pb, worktree, issue, kind, steps, actor, false, &[])
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn gate_fresh_checked(db:&Db,repo_root:&Path,pb:&Playbook,worktree:&Path,issue:Option<&str>,kind:Kind,steps:&[(String,Option<String>)],actor:&Actor,keys:&[&str])->Result<GateRun> {
    gate_inner(db,repo_root,pb,worktree,issue,kind,steps,actor,false,keys)
}

#[allow(clippy::too_many_arguments)]
fn gate_inner(db: &Db, repo_root: &Path, pb: &Playbook, worktree: &Path, issue: Option<&str>, kind: Kind, steps: &[(String, Option<String>)], actor: &Actor, reuse: bool, yolo_keys: &[&str]) -> Result<GateRun> {
    crate::project_modes::check_playbook_authority(db,yolo_keys,pb)?;
    // What is committed, as land will build it: the branch head, never a commit made for the
    // request, so the gate reports the commit the review sees.
    let top = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"])?.trim());
    let head = git(&top, &["rev-parse", "HEAD"])?.trim().to_string();
    let snap = Snapshot { tree: git(&top, &["rev-parse", "HEAD^{tree}"])?.trim().to_string(), commit: head.clone(), base: head, made: false };
    let mut warnings = Vec::new();
    if snapshot(worktree)?.made {
        warnings.push("the worktree has uncommitted work, which this gate leaves out: it builds what is committed, as land will; `run` builds the worktree as it is".to_string());
    }
    if let Some(gate) = crate::executor::gate(db, repo_root, pb, &snap, issue, kind, steps, actor, &warnings)? { return Ok(gate); }
    let short = snap.commit[..snap.commit.len().min(7)].to_string();
    let project = issue.and_then(|k| db.issue(k).ok().flatten()).and_then(|i| i.project).unwrap_or_default();
    let base = pb.integration_branch().to_string();
    let skipped = |why: &str, command: &str| playbook::CommandResult { command: command.to_string(), ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true, timed_out: false };
    // The commands as the playbook writes them: the record's command, and what an earlier pass of
    // the same tree must have run.
    let to_run: Vec<&str> = steps.iter().filter(|(_, skip)| skip.is_none()).map(|(c, _)| c.as_str()).collect();
    let key = to_run.join("\n");
    let root = repo_root.to_string_lossy().into_owned();
    // One command per line is the key, so a command that holds a line break of its own could
    // stand for two: such a gate is never answered from a record.
    let keyed = !to_run.iter().any(|c| c.contains('\n'));
    let passed_before: Option<i64> = if !keyed || !reuse {
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
        return Ok(GateRun { yolo_skipped: false, commit: short, results, passed: true, flaked: vec![], run: Some(id), warnings });
    }

    let mut results: Vec<playbook::CommandResult> = Vec::new();
    let mut flaked: Vec<playbook::Flake> = Vec::new();
    let retries = pb.gate.retries;
    let req = Request { project: &project, issue, kind, snapshot: &snap, command: &key };
    let mut yolo_skipped = false;
    let run = run_with(db, repo_root, pb, &req, actor, &mut |_| {}, yolo_keys, &mut |lane: &Path, log: &Path, limit: Duration, temp: &Path, share: Share| {
        // The machine's share as it is when the gate starts, not when it was queued (NIT-291), with
        // the cap it was admitted with (NIT-323).
        let mut peak: Option<u64> = None;
        for (command, skip) in steps {
            crate::project_modes::check_playbook_authority(db,yolo_keys,pb)?;
            if crate::project_yolo::all_enabled(db, yolo_keys)? {
                yolo_skipped = true;
                // Never an exit-code-zero lane cache entry for unchecked work.
                return Ok(Ended { code: None, timed_out: false, peak });
            }
            if let Some(why) = skip {
                results.push(skipped(why, command));
                continue;
            }
            let command = playbook::expand(command, repo_root, lane, &base).replace("{jobs}", &share.jobs.to_string());
            let started = Instant::now();
            let mut failures: Vec<String> = Vec::new();
            loop {
                let from = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
                let ended = run_logged(&command, lane, log, limit, share, Some(temp))?;
                let (code, timed_out) = (ended.code, ended.timed_out);
                peak = peak.max(ended.peak);
                let output = log_since(log, from);
                let seconds = started.elapsed().as_secs();
                if timed_out {
                    let output = format!("Ran past the lane's {}-minute limit and was stopped.\n{output}", limit.as_secs() / 60);
                    results.push(playbook::CommandResult { command: command.clone(), ok: false, seconds, output, skipped: false, timed_out: true });
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
                    results.push(playbook::CommandResult { command: command.clone(), ok: true, seconds, output, skipped: false, timed_out: false });
                    break;
                }
                if failures.len() as u32 >= retries {
                    results.push(playbook::CommandResult { command: command.clone(), ok: false, seconds, output, skipped: false, timed_out: false });
                    return Ok(Ended { code, timed_out: false, peak });
                }
                failures.push(output);
            }
        }
        Ok(Ended { code: Some(0), timed_out: false, peak })
    });
    let run = match run {
        Ok(run) => run,
        Err(e) if e.is::<crate::project_yolo::SkipGate>() => return Ok(GateRun { yolo_skipped: true, commit: short, passed: false, results: vec![], flaked: vec![], run: None, warnings }),
        Err(e) => return Err(e),
    };
    if yolo_skipped {
        db.conn.execute("UPDATE lane_run SET state = 'abandoned', exit_code = NULL, error = ?2 WHERE id = ?1", params![run.id, crate::project_yolo::NOTE])?;
    }
    if let Some(why) = &run.error {
        // The setup's own output too, so the caller sees why it failed.
        let output = if run.output.trim().is_empty() { why.clone() } else { format!("{why}\n{}", run.output) };
        results.push(playbook::CommandResult { command: format!("lane {}", run.lane.unwrap_or(0)), ok: false, seconds: 0, output, skipped: false, timed_out: false });
    }
    Ok(GateRun { yolo_skipped, commit: short, passed: run.passed() && !yolo_skipped, results, flaked, run: Some(run.id), warnings })
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
    dir(repo_root, pb).join(format!("{}.lock", lane_name(n)))
}

/// `try_lock`, waiting up to `wait` for a lock that is only held for a moment. On Linux and macOS
/// the lock is an `flock` on an open file, and a process spawned while it was held keeps a copy of
/// the file until it starts its program (close-on-exec): so for a moment after the lock is let go,
/// it can still look held, the more so while many processes start (NIT-260).
fn lock_waiting(repo_root: &Path, pb: &Playbook, n: u32, wait: Duration) -> Result<Option<LaneLock>> {
    let started = std::time::Instant::now();
    loop {
        if let Some(lock) = try_lock(repo_root, pb, n)? {
            return Ok(Some(lock));
        }
        if started.elapsed() >= wait {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// Cores per slot, when the machine decides: many small requests (an agent's clippy and one test
/// module) beat a few wide ones, so 32 threads make 5 slots of 6 jobs (NIT-291).
const CORES_PER_SLOT: u32 = 6;

/// Memory per slot above the floor, when the machine decides. A full gate of this repo peaks at
/// 4 to 6 GB (NIT-266); each run is still held to its cap, and nothing starts below the floor.
const MEMORY_PER_SLOT: u64 = 8 * machine::GIB;

/// The cores builds may use between them: all but 2, or 75%, whichever is more, so the person's
/// own apps always have some (NIT-266).
pub fn build_cores(cores: u32) -> u32 {
    let cores = cores.max(1);
    cores.saturating_sub(2).max(cores * 3 / 4).max(1)
}

/// The budget for `cores` and the machine's memory `limits`, unless `set` says how many slots.
/// When the machine decides, a slot per 6 cores, and one per 8 GB above the memory floor.
pub fn budget_for(cores: u32, limits: Option<&machine::Limits>, set: Option<(u32, &str)>) -> Budget {
    let cores = cores.max(1);
    let (slots, from) = match set.filter(|(n, _)| *n > 0) {
        Some((n, from)) => (n, from.to_string()),
        None => {
            let by_cores = cores / CORES_PER_SLOT;
            let by_memory = limits.map(|l| (l.total.saturating_sub(l.floor) / MEMORY_PER_SLOT) as u32).unwrap_or(by_cores);
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
    let budget = machine_budget();
    Share { jobs: budget.jobs, cap: machine::limits().map(|l| cap_for(&l, budget.slots)) }
}

/// One run's memory cap with `slots` running at once: the set (or default) cap, but never more
/// than the memory above the floor shared by the slots, so every slot at its cap still leaves the
/// floor free (NIT-291: 5 slots of a 16 GB cap on 64 GB would not).
pub fn cap_for(limits: &machine::Limits, slots: u32) -> u64 {
    let room = limits.total.saturating_sub(limits.floor) / u64::from(slots.max(1));
    limits.cap.min(room.max(machine::GIB / 4))
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
    try_slot_named(db, &format!("slot-{k}"))
}

/// The machine's one slot beyond its budget, only for a quick run (NIT-347): one in all, not one
/// per repo, and memory admission still decides whether the run starts.
const QUICK_SLOT: &str = "slot-quick";

fn try_slot_named(db: &Db, name: &str) -> Result<Option<SlotLock>> {
    let dir = state_dir(db);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.lock"));
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(SlotLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// The cap a run starting now gets (`cap_for` with the machine's slots now), as admission sees it.
fn run_cap() -> Option<u64> {
    memory_limits().map(|l| cap_for(&l, machine_budget().slots))
}

/// Why request `id` must wait for the memory the running runs are expected to take (NIT-323), or
/// None: a new run starts only while what is set aside for the running runs and its own expected
/// peak fit above the floor. What is set aside for a run is its command's expected peak
/// (`expected_peak`, NIT-363), not the cap it is held to: runs rarely come near their cap, and
/// setting caps aside left slots standing empty with memory to spare. A run with nothing set aside
/// counts at the cap it started with, and one an older nitely started, with neither recorded, at
/// the full cap; a queued one that reserved counts too (`reserve_cap`, under the admission lock).
/// Admission's check of the memory free now against the floor comes first and still stands, and
/// each run is still stopped at its cap.
fn caps_fit(db: &Db, id: i64, limits: Option<&machine::Limits>, slots: u32) -> Result<Option<String>> {
    let Some(limits) = limits.filter(|l| l.floor > 0) else { return Ok(None) };
    let room = limits.total.saturating_sub(limits.floor);
    let mine = expected_peak_of(db, id, cap_for(limits, slots))?;
    // The running runs, and the queued ones that reserved (admitted, about to start).
    let mut stmt = db.conn.prepare(
        "SELECT COALESCE(memory_reserved, memory_cap), pid, pid_started FROM lane_run WHERE id != ?1 AND executor IN ('local', 'worktree') AND (state = 'running' OR (state = 'queued' AND memory_cap IS NOT NULL))",
    )?;
    let rows: Vec<(Option<i64>, Option<i64>, Option<String>)> = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    // A run an older nitely started recorded no cap: it may hold the full one.
    let unrecorded = limits.cap.min(room);
    let taken: u64 = rows
        .into_iter()
        .filter(|(_, pid, started)| pid.is_none_or(|p| p as u32 == std::process::id() || requester_alive(p as u32, started.as_deref())))
        .map(|(cap, _, _)| cap.map_or(unrecorded, |c| c.max(0) as u64))
        .sum();
    if taken.saturating_add(mine) <= room {
        return Ok(None);
    }
    Ok(Some(format!(
        "waiting for memory: the running builds have {} set aside and this one needs {}, more than the {} above the floor",
        machine::gb(taken),
        machine::gb(mine),
        machine::gb(room)
    )))
}

/// Records the cap request `id` is admitted with, and what is set aside for it (its expected peak,
/// NIT-363), while it still holds the admission lock, so the next request to ask counts it before
/// it is running (NIT-323).
fn reserve_cap(db: &Db, id: i64, limits: Option<&machine::Limits>, slots: u32) -> Result<()> {
    let cap = limits.map(|l| cap_for(l, slots));
    let reserved = match cap {
        Some(cap) => Some(expected_peak_of(db, id, cap)? as i64),
        None => None,
    };
    db.conn.execute("UPDATE lane_run SET memory_cap = ?2, memory_reserved = ?3 WHERE id = ?1", params![id, cap.map(|c| c as i64), reserved])?;
    Ok(())
}

/// Whether request `id` may start now, as memory admission sees it (`machine::admit_once_with`,
/// under the machine's admission lock), or why not. Its expected peak must be free above the
/// floor now (NIT-363), and, on a machine slot, what the running runs have set aside and its own
/// must fit above the floor (`caps_fit`). On the quick slot (`quick_slot`, NIT-347), which is past
/// what the slots set aside by design, the memory free now decides alone. Either way it then sets
/// its own aside (`reserve_cap`), and it is stopped at its cap.
fn admit_run(db: &Db, id: i64, limits: Option<&machine::Limits>, slots: u32, quick_slot: bool, memory: &mut dyn FnMut() -> Option<machine::Memory>) -> Result<Option<String>> {
    // What must be free above the floor now: its command's expected peak, or with no history the
    // least a run sets aside. Its whole cap there would hold a new command back for good on a
    // machine whose own programs leave less than the floor and a cap free, with nothing building
    // (NIT-372); what it sets aside, which the next runs count, stays its whole cap.
    // Never more than its cap: a small machine's cap is under `MIN_RESERVED` (NIT-374).
    let need = match limits {
        Some(l) => {
            let cap = cap_for(l, slots);
            known_peak_of(db, id, cap)?.unwrap_or(MIN_RESERVED.min(cap))
        }
        None => 0,
    };
    machine::admit_once_with(
        &state_dir(db),
        limits,
        need,
        memory,
        &mut || if quick_slot { Ok(None) } else { caps_fit(db, id, limits, slots) },
        &mut || reserve_cap(db, id, limits, slots),
    )
}

/// How many of a command's last finished runs in its repo its expected peak is read from.
const PEAK_HISTORY: i64 = 20;

/// The least admission sets aside for a run with history (NIT-363).
const MIN_RESERVED: u64 = 2 * machine::GIB;

/// What request `id` is expected to take at its peak (NIT-363), from the peaks its command's last
/// runs in its repo reached (`expected_peak`).
fn expected_peak_of(db: &Db, id: i64, cap: u64) -> Result<u64> {
    Ok(known_peak_of(db, id, cap)?.unwrap_or(cap))
}

/// `expected_peak_of` when request `id`'s command has history in its repo; None when it has none.
fn known_peak_of(db: &Db, id: i64, cap: u64) -> Result<Option<u64>> {
    let (repo, command): (String, String) = db.conn.query_row("SELECT repo, command FROM lane_run WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut stmt = db.conn.prepare(
        "SELECT peak_memory FROM lane_run WHERE repo = ?1 AND command = ?2 AND id != ?3 AND state = 'done' AND executor IN ('local', 'worktree') AND peak_memory IS NOT NULL ORDER BY id DESC LIMIT ?4",
    )?;
    let peaks: Vec<i64> = stmt.query_map(params![repo, command, id, PEAK_HISTORY], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok((!peaks.is_empty()).then(|| expected_peak(&peaks, cap)))
}

/// The p90 of `peaks` plus a quarter, at least `MIN_RESERVED`, never more than `cap`; `cap` when
/// there are none: a command with no history may take its whole cap.
fn expected_peak(peaks: &[i64], cap: u64) -> u64 {
    if peaks.is_empty() {
        return cap;
    }
    let mut peaks: Vec<u64> = peaks.iter().map(|p| (*p).max(0) as u64).collect();
    peaks.sort_unstable();
    let p90 = peaks[((peaks.len() - 1) * 9).div_ceil(10)];
    (p90 + p90 / 4).max(MIN_RESERVED).min(cap)
}

/// The machine's build slots as a waiting request reads them: the budget again every few seconds
/// (it reads Nitely's config), so raising `build_slots` lets what already waits use them (NIT-291).
fn slots_now() -> impl FnMut() -> u32 {
    let mut last: Option<(u32, Instant)> = None;
    move || match last {
        Some((n, at)) if at.elapsed() < Duration::from_secs(3) => n,
        _ => {
            let n = machine_budget().slots;
            last = Some((n, Instant::now()));
            n
        }
    }
}

/// A queued request as the queue orders them.
#[derive(Debug, Clone)]
struct Waiting {
    id: i64,
    project: String,
    repo: String,
    priority: i64,
    /// run | gate | land
    kind: String,
    queued_at: i64,
}

/// How long a request waits before it counts as one priority higher (NIT-291): an agent's `run`
/// waiting 5 minutes goes as a gate, 10 minutes as a land, so a busy team's gates and lands never
/// keep it waiting for good.
const AGE_STEP_SECS: i64 = 5 * 60;

/// `w`'s priority after its wait at `now`: one step up for each `AGE_STEP_SECS`, at most to a land's.
fn aged(w: &Waiting, now: i64) -> i64 {
    (w.priority - (now - w.queued_at).max(0) / AGE_STEP_SECS).max(1)
}

/// The order requests start in: priority first (land, then submit's gate, then the rest; raised by
/// how long each has waited, `aged`), then each project's first waiting request before any
/// project's second, then the oldest. Among requests at the same place in their projects' queues,
/// the project served longest ago goes first (`last_started`, by upper-case project; never served
/// counts as longest ago), so a project whose request just started or ran waits behind the
/// others' next, whenever its own was queued.
fn in_order(waiting: Vec<Waiting>, last_started: &std::collections::HashMap<String, i64>, now: i64) -> Vec<Waiting> {
    let mut waiting: Vec<(i64, Waiting)> = waiting.into_iter().map(|w| (aged(&w, now), w)).collect();
    waiting.sort_by_key(|(p, w)| (*p, w.id));
    let mut seen: std::collections::HashMap<(i64, String), usize> = std::collections::HashMap::new();
    let mut ranked: Vec<(i64, usize, i64, i64, Waiting)> = waiting
        .into_iter()
        .map(|(p, w)| {
            let project = w.project.to_uppercase();
            let n = seen.entry((p, project.clone())).or_default();
            *n += 1;
            let last = last_started.get(&project).copied().unwrap_or(i64::MIN);
            (p, *n, last, w.id, w)
        })
        .collect();
    ranked.sort_by_key(|(p, n, last, id, _)| (*p, *n, *last, *id));
    ranked.into_iter().map(|(_, _, _, _, w)| w).collect()
}

/// A request in the queue as `turns` reads it.
#[derive(Debug, Clone)]
struct QueueRow {
    w: Waiting,
    running: bool,
    /// Its requester's process still runs.
    alive: bool,
    lane_count: i64,
    /// Large-repo mode: it waits for the person's checkout.
    waits_for_checkout: bool,
    issue: Option<String>,
    started_at: Option<i64>,
    /// Queued under an older scheduler (NIT-325) by a requester that still runs: passed over, it
    /// holds nobody up.
    older: bool,
    /// A quick run (NIT-347): the run's command usually takes this many seconds, under
    /// `QUICK_SECS`, or it has no history and names a test filter.
    quick: Option<i64>,
    /// The lane a running request holds (`QUICK_LANE` for the quick lane).
    lane: Option<i64>,
    /// Its repo has a quick lane (`LaneRules::quick_lane`).
    quick_lane: bool,
    /// A running quick run that holds the machine's slot beyond its budget (`QUICK_SLOT`).
    quick_slot: bool,
}

/// A `run` whose command's recent median is under this, or which names a test filter, is quick
/// (NIT-347): it may take a free lane ahead of its repo's queued gates.
const QUICK_SECS: i64 = 120;

/// How many seconds the `run` `command` in `repo` usually takes, when it is quick: the median of
/// its last five finished runs, under `QUICK_SECS`; with none, `QUICK_SECS` when the command names
/// a test filter (`names_test_filter`). None for a run that is not quick.
fn quick_class(db: &Db, repo: &str, command: &str) -> Result<Option<i64>> {
    let mut stmt = db.conn.prepare(
        "SELECT ended_at - started_at FROM lane_run WHERE repo = ?1 AND command = ?2 AND kind = 'run' AND state = 'done' AND executor = 'local'
         AND started_at IS NOT NULL AND ended_at IS NOT NULL ORDER BY id DESC LIMIT 5",
    )?;
    let mut took: Vec<i64> = stmt.query_map(params![repo, command], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if took.is_empty() {
        return Ok(names_test_filter(command).then_some(QUICK_SECS));
    }
    took.sort_unstable();
    let median = took[took.len() / 2].max(1);
    Ok((median < QUICK_SECS).then_some(median))
}

/// Whether `command` runs only some tests: `cargo test` with a test name or `--test <target>`
/// (`cargo test -p nitely-core waits`, `cargo test --test install`, `cargo test -- lanes::`), a
/// file after `--test` of node or tsx (`node --test dist/a.test.js`, `npx tsx --test test/a.test.ts`,
/// `node --import tsx --test test/a.test.ts`, NIT-359), `vitest` or `jest` with a file or `-t`, a
/// file after `npm test --`, a `--test-name-pattern`, or `pytest` with `-k` or `file::test`.
fn names_test_filter(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let ends = |t: &str| matches!(t, "&&" | "||" | ";" | "|");
    // A program as a command names it: `node`, `C:\tools\node.exe`, `./node_modules/.bin/tsx.cmd`.
    let program = |t: &str| {
        let name = t.rsplit(['/', '\\']).next().unwrap_or(t);
        name.strip_suffix(".exe").or_else(|| name.strip_suffix(".cmd")).unwrap_or(name).to_string()
    };
    // The words of `words[from..]` up to the end of its command.
    let segment = |from: usize| words.get(from..).unwrap_or_default().iter().take_while(|t| !ends(t)).copied().collect::<Vec<&str>>();
    // Whether `args` name a file or test: a word left once the values of the flags that take one
    // (`with_value`, the runners' own option lists) are passed over. Any other flag is taken as one
    // that takes none (`--runInBand lanes` names `lanes`), so a value-taking option a list misses
    // can make a whole-suite run quick; the lists name the ones in use (the NIT-359 review).
    let names_one = |args: &[&str], with_value: &[&str]| {
        let mut j = 0;
        while j < args.len() {
            let t = args[j];
            // One that takes several: every word up to the next option or `--` (NIT-369).
            if JS_RUNNER_MULTI_VALUE_FLAGS.contains(&t) {
                j += 1;
                while j < args.len() && !args[j].starts_with('-') {
                    j += 1;
                }
                continue;
            }
            if with_value.contains(&t) {
                j += 2;
                continue;
            }
            if !t.starts_with('-') {
                return true;
            }
            j += 1;
        }
        false
    };
    let node_value = ["--import", "--require", "-r", "--loader", "--test-reporter", "--test-reporter-destination", "--test-concurrency", "--test-timeout", "--test-shard"];
    for (i, w) in words.iter().enumerate() {
        if *w == "test" && i > 0 && words[i - 1] == "cargo" {
            let with_value = ["-p", "--package", "--features", "-F", "-j", "--jobs", "--target", "--profile", "--manifest-path", "--target-dir", "--color", "--message-format"];
            let mut j = i + 1;
            while j < words.len() && !ends(words[j]) {
                let t = words[j];
                if t == "--" {
                    return words[j + 1..].iter().take_while(|t| !ends(t)).any(|t| !t.starts_with('-'));
                }
                if t == "--test" || t.starts_with("--test=") {
                    return true;
                }
                if with_value.contains(&t) {
                    j += 2;
                    continue;
                }
                if !t.starts_with('-') {
                    return true;
                }
                j += 1;
            }
        }
        // `--test` of node or tsx in the same command, with a file after it.
        if *w == "--test" {
            let start = words[..i].iter().rposition(|t| ends(t)).map_or(0, |e| e + 1);
            if words[start..i].iter().any(|t| matches!(program(t).as_str(), "node" | "tsx")) && names_one(&segment(i + 1), &node_value) {
                return true;
            }
        }
        // vitest or jest with a file, or `-t` a test's name.
        if matches!(program(w).as_str(), "vitest" | "jest") {
            let args: Vec<&str> = segment(i + 1).into_iter().filter(|t| !matches!(*t, "run" | "watch" | "dev")).collect();
            if args.iter().any(|t| matches!(*t, "-t" | "--testNamePattern") || t.starts_with("--testNamePattern=")) || names_one(&args, JS_RUNNER_VALUE_FLAGS) {
                return true;
            }
        }
        // `npm test -- <file>`, `npm run test -- <file>`.
        if program(w) == "npm" {
            let args = segment(i + 1);
            let is_test = matches!(args.first(), Some(&"test") | Some(&"t")) || (args.first() == Some(&"run") && args.get(1) == Some(&"test"));
            if let (true, Some(dash)) = (is_test, args.iter().position(|t| *t == "--")) {
                if names_one(&args[dash + 1..], JS_RUNNER_VALUE_FLAGS) {
                    return true;
                }
            }
        }
        if w.starts_with("--test-name-pattern") || (w.ends_with("pytest") && words[i + 1..].iter().take_while(|t| !ends(t)).any(|t| *t == "-k" || t.contains("::"))) {
            return true;
        }
    }
    false
}

/// The options of vitest and jest (and so of `npm test -- …`, which hands them on) that take a
/// value as the next word: `--maxWorkers 2` and `--outputFile results.json` filter nothing.
const JS_RUNNER_VALUE_FLAGS: &[&str] = &[
    "--config", "-c", "--project", "--reporter", "--outputFile", "--maxWorkers", "-w", "--minWorkers", "--pool", "--root", "-r", "--rootDir",
    "--dir", "--shard", "--seed", "--testTimeout", "--hookTimeout", "--environment", "--testEnvironment", "--coverageDirectory",
    "--coverageProvider", "--mode", "--retry", "--testSequencer", "--workerIdleMemoryLimit",
];

/// Jest's options that take several values, every word up to the next option or `--`:
/// `--projects app lib` filters nothing (NIT-369).
const JS_RUNNER_MULTI_VALUE_FLAGS: &[&str] = &["--projects", "--roots", "--selectProjects", "--testPathIgnorePatterns", "--reporters"];

/// Where a queued request stands.
#[derive(Debug, Clone, PartialEq)]
enum Turn {
    /// It may try for a slot and a lane now.
    Go,
    /// A quick run that may try now, ahead of its repo's queue (NIT-347), and the words for it.
    QuickGo(String),
    /// A quick run that may try now on its repo's quick lane alone (NIT-347), and the words for it.
    QuickLane(String),
    /// It may try, but every lane of its repo is busy, so its lane's lock will decide.
    LanesBusy(String),
    /// Not yet, and why, in words.
    Wait(String),
}

/// Where each live queued request stands, given the machine's `slots`. A request goes when no
/// request of its own repo is ahead of it and fewer requests ahead of it that can start (of repos
/// with a free lane) than there are free slots. While an agent's `run` that could start waits, a
/// gate leaves the last free slot to it on a machine with 3 slots or more, so a short run never
/// waits out a row of long gates (NIT-291). A repo whose lanes are all busy holds up no other.
/// While a live land holds a repo's land lock (`landing`, by normalized repo) and no land of that
/// repo runs yet, the rest leave that repo one lane and the machine one slot: the land's gate is
/// coming, and every other land waits on it, so it never waits out a submit's gate (NIT-332).
fn turns(rows: Vec<QueueRow>, slots: u32, served: &std::collections::HashMap<String, i64>, now: i64, landing: &std::collections::HashSet<String>) -> std::collections::HashMap<i64, Turn> {
    use std::collections::{HashMap, HashSet};
    let repo_of = |w: &Waiting| crate::paths::normalize(&w.repo);
    let mut held_by_checkout: HashSet<String> = HashSet::new();
    let mut running_by_repo: HashMap<String, i64> = HashMap::new();
    let mut lanes_by_repo: HashMap<String, i64> = HashMap::new();
    let mut running: Vec<&QueueRow> = Vec::new();
    let mut waiting = Vec::new();
    // The quick lane (NIT-347): the repos that have one, and those whose quick lane a run holds.
    let mut has_quick_lane: HashSet<String> = HashSet::new();
    let mut quick_lane_busy: HashSet<String> = HashSet::new();
    let mut quick_slot_busy = false;
    for row in &rows {
        if row.quick_lane {
            has_quick_lane.insert(repo_of(&row.w));
        }
        // Whatever its requester: the lane's and slot's locks are held until its run ends.
        if row.running && row.lane == Some(i64::from(QUICK_LANE)) {
            quick_lane_busy.insert(repo_of(&row.w));
        }
        quick_slot_busy |= row.running && row.quick_slot;
    }
    for row in &rows {
        let repo = repo_of(&row.w);
        // Only a request that still holds its place: a stale one's checkout-wait mark would keep
        // its repo's fresh run from the last free slot (the NIT-325 review).
        if row.waits_for_checkout && row.alive {
            held_by_checkout.insert(repo.clone());
        }
        lanes_by_repo.insert(repo.clone(), row.lane_count.max(1));
        // A request whose process is gone holds nothing and waits for nothing.
        if !row.alive {
            continue;
        }
        if row.running {
            // A run on the quick lane takes none of its repo's lanes, and one on the quick slot
            // none of the machine's.
            if !row.quick_slot {
                running.push(row);
            }
            if row.lane != Some(i64::from(QUICK_LANE)) {
                *running_by_repo.entry(repo).or_default() += 1;
            }
        } else {
            waiting.push(row.w.clone());
        }
    }
    let free_lanes = |repo: &str| lanes_by_repo.get(repo).copied().unwrap_or(1) - running_by_repo.get(repo).copied().unwrap_or(0);
    let land_running: HashSet<String> = running.iter().filter(|r| r.w.kind == "land").map(|r| repo_of(&r.w)).collect();
    // A land an older nitely runs, whose gate already waits in the queue under the older order:
    // the queue passes over that gate (NIT-325), so a lane kept for it would only stand empty. Its
    // own order may put a newer gate first, and the gate waiting for the kept lane while the land
    // waits for the gate would hold them both for good (NIT-338). Its lane is not kept.
    let older_land: HashSet<String> = rows.iter().filter(|r| r.older && !r.running && r.w.kind == "land").map(|r| repo_of(&r.w)).collect();
    let keeps_for_land = |repo: &str| landing.contains(repo) && !land_running.contains(repo) && !older_land.contains(repo);
    let kept_slots = landing.iter().filter(|r| keeps_for_land(r)).count() as i64;
    // The lanes a request of `kind` may take: all of them for a land, one fewer for the rest
    // while the repo's land is coming.
    let lanes_for = |repo: &str, kind: &str| free_lanes(repo) - i64::from(kind != "land" && keeps_for_land(repo));
    let can_start = |repo: &str, kind: &str| lanes_for(repo, kind) > 0 && !held_by_checkout.contains(repo);
    let free_slots = (slots as i64 - running.len() as i64).max(0);
    let ordered = in_order(waiting, served, now);
    // A waiting land is its repo's first request, whatever has aged past it: the rest of its repo
    // would otherwise wait for the lane kept for it while it waits behind them (NIT-332).
    let mut land_waiting: HashMap<String, i64> = HashMap::new();
    for w in ordered.iter().filter(|w| w.kind == "land") {
        land_waiting.entry(repo_of(w)).or_insert(w.id);
    }
    let behind_land = |w: &Waiting, repo: &str| if w.kind == "land" { None } else { land_waiting.get(repo).copied() };
    // Only a repo's first request can start, so only a run that is first in its repo keeps a slot:
    // one behind its own repo's gate would keep the slot from that gate and never take it.
    let mut firsts: HashSet<String> = HashSet::new();
    let run_could_start = ordered.iter().any(|w| {
        let repo = repo_of(w);
        behind_land(w, &repo).is_none() && firsts.insert(repo.clone()) && w.kind == "run" && can_start(&repo, &w.kind)
    });
    // Each repo's quick run (NIT-347): its first queued quick run behind another request of its
    // repo may take a free lane ahead of them, in a repo of two lanes or more, while no quick run
    // of the repo holds a lane (one running past three times its usual time is just a run), and
    // never ahead of a waiting land. So one lane at most goes to quick runs, and the rest of the
    // repo's lanes stay the queue's: gates are never starved, and aging holds for all.
    let quick_of: HashMap<i64, i64> = rows.iter().filter(|r| r.alive && !r.running).filter_map(|r| r.quick.map(|m| (r.w.id, m))).collect();
    let quick_holds: HashSet<String> =
        rows.iter().filter(|r| r.alive && r.running && r.quick.is_some_and(|m| now - r.started_at.unwrap_or(now) <= 3 * m)).map(|r| repo_of(&r.w)).collect();
    let mut quick_pick: HashMap<String, (i64, usize)> = HashMap::new();
    {
        let mut before: HashMap<String, usize> = HashMap::new();
        let mut head_is_quick: HashSet<String> = HashSet::new();
        for w in ordered.iter() {
            let repo = repo_of(w);
            if behind_land(w, &repo).is_some() || land_waiting.contains_key(&repo) {
                continue;
            }
            let n = before.entry(repo.clone()).or_default();
            // A quick run already first in its repo goes as its repo's first: no other is picked
            // past it, however many quick runs arrive after it (the NIT-347 review).
            if *n == 0 && w.kind == "run" && quick_of.contains_key(&w.id) {
                head_is_quick.insert(repo.clone());
            }
            let eligible = *n > 0
                && !has_quick_lane.contains(&repo)
                && !head_is_quick.contains(&repo)
                && w.kind == "run"
                && quick_of.contains_key(&w.id)
                && !quick_pick.contains_key(&repo)
                && lanes_by_repo.get(&repo).copied().unwrap_or(1) >= 2
                && !quick_holds.contains(&repo)
                && !held_by_checkout.contains(&repo);
            if eligible {
                quick_pick.insert(repo.clone(), (w.id, *n));
            }
            *n += 1;
        }
    }
    let run_could_start = run_could_start || quick_pick.keys().any(|repo| can_start(repo, "run"));
    // A repo with a quick lane, while it is free: its first quick run takes it, wherever it waits
    // in its repo's queue, and the repo's lanes stay the queue's (NIT-347).
    let mut lane_pick: HashMap<String, (i64, usize)> = HashMap::new();
    {
        let mut before: HashMap<String, usize> = HashMap::new();
        for w in ordered.iter() {
            let repo = repo_of(w);
            let n = before.entry(repo.clone()).or_default();
            // First in its repo with a lane free, it simply takes that lane: the quick lane is
            // made only for a quick run that would wait (NIT-347).
            let would_wait = *n > 0 || lanes_for(&repo, &w.kind) <= 0;
            if would_wait && w.kind == "run" && quick_of.contains_key(&w.id) && has_quick_lane.contains(&repo) && !quick_lane_busy.contains(&repo) && !lane_pick.contains_key(&repo) {
                lane_pick.insert(repo.clone(), (w.id, *n));
            }
            *n += 1;
        }
    }
    let mut quick_slot_given = quick_slot_busy;
    let reserve = i64::from(slots >= 3 && run_could_start);
    let busy = || {
        let mut list: Vec<&&QueueRow> = running.iter().collect();
        list.sort_by_key(|r| r.started_at.unwrap_or(now));
        list.iter()
            .map(|r| {
                let minutes = (now - r.started_at.unwrap_or(now)).max(0) / 60;
                format!("{} {} {minutes} m", r.issue.as_deref().unwrap_or(&r.w.project), r.w.kind)
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut first_of_repo: HashMap<String, i64> = HashMap::new();
    let mut ahead: Vec<i64> = Vec::new();
    // Lanes of each repo given to a request this pass, so a quick run and its repo's first never
    // both count the same lane.
    let mut taken: HashMap<String, i64> = HashMap::new();
    let mut out = HashMap::new();
    for w in ordered {
        let repo = repo_of(&w);
        if let Some((_, n)) = lane_pick.get(&repo).copied().filter(|(id, _)| *id == w.id) {
            // Its own lane: a free slot of the machine's, past what is kept for a land, or the one
            // slot beyond them that only a quick run takes.
            let ahead_of = if n == 0 { String::new() } else { format!(", ahead of {n} {} of its repo", if n == 1 { "request" } else { "requests" }) };
            if (ahead.len() as i64) < free_slots - kept_slots {
                ahead.push(w.id);
                out.insert(w.id, Turn::QuickLane(format!("quick run: on its repo's quick lane{ahead_of}")));
            } else if !quick_slot_given && (ahead.len() as i64) >= free_slots {
                // Only with no slot of the machine's free: one kept for a land would be the one
                // its lock takes.
                quick_slot_given = true;
                out.insert(w.id, Turn::QuickLane(format!("quick run: on its repo's quick lane and the machine's quick slot{ahead_of}")));
            } else if (ahead.len() as i64) < free_slots {
                out.insert(w.id, Turn::Wait(format!("quick run{ahead_of}, waiting: a machine slot is kept for the land in progress")));
            } else {
                out.insert(w.id, Turn::Wait(format!("quick run{ahead_of}, waiting: {} of {slots} machine slots and the quick slot in use ({})", running.len(), busy())));
            }
            continue;
        }
        if let Some(land) = behind_land(&w, &repo) {
            out.insert(w.id, Turn::Wait(format!("waiting: behind request {land} of the same repo")));
            continue;
        }
        let quick = quick_pick.get(&repo).copied().filter(|(id, _)| *id == w.id);
        if quick.is_none() {
            if let Some(first) = first_of_repo.get(&repo) {
                out.insert(w.id, Turn::Wait(format!("waiting: behind request {first} of the same repo")));
                continue;
            }
            first_of_repo.insert(repo.clone(), w.id);
            // The repo's last free lane goes to its quick run first (NIT-347).
            if let Some((q, _)) = quick_pick.get(&repo) {
                if lanes_for(&repo, &w.kind) - taken.get(&repo).copied().unwrap_or(0) <= 1 {
                    out.insert(w.id, Turn::Wait(format!("waiting: its repo's free lane goes to quick run {q} first")));
                    continue;
                }
            }
        }
        // A land takes any free slot; a run leaves one for a land that is coming; a gate leaves one
        // for each, a run that could start and a land.
        let (for_run, for_land) = match w.kind.as_str() {
            "land" => (0, 0),
            "run" => (0, kept_slots),
            _ => (reserve, kept_slots),
        };
        let usable = free_slots - for_run - for_land;
        let turn = if (ahead.len() as i64) < usable {
            if lanes_for(&repo, &w.kind) - taken.get(&repo).copied().unwrap_or(0) > 0 {
                match quick {
                    Some((_, n)) => Turn::QuickGo(format!("quick run: ahead of {n} {} of its repo", if n == 1 { "request" } else { "requests" })),
                    None => Turn::Go,
                }
            } else if w.kind != "land" && keeps_for_land(&repo) {
                // Not "lanes busy", which lets it race for the next lane to free: that one is the
                // land's.
                Turn::Wait("waiting: a lane of its repo is kept for the land in progress".to_string())
            } else {
                Turn::LanesBusy(format!("waiting: all {} of its repo's lanes are busy", lanes_by_repo.get(&repo).copied().unwrap_or(1)))
            }
        } else if free_slots == 0 {
            Turn::Wait(format!("waiting: {} of {slots} machine slots in use ({})", running.len(), busy()))
        } else if (ahead.len() as i64) < free_slots - for_run {
            Turn::Wait("waiting: a machine slot is kept for the land in progress".to_string())
        } else if (ahead.len() as i64) < free_slots {
            Turn::Wait("waiting: the last free machine slot is kept for an agent's run".to_string())
        } else {
            let ids = ahead.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
            Turn::Wait(format!("waiting: the free machine slots go to requests ahead ({ids})"))
        };
        let turn = match (quick, turn) {
            // A quick run that may not go yet still says what it is.
            (Some((_, n)), Turn::Wait(why)) => Turn::Wait(format!("quick run, ahead of {n} of its repo, {why}")),
            (Some((_, n)), Turn::LanesBusy(why)) => Turn::LanesBusy(format!("quick run, ahead of {n} of its repo, {why}")),
            (_, turn) => turn,
        };
        // A quick run first in its repo, a lane free and every machine slot taken: the machine's
        // quick slot, on its repo's lane (NIT-347).
        let turn = match turn {
            Turn::Wait(_)
                if quick_of.contains_key(&w.id)
                    && !quick_slot_given
                    && (ahead.len() as i64) >= free_slots
                    && can_start(&repo, &w.kind)
                    && lanes_for(&repo, &w.kind) - taken.get(&repo).copied().unwrap_or(0) > 0 =>
            {
                quick_slot_given = true;
                *taken.entry(repo.clone()).or_default() += 1;
                out.insert(w.id, Turn::QuickGo("quick run: on the machine's quick slot".to_string()));
                continue;
            }
            turn => turn,
        };
        if matches!(turn, Turn::Go | Turn::QuickGo(_)) && can_start(&repo, &w.kind) {
            ahead.push(w.id);
            *taken.entry(repo.clone()).or_default() += 1;
        }
        out.insert(w.id, turn);
    }
    out
}

/// The queue as `turns` reads it, with each requester's liveness from `alive` (the asking
/// request's own counts as alive), and when each project last had a turn.
#[allow(clippy::type_complexity)]
fn queue_rows(db: &Db, me: Option<i64>, alive: &mut dyn FnMut(&Requester) -> bool) -> Result<(Vec<QueueRow>, std::collections::HashMap<String, i64>)> {
    let mut stmt = db.conn.prepare(
        "SELECT id, project, repo, priority, kind, queued_at, state, pid, lane_count, waiting_for IS NOT NULL, issue_key, started_at, pid_started, polled_at, scheduler, command,
         lane, quick_lane, quick_slot
         FROM lane_run WHERE state IN ('queued', 'running') AND executor IN ('local', 'worktree')",
    )?;
    type Raw = (Waiting, String, Requester, i64, bool, Option<String>, Option<i64>, Option<i64>, Option<i64>, String, (Option<i64>, bool, bool));
    let raw: Vec<Raw> = stmt
        .query_map([], |r| {
            Ok((
                Waiting { id: r.get(0)?, project: r.get(1)?, repo: r.get(2)?, priority: r.get(3)?, kind: r.get(4)?, queued_at: r.get(5)? },
                r.get(6)?,
                Requester { pid: r.get(7)?, started: r.get(12)? },
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(13)?,
                r.get(14)?,
                r.get(15)?,
                (r.get(16)?, r.get(17)?, r.get(18)?),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let now = now();
    // Each run's quick class, once per command (NIT-347).
    let mut classes: std::collections::HashMap<(String, String), Option<i64>> = std::collections::HashMap::new();
    for (w, .., command, _) in &raw {
        if w.kind == "run" && !classes.contains_key(&(w.repo.clone(), command.clone())) {
            let class = quick_class(db, &w.repo, command)?;
            classes.insert((w.repo.clone(), command.clone()), class);
        }
    }
    let rows = raw
        .into_iter()
        .map(|(w, state, requester, lane_count, waits_for_checkout, issue, started_at, polled_at, scheduler, command, (lane, quick_lane, quick_slot))| {
            let quick = if w.kind == "run" { classes.get(&(w.repo.clone(), command)).copied().flatten() } else { None };
            // A queued request whose requester has stopped looking for its turn (hung, suspended)
            // would never take it: it holds nobody up until it looks again (NIT-270).
            let stopped_looking = state == "queued" && polled_at.is_some_and(|t| now - t > STOPPED_LOOKING_SECS);
            // One an older nitely queued orders the queue its own way: the two orders can each put
            // the other's request first, and then neither goes. It keeps its own place in its own
            // view, but holds no newer request up (NIT-325).
            let older = state == "queued" && scheduler.is_none_or(|v| v < SCHEDULER);
            let requester_runs = requester.pid.is_none_or(|_| alive(&requester));
            let is_alive = Some(w.id) == me || (requester_runs && !stopped_looking && !older);
            // Older, as `turns` reads it for a land in progress: only while its requester still
            // runs. A dead older land's row, not settled yet, would otherwise cost the next,
            // current land the lane kept for it (NIT-342).
            let older = older && requester_runs;
            let running = state == "running";
            QueueRow { w, running, alive: is_alive, lane_count, waits_for_checkout, issue, started_at, older, quick, lane: lane.filter(|_| running), quick_lane, quick_slot: quick_slot && running }
        })
        .collect();
    // When each project last had a turn (a request of it started), so the one served longest ago
    // goes first and one project's backlog never holds up the rest.
    let mut served: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut stmt = db.conn.prepare("SELECT project, MAX(started_at) FROM lane_run WHERE started_at IS NOT NULL GROUP BY project")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (project, at) = row?;
        let slot = served.entry(project.to_uppercase()).or_insert(at);
        *slot = (*slot).max(at);
    }
    Ok((rows, served))
}

/// The scheduler this nitely queues under, recorded on each request. Raised whenever the queue's
/// order changes (NIT-291 added aging), so a newer waiter can tell a request an older binary waits
/// for: on 2026-10-18, after an install, an old binary's run and a newer land each put the other
/// first, and NIT's queue stood still for 18 minutes (NIT-325).
const SCHEDULER: i64 = 3;

/// How long a queued request's requester may go without looking for its turn before the requests
/// behind it stop waiting for it. A requester looks every fraction of a second, two seconds at most.
const STOPPED_LOOKING_SECS: i64 = 60;

/// Whether request `id` may try for a slot and a lane now (`turns`). Not queued (any more):
/// nothing to wait for.
fn my_turn(db: &Db, id: i64, slots: u32, alive: &mut dyn FnMut(&Requester) -> bool) -> Result<bool> {
    Ok(my_turn_is(db, id, slots, alive)?.is_some())
}

/// `my_turn`, with the turn when it may try: whether it goes on its repo's quick lane (NIT-347).
fn my_turn_is(db: &Db, id: i64, slots: u32, alive: &mut dyn FnMut(&Requester) -> bool) -> Result<Option<Turn>> {
    // A request taken out of the queue (`nitely lanes cancel`, or abandoned) does not run, however
    // long its requester goes on looking.
    let (state, error): (String, Option<String>) = db.conn.query_row("SELECT state, error FROM lane_run WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if state != "queued" {
        bail!("run {id} was taken out of the queue ({})", error.unwrap_or(state));
    }
    // Looking is what keeps a queued request's place (NIT-270).
    db.conn.execute("UPDATE lane_run SET polled_at = ?2 WHERE id = ?1", params![id, now()])?;
    let (rows, served) = queue_rows(db, Some(id), alive)?;
    let landing = landing_repos(db, &rows)?;
    Ok(match turns(rows, slots, &served, now(), &landing).remove(&id) {
        Some(Turn::Wait(_)) => None,
        Some(turn) => Some(turn),
        None => Some(Turn::Go),
    })
}

/// The repos (normalized) whose land lock a live land holds (NIT-332): of those in the queue, and
/// of every repo that has gated a land in a lane, so a land still rebasing, with nothing of its
/// repo queued yet, keeps its slot too.
fn landing_repos(db: &Db, rows: &[QueueRow]) -> Result<std::collections::HashSet<String>> {
    let mut repos: std::collections::HashSet<String> = rows.iter().map(|r| r.w.repo.clone()).collect();
    let mut stmt = db.conn.prepare("SELECT DISTINCT repo FROM lane_run WHERE kind = 'land'")?;
    for repo in stmt.query_map([], |r| r.get::<_, String>(0))? {
        repos.insert(repo?);
    }
    Ok(repos.into_iter().filter(|repo| crate::land::held_by_live_land(Path::new(repo))).map(|r| crate::paths::normalize(&r)).collect())
}

/// Why each queued request waits, for `nitely lanes` and the app (NIT-291): the machine's slots,
/// its own repo's queue or lanes, the person's checkout, or memory. Reads only.
fn waiting_reasons(db: &Db) -> Result<std::collections::HashMap<i64, String>> {
    let (rows, served) = queue_rows(db, None, &mut |r: &Requester| !r.gone())?;
    let landing = landing_repos(db, &rows)?;
    Ok(turns(rows, machine_budget().slots, &served, now(), &landing)
        .into_iter()
        .filter_map(|(id, t)| match t {
            Turn::Go => None,
            Turn::QuickGo(why) | Turn::QuickLane(why) | Turn::LanesBusy(why) | Turn::Wait(why) => Some((id, why)),
        })
        .collect())
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
    quick: bool,
    slots: &mut dyn FnMut() -> u32,
    memory: &mut dyn FnMut() -> Option<machine::Memory>,
    limits: &mut dyn FnMut() -> Option<machine::Limits>,
    yolo_keys: &[&str],
) -> Result<(u32, LaneLock, SlotLock)> {
    db.conn.execute("UPDATE lane_run SET lane_count = ?2, quick_lane = ?3 WHERE id = ?1", params![id, rules.lanes(), rules.quick_lane()])?;
    // The lanes it may take: the repo's, or for a quick run the scheduler sends there, its quick
    // lane alone, so it leaves the repo's lanes to the queue (NIT-347).
    let repo_lanes: Vec<u32> = (1..=rules.lanes()).collect();
    // Whether a requester's process still runs (asked through settle's short cache).
    let mut alive = |r: &Requester| !r.gone();
    let mut busy_since = None;
    let mut waits_for_memory = false;
    'turn: loop {
        crate::project_yolo::check_queued_gate(db, id, yolo_keys)?;
        crate::project_modes::check_playbook_authority(db,yolo_keys,pb)?;
        // The budget read again on every look, so a change in Settings counts at once (NIT-291).
        let slots = slots();
        if let Some(turn) = my_turn_is(db, id, slots, &mut alive)? {
            let on_quick_lane = quick && rules.quick_lane() && matches!(turn, Turn::QuickLane(_));
            let lanes = if on_quick_lane { vec![QUICK_LANE] } else { repo_lanes.clone() };
            // A checkout a request has (or a dead one left) is looked at under the lock, where one
            // left behind is put back first.
            if rules.large_repo && lent_to_request(repo_root).is_none() {
                if let Some(why) = checkout_busy(repo_root, pb, &rules.keep)? {
                    // Waiting for the checkout now, not memory (NIT-368).
                    forget_memory_wait(db, id, &mut waits_for_memory)?;
                    wait_for_checkout(db, id, &why, &mut busy_since, rules)?;
                    continue 'turn;
                }
            }
            // A machine slot, or for a quick run the one slot beyond the budget (NIT-347).
            let mut slot = None;
            for k in 1..=slots.max(1) {
                if let Some(s) = try_slot(db, k)? {
                    slot = Some(s);
                    break;
                }
            }
            let quick_slot = slot.is_none() && quick;
            if quick_slot {
                slot = try_slot_named(db, QUICK_SLOT)?;
            }
            if let Some(slot) = slot {
                for &n in &lanes {
                    if let Some(lock) = try_lock(repo_root, pb, n)? {
                        // The person's checkout: checked again under the lock, right before use.
                        if rules.large_repo {
                            recover_large(repo_root, &rules.keep);
                            if let Some(why) = checkout_busy(repo_root, pb, &rules.keep)? {
                                drop(lock);
                                drop(slot);
                                forget_memory_wait(db, id, &mut waits_for_memory)?;
                                wait_for_checkout(db, id, &why, &mut busy_since, rules)?;
                                continue 'turn;
                            }
                            db.conn.execute("UPDATE lane_run SET waiting_for = NULL WHERE id = ?1", [id])?;
                        }
                        let limits = limits();
                        let why = admit_run(db, id, limits.as_ref(), slots, quick_slot, memory)?;
                        if let Some(why) = why {
                            // Held while it waits, the slot and lane would keep the rest waiting too.
                            drop(lock);
                            drop(slot);
                            db.conn.execute("UPDATE lane_run SET waiting_memory = ?2 WHERE id = ?1", params![id, why])?;
                            if !waits_for_memory { crate::performance_events::lane(db, id, crate::performance_events::Kind::WaitingForMemory); }
                            waits_for_memory = true;
                            std::thread::sleep(Duration::from_secs(2));
                            continue 'turn;
                        }
                        if waits_for_memory {
                            db.conn.execute("UPDATE lane_run SET waiting_memory = NULL WHERE id = ?1", [id])?;
                            crate::performance_events::lane(db, id, crate::performance_events::Kind::MemoryWaitEnded);
                        }
                        db.conn.execute("UPDATE lane_run SET quick_slot = ?2 WHERE id = ?1", params![id, quick_slot])?;
                        return Ok((n, lock, slot));
                    }
                }
            }
        }
        // This look did not reach admission: what holds it now is its turn, a slot or a lane, so
        // an earlier memory reason would say the wrong thing (NIT-360).
        forget_memory_wait(db, id, &mut waits_for_memory)?;
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Clears request `id`'s memory reason (`waiting_memory`) once it waits for something else.
fn forget_memory_wait(db: &Db, id: i64, waits_for_memory: &mut bool) -> Result<()> {
    if std::mem::take(waits_for_memory) {
        db.conn.execute("UPDATE lane_run SET waiting_memory = NULL WHERE id = ?1", [id])?;
        crate::performance_events::lane(db, id, crate::performance_events::Kind::MemoryWaitEnded);
    }
    Ok(())
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
        crate::performance_events::lane(self.db, self.id, crate::performance_events::Kind::Ended);
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
            crate::performance_events::lane(self.db, self.id, crate::performance_events::Kind::Ended);
        }
    }
}

/// Waits for a machine slot and the machine's memory for a gate in `worktree` outside lanes, and
/// holds the slot until the turn ends (`MachineTurn`).
#[allow(clippy::too_many_arguments)]
pub fn hold_machine<'a>(db: &'a Db, repo_root: &Path, worktree: &Path, project: &str, issue: Option<&str>, kind: Kind, command: &str, actor: &Actor) -> Result<MachineTurn<'a>> {
    hold_machine_checked(db, repo_root, worktree, project, issue, kind, command, actor, &[])
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn hold_machine_checked<'a>(db: &'a Db, repo_root: &Path, worktree: &Path, project: &str, issue: Option<&str>, kind: Kind, command: &str, actor: &Actor, yolo_keys: &[&str]) -> Result<MachineTurn<'a>> {
    let rev = |what: &str| git(worktree, &["rev-parse", what]).map(|s| s.trim().to_string()).unwrap_or_default();
    let (head, tree) = (rev("HEAD"), rev("HEAD^{tree}"));
    let priority = match kind {
        Kind::Land => 1,
        Kind::Gate => 2,
        Kind::Run => 3,
    };
    let mut slots_now = slots_now();
    let slots = slots_now();
    db.conn.execute(
        "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, pid_started, polled_at, scheduler)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8, 'queued', 'worktree', ?9, ?10, ?11, ?12, ?13, ?9, ?14)",
        params![project.to_uppercase(), repo_root.to_string_lossy(), issue, actor.id(), kind.as_str(), head, tree, command, now(), std::process::id(), priority, slots, crate::procs::me().1, SCHEDULER],
    )?;
    let id = db.conn.last_insert_rowid();
    let authorities=yolo_keys.iter().map(|key|crate::project_modes::authority_for_issue(db,key)).collect::<Result<Vec<_>>>()?;
    let mut waits_for_memory = false;
    let taken = (|| -> Result<SlotLock> {
        let mut alive = |r: &Requester| !r.gone();
        loop {
            crate::project_yolo::check_queued_gate(db, id, yolo_keys)?;
            for (key,authority) in yolo_keys.iter().zip(&authorities) {if &crate::project_modes::authority_for_issue(db,key)?!=authority {bail!("project review mode changed while waiting for memory; submit again");}}
            let slots = slots_now();
            if my_turn(db, id, slots, &mut alive)? {
                if let Some(slot) = (1..=slots.max(1)).find_map(|k| try_slot(db, k).ok().flatten()) {
                    // The limits read again on every look, so a change in Settings counts at once.
                    let limits = memory_limits();
                    let why = admit_run(db, id, limits.as_ref(), slots, false, &mut read_memory)?;
                    match why {
                        None => return Ok(slot),
                        Some(why) => {
                            drop(slot);
                            db.conn.execute("UPDATE lane_run SET waiting_memory = ?2 WHERE id = ?1", params![id, why])?;
                            if !waits_for_memory { crate::performance_events::lane(db, id, crate::performance_events::Kind::WaitingForMemory); }
                            waits_for_memory = true;
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    }
                }
            }
            // Waiting for its turn or a slot now, not for memory (NIT-360).
            forget_memory_wait(db, id, &mut waits_for_memory)?;
            std::thread::sleep(Duration::from_millis(300));
        }
    })();
    let slot = match taken {
        Ok(slot) => slot,
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
    let clear = if waits_for_memory { ", waiting_memory = NULL" } else { "" };
    if db.conn.execute(
        &format!("UPDATE lane_run SET state = 'running', started_at = ?2, memory_cap = COALESCE(memory_cap, ?3){clear} WHERE id = ?1 AND state = 'queued'"),
        params![id, now(), run_cap().map(|c| c as i64)],
    )? == 0
    {
        bail!("run {id} was taken out of the queue before it started");
    }
    if waits_for_memory { crate::performance_events::lane(db, id, crate::performance_events::Kind::MemoryWaitEnded); }
    crate::performance_events::lane(db, id, crate::performance_events::Kind::Started);
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
#[allow(clippy::too_many_arguments)]
fn serve(repo_root: &Path, pb: &Playbook, rules: &LaneRules, lane: &Path, log: &Path, req: &Request, share: Share, exec: &mut Exec) -> Result<Outcome> {
    let base = pb.integration_branch();
    if git(repo_root, &["cat-file", "-e", &format!("{}^{{commit}}", req.snapshot.commit)]).is_err() {
        bail!("snapshot {} is missing from the repo (expired or cleaned up); make a new one", &req.snapshot.commit[..req.snapshot.commit.len().min(9)]);
    }
    let limit = Duration::from_secs(rules.timeout_minutes.max(1) * 60);
    if rules.large_repo {
        // The person's checkout: no forced checkout, no clean, nothing else stopped; and back on
        // its branch afterwards, whatever came of the command.
        stop_recorded(lane);
        crate::submodules::check_large(lane, pb, &req.snapshot.commit)?;
        let switched = git(lane, &["diff", "--name-only", "HEAD", &req.snapshot.commit]).map(|o| o.lines().count() as i64).unwrap_or(0);
        switch_large(lane, base, &req.snapshot.commit, &rules.keep)?;
        let ran = (|| -> Result<(bool, Ended)> {
            // After what a dead request left running was stopped, so its files can go.
            let temp = lane_temp(repo_root, pb, lane);
            let setup_ran = ensure_setup(repo_root, pb, lane, base, log, limit, &temp, share)?;
            Ok((setup_ran, exec(lane, log, limit, &temp, share)?))
        })();
        let back = return_large(lane, base);
        let (setup_ran, ended) = ran?;
        back.with_context(|| format!("the command ran (exit {}), but", ended.code.map_or("none".to_string(), |c| c.to_string())))?;
        let output = std::fs::read_to_string(log).map(|t| playbook::tail(&t)).unwrap_or_default();
        return Ok(Outcome { exit_code: ended.code, timed_out: ended.timed_out, peak: ended.peak, setup_ran, files_switched: switched, output });
    }
    let files_switched = if !lane.join(".git").exists() {
        create_lane(repo_root, lane, &req.snapshot.commit)?;
        seed_lane_caches(repo_root, pb, rules, lane, log)?;
        git(lane, &["ls-files"]).map(|o| o.lines().count() as i64).unwrap_or(0)
    } else {
        stop_leftovers(lane);
        let switched = git(lane, &["diff", "--name-only", "HEAD", &req.snapshot.commit]).map(|o| o.lines().count() as i64).unwrap_or(0);
        switch(lane, &req.snapshot.commit, &rules.keep)?;
        switched
    };
    crate::submodules::sync_lane(repo_root, pb, lane, &req.snapshot.commit, &rules.keep)?;
    // After what a dead request left running was stopped, so its files can go.
    let temp = lane_temp(repo_root, pb, lane);
    let setup_ran = ensure_setup(repo_root, pb, lane, base, log, limit, &temp, share)?;
    let Ended { code: exit_code, timed_out, peak } = exec(lane, log, limit, &temp, share)?;
    let output = std::fs::read_to_string(log).map(|t| playbook::tail(&t)).unwrap_or_default();
    // Between requests, still holding the lane: best effort, it never fails the request.
    if let Some(record) = git_path(lane, SIZE_RECORD) {
        let subs: Vec<String> = pb.submodules.iter().map(|m| m.path().to_string()).collect();
        hygiene(lane, rules, &subs, &record, log);
    }
    Ok(Outcome { exit_code, timed_out, peak, setup_ran, files_switched, output })
}

/// The file in a lane's git folder that keeps the size of its build directories after its last
/// request, its baseline, and how many requests it has served: "<bytes> <baseline bytes>
/// <requests>". A record from before NIT-231 has no count, and its baseline was the first build's
/// whole size.
const SIZE_RECORD: &str = "nitely-lane-size";

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// How many requests a lane's baseline learns from: the largest build output among them, so a
/// small first request (a `cargo check` of one crate) does not set the cap for the full builds
/// after it (NIT-231).
const LEARN_REQUESTS: u64 = 10;

/// The smallest cap twice the baseline gives, so a small baseline never wipes a lane's warm build.
const MIN_CAP: u64 = 4 << 30;

/// A lane's size cap in bytes: the playbook's, else twice its baseline, and never below `floor`.
fn cap_bytes(rules: &LaneRules, baseline: u64, floor: u64) -> u64 {
    match rules.size_cap_gb {
        Some(gb) if gb > 0.0 => (gb * GB) as u64,
        _ => baseline.saturating_mul(2).max(floor),
    }
}

/// What a lane's size record says: (size after the last request, baseline, requests served), if
/// any; an old record without a count counts as one request.
fn read_size(record: &Path) -> Option<(u64, u64, u64)> {
    let text = std::fs::read_to_string(record).ok()?;
    let mut parts = text.split_whitespace().map(|p| p.parse::<u64>().ok());
    let (size, baseline) = (parts.next()??, parts.next()??);
    Some((size, baseline, parts.next().flatten().unwrap_or(1)))
}

/// Keeps a lane within its size cap (docs/build-lanes.md, "Lane hygiene"): over the cap, the
/// incremental caches in its build directories (`*/incremental`, cargo's) are emptied; over twice
/// the cap, its build directories go and the next request builds cold. The cap is twice the
/// lane's baseline, at least `MIN_CAP`: the largest build output (incremental caches aside, which
/// are what grows) over its first `LEARN_REQUESTS` requests. Records the size after, the baseline
/// and the count. Nothing it removes is anyone's work. Says what it did in the log.
fn hygiene(lane: &Path, rules: &LaneRules, subs: &[String], record: &Path, log: &Path) {
    hygiene_with(lane, rules, subs, record, log, MIN_CAP)
}

fn hygiene_with(lane: &Path, rules: &LaneRules, subs: &[String], record: &Path, log: &Path, floor: u64) {
    use std::io::Write;
    let dirs = lane_kept(lane, &rules.keep, subs);
    let measured = keep_size(lane, &dirs);
    let output = measured.saturating_sub(incremental_size(lane, &dirs));
    let (recorded, served) = read_size(record).map_or((0, 0), |(_, b, n)| (b, n));
    let served = served + 1;
    let baseline = if served <= LEARN_REQUESTS { recorded.max(output) } else { recorded };
    let cap = cap_bytes(rules, baseline, floor);
    let mut size = measured;
    let mut said = None;
    if cap > 0 && size > cap.saturating_mul(2) {
        for d in &dirs {
            std::fs::remove_dir_all(lane.join(d)).ok();
        }
        size = keep_size(lane, &dirs);
        said = Some(format!("over twice its size cap ({} of {}): removed its build directories; the next request builds cold", human(measured), human(cap)));
    } else if cap > 0 && size > cap {
        for d in incremental_dirs(lane, &dirs) {
            std::fs::remove_dir_all(d).ok();
        }
        size = keep_size(lane, &dirs);
        said = Some(format!("over its size cap ({} of {}): emptied the incremental caches, now {}", human(measured), human(cap), human(size)));
    }
    std::fs::write(record, format!("{size} {baseline} {served}")).ok();
    // Which build directories the lane keeps, for `nitely lanes` and Settings to show.
    let still: Vec<&String> = dirs.iter().filter(|d| lane.join(d).is_dir()).collect();
    std::fs::write(record.with_file_name(KEPT_RECORD), still.iter().map(|d| format!("{d}\n")).collect::<String>()).ok();
    if let (Some(said), Ok(mut f)) = (said, std::fs::OpenOptions::new().append(true).open(log)) {
        writeln!(f, "nitely: lane {said}").ok();
    }
}

/// A lane's build directories: its own, and those its submodules' checkouts (`subs`, paths from
/// the lane) hold for a name entry, whose caches count toward the cap as the lane's do.
fn lane_kept(lane: &Path, keep: &[String], subs: &[String]) -> Vec<String> {
    let mut dirs = kept_dirs(lane, keep);
    let names: Vec<String> = keep.iter().filter(|k| keep_anywhere(k).is_some()).cloned().collect();
    for sub in subs.iter().filter(|_| !names.is_empty()) {
        // Only a submodule checkout inside the lane: never through a link a snapshot put there.
        let Some(at) = keep_dir(lane, sub).filter(|d| d.is_dir()) else { continue };
        dirs.extend(kept_dirs(&at, &names).into_iter().map(|d| format!("{}/{d}", sub.trim_matches('/'))));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The file beside the size record that lists the build directories the lane keeps, one path from
/// the lane's root a line, as of its last request.
const KEPT_RECORD: &str = "nitely-lane-kept";

/// The incremental caches in a lane's build directories (`kept`, from `kept_dirs`): each
/// `<dir>/<profile>/incremental` (cargo's), links neither followed nor counted.
fn incremental_dirs(lane: &Path, kept: &[String]) -> Vec<PathBuf> {
    let mut dirs = vec![];
    for d in kept.iter().map(|k| lane.join(k)) {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let incremental = e.path().join("incremental");
            if !is_link(&e.path()) && incremental.is_dir() && !is_link(&incremental) {
                dirs.push(incremental);
            }
        }
    }
    dirs
}

/// The bytes in a lane's incremental caches.
fn incremental_size(lane: &Path, kept: &[String]) -> u64 {
    incremental_dirs(lane, kept).iter().map(|d| dir_size(d)).sum()
}

/// The bytes in a lane's build directories (`kept`, from `kept_dirs`), links (symlinks,
/// junctions) not followed.
fn keep_size(lane: &Path, kept: &[String]) -> u64 {
    kept.iter().map(|d| dir_size(&lane.join(d))).sum()
}

/// The bytes in `dir`, links (symlinks, junctions) not followed.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            _ if is_link(&e.path()) => 0,
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
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

/// A `[lanes] keep` entry as a name to match at any depth, when it is one: an entry without a slash
/// (a trailing one aside) is a directory name, as in a .gitignore, so `node_modules` keeps the
/// root's and every package's (NIT-268); one with a slash (`/node_modules`, `web/dist`) is that path
/// from the repo's root.
pub(crate) fn keep_anywhere(keep: &str) -> Option<&str> {
    let k = keep.trim().trim_end_matches('/');
    (!k.is_empty() && !k.contains('/') && !k.contains('\\') && k != "." && k != "..").then_some(k)
}

/// The directories `keep` names in the checkout at `root`, as paths from it: each path entry that
/// exists, and every directory a name entry names at any depth. Never through a link, never inside
/// `.git` or inside a directory already found, and never one git tracks files in (a source folder
/// that happens to be called `target`).
pub(crate) fn kept_dirs(root: &Path, keep: &[String]) -> Vec<String> {
    let names: Vec<&str> = keep.iter().filter_map(|k| keep_anywhere(k)).collect();
    let mut found: Vec<String> = keep
        .iter()
        .filter(|k| keep_anywhere(k).is_none())
        .filter_map(|k| keep_dir(root, k).filter(|d| d.is_dir()).map(|_| k.trim_matches('/').to_string()))
        .collect();
    if !names.is_empty() {
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                if name == ".git" || is_link(&e.path()) || !e.file_type().is_ok_and(|t| t.is_dir()) || found.contains(&child) {
                    continue;
                }
                // Another repository inside this one (a nested clone, a worktree, a submodule): its
                // files are not this repo's to judge, so nothing in it is taken for build output.
                if e.path().join(".git").exists() {
                    continue;
                }
                if names.contains(&name.as_str()) {
                    let tracked = git(root, &["ls-files", "--", &child]).is_ok_and(|t| !t.trim().is_empty());
                    if !tracked {
                        found.push(child);
                        continue;
                    }
                    // Source that shares the name: not build output, but it may hold some deeper.
                }
                stack.push(child);
            }
        }
    }
    found.sort();
    found.dedup();
    found
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
fn seed_lane_caches(repo_root: &Path, pb: &Playbook, rules: &LaneRules, lane: &Path, log: &Path) -> Result<()> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) { return Ok(()); }
    let subs: Vec<String> = pb.submodules.iter().map(|m| m.path().to_string()).collect();
    for n in lane_numbers(rules) {
        let donor = lane_path(repo_root, pb, n);
        if donor == lane || !donor.join(".git").is_file() { continue; }
        // Never wait while holding the destination lane lock; busy donors simply mean a cold start.
        let Ok(Some(_donor_lock)) = try_lock(repo_root, pb, n) else { continue; };
        if pid_file(&donor).is_some_and(|p| p.exists()) { continue; }
        let began = Instant::now();
        let mut cloned = 0;
        for relative in lane_kept(&donor, &rules.keep, &subs) {
            if began.elapsed() > Duration::from_secs(30) { break; }
            // Only untracked cache directories. Source files and both worktrees' Git metadata stay independent.
            if !git(lane, &["ls-files", "--", &relative]).is_ok_and(|files| files.trim().is_empty()) { continue; }
            if crate::lane_clone::seed(&donor, lane, &relative)? { cloned += 1; }
        }
        use std::io::Write;
        if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = writeln!(out, "lane cache clone: {cloned} directories from lane {n} in {} ms; unsupported/linked caches start cold; setup is revalidated", began.elapsed().as_millis());
        }
        break;
    }
    Ok(())
}

/// Register a distinct Git worktree. Optional cache cloning never copies its index or setup stamp.
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
#[allow(clippy::too_many_arguments)]
fn ensure_setup(repo_root: &Path, pb: &Playbook, lane: &Path, base: &str, log: &Path, limit: Duration, temp: &Path, share: Share) -> Result<bool> {
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
    for command in &pb.worktree.setup {
        let command = playbook::expand(command, repo_root, lane, base).replace("{jobs}", &share.jobs.to_string());
        // Run again on failure as the gate's commands are (`[gate] retries`): a setup that fails
        // once, on a busy machine, then passes is not the change's fault.
        let mut tries = 0;
        loop {
            let ended = run_logged(&command, lane, log, limit, share, Some(temp))?;
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

/// Where the request a lane serves keeps its temporary files: `.nitely-tmp/<repo>-<lane>` in the
/// folder that holds the lanes, so the test repos, databases and copied programs a build makes
/// there are covered by whatever covers the lanes (a Defender exclusion, a Dev Drive) and never land
/// in the person's own temp folder (NIT-289). Not inside `.nitely-lanes`, where every path counts
/// as a lane's (`light::is_lane_path`), and not inside the lane's checkout, so nothing there is
/// inside a repo.
fn lane_temp_path(repo_root: &Path, pb: &Playbook, lane: &Path) -> PathBuf {
    let lanes = dir(repo_root, pb);
    let repo = lanes.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = lane.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "lane".into());
    let holder = lanes.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| lanes.clone());
    holder.join(".nitely-tmp").join(format!("{repo}-{name}"))
}

/// `lane_temp_path`, emptied for the request about to run there. Best effort: a folder that cannot
/// be emptied is used as it is.
fn lane_temp(repo_root: &Path, pb: &Playbook, lane: &Path) -> PathBuf {
    let temp = lane_temp_path(repo_root, pb, lane);
    std::fs::remove_dir_all(&temp).ok();
    std::fs::create_dir_all(temp.join(GIT_TEMPLATE)).ok();
    temp
}

/// The empty folder in a request's temporary folder that `git init` and `git clone` take their
/// template from there (`GIT_TEMPLATE_DIR`), so a test's repos get no hook samples to scan.
const GIT_TEMPLATE: &str = "git-template";

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
        // A name at any depth (`node_modules/`), a path from the root (`/web/dist`), as git's own
        // ignore patterns read them.
        for k in keep {
            args.push("-e".into());
            args.push(match keep_anywhere(k) {
                Some(name) => format!("{name}/"),
                None => format!("/{}", k.trim_matches('/')),
            });
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
fn run_logged(command: &str, dir: &Path, log: &Path, limit: Duration, share: Share, temp: Option<&Path>) -> Result<Ended> {
    use std::io::Write;
    let command = node_tests_share(command, share.jobs);
    let command = command.as_str();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "$ {command}")?;
    let mut cmd = playbook::shell_command(command);
    cmd.current_dir(dir).stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file.try_clone()?);
    // The machine's share for one request (NIT-199), for the build tools that read it.
    share_env(&mut cmd, share.jobs);
    // Its temporary files in the request's own folder, and git's templates empty (NIT-289).
    if let Some(temp) = temp {
        cmd.env("TMP", temp).env("TEMP", temp).env("TMPDIR", temp).env("GIT_TEMPLATE_DIR", temp.join(GIT_TEMPLATE));
        // A Nitely data folder of the request's own: a branch's nitely run here never opens, nor
        // upgrades, the person's database (NIT-330). NITELY_LANE_REAL_HOME=1 on the requester's
        // side keeps theirs (a command meant to read the real one, with an installed nitely).
        if std::env::var_os("NITELY_LANE_REAL_HOME").is_none() {
            cmd.env("NITELY_HOME", temp.join("nitely-home"));
        }
    }
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
    let tracked = crate::performance_attribution::track_child(&child, "build");
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
    if let Some(t) = tracked { t.finish(guard.as_ref().and_then(|g| g.accounting())); }
    // What the command left running ends before the lane is handed on (NIT-371).
    if let Some(g) = guard {
        g.end(Duration::from_secs(10));
    }
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
    /// Its number, `QUICK_LANE` (0) for the quick lane.
    pub lane: u32,
    /// The quick lane (NIT-347): only for quick runs, one at a time.
    #[serde(default)]
    pub quick: bool,
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
    /// The build directories it keeps, as paths from its root, as of its last request: what
    /// `[lanes] keep` matched (a name at any depth, a path from the root).
    #[serde(default)]
    pub kept: Vec<String>,
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
            let (run, requester) = row(r)?;
            if crate::paths::normalize(&run.repo) != mine {
                continue;
            }
            // A run whose process died counts as finished (abandoned), even where that cannot be
            // written back (the read-only preview); one still going is not the lane's last.
            let run = settle(self, run, requester);
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

/// A repo's waiting and running requests, oldest first, each queued one with why it waits: from
/// the database alone, no git (the app's Waiting list reads it, NIT-337).
fn queue_of(db: &Db, project: &str, root: &str) -> Result<Vec<Run>> {
    // The queue by this repo directly: a long build stays in it whatever finished since (NIT-223).
    let mut queue = db.open_lane_runs(project, root)?;
    // Why each waits: the person's checkout or memory where it said so, else where it stands in
    // the machine's queue (NIT-291).
    if queue.iter().any(|r| r.state == "queued") {
        let mut reasons = waiting_reasons(db)?;
        for r in queue.iter_mut().filter(|r| r.state == "queued") {
            r.waiting_why = r.waiting_for.clone().map(|w| format!("waiting: {w}")).or_else(|| r.waiting_memory.clone()).or_else(|| reasons.remove(&r.id));
        }
    }
    Ok(queue)
}

/// Every connected repo's lane queue with why each request waits, by project: what `overview`
/// shows of them, from the database alone (no playbook read, which can run git in large-repo
/// mode, and no lane's git or size). A repo without lanes has nothing queued.
pub fn queues(config: &crate::Config, db: &Db) -> Result<Vec<(String, Vec<Run>)>> {
    let mut out = Vec::new();
    for repo in &config.repos {
        let Some(project) = repo.project.as_deref() else { continue };
        let queue = queue_of(db, project, &repo.root)?;
        if !queue.is_empty() {
            out.push((project.to_string(), queue));
        }
    }
    Ok(out)
}

/// `status`; with `probe_locks`, a lane is busy when its lock is held (which opens the lock file),
/// otherwise when a running request names it.
fn status_with(db: &Db, repo_root: &Path, pb: &Playbook, project: &str, probe_locks: bool) -> Result<Option<Lanes>> {
    let Some(rules) = pb.lanes.clone() else { return Ok(None) };
    let root = repo_root.to_string_lossy();
    let queue = queue_of(db, project, &root)?;
    let mut lanes = Vec::new();
    for n in lane_numbers(&rules) {
        let path = lane_path(repo_root, pb, n);
        let made = path.join(".git").exists();
        let busy = if probe_locks { try_lock(repo_root, pb, n)?.is_none() } else { queue.iter().any(|r| r.state == "running" && r.lane == Some(n)) };
        let at = made.then(|| git(&path, &["rev-parse", "--short", "HEAD"]).ok().map(|s| s.trim().to_string())).flatten();
        let size = made.then(|| git_path(&path, SIZE_RECORD).and_then(|r| read_size(&r))).flatten();
        let (size_bytes, cap_bytes) = match size {
            Some((size, baseline, _)) => (Some(size), Some(cap_bytes(&rules, baseline, MIN_CAP))),
            None => (None, None),
        };
        let kept = made
            .then(|| git_path(&path, KEPT_RECORD).and_then(|r| std::fs::read_to_string(r).ok()))
            .flatten()
            .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        lanes.push(LaneState { lane: n, quick: n == QUICK_LANE, path: path.to_string_lossy().into_owned(), made, busy, at, size_bytes, cap_bytes, kept });
    }
    let recent = db.finished_lane_runs(project, &root, 10)?;
    // Every project's running requests, each read through `lane_run` so one whose process is gone
    // reads as abandoned and is not counted.
    let running_ids: Vec<i64> = db.conn.prepare("SELECT id FROM lane_run WHERE state = 'running' AND executor IN ('local', 'worktree')")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
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
/// How long `reset` waits for a lane's lock before it calls the lane busy.
const RESET_WAIT: Duration = Duration::from_secs(2);

pub fn reset(repo_root: &Path, pb: &Playbook) -> Result<Vec<String>> {
    let Some(rules) = pb.lanes.clone() else { bail!("{} has no [lanes] in its playbook", repo_root.display()) };
    let mut said = Vec::new();
    if rules.large_repo {
        // The person's checkout is never cleaned: only put back on its branch when a request that
        // died left it detached.
        let Some(_lock) = lock_waiting(repo_root, pb, 1, RESET_WAIT)? else {
            return Ok(vec!["your checkout: busy with a request, left alone".into()]);
        };
        said.push(recover_large(repo_root, &rules.keep).unwrap_or_else(|| "your checkout: nothing to do (it is never cleaned)".into()));
        return Ok(said);
    }
    for n in lane_numbers(&rules) {
        let path = lane_path(repo_root, pb, n);
        let name = if n == QUICK_LANE { "quick lane".to_string() } else { format!("lane {n}") };
        if !path.join(".git").exists() {
            said.push(format!("{name}: not made yet"));
            continue;
        }
        let Some(_lock) = lock_waiting(repo_root, pb, n, RESET_WAIT)? else {
            said.push(format!("{name}: busy with a request, left alone"));
            continue;
        };
        let head = git(&path, &["rev-parse", "HEAD"])?.trim().to_string();
        match switch(&path, &head, &rules.keep) {
            Ok(()) => said.push(format!("{name}: clean at {}", &head[..head.len().min(9)])),
            Err(e) => said.push(format!("{name}: {e:#}")),
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

    /// A monorepo's packages each install their own node_modules: a name in `keep` keeps them all,
    /// at any depth; a path keeps only that one (NIT-268).
    #[test]
    fn a_keep_name_matches_at_any_depth_and_a_path_only_from_the_root() {
        let base = crate::paths::unique_temp("nitely-keep-depth", "");
        let root = repo(&base);
        for pkg in ["pkg-a", "pkg-b/web"] {
            std::fs::create_dir_all(root.join(pkg)).unwrap();
            std::fs::write(root.join(pkg).join("package.json"), "{}").unwrap();
        }
        // A source folder that happens to share the name: tracked, so never build output.
        std::fs::create_dir_all(root.join("src/node_modules")).unwrap();
        std::fs::write(root.join("src/node_modules/vendored.js"), "x").unwrap();
        let g = |args: &[&str]| assert!(playbook::run_git(Command::new("git").arg("-C").arg(&root).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
        g(&["add", "-f", "."]);
        g(&["commit", "-q", "-m", "packages"]);
        let head = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let install = || {
            for dir in ["node_modules", "pkg-a/node_modules", "pkg-b/web/node_modules"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
                std::fs::write(root.join(dir).join("dep.js"), "y").unwrap();
            }
            std::fs::write(root.join("pkg-a/stray.txt"), "left by a request").unwrap();
        };

        install();
        let names = vec!["node_modules".to_string()];
        assert_eq!(kept_dirs(&root, &names), ["node_modules", "pkg-a/node_modules", "pkg-b/web/node_modules"], "every package's, not the tracked src/node_modules");
        switch(&root, &head, &names).unwrap();
        for dir in ["node_modules", "pkg-a/node_modules", "pkg-b/web/node_modules"] {
            assert!(root.join(dir).join("dep.js").exists(), "{dir} kept");
        }
        assert!(!root.join("pkg-a/stray.txt").exists() && root.join("src/node_modules/vendored.js").exists());

        // A path from the root keeps only that one.
        install();
        let path = vec!["/node_modules".to_string()];
        assert_eq!(kept_dirs(&root, &path), ["node_modules"]);
        switch(&root, &head, &path).unwrap();
        assert!(root.join("node_modules/dep.js").exists());
        assert!(!root.join("pkg-a/node_modules").exists() && !root.join("pkg-b/web/node_modules").exists());

        // Another repository inside this one is never read for build output: its `target` may be
        // its own source, and this repo's index cannot say.
        let nested = root.join("vendor/lib");
        std::fs::create_dir_all(nested.join("target")).unwrap();
        std::fs::write(nested.join("target/source.rs"), "theirs").unwrap();
        assert!(playbook::run_git(Command::new("git").arg("-C").arg(&nested).args(["init", "-q"])).unwrap().status.success());
        let targets = vec!["target".to_string()];
        assert!(!kept_dirs(&root, &targets).iter().any(|d| d.starts_with("vendor/")), "{:?}", kept_dirs(&root, &targets));
        std::fs::remove_dir_all(root.join("vendor")).unwrap();

        // In large-repo mode, a changed tracked file counts even in a folder named like a build
        // directory; only untracked files there are build output.
        std::fs::remove_dir_all(root.join("node_modules")).ok();
        g(&["checkout", "-q", "main"]);
        let lend = playbook_with("[lanes]\n");
        assert_eq!(checkout_busy(&root, &lend, &names).unwrap(), None, "clean, but for untracked build output");
        std::fs::create_dir_all(root.join("pkg-a/node_modules")).unwrap();
        std::fs::write(root.join("pkg-a/node_modules/dep.js"), "y").unwrap();
        assert_eq!(checkout_busy(&root, &lend, &names).unwrap(), None, "a package's node_modules is build output");
        std::fs::write(root.join("src/node_modules/vendored.js"), "changed").unwrap();
        assert!(checkout_busy(&root, &lend, &names).unwrap().is_some_and(|w| w.contains("src/node_modules/vendored.js")), "a tracked change counts");
        g(&["checkout", "--", "src/node_modules/vendored.js"]);

        // A snapshot adding a file deep in a kept folder (`assets/target/parser.rs`) still finds an
        // ignored file of the person's where it needs the folder `assets` on the way.
        let targets = vec!["target".to_string()];
        std::fs::create_dir_all(root.join("assets/target")).unwrap();
        std::fs::write(root.join("assets/target/parser.rs"), "x").unwrap();
        g(&["add", "-f", "assets"]);
        g(&["commit", "-q", "-m", "assets"]);
        let adds = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        g(&["reset", "-q", "--hard", "HEAD~1"]);
        std::fs::remove_dir_all(root.join("assets")).ok();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(root.join(".git").join("info").join("exclude"), "assets\n").unwrap();
        std::fs::write(root.join("assets"), "the person's").unwrap();
        assert_eq!(would_overwrite(&root, &adds, &targets).unwrap().as_deref(), Some("assets"));
        std::fs::remove_file(root.join("assets")).unwrap();
        // And where the kept folder's own name is an ignored file of the person's.
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join(".git").join("info").join("exclude"), "assets/target\n").unwrap();
        std::fs::write(root.join("assets/target"), "the person's").unwrap();
        assert_eq!(would_overwrite(&root, &adds, &targets).unwrap().as_deref(), Some("assets/target"));
        std::fs::remove_dir_all(root.join("assets")).unwrap();

        // A tracked folder that shares the name is source, but what it holds deeper is still found.
        std::fs::create_dir_all(root.join("src/node_modules/inner/node_modules")).unwrap();
        std::fs::write(root.join("src/node_modules/inner/node_modules/dep.js"), "y").unwrap();
        assert!(kept_dirs(&root, &names).contains(&"src/node_modules/inner/node_modules".to_string()), "{:?}", kept_dirs(&root, &names));
        std::fs::remove_dir_all(root.join("src/node_modules/inner")).unwrap();

        // A submodule checkout is read for a name entry's folders, never through a link.
        std::fs::create_dir_all(root.join("engine/target")).unwrap();
        std::fs::write(root.join("engine/target/out.o"), "o").unwrap();
        assert!(lane_kept(&root, &targets, &["engine".into()]).contains(&"engine/target".to_string()));
        std::fs::remove_dir_all(root.join("engine")).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("target")).unwrap();
        #[cfg(windows)]
        let linked = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(root.join("engine")).arg(&outside).output().unwrap().status.success();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, root.join("engine")).is_ok();
        assert!(linked, "made the link");
        assert!(!lane_kept(&root, &targets, &["engine".into()]).iter().any(|d| d.starts_with("engine")), "not through a link");
        #[cfg(windows)]
        std::fs::remove_dir(root.join("engine")).ok();
        #[cfg(unix)]
        std::fs::remove_file(root.join("engine")).ok();

        // The same meaning for what counts as build output in a person's checkout.
        assert!(in_keep(&names, "pkg-a/node_modules/dep.js") && in_keep(&names, "pkg-b/web/node_modules/"));
        assert!(!in_keep(&names, "src/node_modules") && !in_keep(&names, "docs/node_modules.md"), "a file of that name is not the directory");
        assert!(in_keep(&path, "node_modules/dep.js") && !in_keep(&path, "pkg-a/node_modules/dep.js"));
        assert_eq!((keep_anywhere("target/"), keep_anywhere("web/dist"), keep_anywhere("/target"), keep_anywhere("..")), (Some("target"), None, None, None));
        std::fs::remove_dir_all(base).ok();
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
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at, error, scheduler)
                     VALUES ('APP', ?1, 'claude', 'run', 's', 't', 'b', 'npm test', ?2, ?3, ?4, ?5, 3)",
                    params![root_s, state, lane, at, error],
                )
                .unwrap();
        };
        insert(2, "lane_error", 1, Some("the switch failed twice"));
        // A long build that started before everything below: still in the queue after 600 more.
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at, pid, scheduler)
                 VALUES ('APP', ?1, 'claude', 'gate', 's', 't', 'b', 'cargo test', 'running', 1, 2, ?2, 3)",
                params![root_s, std::process::id()],
            )
            .unwrap();
        // Another repo of the same project, busier than this one, crowds nothing out.
        for i in 0..80 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at, scheduler)
                     VALUES ('APP', 'C:/Code/other', 'claude', 'run', 's', 't', 'b', 'npm test', 'done', 1, ?1, 3)",
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
        let ended = run_logged(&format!("start \"\" /B {marker} >NUL & exit /b 0"), &base, &log, Duration::from_secs(30), NO_CAP, None).unwrap();
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
        // Without the floor, so a few bytes stand for gigabytes.
        let hygiene = |lane: &Path, rules: &LaneRules, record: &Path, log: &Path| hygiene_with(lane, rules, &[], record, log, 0);
        // The first request's build output sets the baseline, its incremental caches aside: a cap
        // of twice 600 bytes.
        fill("target/debug/app", 600);
        fill("target/debug/incremental/a", 400);
        fill("src/main.rs", 5_000);
        hygiene(&lane, &rules, &record, &log);
        assert_eq!(read_size(&record), Some((1_000, 600, 1)), "only build directories count");

        // Over the cap: the incremental caches go, the rest stays.
        fill("target/debug/incremental/b", 1_000);
        hygiene(&lane, &rules, &record, &log);
        assert!(!lane.join("target/debug/incremental").exists() && lane.join("target/debug/app").exists());
        assert_eq!(read_size(&record), Some((600, 600, 2)));

        // A full build after a small first request (NIT-231): within the first requests it raises
        // the baseline, and the warm build stays.
        fill("target/debug/deps/everything", 4_000);
        hygiene(&lane, &rules, &record, &log);
        assert!(lane.join("target/debug/deps/everything").exists(), "a bigger build is not wiped while the baseline learns");
        assert_eq!(read_size(&record), Some((4_600, 4_600, 3)));

        // Once it has learned, over twice the cap: the build directories go; sources never.
        for _ in 3..LEARN_REQUESTS {
            hygiene(&lane, &rules, &record, &log);
        }
        fill("target/release/big", 15_000);
        hygiene(&lane, &rules, &record, &log);
        assert!(!lane.join("target").exists() && lane.join("src/main.rs").exists());
        assert_eq!(read_size(&record), Some((0, 4_600, LEARN_REQUESTS + 1)), "the baseline is kept");
        let said = std::fs::read_to_string(&log).unwrap();
        assert!(said.contains("emptied the incremental caches") && said.contains("builds cold"), "{said}");

        // A record from before NIT-231 (no count) learns on from its first build's size.
        std::fs::write(&record, "700 666").unwrap();
        assert_eq!(read_size(&record), Some((700, 666, 1)));
        // And with the floor, a small baseline never wipes a lane: twice 666 bytes is under it.
        assert_eq!(cap_bytes(&rules, 666, MIN_CAP), MIN_CAP);
        assert_eq!(cap_bytes(&rules, 3 << 30, MIN_CAP), 6 << 30);

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

        // The playbook's cap wins over twice the baseline, and over the floor.
        assert_eq!(cap_bytes(&LaneRules { size_cap_gb: Some(1.0), ..Default::default() }, 10, MIN_CAP), 1 << 30);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn changed_paths_reads_porcelain_v2() {
        let status = ["1 .M N... 100644 100644 100644 abc abc src/a.txt", "? src/new file.txt", "2 R. N... 100644 100644 100644 abc abc R100 src/c.txt", "src/b.txt", ""].join("\0");
        assert_eq!(changed_paths(&status), ["src/a.txt", "src/b.txt", "src/c.txt", "src/new file.txt"]);
    }

    /// A snapshot never writes the worktree's own index, not even the refresh `git status` makes
    /// when it can, so one killed mid-run leaves no index.lock for the agent's next commit (NIT-341).
    #[test]
    fn a_snapshot_leaves_the_worktrees_index_alone_even_when_killed() {
        let base = crate::paths::unique_temp("nitely-snap-index", "");
        let root = repo(&base);
        for i in 0..300 {
            std::fs::write(root.join(format!("src/f{i}.txt")), format!("{i}\n")).unwrap();
        }
        git(&root, &["add", "."]).unwrap();
        git(&root, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "files"]).unwrap();
        let git_dir = PathBuf::from(git(&root, &["rev-parse", "--absolute-git-dir"]).unwrap().trim());
        let (index, lock) = (git_dir.join("index"), git_dir.join("index.lock"));
        // Every tracked file touched, none changed but one: a status that may refresh rewrites the index.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
        for i in 0..300 {
            std::fs::File::options().write(true).open(root.join(format!("src/f{i}.txt"))).unwrap().set_modified(later).unwrap();
        }
        std::fs::write(root.join("src/a.txt"), "one, changed\n").unwrap();
        let before = std::fs::read(&index).unwrap();
        // Killed at different points of the status a snapshot runs.
        for wait_ms in [0, 1, 3, 8, 20, 50] {
            let mut status = playbook::shell_free_command("git");
            status.arg("-C").arg(&root).args(["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=all"]);
            let mut child = status.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(wait_ms));
            child.kill().ok();
            child.wait().ok();
            assert!(!lock.exists(), "killed after {wait_ms} ms, it left index.lock");
        }
        let snap = snapshot(&root).unwrap();
        assert!(snap.made);
        assert!(!lock.exists());
        assert_eq!(std::fs::read(&index).unwrap(), before, "the worktree's index is as it was");
        std::fs::remove_dir_all(&base).ok();
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
        assert_eq!(lanes.lanes.len(), 2, "its lane, and its quick lane");
        assert!(lanes.lanes[0].made && !lanes.lanes[0].busy);
        assert_eq!(lanes.lanes[0].kept, ["out"], "the build directories it keeps");
        assert!(lanes.lanes[1].quick && !lanes.lanes[1].made, "made only for a quick run that would wait: {:?}", lanes.lanes[1]);
        assert!(lanes.queue.is_empty() && lanes.recent.len() == 3);
        let said = reset(&root, &pb).unwrap();
        assert!(said.len() == 2 && said[0].starts_with("lane 1: clean at") && said[1] == "quick lane: not made yet", "{said:?}");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_lock_held_for_a_moment_is_waited_out_and_one_held_on_is_busy() {
        let base = crate::paths::unique_temp("nitely-lane-wait", "");
        let root = repo(&base);
        let pb = playbook_with("[lanes]\n");
        // Let go while it waits: it gets the lock.
        let held = try_lock(&root, &pb, 1).unwrap().unwrap();
        let letting_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        assert!(lock_waiting(&root, &pb, 1, Duration::from_secs(5)).unwrap().is_some(), "waited out");
        letting_go.join().unwrap();
        // Held throughout: busy, after the wait.
        let _held = try_lock(&root, &pb, 1).unwrap().unwrap();
        let started = std::time::Instant::now();
        assert!(lock_waiting(&root, &pb, 1, Duration::from_millis(150)).unwrap().is_none());
        assert!(started.elapsed() >= Duration::from_millis(150));
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
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, scheduler) VALUES ('APP', ?1, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, 999999, 3)",
                [root.to_string_lossy()],
            )
            .unwrap();
let id = db.conn.last_insert_rowid();
        let gone = db.lane_run(id).unwrap().unwrap();
        assert_eq!((gone.state.as_str(), gone.error.as_deref()), ("abandoned", Some("its requester exited while it was queued")));

        // The OS gave a dead requester's number to another process (here this one, which started
        // at another time): not the requester, so the request is abandoned too (NIT-270).
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, pid_started, scheduler)
                 VALUES ('APP', ?1, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, ?2, 'another start', 3)",
                params![root.to_string_lossy(), std::process::id()],
            )
            .unwrap();
        let reused = db.conn.last_insert_rowid();
        assert_eq!(db.lane_run(reused).unwrap().unwrap().state, "abandoned");
        // The same process, started when it says: still waiting.
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, pid_started, scheduler)
                 VALUES ('APP', ?1, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, ?2, ?3, 3)",
                params![root.to_string_lossy(), std::process::id(), crate::procs::me().1],
            )
            .unwrap();
        let mine = db.conn.last_insert_rowid();
        assert_eq!(db.lane_run(mine).unwrap().unwrap().state, "queued");

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
        // 32 threads and 64 GB: 5 slots of 6 jobs (30 of 32 cores), 6 by the memory above the floor.
        let b = budget_for(32, Some(&gb(64)), None);
        assert_eq!((b.slots, b.jobs, b.from.as_str(), b.memory_gb), (5, 6, "machine", Some(64)));
        assert_eq!(budget_for(32, Some(&gb(24)), None).slots, 2, "memory caps it");
        assert_eq!((budget_for(4, Some(&gb(8)), None).slots, budget_for(4, Some(&gb(8)), None).jobs), (1, 3), "always one, and a core left over");
        assert_eq!(budget_for(8, None, None).slots, 1, "memory unknown: cores decide");
        let set = budget_for(32, Some(&gb(64)), Some((6, "settings")));
        assert_eq!((set.slots, set.jobs, set.from.as_str()), (6, 5, "settings"));
        assert_eq!(budget_for(32, Some(&gb(64)), Some((0, "settings"))).from, "machine", "0 is not a setting");
    }

    /// Every slot at its cap still leaves the floor free (the NIT-291 review): on 64 GB, 5 slots
    /// get 10.2 GB each rather than 16.
    #[test]
    fn a_runs_cap_fits_the_slots_above_the_floor() {
        let l = machine::limits_for(64 * machine::GIB, None, None);
        assert_eq!(cap_for(&l, 5), (64 * machine::GIB - l.floor) / 5);
        assert_eq!(cap_for(&l, 3), 16 * machine::GIB, "the set cap when the slots fit");
        assert!(cap_for(&l, 1000) >= machine::GIB / 4, "never below a quarter GB");
        assert!(5 * cap_for(&l, 5) <= l.total - l.floor);
    }

    /// Raising the slots while builds run starts no new one until the caps the running ones
    /// started with and its own fit above the floor (NIT-323).
    #[test]
    fn a_new_run_waits_while_the_running_runs_caps_fill_the_room() {
        let base = crate::paths::unique_temp("nitely-lane-caps", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let gib = machine::GIB as i64;
        let add = |state: &str, cap: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, memory_cap)
                     VALUES ('APP', 'C:/Code/app', 'x', 'gate', 's', 't', 'b', 'c', ?1, 'local', 1, ?2, 2, 3, ?3)",
                    params![state, me, cap],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let l = machine::limits_for(64 * machine::GIB, None, None); // 51.2 GB above the floor
        let waiting = add("queued", None);
        assert_eq!(caps_fit(&db, waiting, Some(&l), 3).unwrap(), None, "nothing running");
        for _ in 0..3 {
            add("running", Some(16 * gib));
        }
        // The slots raised to 6: 8.5 GB each, but three runs still hold 16 GB caps.
        let why = caps_fit(&db, waiting, Some(&l), 6).unwrap().unwrap();
        assert!(why.starts_with("waiting for memory: the running builds have 48.0 GB set aside"), "{why}");
        db.conn.execute("UPDATE lane_run SET state = 'done' WHERE memory_cap = ?1 AND id = (SELECT MIN(id) FROM lane_run WHERE memory_cap = ?1)", [16 * gib]).unwrap();
        assert_eq!(caps_fit(&db, waiting, Some(&l), 6).unwrap(), None, "one ended: 32 + 8.5 fit");
        let off = machine::limits_for(64 * machine::GIB, Some((0.0, "settings")), None);
        assert_eq!(caps_fit(&db, waiting, Some(&off), 6).unwrap(), None, "a floor of 0 turns it off");

        // A queued request that reserved its cap under the admission lock counts before it runs:
        // two requests never both take the same room (the NIT-323 review).
        let other = add("queued", None);
        reserve_cap(&db, other, Some(&l), 6).unwrap();
        add("queued", None); // waiting, nothing reserved: counts for nothing
        assert_eq!(caps_fit(&db, waiting, Some(&l), 6).unwrap(), None, "32 running, 8.5 reserved and 8.5 more fit in 51.2");
        let third = add("queued", None);
        reserve_cap(&db, third, Some(&l), 6).unwrap();
        let why = caps_fit(&db, waiting, Some(&l), 6).unwrap().unwrap();
        assert!(why.contains("have 49.1 GB set aside"), "32 GB running and two 8.5 GB reserved: {why}");

        // A run an older nitely started recorded no cap: it counts at the full one, 16 GB.
        db.conn.execute("DELETE FROM lane_run WHERE id != ?1", [waiting]).unwrap();
        for _ in 0..3 {
            add("running", None);
        }
        let why = caps_fit(&db, waiting, Some(&l), 6).unwrap().unwrap();
        assert!(why.contains("have 48.0 GB set aside"), "{why}");
        std::fs::remove_dir_all(base).ok();
    }

    /// What admission sets aside for a run is its command's expected peak, not its cap (NIT-363):
    /// the p90 of its last peaks plus a quarter, at least 2 GB, at most the cap; the cap with no
    /// history.
    #[test]
    fn a_run_reserves_its_commands_expected_peak() {
        let gib = machine::GIB as i64;
        let cap = 10 * machine::GIB;
        assert_eq!(expected_peak(&[], cap), cap, "no history: the whole cap");
        assert_eq!(expected_peak(&[4 * gib], cap), 5 * machine::GIB, "4 GB and a quarter");
        assert_eq!(expected_peak(&[gib / 2, gib], cap), 2 * machine::GIB, "at least 2 GB");
        assert_eq!(expected_peak(&[9 * gib, 9 * gib], cap), cap, "never past the cap it is held to");
        let peaks: Vec<i64> = (1..=10).map(|g| g * gib).collect();
        assert_eq!(expected_peak(&peaks, 20 * machine::GIB), 10 * machine::GIB + 10 * machine::GIB / 4, "the p90 of ten is the largest");
        let peaks: Vec<i64> = (1..=20).map(|g| g * gib / 4).collect();
        assert_eq!(expected_peak(&peaks, cap), 19 * machine::GIB / 4 + 19 * machine::GIB / 16, "of twenty, the 19th");

        // In admission: five gates whose command peaked near 4 GB leave room for a sixth run, where
        // their 10 GB caps would not; one that never ran sets its whole cap aside.
        let base = crate::paths::unique_temp("nitely-lane-reserved", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let add = |state: &str, command: &str, peak: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, peak_memory)
                     VALUES ('APP', 'C:/Code/app', 'x', 'gate', 's', 't', 'b', ?1, ?2, 'local', 1, ?3, 2, 3, ?4)",
                    params![command, state, me, peak],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        for _ in 0..5 {
            add("done", "cargo test --workspace", Some(4 * gib));
        }
        let l = machine::limits_for(64 * machine::GIB, None, None); // 51.2 GB above the floor, 10.2 GB a slot
        for _ in 0..5 {
            let id = add("queued", "cargo test --workspace", None);
            reserve_cap(&db, id, Some(&l), 5).unwrap();
            db.conn.execute("UPDATE lane_run SET state = 'running' WHERE id = ?1", [id]).unwrap();
        }
        let run = db.lane_run(db.conn.last_insert_rowid()).unwrap().unwrap();
        assert_eq!((run.memory_reserved, run.memory_cap), (Some(5 * gib), Some(cap_for(&l, 5) as i64)), "shown in `nitely lanes`: what is set aside, and the cap it is held to");
        let sixth = add("queued", "cargo test --workspace", None);
        assert_eq!(caps_fit(&db, sixth, Some(&l), 5).unwrap(), None, "25 GB set aside and 5 more fit in 51.2");
        let new = add("queued", "cargo build --release", None);
        assert_eq!(caps_fit(&db, new, Some(&l), 5).unwrap(), None, "25 + its whole 10.2 GB cap fit too");
        for _ in 0..2 {
            let id = add("queued", "cargo build --release", None);
            reserve_cap(&db, id, Some(&l), 5).unwrap();
        }
        let why = caps_fit(&db, new, Some(&l), 5).unwrap().unwrap();
        assert!(why.starts_with("waiting for memory: the running builds have 45.5 GB set aside and this one needs 10.2 GB"), "{why}");
        std::fs::remove_dir_all(base).ok();
    }

    /// The review's case (NIT-363): five running runs with no history set their whole caps aside,
    /// which fill the room above the floor. A run on a machine slot waits; a quick run on the quick
    /// slot starts while its expected peak is free above the floor, and waits once it is not.
    #[test]
    fn the_quick_slot_goes_by_the_memory_free_above_the_floor() {
        let base = crate::paths::unique_temp("nitely-lane-quick-slot", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let gib = machine::GIB as i64;
        let add = |state: &str, command: &str, peak: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, peak_memory)
                     VALUES ('APP', 'C:/Code/app', 'x', 'gate', 's', 't', 'b', ?1, ?2, 'local', 1, ?3, 2, 3, ?4)",
                    params![command, state, me, peak],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let l = machine::limits_for(64 * machine::GIB, None, None); // 12.8 GB floor, 10.2 GB a slot
        for n in 0..5 {
            let id = add("queued", &format!("cargo build -p new-{n}"), None);
            reserve_cap(&db, id, Some(&l), 5).unwrap();
            db.conn.execute("UPDATE lane_run SET state = 'running' WHERE id = ?1", [id]).unwrap();
        }
        for _ in 0..3 {
            add("done", "cargo test -p app waits", Some(gib));
        }
        let quick = add("queued", "cargo test -p app waits", None);
        let free = |gb: u64| move || Some(machine::Memory { total: 64 * machine::GIB, available: gb * machine::GIB });
        let why = admit_run(&db, quick, Some(&l), 5, false, &mut free(30)).unwrap().unwrap();
        assert!(why.contains("set aside"), "on a machine slot, the caps set aside fill the room: {why}");
        // Its last peaks were 1 GB: 2 GB expected. 30 GB free is plenty above the 12.8 GB floor.
        assert_eq!(admit_run(&db, quick, Some(&l), 5, true, &mut free(30)).unwrap(), None);
        assert_eq!(db.lane_run(quick).unwrap().unwrap().memory_reserved, Some(2 * gib), "and it sets its own aside");
        db.conn.execute("UPDATE lane_run SET memory_cap = NULL, memory_reserved = NULL WHERE id = ?1", [quick]).unwrap();
        std::fs::remove_file(base.join("lanes").join("last-start")).ok();
        let why = admit_run(&db, quick, Some(&l), 5, true, &mut free(14)).unwrap().unwrap();
        assert!(why.contains("expected to take 2.0 GB above the"), "14 GB free is under the floor plus its 2 GB: {why}");
        std::fs::remove_dir_all(base).ok();
    }

    /// A command with no history starts with nothing running though the machine's own programs
    /// leave less than the floor and a whole cap free (NIT-372): it needs the least a run sets
    /// aside free above the floor, and still sets its whole cap aside for the next runs to count.
    #[test]
    fn a_new_command_starts_on_a_machine_short_of_a_whole_cap() {
        let base = crate::paths::unique_temp("nitely-lane-new-command", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count)
                 VALUES ('APP', 'C:/Code/app', 'x', 'run', 's', 't', 'b', 'cargo test -p app new', 'queued', 'local', 1, ?1, 3, 3)",
                [std::process::id() as i64],
            )
            .unwrap();
        let id = db.conn.last_insert_rowid();
        let l = machine::limits_for(64 * machine::GIB, None, None); // 12.8 GB floor, 10.2 GB a slot
        let free = |gb: u64| move || Some(machine::Memory { total: 64 * machine::GIB, available: gb * machine::GIB });
        assert_eq!(admit_run(&db, id, Some(&l), 5, false, &mut free(22)).unwrap(), None, "22 GB free, under 12.8 + 10.2, nothing running");
        let run = db.lane_run(id).unwrap().unwrap();
        assert_eq!(run.memory_reserved, Some(cap_for(&l, 5) as i64), "its whole cap set aside");
        db.conn.execute("UPDATE lane_run SET memory_cap = NULL, memory_reserved = NULL WHERE id = ?1", [id]).unwrap();
        std::fs::remove_file(base.join("lanes").join("last-start")).ok();
        let why = admit_run(&db, id, Some(&l), 5, false, &mut free(14)).unwrap().unwrap();
        assert!(why.contains("expected to take 2.0 GB above the"), "the least a run sets aside: {why}");
        // A 4 GB machine: a 2 GB floor and a cap under 2 GB. 3.5 GB free is enough (NIT-374).
        db.conn.execute("UPDATE lane_run SET memory_cap = NULL, memory_reserved = NULL WHERE id = ?1", [id]).unwrap();
        std::fs::remove_file(base.join("lanes").join("last-start")).ok();
        let small = machine::limits_for(4 * machine::GIB, None, None);
        let mut half = || Some(machine::Memory { total: 4 * machine::GIB, available: 7 * machine::GIB / 2 });
        assert_eq!(admit_run(&db, id, Some(&small), 5, false, &mut half).unwrap(), None, "no more than its {} cap", machine::gb(cap_for(&small, 5)));
        std::fs::remove_dir_all(base).ok();
    }

    /// Checked and reserved under the one lock: the second request to ask sees the first's cap.
    #[test]
    fn admission_checks_and_reserves_under_one_lock() {
        let dir = crate::paths::unique_temp("nitely-admit-with", "");
        let l = machine::limits_for(64 * machine::GIB, None, None);
        let plenty = || Some(machine::Memory { total: 64 * machine::GIB, available: 50 * machine::GIB });
        let reserved = std::cell::Cell::new(0u64);
        let ask = || {
            machine::admit_once_with(
                &dir,
                Some(&l),
                0,
                &mut || plenty(),
                &mut || Ok((reserved.get() + 30 * machine::GIB > 51 * machine::GIB).then(|| "no room".to_string())),
                &mut || {
                    reserved.set(reserved.get() + 30 * machine::GIB);
                    Ok(())
                },
            )
            .unwrap()
        };
        assert_eq!(ask(), None, "the first takes its room");
        assert_eq!(ask().as_deref(), Some("no room"), "the second sees it taken");
        std::fs::remove_dir_all(dir).ok();
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

    /// A request that waited on memory and then waits for a slot says so, not "waiting for
    /// memory" (NIT-360): the memory reason goes once a look does not reach admission.
    #[test]
    fn a_memory_reason_goes_once_a_request_waits_for_a_slot() {
        let base = crate::paths::unique_temp("nitely-lane-memory-gone", "");
        let root = repo(&base);
        let db_path = base.join("t.db");
        let db = Db::open(&db_path).unwrap();
        let pb = playbook_with("[lanes]\n");
        let snap = snapshot(&root).unwrap();
        let mem = |free_gb: u64| Some(machine::Memory { total: 64 * machine::GIB, available: free_gb * machine::GIB });
        let mut first = true;
        let next: Readings = Box::new(move || if std::mem::take(&mut first) { mem(5) } else { mem(40) });
        FAKE_MEMORY.with(|f| *f.borrow_mut() = Some(next));
        // Another request takes every slot while this one waits on memory, and gives them back once
        // the queue stops saying memory.
        let path = db_path.clone();
        let other = std::thread::spawn(move || {
            let db = Db::open(&path).unwrap();
            let reason = || db.conn.query_row("SELECT waiting_memory, state FROM lane_run ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))).optional().unwrap();
            let started = Instant::now();
            while !reason().is_some_and(|(why, _)| why.is_some()) {
                assert!(started.elapsed() < Duration::from_secs(30), "it never waited on memory");
                std::thread::sleep(Duration::from_millis(20));
            }
            let held: Vec<SlotLock> = (1..=machine_budget().slots.max(1)).filter_map(|k| try_slot(&db, k).unwrap()).collect();
            let cleared = loop {
                match reason() {
                    Some((None, state)) if state == "queued" => break true,
                    Some((_, state)) if state != "queued" => break false,
                    _ if started.elapsed() > Duration::from_secs(30) => break false,
                    _ => std::thread::sleep(Duration::from_millis(20)),
                }
            };
            drop(held);
            cleared
        });
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo built" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {});
        FAKE_MEMORY.with(|f| *f.borrow_mut() = None);
        assert!(other.join().unwrap(), "while it waited for a slot it no longer said memory");
        assert!(ran.unwrap().passed());
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
        let err = run_logged(hog, &base, &log, Duration::from_secs(60), cap, None).unwrap_err();
        assert!(format!("{err:#}").starts_with("out of memory in the lane"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(14), "stopped at its cap, not waited out");
        assert!(std::fs::read_to_string(&log).unwrap().contains("out of memory in the lane"));
        // Under its cap, a run ends as usual, with its peak where the system tells it.
        let fine = run_logged("node -e \"console.log(1)\"", &base, &log, Duration::from_secs(60), Share { jobs: 1, cap: Some(2 << 30) }, None).unwrap();
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
        let ended = run_logged(ask, &base, &log, Duration::from_secs(60), NO_CAP, None).unwrap();
        assert_eq!(ended.code, Some(0));
        let said = std::fs::read_to_string(&log).unwrap();
        let want = if cfg!(windows) { "BelowNormal" } else { "10" };
        assert!(said.lines().any(|l| l.trim() == want), "{said}");
        std::fs::remove_dir_all(base).ok();
    }

    /// A lane command's temporary files go to the request's own folder beside the lanes, emptied for
    /// the next request, and `git init` there makes no hook samples (NIT-289).
    #[test]
    fn a_lane_command_keeps_its_temporary_files_beside_the_lane() {
        let base = crate::paths::unique_temp("nitely-lane-temp", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]
");
        let snap = snapshot(&root).unwrap();
        let actor = Actor::Agent("claude:test".into());
        let leave = if cfg!(windows) {
            r#"echo home=%NITELY_HOME% && echo temp=%TEMP% tmp=%TMP% && echo left> "%TEMP%\left.txt" && git init -q "%TEMP%\made" && dir /b "%TEMP%\made\.git""#
        } else {
            r#"echo home=$NITELY_HOME && echo temp=$TEMP tmp=$TMPDIR && echo left > "$TMPDIR/left.txt" && git init -q "$TMPDIR/made" && ls "$TMPDIR/made/.git""#
        };
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: leave };
        let ran = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert!(ran.passed(), "{}", ran.output);
        let temp = lane_temp_path(&root, &pb, &lane_path(&root, &pb, 1));
        assert!(!crate::light::is_lane_path(&temp.to_string_lossy()), "{temp:?} is no lane");
        let shown = temp.to_string_lossy().into_owned();
        assert!(ran.output.contains(&format!("temp={shown}")) && ran.output.contains(&format!("tmp={shown}")), "{}
wanted {shown}", ran.output);
        assert!(!ran.output.contains("hooks"), "no hook samples: {}", ran.output);
        // A Nitely data folder of its own, never the person's (NIT-330).
        let home = temp.join("nitely-home").to_string_lossy().into_owned();
        assert!(ran.output.contains(&format!("home={home}")), "{}\nwanted home={home}", ran.output);
        assert!(temp.join("left.txt").exists());
        let next = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo next" };
        assert!(run(&db, &root, &pb, &next, &actor, &mut |_| {}).unwrap().passed());
        assert!(!temp.join("left.txt").exists(), "emptied for the next request");
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
        let w = |id, project: &str, priority| Waiting { id, project: project.into(), repo: format!("C:/Code/{project}"), priority, kind: "run".into(), queued_at: 1000 };
        let none = std::collections::HashMap::new();
        let order: Vec<i64> = in_order(vec![w(1, "A", 3), w(2, "A", 3), w(3, "B", 3), w(4, "A", 2), w(5, "C", 1), w(6, "B", 3)], &none, 1000).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [5, 4, 1, 3, 2, 6]);
        // A1 started: A2 waits behind B1, though it is older.
        // A was served last (A1 started): A2 waits behind B1, though it is older.
        let a_served = std::collections::HashMap::from([("A".to_string(), 10)]);
        let order: Vec<i64> = in_order(vec![w(2, "A", 3), w(3, "B", 3)], &a_served, 1000).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [3, 2]);
        // Both served, B more recently: A's goes first.
        let both = std::collections::HashMap::from([("A".to_string(), 10), ("B".to_string(), 20)]);
        let order: Vec<i64> = in_order(vec![w(2, "A", 3), w(3, "B", 3)], &both, 1000).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [2, 3]);
    }

    fn waiting(id: i64, project: &str, kind: &str, queued_at: i64) -> Waiting {
        let priority = match kind {
            "land" => 1,
            "gate" => 2,
            _ => 3,
        };
        Waiting { id, project: project.into(), repo: format!("C:/Code/{project}"), priority, kind: kind.into(), queued_at }
    }

    fn queued(w: Waiting) -> QueueRow {
        QueueRow { w, running: false, alive: true, lane_count: 1, waits_for_checkout: false, issue: None, started_at: None, older: false, quick: None, lane: None, quick_lane: false, quick_slot: false }
    }

    fn running(w: Waiting, issue: &str, started_at: i64) -> QueueRow {
        QueueRow { running: true, issue: Some(issue.into()), started_at: Some(started_at), ..queued(w) }
    }

    /// A run's priority rises with its wait: 5 minutes makes it a gate, 10 a land (NIT-291).
    #[test]
    fn a_waiting_run_rises_past_newer_gates_and_lands() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let order = |list: Vec<Waiting>| in_order(list, &none, now).into_iter().map(|w| w.id).collect::<Vec<_>>();
        assert_eq!(order(vec![waiting(1, "A", "run", now - 60), waiting(2, "B", "gate", now)]), [2, 1], "a fresh run waits for a gate");
        assert_eq!(order(vec![waiting(1, "A", "run", now - 6 * 60), waiting(2, "B", "gate", now)]), [1, 2], "after 5 minutes it goes as one, and is older");
        assert_eq!(order(vec![waiting(1, "A", "run", now - 11 * 60), waiting(2, "B", "land", now)]), [1, 2], "after 10, as a land");
        assert_eq!(aged(&waiting(1, "A", "run", 0), now), 1, "never past a land's");
    }

    /// While a run that could start waits, gates and lands leave it the last free slot, and every
    /// request that waits says why (NIT-291).
    #[test]
    fn the_last_free_slot_is_kept_for_a_run_and_each_wait_says_why() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let rows = vec![
            running(waiting(1, "AND", "gate", now - 1500), "AND-302", now - 1440),
            running(waiting(2, "AND2", "gate", now - 1200), "AND-335", now - 1140),
            queued(waiting(3, "NIT", "gate", now - 100)),
            queued(waiting(4, "APP", "run", now - 30)),
            queued(waiting(5, "APP", "run", now - 20)),
        ];
        let t = turns(rows.clone(), 3, &none, now, &Default::default());
        assert_eq!(t[&4], Turn::Go, "{t:?}");
        assert_eq!(t[&3], Turn::Wait("waiting: the last free machine slot is kept for an agent's run".into()));
        assert_eq!(t[&5], Turn::Wait("waiting: behind request 4 of the same repo".into()));
        // With two slots none is kept: the gate, first in line, goes.
        let t = turns(rows.clone(), 2, &none, now, &Default::default());
        assert_eq!(t[&3], Turn::Wait("waiting: 2 of 2 machine slots in use (AND-302 gate 24 m, AND-335 gate 19 m)".into()), "{t:?}");
        let t = turns(rows.clone(), 4, &none, now, &Default::default());
        assert_eq!((&t[&3], &t[&4]), (&Turn::Go, &Turn::Go), "a slot each: {t:?}");
        // A run whose repo's lanes are all busy keeps no slot from the gate.
        let mut busy = rows.clone();
        busy.push(running(waiting(6, "APP", "run", now - 50), "APP-1", now - 40));
        let t = turns(busy, 4, &none, now, &Default::default());
        assert_eq!(t[&3], Turn::Go, "{t:?}");
        assert_eq!(t[&4], Turn::Wait("waiting: the free machine slots go to requests ahead (3)".into()));
        // A run behind its own repo's gate cannot start before that gate, so it keeps no slot from
        // it: the gate goes (the NIT-291 review's case).
        let same_repo = vec![
            running(waiting(1, "AND", "gate", now - 1500), "AND-302", now - 1440),
            running(waiting(2, "AND2", "gate", now - 1200), "AND-335", now - 1140),
            queued(waiting(3, "NIT", "gate", now - 100)),
            queued(waiting(4, "NIT", "run", now - 30)),
        ];
        let t = turns(same_repo, 3, &none, now, &Default::default());
        assert_eq!(t[&3], Turn::Go, "{t:?}");
        assert_eq!(t[&4], Turn::Wait("waiting: behind request 3 of the same repo".into()));
    }

    /// While a land holds its repo's land lock and its gate has not started, the rest leave that
    /// repo a lane and the machine a slot, so the land's gate starts as soon as it is asked for
    /// (NIT-332).
    #[test]
    fn a_land_in_progress_keeps_a_lane_and_a_slot_for_its_gate() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let nit = |w: Waiting| QueueRow { lane_count: 3, ..queued(w) };
        let nit_running = |w: Waiting, issue: &str| QueueRow { lane_count: 3, ..running(w, issue, now - 60) };
        let rows = vec![
            nit_running(waiting(1, "NIT", "gate", now - 300), "NIT-1"),
            nit_running(waiting(2, "NIT", "gate", now - 200), "NIT-2"),
            nit(waiting(3, "NIT", "gate", now - 100)),
            queued(waiting(4, "APP", "run", now - 30)),
        ];
        let landing = std::collections::HashSet::from([crate::paths::normalize("C:/Code/NIT")]);
        // No land in progress: the gate takes NIT's last lane.
        let t = turns(rows.clone(), 5, &none, now, &Default::default());
        assert_eq!((&t[&3], &t[&4]), (&Turn::Go, &Turn::Go), "{t:?}");
        // A land holds the lock: NIT's last lane waits for its gate; another repo's run still goes.
        let t = turns(rows.clone(), 5, &none, now, &landing);
        assert_eq!(t[&3], Turn::Wait("waiting: a lane of its repo is kept for the land in progress".into()), "{t:?}");
        assert_eq!(t[&4], Turn::Go);
        // Its gate, when asked for, goes ahead of the queue into that lane.
        let mut asked = rows.clone();
        asked.push(nit(waiting(5, "NIT", "land", now - 1)));
        let t = turns(asked, 5, &none, now, &landing);
        assert_eq!(t[&5], Turn::Go, "{t:?}");
        assert_eq!(t[&3], Turn::Wait("waiting: behind request 5 of the same repo".into()));
        // Once the land's gate runs, nothing more is kept: the gate takes the last lane.
        let mut gating = rows.clone();
        gating[1] = nit_running(waiting(2, "NIT", "land", now - 200), "NIT-9");
        let t = turns(gating, 5, &none, now, &landing);
        assert_eq!(t[&3], Turn::Go, "{t:?}");
        // On a full machine the slot is kept too: another repo's gate leaves the last one free.
        let tight = vec![
            running(waiting(1, "AND", "gate", now - 300), "AND-1", now - 60),
            running(waiting(2, "AND2", "gate", now - 300), "AND-2", now - 60),
            queued(waiting(3, "WEB", "gate", now - 100)),
        ];
        let t = turns(tight.clone(), 3, &none, now, &landing);
        assert_eq!(t[&3], Turn::Wait("waiting: a machine slot is kept for the land in progress".into()), "{t:?}");
        let t = turns(tight, 3, &none, now, &Default::default());
        assert_eq!(t[&3], Turn::Go, "{t:?}");
        // A gate that aged past the land still waits behind it: one lane, both queued, the land
        // goes (the review's deadlock).
        let one_lane = vec![queued(waiting(1, "NIT", "gate", now - 11 * 60)), queued(waiting(2, "NIT", "land", now - 5))];
        let t = turns(one_lane, 5, &none, now, &landing);
        assert_eq!(t[&2], Turn::Go, "{t:?}");
        assert_eq!(t[&1], Turn::Wait("waiting: behind request 2 of the same repo".into()));
        // Every lane busy: the rest wait rather than race for the next lane to free.
        let full = vec![
            nit_running(waiting(1, "NIT", "gate", now - 300), "NIT-1"),
            nit_running(waiting(2, "NIT", "gate", now - 300), "NIT-2"),
            nit_running(waiting(3, "NIT", "run", now - 300), "NIT-3"),
            nit(waiting(4, "NIT", "gate", now - 100)),
        ];
        let t = turns(full.clone(), 5, &none, now, &landing);
        assert_eq!(t[&4], Turn::Wait("waiting: a lane of its repo is kept for the land in progress".into()), "{t:?}");
        let t = turns(full, 5, &none, now, &Default::default());
        assert!(matches!(t[&4], Turn::LanesBusy(_)), "{t:?}");
    }

    /// A land an older nitely runs keeps no lane once its gate waits under the older order: in a
    /// one-lane repo the newer gate, which the older order puts first, goes, and the land after it
    /// (NIT-338). Without the older mark the lane is kept, as for any land in progress.
    /// A dead older land's row, its requester gone, takes nothing from the land after it: the
    /// current land holding the lock keeps its lane (NIT-342).
    #[test]
    fn a_dead_older_lands_row_leaves_the_next_land_its_lane() {
        let base = crate::paths::unique_temp("nitely-lane-dead-older", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let add = |kind: &str, pid: i64, scheduler: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, scheduler)
                     VALUES ('NIT', 'C:/Code/nitely', 'x', ?1, 's', 't', 'b', 'c', 'queued', 'local', ?2, ?3, 2, 1, ?4)",
                    params![kind, now() - 60, pid, scheduler],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let dead = 999_999;
        add("land", dead, None);
        let gate = add("gate", me, Some(SCHEDULER));
        let mut alive = |r: &Requester| r.pid == Some(me);
        let (rows, served) = queue_rows(&db, Some(gate), &mut alive).unwrap();
        assert!(rows.iter().all(|r| !r.older), "a dead requester's row is not an older land in progress: {rows:?}");
        let landing = std::collections::HashSet::from([crate::paths::normalize("C:/Code/nitely")]);
        let t = turns(rows, 5, &served, now(), &landing);
        assert_eq!(t[&gate], Turn::Wait("waiting: a lane of its repo is kept for the land in progress".into()), "{t:?}");
        std::fs::remove_dir_all(base).ok();
    }

    /// What names a test filter, so a run with no history still counts as quick (NIT-347).
    #[test]
    fn a_run_that_names_a_test_filter_is_quick() {
        for quick in [
            "cargo test -p nitely-core waits",
            "cargo test -p nitely-core -- lanes::tests",
            "cargo test --test install_and_forward",
            "cargo clippy -- -D warnings && cargo test -p nitely-core machine::",
            "node --test dist/lanes.test.js",
            "node --test --test-name-pattern=chips",
            "cd app && npx tsx --test test/tabScope.test.ts",
            "tsx --test test/one.test.ts",
            "node_modules\\.bin\\tsx.cmd --test test/one.test.ts",
            "node --import tsx --test test/one.test.ts",
            "node --test --test-reporter dot dist/lanes.test.js",
            "npx vitest run src/lanes.test.ts",
            "npx vitest -t chips",
            "npx jest src/lanes.test.ts",
            "npm test -- src/lanes.test.ts",
            "npm run test -- src/lanes.test.ts",
            "npx vitest lanes",
            "npx jest --watch src/lanes.test.ts",
            "npx vitest run --maxWorkers 2 src/lanes.test.ts",
            "npx jest --runInBand lanes",
            "npx jest --projects app -- lanes",
            "npx jest --projects app lib --runInBand src/lanes.test.ts",
            "pytest -k quick_lane",
            "pytest tests/test_lanes.py::test_one",
        ] {
            assert!(names_test_filter(quick), "{quick}");
        }
        for whole in [
            "cargo test --workspace",
            "cargo test -p nitely-core",
            "cargo test -p nitely-core -- --nocapture",
            "npm test",
            "npm test -- --coverage",
            "cargo clippy --workspace -- -D warnings",
            "node --test",
            "npx tsx --test",
            "tsx --test --test-reporter dot",
            "node --import tsx --test",
            "npx vitest run",
            "npx jest",
            "npx vitest run --maxWorkers 2",
            "npx jest --maxWorkers 2",
            "npm test -- --maxWorkers 2",
            "npx vitest run --reporter json --outputFile results.json",
            "npm test -- --outputFile=results.json --maxWorkers 2",
            "npx jest --projects app lib",
            "npx jest --selectProjects app lib --reporters default summary",
            "npm test -- --testPathIgnorePatterns e2e slow",
            "node --test --test-concurrency 2",
            "node build.js && echo --test done",
        ] {
            assert!(!names_test_filter(whole), "{whole}");
        }
    }

    /// A run is quick when its command's last runs took under two minutes at the median, or, with
    /// none, when it names a test filter.
    #[test]
    fn a_runs_quick_class_comes_from_its_last_runs() {
        let base = crate::paths::unique_temp("nitely-lane-quick", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let ran = |command: &str, seconds: i64| {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, started_at, ended_at)
                     VALUES ('NIT', 'C:/Code/nitely', 'x', 'run', 's', 't', 'b', ?1, 'done', 'local', 100, 100, ?2)",
                    params![command, 100 + seconds],
                )
                .unwrap();
        };
        for seconds in [20, 45, 30] {
            ran("cargo clippy -p nitely-core", seconds);
        }
        for seconds in [400, 380, 20] {
            ran("cargo test --workspace", seconds);
        }
        assert_eq!(quick_class(&db, "C:/Code/nitely", "cargo clippy -p nitely-core").unwrap(), Some(30), "the median of its last runs");
        assert_eq!(quick_class(&db, "C:/Code/nitely", "cargo test --workspace").unwrap(), None, "usually long");
        assert_eq!(quick_class(&db, "C:/Code/nitely", "cargo test -p nitely-core waits").unwrap(), Some(QUICK_SECS), "no history, a test filter");
        assert_eq!(quick_class(&db, "C:/Code/nitely", "cargo build --release").unwrap(), None, "no history, no filter");
        std::fs::remove_dir_all(base).ok();
    }

    /// A quick run takes a free lane ahead of its repo's queued gates, and says so; its repo's
    /// first leaves it that lane (NIT-347).
    #[test]
    fn a_quick_run_goes_ahead_of_a_gate_backlog() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let nit = |w: Waiting| QueueRow { lane_count: 3, ..queued(w) };
        let nit_running = |w: Waiting, issue: &str| QueueRow { lane_count: 3, ..running(w, issue, now - 60) };
        let quick = |w: Waiting| QueueRow { quick: Some(30), ..nit(w) };
        let rows = vec![
            nit_running(waiting(1, "NIT", "gate", now - 900), "NIT-1"),
            nit_running(waiting(2, "NIT", "gate", now - 800), "NIT-2"),
            nit(waiting(10, "NIT", "gate", now - 200)),
            nit(waiting(11, "NIT", "gate", now - 150)),
            quick(waiting(12, "NIT", "run", now - 20)),
        ];
        let t = turns(rows.clone(), 5, &none, now, &Default::default());
        assert_eq!(t[&12], Turn::QuickGo("quick run: ahead of 2 requests of its repo".into()), "{t:?}");
        assert_eq!(t[&10], Turn::Wait("waiting: its repo's free lane goes to quick run 12 first".into()));
        assert_eq!(t[&11], Turn::Wait("waiting: behind request 10 of the same repo".into()));
        // The same run, not quick: it waits its turn.
        let mut slow = rows.clone();
        slow[4] = nit(waiting(12, "NIT", "run", now - 20));
        let t = turns(slow, 5, &none, now, &Default::default());
        assert_eq!((&t[&10], &t[&12]), (&Turn::Go, &Turn::Wait("waiting: behind request 10 of the same repo".into())), "{t:?}");
        // Never ahead of a waiting land.
        let mut landing = rows.clone();
        landing.push(nit(waiting(13, "NIT", "land", now - 5)));
        let t = turns(landing, 5, &none, now, &Default::default());
        assert_eq!(t[&13], Turn::Go, "{t:?}");
        assert!(matches!(&t[&12], Turn::Wait(why) if why.contains("behind request 13")), "{t:?}");
        // Never in a one-lane repo, where the one lane is the queue's.
        let one_lane: Vec<QueueRow> = rows.iter().cloned().map(|r| QueueRow { lane_count: 1, ..r }).filter(|r| !r.running).collect();
        let t = turns(one_lane, 5, &none, now, &Default::default());
        assert_eq!(t[&10], Turn::Go, "{t:?}");
    }

    /// A repo's quick lane (NIT-347): on unless `quick = false`, never in large-repo mode, and after
    /// the repo's numbered lanes.
    #[test]
    fn a_repo_has_a_quick_lane_unless_it_turns_it_off_or_is_large() {
        let rules: LaneRules = toml::from_str("count = 2").unwrap();
        assert!(rules.quick_lane());
        assert_eq!(lane_numbers(&rules), [1, 2, QUICK_LANE]);
        let off: LaneRules = toml::from_str("count = 2\nquick = false").unwrap();
        assert!(!off.quick_lane());
        assert_eq!(lane_numbers(&off), [1, 2]);
        let large = LaneRules { large_repo: true, ..Default::default() };
        assert!(!large.quick_lane(), "a Chromium-sized lane is too much disk for it");
        assert_eq!(lane_name(QUICK_LANE), "lane-quick");
    }

    /// With every lane of its repo busy, a quick run takes the repo's quick lane at once; one at a
    /// time, and the repo's lanes stay its queue's (NIT-347, the review's P1).
    #[test]
    fn a_quick_run_takes_the_quick_lane_when_every_lane_is_busy() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let nit = |w: Waiting| QueueRow { lane_count: 3, quick_lane: true, ..queued(w) };
        let gate_running = |id: i64, lane: i64| QueueRow { lane_count: 3, quick_lane: true, lane: Some(lane), ..running(waiting(id, "NIT", "gate", now - 900), "NIT-1", now - 300) };
        let quick = |w: Waiting| QueueRow { quick: Some(30), ..nit(w) };
        let rows = vec![
            gate_running(1, 1),
            gate_running(2, 2),
            gate_running(3, 3),
            nit(waiting(10, "NIT", "gate", now - 200)),
            quick(waiting(12, "NIT", "run", now - 20)),
            quick(waiting(13, "NIT", "run", now - 10)),
        ];
        let t = turns(rows.clone(), 5, &none, now, &Default::default());
        assert_eq!(t[&12], Turn::QuickLane("quick run: on its repo's quick lane, ahead of 1 request of its repo".into()), "{t:?}");
        assert_eq!(t[&10], Turn::LanesBusy("waiting: all 3 of its repo's lanes are busy".into()));
        assert_eq!(t[&13], Turn::Wait("waiting: behind request 10 of the same repo".into()), "one quick run at a time");
        // A lane free: the gate takes it, and the quick run still its own.
        let mut one_free = rows.clone();
        one_free.remove(2);
        let t = turns(one_free, 5, &none, now, &Default::default());
        assert_eq!((&t[&10], &t[&12]), (&Turn::Go, &Turn::QuickLane("quick run: on its repo's quick lane, ahead of 1 request of its repo".into())), "{t:?}");
        // The quick lane busy: the next quick run waits its turn like any other.
        let mut busy = rows.clone();
        busy.remove(4);
        busy.push(QueueRow { lane: Some(i64::from(QUICK_LANE)), quick: Some(30), ..QueueRow { lane_count: 3, quick_lane: true, ..running(waiting(4, "NIT", "run", now - 30), "NIT-5", now - 10) } });
        let t = turns(busy, 5, &none, now, &Default::default());
        assert_eq!(t[&13], Turn::Wait("waiting: behind request 10 of the same repo".into()), "{t:?}");
        assert_eq!(t[&10], Turn::LanesBusy("waiting: all 3 of its repo's lanes are busy".into()), "the quick lane's run holds none of the repo's lanes");
    }

    /// The machine's quick slot: one in all, only for a quick run, and only with every slot busy
    /// (NIT-347). Memory admission still decides when it starts (`take_turn`).
    #[test]
    fn the_quick_slot_is_one_for_the_machine_and_only_for_a_quick_run() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let other = |id: i64| QueueRow { lane_count: 3, ..running(waiting(id, "B", "gate", now - 900), &format!("B-{id}"), now - 300) };
        let lane_of = |project: &str, w: Waiting| QueueRow { lane_count: 3, quick_lane: true, ..queued(Waiting { project: project.into(), repo: format!("C:/Code/{project}"), ..w }) };
        let rows = vec![
            other(1),
            other(2),
            lane_of("NIT", waiting(10, "NIT", "gate", now - 200)),
            QueueRow { quick: Some(30), ..lane_of("NIT", waiting(12, "NIT", "run", now - 20)) },
            QueueRow { quick: Some(30), ..lane_of("C", waiting(20, "C", "run", now - 10)) },
        ];
        let t = turns(rows.clone(), 2, &none, now, &Default::default());
        assert!(matches!(&t[&10], Turn::Wait(why) if why.starts_with("waiting: 2 of 2 machine slots in use")), "{t:?}");
        assert_eq!(t[&12], Turn::QuickLane("quick run: on its repo's quick lane and the machine's quick slot, ahead of 1 request of its repo".into()), "{t:?}");
        assert!(matches!(&t[&20], Turn::Wait(why) if why.starts_with("waiting: 2 of 2 machine slots in use")), "one quick slot for the machine: {t:?}");
        // First in its repo with a lane free, a quick run takes the quick slot on that lane; a gate never.
        let alone: Vec<QueueRow> = rows.iter().filter(|r| r.w.project != "NIT").cloned().collect();
        let t = turns(alone, 2, &none, now, &Default::default());
        assert_eq!(t[&20], Turn::QuickGo("quick run: on the machine's quick slot".into()), "{t:?}");
        // A quick run holds the quick slot: it counts against no slot of the machine's, and the
        // next quick run waits.
        let mut held = rows.clone();
        held.push(QueueRow { quick_slot: true, lane: Some(i64::from(QUICK_LANE)), quick: Some(30), quick_lane: true, lane_count: 3, ..running(waiting(30, "D", "run", now - 30), "D-1", now - 10) });
        let t = turns(held, 2, &none, now, &Default::default());
        assert!(matches!(&t[&12], Turn::Wait(why) if why.starts_with("quick run, ahead of 1 request of its repo, waiting: 2 of 2 machine slots")), "{t:?}");
    }

    /// A quick run first in its repo is never passed over for a later one (the NIT-347 review).
    #[test]
    fn the_oldest_quick_run_goes_first() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let quick = |w: Waiting| QueueRow { lane_count: 3, quick: Some(30), ..queued(w) };
        let rows = vec![
            QueueRow { lane_count: 3, ..running(waiting(1, "NIT", "gate", now - 900), "NIT-1", now - 300) },
            QueueRow { lane_count: 3, ..running(waiting(2, "NIT", "gate", now - 900), "NIT-2", now - 300) },
            quick(waiting(10, "NIT", "run", now - 200)),
            quick(waiting(11, "NIT", "run", now - 10)),
        ];
        let t = turns(rows, 5, &none, now, &Default::default());
        assert_eq!(t[&10], Turn::Go, "{t:?}");
        assert_eq!(t[&11], Turn::Wait("waiting: behind request 10 of the same repo".into()));
    }

    /// A stream of quick runs never starves a gate: one lane at most goes to them, and while a
    /// quick run holds it the repo's first takes the next free lane. One that runs past three
    /// times its usual time is just a run, and frees the quick lane for the next (NIT-347).
    #[test]
    fn quick_runs_take_one_lane_at_most_and_never_starve_a_gate() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let nit = |w: Waiting| QueueRow { lane_count: 3, ..queued(w) };
        let quick_running = |w: Waiting, started: i64| QueueRow { lane_count: 3, quick: Some(30), ..running(w, "NIT-5", started) };
        let rows = vec![
            QueueRow { lane_count: 3, ..running(waiting(1, "NIT", "gate", now - 900), "NIT-1", now - 300) },
            quick_running(waiting(2, "NIT", "run", now - 40), now - 20),
            nit(waiting(10, "NIT", "gate", now - 200)),
            QueueRow { quick: Some(30), ..nit(waiting(12, "NIT", "run", now - 10)) },
        ];
        let t = turns(rows.clone(), 5, &none, now, &Default::default());
        assert_eq!(t[&10], Turn::Go, "a quick run already holds a lane: the gate takes the free one: {t:?}");
        assert_eq!(t[&12], Turn::Wait("waiting: behind request 10 of the same repo".into()));
        // The running quick run is past three times its usual 30 s: just a run now.
        let mut overran = rows.clone();
        overran[1] = quick_running(waiting(2, "NIT", "run", now - 200), now - 100);
        let t = turns(overran, 5, &none, now, &Default::default());
        assert!(matches!(t[&12], Turn::QuickGo(_)), "{t:?}");
        assert!(matches!(&t[&10], Turn::Wait(why) if why.contains("quick run 12")), "{t:?}");
    }

    #[test]
    fn an_older_nitelys_land_keeps_no_lane_from_the_gate_it_waits_behind() {
        let none = std::collections::HashMap::new();
        let now = 10_000;
        let landing = std::collections::HashSet::from([crate::paths::normalize("C:/Code/NIT")]);
        let gate = queued(waiting(1, "NIT", "gate", now - 5 * 60 - 10));
        // The older binary's land: passed over, as queue_rows marks it (not alive, older).
        let old_land = QueueRow { alive: false, older: true, ..queued(waiting(2, "NIT", "land", now - 60)) };
        let t = turns(vec![gate.clone(), old_land], 5, &none, now, &landing);
        assert_eq!(t[&1], Turn::Go, "the gate goes, and the older land after it: {t:?}");
        // A current land's gate still gets the lane kept for it.
        let t = turns(vec![gate], 5, &none, now, &landing);
        assert_eq!(t[&1], Turn::Wait("waiting: a lane of its repo is kept for the land in progress".into()), "{t:?}");
    }

    /// `nitely lanes` and the app show why each queued request waits.
    #[test]
    fn the_queue_says_why_a_request_waits() {
        let base = crate::paths::unique_temp("nitely-lane-why", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]
");
        let me = std::process::id() as i64;
        let add = |state: &str, waiting_memory: Option<&str>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, started_at, pid, priority, lane_count, waiting_memory, scheduler)
                     VALUES ('APP', ?1, 'APP-1', 'x', 'run', 's', 't', 'b', 'c', ?2, 'local', ?3, CASE WHEN ?2 = 'running' THEN ?3 ELSE NULL END, ?4, 3, 1, ?5, 3)",
                    params![root.to_string_lossy(), state, now(), me, waiting_memory],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        add("running", None);
        let behind = add("queued", None);
        let memory = add("queued", Some("waiting for memory: 5.0 GB free, new work starts above 12.8 GB"));
        let shown = status(&db, &root, &pb, "APP").unwrap().unwrap();
        let why = |id: i64| shown.queue.iter().find(|r| r.id == id).and_then(|r| r.waiting_why.clone());
        // Its repo's one lane is busy, or (a machine with one slot) the machine's: either way, said.
        assert!(why(behind).is_some_and(|w| w == "waiting: all 1 of its repo's lanes are busy" || w.starts_with("waiting: 1 of 1 machine slots in use (APP-1 run")), "{:?}", why(behind));
        assert!(why(memory).unwrap().starts_with("waiting for memory"));
        std::fs::remove_dir_all(base).ok();
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
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, started_at, scheduler)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', ?3, 'local', CAST(strftime('%s', 'now') AS INTEGER), ?4, ?5, ?6, CASE WHEN ?3 = 'queued' THEN NULL ELSE 2 END, 3)",
                    params![project, format!("C:/Code/{project}"), state, pid, priority, lanes],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |r: &Requester| r.pid == Some(me);
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
        let mut all_alive = |_: &Requester| true;
        assert!(!my_turn(&db, a, 3, &mut all_alive).unwrap(), "while it lives, it does");
        std::fs::remove_dir_all(base).ok();

        // A1 has run and finished; A2 and B1 wait for one slot: B1 goes first.
        let base = crate::paths::unique_temp("nitely-lane-turn2", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let add = |project: &str, state: &str, queued: i64, started: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, started_at, pid, priority, lane_count, scheduler)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', ?3, 'local', ?4, ?5, ?6, 3, 1, 3)",
                    params![project, format!("C:/Code/{project}"), state, queued, started, me],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        // A1 started (10) before A2 (11) and B1 (12) were even queued, and has finished.
        add("A", "done", 9, Some(10));
        let a2 = add("A", "queued", 11, None);
        let b1 = add("B", "queued", 12, None);
        let mut alive = |r: &Requester| r.pid == Some(me);
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

    /// A request that waited on memory and then waits for the person's checkout says only the
    /// checkout: the strip no longer reads its old memory reason (NIT-368).
    #[test]
    fn a_memory_reason_goes_once_a_request_waits_for_the_checkout() {
        let base = crate::paths::unique_temp("nitely-large-memory", "");
        let (root, agent, pb) = large(&base, "");
        let db_path = base.join("t.db");
        let db = Db::open(&db_path).unwrap();
        let mem = |free_gb: u64| Some(machine::Memory { total: 64 * machine::GIB, available: free_gb * machine::GIB });
        // Memory is short at its first look, and the person changes a file in their checkout then.
        let (r, mut first) = (root.clone(), true);
        let next: Readings = Box::new(move || {
            if std::mem::take(&mut first) {
                std::fs::write(r.join("notes.txt"), "mine\n").unwrap();
                mem(5)
            } else {
                mem(40)
            }
        });
        FAKE_MEMORY.with(|f| *f.borrow_mut() = Some(next));
        // Once it says it waits for the checkout, what does it say about memory? Then the person
        // finishes their change.
        let (r, n) = (root.clone(), db_path.clone());
        let person = std::thread::spawn(move || {
            let db = Db::open(&n).unwrap();
            let started = Instant::now();
            loop {
                let row: Option<(Option<String>, Option<String>)> =
                    db.conn.query_row("SELECT waiting_for, waiting_memory FROM lane_run ORDER BY id DESC LIMIT 1", [], |row| Ok((row.get(0)?, row.get(1)?))).optional().unwrap();
                if let Some((Some(_), memory)) = row {
                    std::fs::remove_file(r.join("notes.txt")).unwrap();
                    return memory;
                }
                assert!(started.elapsed() < Duration::from_secs(30), "the request never waited for the checkout");
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let snap = snapshot(&agent).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo ok" };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {});
        FAKE_MEMORY.with(|f| *f.borrow_mut() = None);
        assert_eq!(person.join().unwrap(), None, "waiting for the checkout, it no longer says memory");
        assert!(ran.unwrap().passed());
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

    /// Writes the repo's own ignore list, `info/exclude`, whose folder a repo made without a
    /// template (as in a lane, NIT-289) does not have.
    fn exclude(root: &Path, text: &str) {
        let path = git_path(root, "info/exclude").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn an_ignored_file_where_the_commit_needs_a_folder_is_refused() {
        let base = crate::paths::unique_temp("nitely-large-parent", "");
        let (root, agent, pb) = large(&base, "");
        let db = Db::open(&base.join("t.db")).unwrap();
        // `cache` is ignored here only, and the person has a file by that name.
        exclude(&root, "cache\n");
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
        exclude(&root, ".nitely/\n");
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
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, waiting_for, scheduler)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, ?3, 3, 1, ?4, 3)",
                    params![project, format!("C:/Code/{project}"), me, waiting_for],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |_: &Requester| true;
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
        let ended = run_logged(sleeper, &base, &log, Duration::from_secs(1), NO_CAP, None).unwrap();
        assert!(ended.timed_out && ended.code.is_none());
        assert!(started.elapsed() < Duration::from_secs(20), "stopped, not waited out");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_queued_run_can_be_cancelled_and_a_running_one_says_how_to_end_it() {
        let base = crate::paths::unique_temp("nitely-lane-cancel", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let row = |state: &str| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, lane, scheduler)
                     VALUES ('APP', 'C:/Code/my-app', 'x', 'run', 's', 't', 'b', 'c', ?1, 'local', 1, ?2, 1, 3)",
                    params![state, std::process::id()],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let (queued, running) = (row("queued"), row("running"));
        let cancelled = cancel(&db, queued, &Actor::Human).unwrap();
        assert_eq!((cancelled.state.as_str(), cancelled.error.as_deref()), ("abandoned", Some("cancelled by human while it was queued")));
        // Its requester, still looking for its turn, is told and does not run it.
        let mut alive = |_: &Requester| true;
        let told = my_turn(&db, queued, 3, &mut alive).unwrap_err().to_string();
        assert!(told.contains("was taken out of the queue (cancelled by human while it was queued)"), "{told}");
        let refused = cancel(&db, running, &Actor::Human).unwrap_err().to_string();
        assert!(refused.contains("is running in lane 1") && refused.contains(&format!("process {}", std::process::id())), "{refused}");
        std::fs::remove_dir_all(base).ok();
    }


    #[test]
    fn a_reused_process_number_is_not_the_process_that_had_it() {
        let (pid, started) = crate::procs::me();
        assert!(crate::procs::still_runs(pid, started.as_deref()));
        assert!(crate::procs::still_runs(pid, None), "nothing recorded to compare: the pid decides");
        if started.is_some() {
            assert!(!crate::procs::still_runs(pid, Some("another start")));
        }
        assert!(!crate::procs::still_runs(999_999, None));
        assert!(!crate::procs::still_runs(999_999, Some("whenever")));
    }


    #[test]
    fn an_older_schedulers_request_holds_up_no_newer_one() {
        let base = crate::paths::unique_temp("nitely-lane-older", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let add = |kind: &str, priority: i64, queued_at: i64, scheduler: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, scheduler)
                     VALUES ('NIT', 'C:/Code/nitely', 'x', ?1, 's', 't', 'b', 'c', 'queued', 'local', ?2, ?3, ?4, 3, ?5)",
                    params![kind, queued_at, me, priority, scheduler],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |_: &Requester| true;
        // As on 2026-10-18: an old binary's run, queued 12 minutes ago, which aging puts ahead of
        // a gate the new binary queued since (a waiting land goes first in its repo whatever aged
        // past it, NIT-332, so the gate stands in for the newer request here).
        let old = add("run", 3, now() - 12 * 60, None);
        let gate = add("gate", 2, now() - 10, Some(SCHEDULER));
        assert!(my_turn(&db, gate, 5, &mut alive).unwrap(), "the old binary's run holds the gate up no longer");
        assert!(my_turn(&db, old, 5, &mut alive).unwrap(), "asking, a request is never passed over itself");
        db.conn.execute("UPDATE lane_run SET scheduler = ?2 WHERE id = ?1", params![old, SCHEDULER]).unwrap();
        assert!(!my_turn(&db, gate, 5, &mut alive).unwrap(), "under one scheduler, the order holds as before");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_queued_request_whose_requester_stopped_looking_holds_nobody_up() {
        let base = crate::paths::unique_temp("nitely-lane-head", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let row = |polled: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, polled_at, scheduler)
                     VALUES ('APP', 'C:/Code/my-app', 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, ?1, 3, 1, ?2, 3)",
                    params![me, polled],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |_: &Requester| true;
        // Its requester lives but last looked two minutes ago (hung, or suspended): the request
        // behind it goes.
        let head = row(Some(now() - 120));
        let behind = row(Some(now()));
        assert!(my_turn(&db, behind, 2, &mut alive).unwrap());
        // Looking again puts it back at the head, and the one behind waits.
        assert!(my_turn(&db, head, 2, &mut alive).unwrap());
        assert!(!my_turn(&db, behind, 2, &mut alive).unwrap());
        std::fs::remove_dir_all(base).ok();
    }
    #[test]
    fn executor_remote_runs_do_not_reserve_local_memory_or_queue_slots() {
        let base=crate::paths::unique_temp("nitely-executor-admission", "");
        let db=Db::open(&base.join("t.db")).unwrap();
        for (executor,state,cap) in [("local","queued",None),("machine:builder","running",Some(100*machine::GIB as i64)),("ci","running",None)] {
            db.conn.execute("INSERT INTO lane_run(project,repo,requested_by,kind,snapshot,tree,base,command,state,executor,queued_at,pid,priority,lane_count,memory_cap) VALUES ('APP','C:/Code/app','x','gate','s','t','b','c',?1,?2,1,?3,2,1,?4)",params![state,executor,std::process::id(),cap]).unwrap();
        }
        let limits=machine::limits_for(64*machine::GIB,None,None);
        assert_eq!(caps_fit(&db,1,Some(&limits),1).unwrap(),None);
        let (rows,_)=queue_rows(&db,Some(1),&mut |_|true).unwrap();
        assert_eq!(rows.len(),1);
        drop(db);let _=std::fs::remove_dir_all(base);
    }

}
