//! Private terms: names, paths and patterns you keep out of a project's repos, such as another
//! project's name, a client or a local folder. The list lives in Nitely's data folder on this
//! machine only (listing the terms in a public repo would publish them). `submit` and `land` scan
//! what a branch adds, its added lines, new file paths and commit messages, and refuse it with
//! where each term was found. They never show the matched text or the list, so the list does
//! not reach an agent's context. Only the app reads and writes the list; no MCP tool or CLI
//! command shows it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::playbook::git;

/// One term per entry: plain text matches anywhere in a line, ignoring case, with `\` and `/`
/// treated alike; `re:` starts a regular expression. Blank entries and `#` comments are skipped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrivateTerms {
    /// Kept out of every project's repos.
    #[serde(default)]
    pub all: Vec<String>,
    /// Kept out of one project's repos, by project key.
    #[serde(default)]
    pub projects: BTreeMap<String, Vec<String>>,
}

/// Where a branch adds a private term. `term` is the entry's number in its list (1-based), so the
/// person can find it; the matched text itself is never reported.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    /// `src/app.ts:12 (commit 1a2b3c4)`, `path src/acme/x.ts (commit 1a2b3c4)`, or
    /// `commit 1a2b3c4 message line 2`.
    pub at: String,
    /// `every project` or the project key.
    pub list: String,
    pub term: usize,
}

const FILE: &str = "private-terms.json";
/// Hits listed in a refusal; the rest are counted.
const SHOWN: usize = 30;

pub fn load() -> Result<PrivateTerms> {
    load_from(&crate::paths::data_dir())
}

pub fn load_from(data: &Path) -> Result<PrivateTerms> {
    let path = data.join(FILE);
    if !path.exists() {
        return Ok(PrivateTerms::default());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    // Not the parse error itself: serde's message can quote a term.
    serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("{} is not valid JSON", path.display()))
}

/// Saves the list after checking every pattern compiles. Empty lists are dropped.
pub fn save(terms: &PrivateTerms) -> Result<PrivateTerms> {
    save_in(&crate::paths::data_dir(), terms)
}

pub fn save_in(data: &Path, terms: &PrivateTerms) -> Result<PrivateTerms> {
    let mut clean = PrivateTerms { all: tidy(&terms.all), projects: BTreeMap::new() };
    for (key, list) in &terms.projects {
        let list = tidy(list);
        if !list.is_empty() {
            clean.projects.insert(key.trim().to_uppercase(), list);
        }
    }
    compile("every project", &clean.all)?;
    for (key, list) in &clean.projects {
        compile(key, list)?;
    }
    std::fs::create_dir_all(data).with_context(|| format!("creating {}", data.display()))?;
    let path = data.join(FILE);
    std::fs::write(&path, serde_json::to_string_pretty(&clean)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(clean)
}

/// Moves a project's list to its new key, or drops it with the project (`to` None).
pub fn retarget(from: &str, to: Option<&str>) -> Result<bool> {
    retarget_in(&crate::paths::data_dir(), from, to)
}

pub fn retarget_in(data: &Path, from: &str, to: Option<&str>) -> Result<bool> {
    let mut terms = load_from(data)?;
    let Some(list) = terms.projects.remove(&from.trim().to_uppercase()) else { return Ok(false) };
    if let Some(to) = to {
        terms.projects.entry(to.trim().to_uppercase()).or_default().extend(list);
    }
    save_in(data, &terms)?;
    Ok(true)
}

fn tidy(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in list.iter().map(|t| t.trim()) {
        if !t.is_empty() && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

enum Matcher {
    /// Lowercase, with `\` as `/`.
    Text(String),
    Pattern(regex::Regex),
}

impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::Text(t) => line.to_lowercase().replace('\\', "/").contains(t.as_str()),
            Matcher::Pattern(re) => re.is_match(line),
        }
    }
}

