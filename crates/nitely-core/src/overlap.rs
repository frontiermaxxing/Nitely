//! Keeping parallel agents out of each other's files. Every issue in flight holds the files its
//! branch changes (against the integration branch) and the paths its body names; `next_task`
//! prefers a ready issue that expects none of them over a slightly more urgent one that collides,
//! and says why it picked what it picked. Files every change touches (a playbook's
//! `crew.shared_files`) count for a quarter, so they do not hold everything back.

use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::db::Db;
use crate::issues::IssueRecord;
use crate::playbook::{self, Playbook};

/// What a match on a shared file weighs; any other file weighs 1.
const SHARED_WEIGHT: f64 = 0.25;

/// What each candidate collides with: its weight, and (issue holding it, file) pairs.
type Clash = HashMap<String, (f64, Vec<(String, String)>)>;

/// Extensions a named file must have to count: `issue.goal` or `github.com` in prose are not files.
const FILE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "toml", "md", "css", "scss", "html", "yml", "yaml", "py", "go", "java",
    "kt", "swift", "c", "h", "cc", "cpp", "hpp", "cs", "rb", "php", "sh", "ps1", "bat", "sql", "txt", "lock", "png", "svg", "xml", "gradle", "vue",
];

/// A collision at this weight or more holds an issue back.
const COLLIDES: f64 = 1.0;

/// An issue in flight and the files it holds.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Held {
    pub key: String,
    /// Its branch, when it has one.
    pub branch: Option<String>,
    /// Files its branch changes, then paths its body names: `/`-separated, as git prints them.
    pub files: Vec<String>,
}

/// What `next_task` picked, and why when in-flight work had a say.
#[derive(Debug, Clone, Serialize)]
pub struct Picked {
    pub issue: IssueRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The states in which an issue's files are taken.
fn in_flight(issue: &IssueRecord) -> bool {
    matches!(issue.state.as_str(), "In Progress" | "In Review" | "Approved" | "Needs You")
}

/// Path-like words in `text`: `src/App.tsx`, `crates/nitely-core/src/crew.rs`, `App.tsx`. Quotes,
/// backticks and trailing punctuation are dropped; backslashes become `/`.
pub fn paths_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';')) {
        let w = word.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?' | '*')).replace('\\', "/");
        let w = w.trim_start_matches("./").trim_end_matches('/');
        // `src/Inbox.tsx:42` and `src/Inbox.tsx:42:7` name the file.
        let mut w = w;
        while let Some((head, tail)) = w.rsplit_once(':') {
            if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
                break;
            }
            w = head;
        }
        if w.is_empty() || w.contains("://") || w.starts_with('/') {
            continue;
        }
        let name = w.rsplit('/').next().unwrap_or("");
        // `v1.2`, `e.g` and `issue.goal` are not files: a file has a known extension and a real stem.
        let has_ext = name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| (stem.len() >= 2 || w.contains('/')) && FILE_EXTENSIONS.contains(&ext.to_lowercase().as_str()));
        let dir = w.contains('/') && !w.contains('.') && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'));
        if (has_ext || dir) && !w.chars().any(|c| c.is_whitespace()) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// Whether `expected` (a path, a file name, or a label) names `file` (a repo path).
fn names(expected: &str, file: &str) -> bool {
    let (e, f) = (expected.to_lowercase(), file.to_lowercase());
    if e.contains('/') {
        let e = e.trim_end_matches('/');
        return f == e || f.starts_with(&format!("{e}/")) || f.ends_with(&format!("/{e}"));
    }
    let name = f.rsplit('/').next().unwrap_or(&f);
    let stem = name.split('.').next().unwrap_or(name);
    name == e || stem == e
}

