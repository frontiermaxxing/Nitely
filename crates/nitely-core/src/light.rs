//! Light worktrees (docs/build-lanes.md): an agent's worktree holds source only, with no setup that
//! builds or installs anything, optionally sparse (cone mode) and widened on demand. Code outside
//! it is read in the reference tree, one full source-only checkout of the project's base commit.
//! Off unless the playbook says `[worktree] mode = "light"`.

use crate::playbook::{git, normalize_lexically, Playbook, WorktreeMode};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The folder beside a repo's worktrees that holds what Nitely keeps for it, not agents' work: the
/// reference tree (`ref`) and the build lanes (`lane-<n>`).
pub const LANES_DIR: &str = ".nitely-lanes";

/// Whether `path` is under a `.nitely-lanes` folder: the reference tree or a lane, which no
/// listing of agents' worktrees shows.
pub fn is_lane_path(path: &str) -> bool {
    crate::paths::normalize(path).split('/').any(|part| part == LANES_DIR)
}

/// `.nitely-lanes/<repo>` beside the repo's worktrees: under the parent of the playbook's worktree
/// folder (`../{repo}-worktrees` by default), named after the repo, since a lane is a checkout of
/// one repo and repos can share a worktree folder.
pub fn lanes_dir(pb: &Playbook, repo_root: &Path) -> PathBuf {
    let repo = repo_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = pb.worktree.dir.replace("{repo}", &repo);
    let parent = Path::new(&dir).parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty() && !p.to_string_lossy().contains('{'));
    let parent = parent.unwrap_or_else(|| PathBuf::from(format!("../{repo}-worktrees")));
    normalize_lexically(&repo_root.join(parent).join(LANES_DIR).join(repo))
}

/// The commit the project's work starts from and its reference tree shows: the integration
/// branch's tip, origin's when the local branch is behind it; or, with `[lanes] base = "every
/// <n>h"`, that tip as it was when the base last moved, at most `n` hours ago.
pub fn base_commit(pb: &Playbook, repo_root: &Path) -> Result<String> {
    let base = crate::playbook::diff_base(repo_root, pb.integration_branch());
    let tip = git(repo_root, &["rev-parse", "--verify", &format!("{base}^{{commit}}")])
        .with_context(|| format!("the integration branch {base} has no commit"))?
        .trim()
        .to_string();
    Ok(crate::lanes::scheduled_base(pb, repo_root, &tip)?.unwrap_or(tip))
}

/// The reference tree's folder, whether or not it exists yet.
pub fn reference_path(pb: &Playbook, repo_root: &Path) -> PathBuf {
    lanes_dir(pb, repo_root).join("ref")
}

/// Creates the reference tree, or moves it to the base commit when the base moved: a detached,
/// source-only worktree, read-only by convention. Anything left in it is overwritten.
pub fn reference_tree(pb: &Playbook, repo_root: &Path) -> Result<PathBuf> {
    let path = reference_path(pb, repo_root);
    let base = base_commit(pb, repo_root)?;
    let at = path.to_string_lossy().into_owned();
    if !path.join(".git").exists() {
        // A folder git still lists but that is gone (deleted by hand) would refuse the add.
        git(repo_root, &["worktree", "prune"]).ok();
        if path.exists() {
            crate::unlink::remove_tree(&path).with_context(|| format!("clearing {}", path.display()))?;
        }
        git(repo_root, &["worktree", "add", "--detach", &at, &base]).context("creating the reference tree")?;
        return Ok(path);
    }
    let head = git(&path, &["rev-parse", "HEAD"]).unwrap_or_default();
    if head.trim() != base {
        git(&path, &["checkout", "--force", "--detach", &base]).context("moving the reference tree")?;
        git(&path, &["clean", "-ffdq"]).ok();
    }
    Ok(path)
}

/// Where a worktree marks itself light: in its own git folder, so it is never committed and goes
/// with the worktree.
fn marker(worktree: &Path) -> Option<PathBuf> {
    let path = git(worktree, &["rev-parse", "--git-path", "nitely-light"]).ok()?;
    let path = PathBuf::from(path.trim());
    Some(if path.is_absolute() { path } else { worktree.join(path) })
}