/// The list's terms, each with its number. An error names the entry, never its text.
fn compile(list: &str, terms: &[String]) -> Result<Vec<(usize, Matcher)>> {
    let mut out = Vec::new();
    for (i, term) in terms.iter().enumerate() {
        let term = term.trim();
        if term.is_empty() || term.starts_with('#') {
            continue;
        }
        let matcher = match term.strip_prefix("re:") {
            Some(p) => Matcher::Pattern(regex::Regex::new(p.trim()).map_err(|_| anyhow::anyhow!("private term {} of {list} is not a valid regular expression", i + 1))?),
            None => Matcher::Text(term.to_lowercase().replace('\\', "/")),
        };
        out.push((i + 1, matcher));
    }
    Ok(out)
}

/// Settings that keep every added line visible and parsed the same way, whatever the user's git
/// config says: fixed `a/` and `b/` prefixes, no textconv, no external diff, unquoted paths, and
/// every file as text, even one `.gitattributes` calls binary.
const DIFF: &[&str] = &["--text", "--no-color", "--no-ext-diff", "--no-textconv", "--no-renames", "--src-prefix=a/", "--dst-prefix=b/"];
const PLAIN: &[&str] = &["-c", "core.quotepath=false", "-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false"];

fn git_plain_with_input(worktree: &Path, args: &[&str], input: &str) -> Result<String> {
    let all: Vec<&str> = PLAIN.iter().chain(args).copied().collect();
    crate::playbook::git_with_input(worktree, &all, input)
}

/// What the branch's commits add since `base` (a commit or branch), one commit at a time, since
/// pushing and landing keep every commit: a term added in one commit and removed in a later one is
/// still published. `(where, text)` for every line a commit adds (for a merge, the lines in none
/// of its parents, as a conflict resolution adds), the path of every file it adds (binary and empty
/// ones too), and every line of its message.
///
/// Three git calls for the whole branch, however many commits it has (NIT-221): the commits with
/// their parents and messages, then every commit's added files and every commit's patch, each
/// from one `diff-tree --stdin`.
fn added(worktree: &Path, base: &str) -> Result<Vec<(String, String)>> {
    // `<sha> <parents>` and the message, NUL between commits (a message holds no NUL).
    let log = git(worktree, &["log", "--reverse", "-z", "--no-show-signature", "--format=%H %P%n%B", &format!("{base}..HEAD")])?;
    let commits: Vec<(&str, Vec<&str>, &str)> = log
        .split('\0')
        .filter(|c| !c.trim().is_empty())
        .map(|c| {
            let (head, message) = c.split_once('\n').unwrap_or((c, ""));
            let mut ids = head.split_whitespace();
            (ids.next().unwrap_or(""), ids.collect(), message)
        })
        .collect();
    if commits.is_empty() {
        return Ok(vec![]);
    }

    // The files each commit adds: a line per parent (a root commit alone, against nothing), and a
    // merge adds a file only when none of its parents has it. NUL-separated, so no path is quoted
    // or split.
    let pairs: Vec<(&str, Option<&str>)> =
        commits.iter().flat_map(|(sha, parents, _)| if parents.is_empty() { vec![(*sha, None)] } else { parents.iter().map(|p| (*sha, Some(*p))).collect() }).collect();
    let input: String = pairs.iter().map(|(sha, parent)| parent.map_or(format!("{sha}\n"), |p| format!("{sha} {p}\n"))).collect();
    let names = git_plain_with_input(worktree, &["diff-tree", "--stdin", "-r", "-z", "--name-only", "--no-renames", "--diff-filter=A", "--root"], &input)?;
    let pair_shas: Vec<&str> = pairs.iter().map(|(sha, _)| *sha).collect();
    let mut against: Vec<Option<Vec<String>>> = vec![None; pairs.len()];
    let mut at = None;
    for token in names.split('\0').filter(|t| !t.is_empty()) {
        match next_header(token, &pair_shas, at) {
            Some(i) => {
                against[i] = Some(vec![]);
                at = Some(i);
            }
            None => {
                if let Some(paths) = at.and_then(|i| against[i].as_mut()) {
                    paths.push(token.to_string());
                }
            }
        }
    }

    // Every commit's patch, each after a line with its id. A merge's is the combined one.
    let mut args = vec!["diff-tree", "--stdin", "-p", "--unified=0", "--cc", "--root"];
    args.extend_from_slice(DIFF);
    let shas: Vec<&str> = commits.iter().map(|(sha, _, _)| *sha).collect();
    let input: String = shas.iter().map(|sha| format!("{sha}\n")).collect();
    let mut patches: Vec<String> = vec![String::new(); shas.len()];
    let mut at = None;
    for line in git_plain_with_input(worktree, &args, &input)?.lines() {
        match next_header(line, &shas, at) {
            Some(i) => at = Some(i),
            None => {
                if let Some(i) = at {
                    patches[i].push_str(line);
                    patches[i].push('\n');
                }
            }
        }
    }

    let mut out = Vec::new();
    for (n, (sha, _, message)) in commits.iter().enumerate() {
        let short = &sha[..sha.len().min(7)];
        for (k, text) in message.lines().enumerate() {
            out.push((format!("commit {short} message line {}", k + 1), text.to_string()));
        }
        // A parent with no block added nothing against it, so the commit adds nothing new.
        let mut lists = pair_shas.iter().zip(&against).filter(|(s, _)| *s == sha).map(|(_, paths)| paths.clone().unwrap_or_default());
        let mut paths = lists.next().unwrap_or_default();
        for also in lists {
            paths.retain(|p| also.contains(p));
        }
        out.extend(paths.into_iter().map(|f| (format!("path {f} (commit {short})"), f)));
        out.extend(patch_lines(&patches[n], short));
    }
    Ok(out)
}