/// The issues of `project` in flight and the files each holds. Repos whose git cannot be read add
/// nothing rather than failing the pick.
pub fn held(config: &crate::Config, db: &Db, project: &str) -> Result<Vec<Held>> {
    let issues: Vec<IssueRecord> = db.list_issues(Some(project), false)?.into_iter().filter(in_flight).collect();
    if issues.is_empty() {
        return Ok(vec![]);
    }
    let mut branch_files: HashMap<String, (String, Vec<String>)> = HashMap::new();
    let roots: Vec<&Path> = config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
        .map(|r| Path::new(&r.root))
        .collect();
    // Prose like `Overview/Pulse` reads as a folder; one counts only when a repo has it.
    let real = |p: &str| p.contains('.') || roots.iter().any(|r| r.join(p).is_dir());
    for repo in config.repos.iter().filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))) {
        let root = Path::new(&repo.root);
        let base = Playbook::load_with(root, config).map(|p| p.integration_branch().to_string()).unwrap_or_else(|_| repo.integration_branch.clone());
        let since = playbook::diff_base(root, &base);
        let refs = playbook::git(root, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"]).unwrap_or_default();
        for branch in refs.lines().map(str::trim) {
            let Some(key) = crate::git::parse_branch(branch).1 else { continue };
            let Some(issue) = issues.iter().find(|i| i.key.eq_ignore_ascii_case(&key)) else { continue };
            // -z: names as they are, not quoted and escaped as git prints names outside ASCII.
            let diff = playbook::git(root, &["diff", "--name-only", "-z", "--no-renames", &format!("{since}...{branch}")]).unwrap_or_default();
            let entry = branch_files.entry(issue.key.clone()).or_insert_with(|| (branch.to_string(), vec![]));
            entry.1.extend(diff.split('\0').map(str::trim).filter(|l| !l.is_empty()).map(str::to_string));
        }
    }
    Ok(issues
        .into_iter()
        .map(|i| {
            let (branch, mut files) = branch_files.remove(&i.key).map_or((None, vec![]), |(b, f)| (Some(b), f));
            let mut seen: BTreeSet<String> = files.iter().map(|f| f.to_lowercase()).collect();
            for p in paths_in(&format!("{}\n{}", i.title, i.body)).into_iter().filter(|p| real(p)) {
                if seen.insert(p.to_lowercase()) {
                    files.push(p);
                }
            }
            Held { key: i.key, branch, files }
        })
        .collect())
}

/// What a candidate expects to touch: paths its title and body name, and its labels.
fn expected(issue: &IssueRecord) -> Vec<String> {
    let mut e = paths_in(&format!("{}\n{}", issue.title, issue.body));
    e.extend(issue.labels.iter().map(|l| l.trim().to_lowercase()).filter(|l| l.len() >= 3));
    e
}

/// How much `issue` collides with work in flight, and the files that collide, by the issue that
/// holds them.
fn collisions(issue: &IssueRecord, held: &[Held], shared: &[String]) -> (f64, Vec<(String, String)>) {
    let mut weight = 0.0;
    let mut hits: Vec<(String, String)> = Vec::new();
    for e in expected(issue) {
        for h in held.iter().filter(|h| !h.key.eq_ignore_ascii_case(&issue.key)) {
            // Either side may be the wider one: a held folder or file name catches a full path.
            for f in h.files.iter().filter(|f| names(&e, f) || names(f, &e)) {
                if hits.iter().any(|(k, x)| k == &h.key && x == f) {
                    continue;
                }
                // Shared entries are full paths; a held bare `types.ts` is the same file.
                // (Only a bare name: a held folder is not one shared file.)
                let shared_file = shared.iter().any(|s| names(s, f) || (!f.contains('/') && names(f, s)));
                weight += if shared_file { SHARED_WEIGHT } else { 1.0 };
                hits.push((h.key.clone(), f.clone()));
            }
        }
    }
    (weight, hits)
}