/// Whether `worktree` was started light: source only, its setup not run.
pub fn is_light(worktree: &Path) -> bool {
    marker(worktree).is_some_and(|m| m.exists())
}

/// The cone-mode directories a sparse worktree starts with: the playbook's, then the directories
/// of the paths the issue names that exist at `base`. Empty when the playbook sets none (the
/// worktree is then whole).
pub fn sparse_dirs(pb: &Playbook, repo_root: &Path, base: &str, text: &str) -> Vec<String> {
    if pb.worktree.sparse.is_empty() {
        return vec![];
    }
    let named: Vec<Option<String>> = crate::overlap::paths_in(text).iter().map(|p| dir_of(repo_root, base, p)).collect();
    cone(&pb.worktree.sparse, named.into_iter().flatten())
}

/// The cone a sparse worktree starts with: the playbook's directories, then those of the paths an
/// issue names, without repeats or a directory another one holds.
pub(crate) fn cone(sparse: &[String], named: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut dirs: Vec<String> = sparse.iter().map(|d| d.trim().trim_matches('/').to_string()).filter(|d| !d.is_empty()).collect();
    dirs.extend(named);
    dedupe(dirs)
}

/// The directory a path names at `rev`: itself when it is one, its parent when it is a file, and
/// nothing for a file at the root (cone mode always has those) or a path that does not exist.
pub(crate) fn dir_of(repo_root: &Path, rev: &str, path: &str) -> Option<String> {
    let path = path.trim().trim_start_matches("./").trim_matches('/');
    let kind = git(repo_root, &["cat-file", "-t", &format!("{rev}:{path}")]).ok()?;
    dir_for(path, kind.trim())
}

/// The directory for `path` given what git says it is (`tree` or `blob`): itself, its parent, or
/// nothing for a file at the root or anything else.
pub(crate) fn dir_for(path: &str, kind: &str) -> Option<String> {
    let path = path.trim().trim_start_matches("./").trim_matches('/');
    match kind {
        "tree" => Some(path.to_string()),
        "blob" => path.rsplit_once('/').map(|(dir, _)| dir.to_string()),
        _ => None,
    }
}

/// The directories of `dirs` the cone `before` does not hold yet.
pub(crate) fn not_yet_in(before: &[String], dirs: Vec<String>) -> Vec<String> {
    dedupe(dirs).into_iter().filter(|d| !before.iter().any(|b| d == b || d.starts_with(&format!("{b}/")))).collect()
}

/// Without repeats, and without a directory another one in the list already holds.
pub(crate) fn dedupe(mut dirs: Vec<String>) -> Vec<String> {
    dirs.sort();
    dirs.dedup();
    let all = dirs.clone();
    dirs.retain(|d| !all.iter().any(|o| o != d && d.starts_with(&format!("{o}/"))));
    dirs
}

/// Adds a light worktree at `worktree` on `branch` (new from `base` unless `exists`): no setup,
/// and sparse in cone mode over `sparse` when that is not empty.
pub(crate) fn add(repo_root: &Path, worktree: &Path, branch: &str, base: &str, exists: bool, sparse: &[String]) -> Result<()> {
    let at = worktree.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add"];
    if !sparse.is_empty() {
        args.push("--no-checkout");
    }
    if exists {
        args.extend([at.as_str(), branch]);
    } else {
        args.extend(["-b", branch, at.as_str(), base]);
    }
    git(repo_root, &args)?;
    if !sparse.is_empty() {
        let mut set = vec!["sparse-checkout", "set", "--cone"];
        set.extend(sparse.iter().map(String::as_str));
        git(worktree, &set).context("making the worktree sparse")?;
        git(worktree, &["checkout"]).context("checking out the sparse worktree")?;
    }
    if let Some(m) = marker(worktree) {
        std::fs::write(m, "light").ok();
    }
    // A setup record that never matches a fingerprint: the worktree counts as set up (the crew
    // adopts it), and its first gate still runs setup (`refresh_setup`) until gates run in lanes.
    if let Ok(record) = git(worktree, &["rev-parse", "--git-path", "nitely-setup"]) {
        let record = PathBuf::from(record.trim());
        std::fs::write(if record.is_absolute() { record } else { worktree.join(record) }, "light: not set up").ok();
    }
    Ok(())
}