/// The index of the input line whose block starts at `token`, `diff-tree --stdin`'s line with a
/// commit's id: the first line after `at` with that id. Only later lines are looked at, and no
/// path or patch line can name a later commit (its id depends on everything before it).
fn next_header(token: &str, shas: &[&str], at: Option<usize>) -> Option<usize> {
    let from = at.map_or(0, |i| i + 1);
    (from..shas.len()).find(|&i| shas[i] == token)
}

/// Added lines in one commit's patch (`--unified=0`, `a/` and `b/` prefixes). A merge's combined
/// patch marks each line with one column per parent; a line is new only when every column is `+`.
fn patch_lines(patch: &str, short: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut file: Option<String> = None;
    let mut columns = 1;
    let mut line = 0usize;
    let mut in_hunk = false;
    for l in patch.lines() {
        if l.starts_with("diff --git ") || l.starts_with("diff --cc ") || l.starts_with("diff --combined ") {
            (in_hunk, file) = (false, None);
        } else if !in_hunk && l.starts_with("+++ ") {
            file = header_path(l);
        } else if !in_hunk && l.starts_with("@@") {
            // `@@ -a,b +c,d @@`, or `@@@ -a -b +c,d @@@` for a merge: added lines count from c.
            columns = l.chars().take_while(|c| *c == '@').count().saturating_sub(1).max(1);
            line = l.split_whitespace().find_map(|p| p.strip_prefix('+')).and_then(|p| p.split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
            in_hunk = true;
        } else if in_hunk && l.starts_with("@@") && l.chars().take_while(|c| *c == '@').count() == columns + 1 {
            line = l.split_whitespace().find_map(|p| p.strip_prefix('+')).and_then(|p| p.split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
        } else if in_hunk {
            let (marks, text) = l.split_at(l.char_indices().nth(columns).map_or(l.len(), |(i, _)| i));
            if marks.contains('-') {
                continue;
            }
            if marks.len() == columns && marks.chars().all(|c| c == '+') {
                // Scanned even when the header could not be read: only the place is less exact.
                let f = file.as_deref().unwrap_or("a file");
                out.push((format!("{f}:{line} (commit {short})"), text.to_string()));
            }
            line += 1;
        }
    }
    out
}

/// The new side's path in a `+++ b/path` header, which git C-quotes (`+++ "b/a\tb"`) when the
/// path holds a tab, a quote, a backslash or a control character.
fn header_path(l: &str) -> Option<String> {
    let rest = l.strip_prefix("+++ ")?;
    let path = match rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(quoted) => unquote(quoted),
        None => rest.to_string(),
    };
    path.strip_prefix("b/").map(str::to_string)
}

/// Undoes git's C-style quoting: `\t`, `\n`, `\"`, `\\` and octal bytes such as `\303\251`.
fn unquote(q: &str) -> String {
    let mut bytes = Vec::new();
    let mut chars = q.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('t') => bytes.push(b'\t'),
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(7),
            Some('b') => bytes.push(8),
            Some('f') => bytes.push(12),
            Some('v') => bytes.push(11),
            Some(d @ '0'..='7') => {
                let mut n = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    if let Some(next) = chars.peek().and_then(|c| c.to_digit(8)) {
                        n = n * 8 + next;
                        chars.next();
                    }
                }
                bytes.push(n as u8);
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Every place the branch adds one of `terms`, checking the project's list and the list for
/// every project.
pub fn scan(terms: &PrivateTerms, project: Option<&str>, worktree: &Path, base: &str) -> Result<Vec<Hit>> {
    let mut lists = vec![("every project".to_string(), compile("every project", &terms.all)?)];
    if let Some(key) = project.map(|p| p.trim().to_uppercase()) {
        if let Some(list) = terms.projects.get(&key) {
            lists.push((key.clone(), compile(&key, list)?));
        }
    }
    if lists.iter().all(|(_, m)| m.is_empty()) {
        return Ok(vec![]);
    }
    Ok(hits(&lists, &added(worktree, base)?))
}

/// Each place in `added` (where, text) that holds a term of one of `lists` (NIT-537).
fn hits(lists: &[(String, Vec<(usize, Matcher)>)], added: &[(String, String)]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (at, text) in added {
        for (list, matchers) in lists {
            for (n, m) in matchers {
                if m.matches(text) {
                    hits.push(Hit { at: at.clone(), list: list.clone(), term: *n });
                }
            }
        }
    }
    hits
}

/// Refuses a branch that adds a private term, saying where and which entry, never the text.
pub fn check(project: Option<&str>, worktree: &Path, base: &str) -> Result<()> {
    check_with(&load()?, project, worktree, base)
}

pub fn check_with(terms: &PrivateTerms, project: Option<&str>, worktree: &Path, base: &str) -> Result<()> {
    match refusal(&scan(terms, project, worktree, base)?) {
        Some(msg) => bail!(msg),
        None => Ok(()),
    }
}

/// What a branch with `hits` is refused with (NIT-537): where and which entry of which list, a
/// few places at most, never the term. None without hits.
fn refusal(hits: &[Hit]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut msg = format!(
        "the branch adds {} you keep out of this repo. Every commit is pushed, so take {} out of the commits that add {}: \
         amend, or rebase and reword or squash them, then try again. The list is in Nitely's project settings; it is not shown here.",
        if hits.len() == 1 { "a private term".to_string() } else { format!("private terms in {} places", hits.len()) },
        if hits.len() == 1 { "it" } else { "them" },
        if hits.len() == 1 { "it" } else { "them" },
    );
    for h in hits.iter().take(SHOWN) {
        let list = if h.list == "every project" { "the list for every project".to_string() } else { format!("{}'s list", h.list) };
        msg.push_str(&format!("\n  {}: term {} of {list}", h.at, h.term));
    }
    if hits.len() > SHOWN {
        msg.push_str(&format!("\n  and {} more", hits.len() - SHOWN));
    }
    Some(msg)
}

/// Which term `text` holds, as `term n of <list>`, checking the project's list and the list for
/// every project; never the term itself. For text that leaves the machine another way than a
/// commit (the issue mirror).
pub fn find_in_text(terms: &PrivateTerms, project: Option<&str>, text: &str) -> Result<Option<String>> {
    let mut lists = vec![("the list for every project".to_string(), compile("every project", &terms.all)?)];
    if let Some(key) = project.map(|p| p.trim().to_uppercase()).filter(|k| !k.is_empty()) {
        if let Some(list) = terms.projects.get(&key) {
            lists.push((format!("{key}'s list"), compile(&key, list)?));
        }
    }
    for line in text.lines() {
        for (list, matchers) in &lists {
            if let Some((n, _)) = matchers.iter().find(|(_, m)| m.matches(line)) {
                return Ok(Some(format!("term {n} of {list}")));
            }
        }
    }
    Ok(None)
}

/// The data folder's list file, for the app to say where it is kept.
pub fn path() -> PathBuf {
    crate::paths::data_dir().join(FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which added lines hit which list's terms, and what the refusal says, over plain data
    /// (NIT-537): what the slower branch-scanning tests below decide.
    #[test]
    fn hits_name_the_place_and_entry_and_the_refusal_never_the_term() {
        let lists = vec![
            ("every project".to_string(), compile("every project", &["# a comment".into(), "Acme".into()]).unwrap()),
            ("APP".to_string(), compile("APP", &["re:secret-\\d+".into()]).unwrap()),
        ];
        let added = vec![
            ("src/a.rs:3 (commit 1a2b3c4)".to_string(), "// talks to ACME's api".to_string()),
            ("path docs/secret-42.md (commit 1a2b3c4)".to_string(), "docs/secret-42.md".to_string()),
            ("src/b.rs:9 (commit 1a2b3c4)".to_string(), "nothing here".to_string()),
        ];
        let found = hits(&lists, &added);
        let summary: Vec<(String, String, usize)> = found.iter().map(|h| (h.at.clone(), h.list.clone(), h.term)).collect();
        assert_eq!(summary, vec![
            ("src/a.rs:3 (commit 1a2b3c4)".into(), "every project".into(), 2),
            ("path docs/secret-42.md (commit 1a2b3c4)".into(), "APP".into(), 1),
        ]);
        assert_eq!(refusal(&[]), None);
        let said = refusal(&found).unwrap();
        assert!(said.contains("private terms in 2 places") && said.contains("src/a.rs:3 (commit 1a2b3c4): term 2 of the list for every project") && said.contains("term 1 of APP's list"), "{said}");
        assert!(!said.to_lowercase().contains("acme"), "the term itself is never echoed");
        let many: Vec<Hit> = (0..SHOWN + 2).map(|i| Hit { at: format!("a.rs:{i}"), list: "APP".into(), term: 1 }).collect();
        assert!(refusal(&many).unwrap().ends_with("and 2 more"));
    }
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = crate::playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn terms(all: &[&str], app: &[&str]) -> PrivateTerms {
        PrivateTerms {
            all: all.iter().map(|s| s.to_string()).collect(),
            projects: BTreeMap::from([("APP".to_string(), app.iter().map(|s| s.to_string()).collect())]),
        }
    }

    /// A copy of a repo whose branch `claude/app-1-x` adds terms in every way a branch can.
    fn branch_with_terms() -> PathBuf {
        crate::test_repo::copy("terms", |base| history(&base.join("repo"))).join("repo")
    }

    fn history(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        sh(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "old line mentions Bluebird already\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "init"]);
        sh(&dir, &["checkout", "-q", "-b", "claude/app-1-x"]);
        std::fs::write(dir.join("a.txt"), "old line mentions Bluebird already\nnew line\nsee C:\\Work\\Secret-Client\\notes\n").unwrap();
        std::fs::create_dir_all(dir.join("bluebird")).unwrap();
        std::fs::write(dir.join("bluebird/b.txt"), "clean\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "Port the change\n\nas in the BLUEBIRD repo"]);
        // A term a later commit takes out again is still in the pushed history.
        std::fs::write(dir.join("c.txt"), "draft for ticket-42\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "draft"]);
        std::fs::remove_file(dir.join("c.txt")).unwrap();
        sh(&dir, &["commit", "-q", "-am", "drop the draft"]);
        // An empty file and a binary one have no lines, only a path.
        std::fs::write(dir.join("bluebird.keep"), "").unwrap();
        std::fs::write(dir.join("bluebird.bin"), [0u8, 159, 146, 150]).unwrap();
        // A control character in a line hides nothing after it.
        std::fs::write(dir.join("d.txt"), "start\u{1e}ticket-7 end\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "more"]);
        // A file .gitattributes calls binary.
        std::fs::write(dir.join(".gitattributes"), "*.dat -diff\n").unwrap();
        std::fs::write(dir.join("notes.dat"), "see ticket-11\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "binary by attribute"]);
        // A merge whose resolution adds a line in neither parent.
        sh(&dir, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        std::fs::write(dir.join("e.txt"), "side\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "side"]);
        sh(&dir, &["checkout", "-q", "claude/app-1-x"]);
        sh(&dir, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
        std::fs::write(dir.join("e.txt"), "side\nresolved with ticket-9\n").unwrap();
        // And a file in neither parent, empty, so only its path says anything.
        std::fs::write(dir.join("bluebird-merge.keep"), "").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "merge side"]);
        // A name git has to quote, written straight to the index: Windows allows no `"` in a file name.
        std::fs::write(dir.join("blob.txt"), "see ticket-12\n").unwrap();
        let blob = sh(&dir, &["hash-object", "-w", "blob.txt"]).trim().to_string();
        std::fs::remove_file(dir.join("blob.txt")).unwrap();
        sh(&dir, &["-c", "core.protectNTFS=false", "update-index", "--add", "--cacheinfo", &format!("100644,{blob},we\"ird.txt")]);
        sh(&dir, &["commit", "-q", "-m", "odd name"]);
        // The user's diff settings change nothing.
        sh(&dir, &["config", "diff.noprefix", "true"]);
    }

    fn app_terms() -> PrivateTerms {
        terms(&["bluebird"], &["c:/work/secret-client", "re:ticket-\\d+"])
    }

    #[test]
    fn finds_added_lines_paths_and_messages() {
        let dir = branch_with_terms();
        let none = scan(&PrivateTerms::default(), Some("APP"), &dir, "main").unwrap();
        assert!(none.is_empty(), "no list, nothing to find");

        let t = app_terms();
        let hits = scan(&t, Some("app"), &dir, "main").unwrap();
        let at: Vec<&str> = hits.iter().map(|h| h.at.as_str()).collect();
        let first = sh(&dir, &["rev-parse", "--short=7", ":/Port the change"]).trim().to_string();
        let merge = sh(&dir, &["rev-parse", "--short=7", ":/merge side"]).trim().to_string();
        assert!(at.contains(&format!("path bluebird/b.txt (commit {first})").as_str()), "{at:?}");
        assert!(at.contains(&format!("a.txt:3 (commit {first})").as_str()), "a path written with backslashes: {at:?}");
        assert!(at.iter().any(|a| a.starts_with("c.txt:1 (commit ")), "added, then removed in a later commit: {at:?}");
        assert!(at.iter().any(|a| a.starts_with("path bluebird.keep ")) && at.iter().any(|a| a.starts_with("path bluebird.bin ")), "{at:?}");
        assert!(at.iter().any(|a| a.starts_with("d.txt:1 ")), "{at:?}");
        assert!(at.iter().any(|a| a.starts_with("e.txt:2 ")), "the merge's own line: {at:?}");
        assert!(!at.iter().any(|a| a.starts_with("e.txt:1 ")), "a line from a parent is not the merge's: {at:?}");
        assert!(at.iter().any(|a| a.starts_with("notes.dat:1 ")), "{at:?}");
        assert!(at.iter().any(|a| a.starts_with("we\"ird.txt:1 ")), "{at:?}");
        assert!(!at.iter().any(|a| a.starts_with("path bluebird.keep") && a.contains(&merge)), "the merge adds no file its parents have: {at:?}");
        assert!(at.contains(&format!("path bluebird-merge.keep (commit {merge})").as_str()), "a file only the merge adds: {at:?}");
        // Each place once, though a merge is read against each of its parents.
        let mut unique: Vec<(&str, &str, usize)> = hits.iter().map(|h| (h.at.as_str(), h.list.as_str(), h.term)).collect();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), hits.len(), "{at:?}");
        assert!(at.iter().any(|a| a.starts_with("commit ") && a.ends_with("message line 3")), "{at:?}");
        assert!(!at.iter().any(|a| a.starts_with("a.txt:1 ")), "a line the branch did not add: {at:?}");
        assert_eq!(hits.iter().find(|h| h.at.starts_with("a.txt:3 ")).map(|h| (h.list.as_str(), h.term)), Some(("APP", 1)));
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn scans_a_branch_that_shares_no_history_with_its_base() {
        // An orphan branch: its first commit has no parent, so it adds everything in it.
        let dir = crate::paths::unique_temp("nitely-terms-orphan", "");
        crate::test_repo::init(&dir, &[("a.txt", "clean\n")]);
        sh(&dir, &["checkout", "-q", "--orphan", "claude/app-2-y"]);
        sh(&dir, &["rm", "-q", "-r", "--cached", "."]);
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::fs::write(dir.join("bluebird.txt"), "first\nticket-5\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "start over"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "nothing but ticket-6"]);
        let root = sh(&dir, &["rev-parse", "--short=7", "HEAD~1"]).trim().to_string();
        let tip = sh(&dir, &["rev-parse", "--short=7", "HEAD"]).trim().to_string();
        let at: Vec<String> = scan(&app_terms(), Some("APP"), &dir, "main").unwrap().into_iter().map(|h| h.at).collect();
        assert_eq!(
            at,
            [format!("path bluebird.txt (commit {root})"), format!("bluebird.txt:2 (commit {root})"), format!("commit {tip} message line 1")],
            "the root commit's file and line, and the empty commit's message"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_quoted_headers() {
        assert_eq!(header_path("+++ b/src/a.ts").as_deref(), Some("src/a.ts"));
        assert_eq!(header_path("+++ \"b/we\\\"ird\\tname\\303\\251.txt\"").as_deref(), Some("we\"ird\tnameé.txt"));
        assert_eq!(header_path("+++ /dev/null"), None);
    }

    #[test]
    fn saves_tidily_refuses_bad_patterns_and_follows_a_new_key() {
        let data = crate::paths::unique_temp("nitely-terms-data", "");
        let saved = save_in(&data, &PrivateTerms {
            all: vec![" Bluebird ".into(), "".into(), "Bluebird".into()],
            projects: BTreeMap::from([("app".to_string(), vec!["x".to_string()]), ("WEB".to_string(), vec![" ".to_string()])]),
        })
        .unwrap();
        assert_eq!(saved.all, ["Bluebird"]);
        assert_eq!(saved.projects.keys().collect::<Vec<_>>(), ["APP"], "keys uppercase, empty lists dropped");
        assert_eq!(load_from(&data).unwrap(), saved);

        let err = save_in(&data, &terms(&[], &["fine", "re:(unclosed"])).unwrap_err().to_string();
        assert!(err.contains("term 2 of APP") && !err.contains("unclosed"), "{err}");
        assert_eq!(load_from(&data).unwrap(), saved, "a refused save changes nothing");

        assert!(retarget_in(&data, "APP", Some("AP")).unwrap());
        assert_eq!(load_from(&data).unwrap().projects.keys().collect::<Vec<_>>(), ["AP"]);
        assert!(retarget_in(&data, "AP", None).unwrap());
        assert!(load_from(&data).unwrap().projects.is_empty());
        assert!(!retarget_in(&data, "NONE", None).unwrap());
        std::fs::remove_dir_all(&data).ok();
    }
}