fn describe(hits: &[(String, String)]) -> String {
    let mut by_key: Vec<(String, Vec<String>)> = Vec::new();
    for (k, f) in hits {
        match by_key.iter_mut().find(|(x, _)| x == k) {
            Some((_, fs)) => fs.push(f.clone()),
            None => by_key.push((k.clone(), vec![f.clone()])),
        }
    }
    by_key.into_iter().map(|(k, fs)| format!("{k} holds {}", fs.join(", "))).collect::<Vec<_>>().join("; ")
}

/// Chooses among `candidates` (key, urgency rank, number; most urgent first) given what each
/// collides with. An urgent (rank 1) issue is never passed over; otherwise a clear issue at most
/// one urgency level below a colliding one is taken instead of it.
fn choose(candidates: &[(String, i64, i64)], clash: &Clash) -> Option<(String, Option<String>)> {
    let (top, top_rank, _) = candidates.first()?;
    let collides = |k: &str| clash.get(k).is_some_and(|(w, _)| *w >= COLLIDES);
    let hits = |k: &str| clash.get(k).map(|(_, h)| describe(h)).unwrap_or_default();
    if !collides(top) {
        return Some((top.clone(), None));
    }
    if *top_rank <= 1 {
        return Some((top.clone(), Some(format!("{top} is urgent, so it was taken though {}", hits(top)))));
    }
    match candidates.iter().find(|(k, r, _)| *r <= top_rank + 1 && !collides(k)) {
        Some((k, _, _)) => Some((k.clone(), Some(format!("took {k} before the more urgent {top}, which would collide: {}", hits(top))))),
        None => Some((top.clone(), Some(format!("{}; no other ready issue about as urgent is clear of work in flight", hits(top))))),
    }
}

/// `Db::next_task`, preferring an issue clear of the files work in flight holds (see `choose`).
/// The playbook's `crew.shared_files` of the project's repos count for a quarter.
pub fn next_task(config: &crate::Config, db: &mut Db, project: &str, agent: &str, except: &[String]) -> Result<Option<Picked>> {
    // One pick at a time: reading what is held and claiming happen under one lock, so a second
    // caller sees the first one's claim (its body's paths) as work in flight.
    let _turn = PickLock::take(db);
    let held = held(config, db, project)?;
    let shared: Vec<String> = config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
        .filter_map(|r| Playbook::load_with(Path::new(&r.root), config).ok())
        .flat_map(|p| p.crew.shared_files)
        .collect();
    let clash: Clash = if held.iter().all(|h| h.files.is_empty()) {
        HashMap::new()
    } else {
        db.list_issues(Some(project), false)?.iter().map(|i| (i.key.clone(), collisions(i, &held, &shared))).collect()
    };
    let mut why = None;
    let issue = db.next_task_choosing(project, agent, except, |candidates| {
        let (key, reason) = choose(candidates, &clash)?;
        why = reason;
        Some(key)
    })?;
    Ok(issue.map(|issue| Picked { issue, why }))
}

/// A lock file beside the database, held while one caller reads what is in flight and claims.
/// Taken with a short wait; one older than `STALE_SECS` was left by a caller that died. Without a
/// database file (in memory) there is nothing to share and no lock.
struct PickLock(Option<std::path::PathBuf>);

impl PickLock {
    const STALE_SECS: u64 = 30;