/// Turns a sparse worktree whole, so a gate run in it sees every file; nothing for a whole one.
/// Returns the step to show among the gate's results.
pub fn make_whole(worktree: &Path) -> Result<Vec<crate::playbook::CommandResult>> {
    let Some(dirs) = sparse_set(worktree) else { return Ok(vec![]) };
    let started = std::time::Instant::now();
    git(worktree, &["sparse-checkout", "disable"]).context("making the sparse worktree whole for the gate")?;
    Ok(vec![crate::playbook::CommandResult {
        command: "git sparse-checkout disable (the gate builds every file until gates run in build lanes)".into(),
        ok: true,
        seconds: started.elapsed().as_secs(),
        output: format!("It held {}; it is whole now.", dirs.join(", ")),
        skipped: false,
        timed_out: false,
    }])
}

/// The directories a sparse worktree has, or None when it is whole.
pub fn sparse_set(worktree: &Path) -> Option<Vec<String>> {
    let on = git(worktree, &["config", "--bool", "core.sparseCheckout"]).ok()?;
    if on.trim() != "true" {
        return None;
    }
    Some(git(worktree, &["sparse-checkout", "list"]).ok()?.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct Widened {
    /// The directories added this time.
    pub added: Vec<String>,
    /// Every directory the worktree has now; empty when it is whole.
    pub sparse: Vec<String>,
    pub note: String,
}

/// Adds the directories of `paths` (a file's own directory, or the directory itself) to a sparse
/// worktree. A whole worktree already has everything, and says so.
pub fn widen(worktree: &Path, paths: &[String]) -> Result<Widened> {
    if paths.iter().all(|p| p.trim().is_empty()) {
        bail!("name the paths to add, e.g. src/billing or src/billing/invoice.rs");
    }
    // Paths in a declared submodule go to the submodule's worktree, made now when the task has none.
    let root = crate::git::main_root(&worktree.to_string_lossy()).map(PathBuf::from);
    let (paths, said) = match root.as_ref().map(|r| (r, Playbook::load(r))) {
        Some((root, Ok(pb))) if !pb.submodules.is_empty() => crate::submodules::widen(root, &pb, worktree, paths)?,
        _ => (paths.to_vec(), vec![]),
    };
    if paths.iter().all(|p| p.trim().is_empty()) {
        return Ok(Widened { added: vec![], sparse: sparse_set(worktree).unwrap_or_default(), note: said.join(" ") });
    }
    let said = |note: String| if said.is_empty() { note } else { format!("{} {note}", said.join(" ")) };
    let Some(before) = sparse_set(worktree) else {
        return Ok(Widened { added: vec![], sparse: vec![], note: said("This worktree is not sparse: every path is already here.".into()) });
    };
    let mut missing = Vec::new();
    let mut dirs = Vec::new();
    for p in paths.iter().map(|p| p.trim().replace('\\', "/")).filter(|p| !p.is_empty()) {
        match dir_of(worktree, "HEAD", &p) {
            Some(d) => dirs.push(d),
            None if git(worktree, &["cat-file", "-e", &format!("HEAD:{}", p.trim_matches('/'))]).is_ok() => {} // a root file: always here
            None => missing.push(p),
        }
    }
    if !missing.is_empty() {
        bail!("not in this branch: {} (paths are relative to the repo root)", missing.join(", "));
    }
    let added = not_yet_in(&before, dirs);
    if !added.is_empty() {
        let mut args = vec!["sparse-checkout", "add"];
        args.extend(added.iter().map(String::as_str));
        git(worktree, &args).context("widening the sparse checkout")?;
    }
    let sparse = sparse_set(worktree).unwrap_or_default();
    let note = said(if added.is_empty() { "Already here.".to_string() } else { format!("Added {}.", added.join(", ")) });
    Ok(Widened { added, sparse, note })
}

/// What a session in a light worktree is told, on top of the workflow: or None for a full one.
pub fn session_note(pb: &Playbook, repo_root: &Path, worktree: &Path) -> Option<String> {
    if !is_light(worktree) {
        return None;
    }
    let reference = reference_path(pb, repo_root);
    let read = if reference.join(".git").exists() {
        format!(" To read code outside it, use the reference tree at {}: the project's base commit, whole, read-only (do not edit or build there).", reference.display())
    } else {
        String::new()
    };
    let sparse = match sparse_set(worktree) {
        Some(dirs) => format!(" It is sparse: it holds {} and the files at the root. Add more with `widen` (paths or directories) before you edit them.", dirs.join(", ")),
        None => String::new(),
    };
    let build = if pb.lanes.is_some() { format!(" {RUN_LINE}") } else { String::new() };
    Some(format!(
        "This is a light worktree: source only, with no dependencies installed and no build output, on purpose, so many agents fit on one machine.{build}{sparse}{read}"
    ))
}

/// How a light worktree builds and tests, for its session and the crew's brief.
pub const RUN_LINE: &str = "Build and test through `run` (MCP; `nitely run -- <command>` from a shell): it builds a snapshot of this worktree, committed or not, in a warm build lane that keeps its build output, and returns the exit code and the end of the output (`run_status` waits on a long one). Do not build or test here: it would install and compile everything again, for this worktree alone.";

/// The commands of a shell line's first line, each trimmed: split at `&&`, `||`, `;` and `|` outside
/// quotes, so a quoted argument (`nitely run --shell "cargo test && cargo clippy"`) stays part of
/// its command (NIT-238). Only the first line: what follows can be a heredoc's text (a prompt, a
/// file), which is not a command and is never read or kept.
fn commands_of(line: &str) -> Vec<String> {
    let mut commands = vec![];
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.lines().next().unwrap_or("").chars() {
        match (quote, c) {
            _ if escaped => escaped = false,
            // A backslash escapes the next character in double quotes and unquoted (sh); in single
            // quotes it is a character like any other.
            (Some('"') | None, '\\') => escaped = true,
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, ';' | '|' | '&') => {
                commands.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    commands.push(current);
    commands.into_iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
}

/// Whether a command is `nitely run`, which already sends its command to a lane.
fn is_lane_run(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or("").trim_matches(['"', '\'']).replace('\\', "/");
    let name = program.rsplit('/').next().unwrap_or("").to_lowercase();
    matches!(name.as_str(), "nitely" | "nitely.exe" | "nitely.cmd") && words.next() == Some("run")
}

/// The folder a shell line works in: the folder of a leading `cd <dir>`, else `cwd`.
fn folder_of(line: &str, cwd: &Path) -> PathBuf {
    let first = commands_of(line).into_iter().next().unwrap_or_default();
    let Some(dir) = first.strip_prefix("cd ").map(|d| d.trim().trim_matches(['"', '\'']).to_string()) else { return cwd.to_path_buf() };
    // Git Bash spells C:\x as /c/x.
    let dir = match dir.as_bytes() {
        [b'/', drive, b'/', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => format!("{}:{}", *drive as char, &dir[2..]),
        _ => dir,
    };
    let p = PathBuf::from(&dir);
    if p.is_absolute() { p } else { cwd.join(p) }
}

/// The first command of `line` that the playbook's `[lanes] heavy` names, when there are lanes.
pub fn heavy_in(pb: &Playbook, line: &str) -> Option<String> {
    let rules = pb.lanes.as_ref()?;
    let patterns: Vec<regex::Regex> = rules.heavy.iter().filter_map(|h| regex::Regex::new(h).ok()).collect();
    commands_of(line).into_iter().filter(|c| !is_lane_run(c)).find(|c| patterns.iter().any(|p| p.is_match(c)))
}

/// A reminder for an agent about to build or test in a light worktree: the heavy command, the
/// worktree, its issue and the words to say. None anywhere else.
#[derive(Debug, Clone)]
pub struct Nudge {
    pub command: String,
    pub worktree: PathBuf,
    pub issue: Option<String>,
    pub message: String,
}

/// Whether the shell line an agent is about to run in `cwd` builds or tests in a light worktree.
pub fn nudge(config: &crate::Config, cwd: &str, line: &str) -> Option<Nudge> {
    let folder = folder_of(line, Path::new(cwd));
    let folder = folder.to_string_lossy();
    let repo = config.repo_for_dir(&folder)?;
    let (worktree, issue) = crate::playbook::current_worktree(&folder).ok()?;
    if !is_light(&worktree) {
        return None;
    }
    let pb = Playbook::load(Path::new(&repo.root)).ok()?;
    let command = heavy_in(&pb, line)?;
    let message = format!(
        "Nitely: `{command}` is about to build in a light worktree ({}), which has no build output on purpose. It still runs, but this time and next, use `run` with this command instead: it builds a snapshot of this worktree in a warm build lane and returns the result, in a fraction of the time and none of the disk.",
        worktree.display()
    );
    Some(Nudge { command, worktree, issue, message })
}

/// Whether the playbook asks for light worktrees.
pub fn wanted(pb: &Playbook) -> bool {
    pb.worktree.mode == WorktreeMode::Light
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::{Actor, IssueRecord};

    /// The cone rules `a_sparse_worktree_holds_the_playbook_s_and_the_issue_s_directories_and_widens`
    /// checks with a real sparse worktree.
    #[test]
    fn the_cone_holds_the_playbook_s_and_the_named_directories_and_widening_adds_only_new_ones() {
        assert_eq!(dir_for("./src/billing/", "tree").as_deref(), Some("src/billing"));
        assert_eq!(dir_for("src/billing/invoice.rs", "blob").as_deref(), Some("src/billing"));
        assert_eq!(dir_for("README.md", "blob"), None, "a root file: cone mode always has it");
        assert_eq!(dir_for("src/missing.rs", ""), None);
        let sparse = vec!["/crates/core/".to_string(), " ".to_string()];
        assert_eq!(cone(&sparse, ["crates/core/src".to_string(), "src/billing".to_string()]), ["crates/core", "src/billing"]);
        let before = vec!["crates/core".to_string()];
        assert_eq!(not_yet_in(&before, vec!["crates/core/src".into(), "src".into(), "src/billing".into()]), ["src"]);
    }
    use crate::playbook::{run_gate, start_task, PLAYBOOK_PATH};
    use crate::Db;
    use std::process::Command;

    fn repo(name: &str, playbook: &str) -> (PathBuf, PathBuf) {
        let base = crate::paths::unique_temp(name, "");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        for (path, text) in [("crates/core/src/lib.rs", "pub fn a() {}"), ("crates/cli/src/main.rs", "fn main() {}"), ("src/ui/Meter.tsx", "x"), ("docs/a.md", "d"), ("README.md", "r")] {
            let p = repo.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        std::fs::write(repo.join(PLAYBOOK_PATH), playbook).unwrap();
        let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&repo).args(args)).unwrap().status.success(), "{args:?}");
        sh(&["init", "-q", "-b", "main"]);
        sh(&["add", "-A"]);
        sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
        (base, repo)
    }

    fn issue(body: &str) -> IssueRecord {
        IssueRecord { key: "NIT-7".into(), title: "Meter colors".into(), body: body.into(), ..Default::default() }
    }

    #[test]
    fn a_light_worktree_runs_no_setup_and_gets_a_reference_tree_its_gate_sets_up_once() {
        // Setup writes a file, so whether it ran shows; the gate checks that it did.
        let check = if cfg!(windows) { "type setup.txt" } else { "cat setup.txt" };
        let (base, repo) = repo("nitely-light", &format!("[worktree]\nmode = \"light\"\ndir = \"../wt/{{key}}\"\nsetup = [\"echo set > setup.txt\"]\n[gate]\ncommands = [{check:?}]\n"));
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        let started = start_task(&db, &repo, &issue("Change src/ui/Meter.tsx"), "claude", &actor).unwrap();
        let wt = PathBuf::from(&started.worktree);
        assert!(started.created && started.light && started.setup.is_empty() && started.sparse.is_empty(), "{started:?}");
        assert!(!wt.join("setup.txt").exists() && wt.join("crates/cli/src/main.rs").exists(), "whole, and nothing installed");
        assert!(is_light(&wt));
        let reference = PathBuf::from(started.reference.clone().unwrap());
        assert_eq!(reference, base.join("wt").join(LANES_DIR).join("repo").join("ref"));
        assert!(reference.join("crates/core/src/lib.rs").exists());
        let listed = crate::git::list_worktrees(&repo.to_string_lossy()).unwrap();
        assert_eq!(listed.len(), 2, "the reference tree is nobody's work: {listed:?}");
        assert!(session_note(&Playbook::load(&repo).unwrap(), &repo, &wt).unwrap().contains(&reference.display().to_string()));

        // Until gates run in build lanes, the first gate runs setup, then not again.
        let gate = run_gate(&db, &repo, &wt, Some("NIT-7"), &actor).unwrap();
        assert!(gate.passed && gate.results[0].command.contains("setup again"), "{gate:?}");
        let gate = run_gate(&db, &repo, &wt, Some("NIT-7"), &actor).unwrap();
        assert_eq!(gate.results.len(), 1, "set up once: {gate:?}");

        // The base moves: so does the reference tree.
        std::fs::write(repo.join("docs/b.md"), "new").unwrap();
        let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&repo).args(args)).unwrap().status.success());
        sh(&["add", "-A"]);
        sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "more"]);
        std::fs::write(reference.join("docs/a.md"), "an edit nobody should make").unwrap();
        reference_tree(&Playbook::load(&repo).unwrap(), &repo).unwrap();
        assert!(reference.join("docs/b.md").exists());
        assert_eq!(std::fs::read_to_string(reference.join("docs/a.md")).unwrap(), "d", "read-only: edits there do not last");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn heavy_commands_in_a_light_worktree_with_lanes_get_a_reminder() {
        let (base, repo) = repo("nitely-nudge", "[worktree]\nmode = \"light\"\ndir = \"../wt/{key}\"\n[lanes]\ncount = 1\n");
        let db = Db::open(&base.join("t.db")).unwrap();
        let started = start_task(&db, &repo, &issue(""), "claude", &Actor::Agent("claude:test".into())).unwrap();
        let wt = PathBuf::from(&started.worktree);
        let config = crate::Config { repos: vec![crate::RepoConfig { name: "repo".into(), root: repo.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some("NIT".into()) }], ..Default::default() };
        let at = wt.to_string_lossy().into_owned();
        let n = nudge(&config, &at, "git status && cargo test -p core").unwrap();
        assert_eq!((n.command.as_str(), n.issue.as_deref()), ("cargo test -p core", Some("NIT-7")));
        assert!(n.message.contains("use `run`"), "{}", n.message);
        assert!(nudge(&config, &at, "cargo fmt && git diff").is_none(), "not a build");
        assert!(nudge(&config, &at, "npm testing-library").is_none(), "a word, not the command");
        assert!(nudge(&config, &at, "cat > notes.md <<'EOF'\ncargo test is what we run\nEOF").is_none(), "a heredoc's text is not a command");
        // `nitely run` is the lane form the nudge asks for, in either spelling (NIT-238).
        assert!(nudge(&config, &at, "nitely run --shell \"cargo test -p nitely-core && cargo clippy --workspace\"").is_none(), "a quoted lane command");
        assert!(nudge(&config, &at, "nitely run -- cargo test -p core").is_none(), "words after --");
        assert!(nudge(&config, &at, "C:/Users/sam/.cargo/bin/nitely.exe run -- npm run build").is_none(), "by its path");
        // What runs here after it still counts.
        assert_eq!(nudge(&config, &at, "nitely run -- cargo test && cargo clippy --workspace").map(|n| n.command).as_deref(), Some("cargo clippy --workspace"));
        assert_eq!(commands_of(r#"echo "a;b" && echo 'c|d' ; echo e\&f"#), [r#"echo "a;b""#, "echo 'c|d'", r"echo e\&f"]);
        // From the main checkout, a line that goes into the worktree first.
        let main = repo.to_string_lossy().into_owned();
        assert!(nudge(&config, &main, &format!("cd {} && npm run build", wt.display())).is_some());
        assert!(nudge(&config, &main, "cargo test").is_none(), "the main checkout is not light");
        assert!(session_note(&Playbook::load(&repo).unwrap(), &repo, &wt).unwrap().contains("through `run`"));

        // Without [lanes] there is no `run` to send anyone to.
        std::fs::write(repo.join(PLAYBOOK_PATH), "[worktree]\nmode = \"light\"\ndir = \"../wt/{key}\"\n").unwrap();
        assert!(nudge(&config, &at, "cargo test").is_none());
        assert!(!session_note(&Playbook::load(&repo).unwrap(), &repo, &wt).unwrap().contains("`run`"));
        // A playbook's own list replaces the defaults.
        let pb = Playbook::parse("[lanes]\nheavy = [\"^make\\\\b\"]\n").unwrap();
        assert_eq!(heavy_in(&pb, "cd x; make all").as_deref(), Some("make all"));
        assert!(heavy_in(&pb, "cargo test").is_none());
        // Even a pattern that is not anchored leaves `nitely run` alone.
        let pb = Playbook::parse("[lanes]\nheavy = [\"cargo test\"]\n").unwrap();
        assert!(heavy_in(&pb, "nitely run -- cargo test").is_none());
        assert_eq!(heavy_in(&pb, "time cargo test").as_deref(), Some("time cargo test"));
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_new_branch_starts_from_the_scheduled_base_not_the_tip() {
        let (base, repo) = repo("nitely-sched", "[worktree]\ndir = \"../wt/{key}\"\n[lanes]\nbase = \"every 4h\"\n");
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = Playbook::load(&repo).unwrap();
        let first = base_commit(&pb, &repo).unwrap();
        std::fs::write(repo.join("README.md"), "moved").unwrap();
        assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", "next"])).unwrap().status.success());
        let started = start_task(&db, &repo, &issue(""), "claude", &Actor::Agent("claude:test".into())).unwrap();
        let head = git(Path::new(&started.worktree), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head.trim(), first, "cut from the base, which moves every 4 hours, not from main's newer tip");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn full_stays_the_default_and_lanes_sit_beside_the_worktrees() {
        let (base, repo) = repo("nitely-full", "[worktree]\ndir = \"../wt/{key}\"\nsetup = [\"echo set > setup.txt\"]\n");
        let db = Db::open(&base.join("t.db")).unwrap();
        let started = start_task(&db, &repo, &issue(""), "claude", &Actor::Agent("claude:test".into())).unwrap();
        assert!(!started.light && started.reference.is_none() && PathBuf::from(&started.worktree).join("setup.txt").exists());
        assert!(!is_light(Path::new(&started.worktree)));
        let pb = Playbook::default();
        assert_eq!(lanes_dir(&pb, Path::new("/dev/my-app")), PathBuf::from("/dev/my-app-worktrees/.nitely-lanes/my-app"));
        // Repos sharing one worktree folder keep their lanes apart.
        let shared = Playbook::parse("[worktree]\ndir = \"../worktrees/{repo}-{key}\"\n").unwrap();
        assert_ne!(lanes_dir(&shared, Path::new("/dev/app")), lanes_dir(&shared, Path::new("/dev/api")));
        assert!(is_lane_path(r"C:\Code\my-app-worktrees\.nitely-lanes\lane-1") &&!is_lane_path("C:/Code/my-app-worktrees/app-3-x"));
        std::fs::remove_dir_all(base).ok();
    }
}