    fn take(db: &Db) -> PickLock {
        let Some(path) = db.conn.path().filter(|p| !p.is_empty()).map(|p| std::path::PathBuf::from(format!("{p}.next-task.lock"))) else {
            return PickLock(None);
        };
        for _ in 0..200 {
            if std::fs::OpenOptions::new().write(true).create_new(true).open(&path).is_ok() {
                return PickLock(Some(path));
            }
            let stale = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > Self::STALE_SECS);
            if stale {
                std::fs::remove_file(&path).ok();
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Ten seconds without a turn: go ahead rather than fail the call.
        PickLock(None)
    }
}

impl Drop for PickLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            std::fs::remove_file(path).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_paths_in_prose() {
        let text = "Fix the badge in `src/Inbox.tsx` and crates\\nitely-core\\src\\crew.rs. See https://x.dev/a.b, e.g. App.tsx or src/ui/. Not v1.2 nor a sentence.";
        assert_eq!(paths_in(text), ["src/Inbox.tsx", "crates/nitely-core/src/crew.rs", "App.tsx", "src/ui"]);
        assert_eq!(paths_in("see src/Inbox.tsx:42 and src/crew.rs:10:4, then App.tsx:7."), ["src/Inbox.tsx", "src/crew.rs", "App.tsx"]);
        // Prose that looks like paths but names no file.
        let prose = "issue.goal on github.com/frontiermaxxing/Nitely and github.event.pull_request.head.repo.fork";
        assert_eq!(paths_in(prose), Vec::<String>::new());
    }

    #[test]
    fn a_name_matches_paths_folders_file_names_and_labels() {
        assert!(names("src/Inbox.tsx", "src/Inbox.tsx"));
        assert!(names("src/ui", "src/ui/StateDot.tsx"));
        assert!(names("Inbox.tsx", "src/Inbox.tsx"));
        assert!(names("inbox", "src/Inbox.tsx"), "a label names the file it is the stem of");
        assert!(!names("src/ui", "src/uiextra.tsx"));
        assert!(!names("box", "src/Inbox.tsx"));
    }

    fn issue(key: &str, priority: i64, body: &str, labels: &[&str]) -> IssueRecord {
        IssueRecord { key: key.into(), title: format!("{key} title"), body: body.into(), priority, labels: labels.iter().map(|l| l.to_string()).collect(), ..Default::default() }
    }

    fn clash_of(issues: &[IssueRecord], held: &[Held], shared: &[String]) -> Clash {
        issues.iter().map(|i| (i.key.clone(), collisions(i, held, shared))).collect()
    }

    #[test]
    fn prefers_a_clear_issue_over_a_slightly_more_urgent_one_that_collides() {
        let held = vec![Held { key: "NIT-40".into(), branch: Some("claude/nit-40-x".into()), files: vec!["src/Inbox.tsx".into(), "src/types.ts".into()] }];
        let shared = vec!["src/types.ts".to_string()];
        let issues = [
            issue("NIT-50", 2, "The inbox badge in src/Inbox.tsx is off by one", &[]),
            issue("NIT-51", 3, "Add a field to src/types.ts and src/Pulse.tsx", &[]),
            issue("NIT-52", 4, "Rename a column in src/Backlog.tsx", &[]),
        ];
        let clash = clash_of(&issues, &held, &shared);
        let cands = |ranks: &[(&str, i64)]| ranks.iter().enumerate().map(|(n, (k, r))| (k.to_string(), *r, n as i64)).collect::<Vec<_>>();

        let (key, why) = choose(&cands(&[("NIT-50", 2), ("NIT-51", 3), ("NIT-52", 4)]), &clash).unwrap();
        assert_eq!(key, "NIT-51", "a shared file alone does not hold an issue back");
        assert_eq!(why.unwrap(), "took NIT-51 before the more urgent NIT-50, which would collide: NIT-40 holds src/Inbox.tsx");

        // Nothing within one level of urgency is clear: the most urgent is taken, and says so.
        let (key, why) = choose(&cands(&[("NIT-50", 2), ("NIT-52", 4)]), &clash).unwrap();
        assert_eq!(key, "NIT-50");
        assert!(why.unwrap().contains("no other ready issue about as urgent"));

        // An urgent issue is never passed over.
        let (key, why) = choose(&cands(&[("NIT-50", 1), ("NIT-51", 1)]), &clash).unwrap();
        assert_eq!(key, "NIT-50");
        assert!(why.unwrap().starts_with("NIT-50 is urgent"));

        // A shared file held under its bare name is still shared.
        let bare = vec![Held { key: "NIT-42".into(), branch: None, files: vec!["types.ts".into()] }];
        let touching = [issue("NIT-56", 3, "Add a field to src/types.ts", &[])];
        assert_eq!(clash_of(&touching, &bare, &shared)["NIT-56"].0, SHARED_WEIGHT);
        // A held folder holding a shared file still holds the rest at full weight.
        let folder = vec![Held { key: "NIT-43".into(), branch: None, files: vec!["src".into()] }];
        let button = [issue("NIT-57", 3, "Fix src/Button.tsx", &[])];
        assert_eq!(clash_of(&button, &folder, &shared)["NIT-57"].0, 1.0);

        // A held folder or bare file name catches a candidate's full path.
        let wide = vec![Held { key: "NIT-41".into(), branch: None, files: vec!["src/ui".into(), "Backlog.tsx".into()] }];
        let named = [issue("NIT-54", 3, "Fix src/ui/StateDot.tsx", &[]), issue("NIT-55", 3, "Fix src/Backlog.tsx", &[])];
        let c = clash_of(&named, &wide, &[]);
        assert_eq!((c["NIT-54"].0, c["NIT-55"].0), (1.0, 1.0));

        // Labels name files too; nothing in flight, nothing said.
        let labelled = [issue("NIT-53", 3, "Tidy the list", &["inbox"])];
        assert_eq!(clash_of(&labelled, &held, &shared)["NIT-53"].0, 1.0);
        assert_eq!(choose(&cands(&[("NIT-52", 4)]), &HashMap::new()).unwrap(), ("NIT-52".to_string(), None));
    }

    #[test]
    fn next_task_reads_held_files_from_branches_and_says_why() {
        let base = crate::paths::unique_temp("nitely-overlap", "");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let git = |args: &[&str]| assert!(playbook::run_git(std::process::Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), "[crew]\nshared_files = [\"src/types.ts\"]\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        // NIT-1 is in flight on a branch that changes src/Inbox.tsx.
        git(&["checkout", "-q", "-b", "claude/nit-1-inbox"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/Inbox.tsx"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "inbox"]);
        git(&["checkout", "-q", "main"]);

        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let human = crate::Actor::Human;
        for (title, body, priority) in [
            ("Inbox work", "Started", 3),
            ("Badge is off", "In src/Inbox.tsx the badge counts twice", 2),
            ("Backlog column", "Rename it in src/Backlog.tsx", 3),
        ] {
            let new = crate::NewIssue { project: "NIT".into(), title: title.into(), body: body.into(), kind: "bug".into(), source: "human".into(), evidence: "seen".into(), priority, ..Default::default() };
            db.create_issue(&new, &human).unwrap();
        }
        db.move_issue("NIT-1", "In Progress", None, None, &human).unwrap();
        let config = crate::Config {
            repos: vec![crate::RepoConfig { name: "repo".into(), root: repo.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some("NIT".into()) }],
            ..Default::default()
        };
        let held = held(&config, &db, "NIT").unwrap();
        assert_eq!(held, [Held { key: "NIT-1".into(), branch: Some("claude/nit-1-inbox".into()), files: vec!["src/Inbox.tsx".into()] }]);

        let picked = next_task(&config, &mut db, "NIT", "codex:x", &[]).unwrap().unwrap();
        assert_eq!(picked.issue.key, "NIT-3");
        assert_eq!(picked.why.as_deref(), Some("took NIT-3 before the more urgent NIT-2, which would collide: NIT-1 holds src/Inbox.tsx"));
        let picked = next_task(&config, &mut db, "NIT", "codex:y", &[]).unwrap().unwrap();
        assert_eq!(picked.issue.key, "NIT-2", "with nothing clear left, the colliding one is still handed out");
        let lock = std::path::PathBuf::from(format!("{}.next-task.lock", base.join("t.db").display()));
        assert!(!lock.exists(), "the pick's turn is given back");
        std::fs::remove_dir_all(base).ok();
    }
}
