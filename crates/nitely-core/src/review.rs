//! Cross-vendor review: another agent reads the branch before it can land (Codex reviews Claude's
//! branches, Claude reviews Codex's). Every reviewer gets the same prompt and must end with the same
//! closing lines, so a review always has an explicit result; one without them is unfinished, not
//! clean. The reviewers, their commands and the yes/no questions come from the playbook.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::playbook::{shell_free_command, tail};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewRules {
    /// Who reviews whose branches: the branch's vendor to the reviewer. `*` covers the rest.
    pub reviewer: BTreeMap<String, String>,
    /// How to run each reviewer. The prompt goes on stdin; an argument `{output}` is replaced with a
    /// file the reviewer writes its reply to, otherwise the reply is read from stdout. Each runs as a
    /// fresh read-only session with no memory of writing the change, so a same-vendor review is
    /// still a second reading. Cursor has no default: `cursor-agent -p` can write and run anything,
    /// and its read-only `--mode ask` has not been shown to run the `git diff` a review needs.
    pub commands: BTreeMap<String, Vec<String>>,
    /// Yes-or-no questions the reviewer answers; a yes sends the change to a human. Use them for
    /// what path rules cannot see, like money logic in a file whose name does not say so.
    pub flags: Vec<Flag>,
    /// Extra instructions for the reviewer, e.g. what matters most in this codebase.
    pub focus: String,
    pub timeout_minutes: u64,
    /// A review by the author's own vendor (because no other reviewer is installed, or the
    /// `reviewer` map says so) sends the change to a human instead of approving it.
    pub require_cross_vendor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flag {
    /// An uppercase name, e.g. MONEY_OR_RELEASE; it becomes the reviewer's closing line.
    pub name: String,
    /// When the answer is yes.
    pub question: String,
    /// Shown to the human when it is yes; defaults to the question.
    #[serde(default)]
    pub reason: String,
}

impl Default for ReviewRules {
    fn default() -> Self {
        let pairs = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        ReviewRules {
            reviewer: pairs(&[("claude", "codex"), ("codex", "claude"), ("*", "claude")]),
            commands: BTreeMap::from([
                (
                    "claude".to_string(),
                    args(&["claude", "-p", "--allowedTools", "Read,Grep,Glob,Bash(git diff:*),Bash(git log:*),Bash(git show:*)"]),
                ),
                // Not --ephemeral: the session log is how a review's tokens count toward the issue (usage.rs).
                ("codex".to_string(), args(&["codex", "exec", "--sandbox", "read-only", "-o", "{output}", "-"])),
            ]),
            flags: vec![],
            focus: String::new(),
            timeout_minutes: 20,
            require_cross_vendor: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub priority: u8,
    pub text: String,
    /// Raised in an earlier round and not fixed since; Nitely carries it forward, the reviewer
    /// does not list it again.
    #[serde(default)]
    pub carried: bool,
    /// The priority it was raised at, when it is carried lower because the code it cites was not on
    /// the branch: a later round that confirms it open restores this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raised_as: Option<u8>,
}

/// Appended to a carried finding whose cited code was not on the branch.
const UNCONFIRMED: &str = " (unconfirmed: the code it cites is not on the branch now)";

/// What the previous review of the branch found, so the next one checks those findings first and
/// reviews what changed since instead of starting over.
#[derive(Debug, Clone)]
pub struct PreviousRound {
    /// The commit that round reviewed.
    pub commit: String,
    pub findings: Vec<Finding>,
    /// Whether `commit` is still in the branch's history. After a rebase it is not, so there is no
    /// diff since that round and the whole branch is reviewed again.
    pub in_history: bool,
}

/// A previous round's finding and what this round says about it: `fixed`; `open`, with current code
/// that still shows it; `unchecked` when the reviewer did not say; or `unconfirmed` when it was
/// called open (or left unchecked) but nothing it points at is on the branch any more.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recheck {
    pub priority: u8,
    pub text: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    /// `pass` (no P0 or P1 findings) or `changes`.
    pub verdict: String,
    pub findings: Vec<Finding>,
    /// The flags the reviewer answered yes to.
    pub flagged: Vec<String>,
    pub summary: String,
    pub seconds: u64,
    /// The previous round's findings, each marked fixed, open or unchecked. Open and unchecked
    /// ones are also in `findings`, marked carried.
    #[serde(default)]
    pub previous: Vec<Recheck>,
    /// How independent the review was, when it was not the playbook's choice of another vendor:
    /// e.g. `same vendor: codex not installed`. Empty otherwise.
    #[serde(default)]
    pub note: String,
}

/// The reviewer `choose` picked for a branch.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub reviewer: String,
    /// The reviewer is known to be another vendor than the author's.
    pub independent: bool,
    /// As `Review::note`.
    pub note: String,
}

impl ReviewRules {
    pub fn reviewer_for(&self, vendor: &str) -> Option<&str> {
        self.reviewer.get(vendor).or_else(|| self.reviewer.get("*")).map(String::as_str)
    }

    /// Whether the reviewer's CLI is on this machine. A relative path counts from `worktree`,
    /// where the reviewer runs.
    pub fn is_installed(&self, reviewer: &str, worktree: &Path) -> bool {
        self.commands.get(reviewer).and_then(|c| c.first()).is_some_and(|p| program_exists(&resolve_program(p), worktree))
    }

    /// Who reviews a branch by `vendor`, among the reviewers `installed` says can run. The
    /// `reviewer` map is the preference; when that one is missing, another vendor that is
    /// installed reviews, and only when none is, the author's own vendor in a fresh session.
    pub fn choose(&self, vendor: &str, installed: &dyn Fn(&str) -> bool) -> Result<Choice> {
        let preferred = self.reviewer_for(vendor).filter(|p| self.commands.contains_key(*p));
        let author = self.author_vendor(vendor);
        let same = |r: &str| author.is_some_and(|a| a == r);
        let others = || self.commands.keys().map(String::as_str).filter(move |r| Some(*r) != preferred);
        let candidates: Vec<&str> =
            preferred.into_iter().chain(others().filter(|r| !same(r))).chain(others().filter(|r| same(r))).collect();
        let Some(reviewer) = candidates.iter().copied().find(|r| installed(r)) else {
            if candidates.is_empty() {
                bail!("the playbook has no reviewer commands (add review.commands)");
            }
            let wanted: Vec<String> = candidates.iter().map(|r| install_hint(r, self)).collect();
            bail!(
                "no reviewer can run on this machine: install {} (or point review.commands in the playbook at a reviewer you have)",
                wanted.join(" or ")
            );
        };
        let missing = self.reviewer_for(vendor).filter(|p| *p != reviewer).map(|m| format!("{m} not installed"));
        let note = match (author, missing) {
            (Some(a), Some(m)) if a == reviewer => format!("same vendor: {m}"),
            (Some(a), None) if a == reviewer => "same vendor".to_string(),
            (None, Some(m)) => format!("author's vendor unknown; {m}"),
            (None, None) => "author's vendor unknown".to_string(),
            (_, m) => m.unwrap_or_default(),
        };
        Ok(Choice { reviewer: reviewer.to_string(), independent: self.is_independent(reviewer, vendor), note })
    }

    /// The branch's vendor when it names one Nitely knows (an agent vendor or a reviewer), so a
    /// branch like `feature/x` does not pass for another vendor's.
    fn author_vendor<'a>(&self, vendor: &'a str) -> Option<&'a str> {
        (KNOWN_VENDORS.contains(&vendor) || self.commands.contains_key(vendor)).then_some(vendor)
    }

    /// Whether `reviewer` is known to be another vendor than the branch's `vendor`.
    pub fn is_independent(&self, reviewer: &str, vendor: &str) -> bool {
        self.author_vendor(vendor).is_some_and(|a| a != reviewer)
    }

    /// Why a review needs a human because of who ran it: the playbook requires another vendor's
    /// review, and this one was by the author's own vendor or by one the branch does not rule out.
    /// Decided from the branch, so it holds for reviews recorded before notes were kept.
    pub fn independence_reason(&self, reviewer: &str, vendor: &str, note: &str) -> Option<String> {
        if !self.require_cross_vendor || self.is_independent(reviewer, vendor) {
            return None;
        }
        let note = if !note.is_empty() {
            note.to_string()
        } else if self.author_vendor(vendor).is_some() {
            "same vendor".to_string()
        } else {
            "author's vendor unknown".to_string()
        };
        Some(format!("{reviewer} review ({note}), and this playbook requires a review by another vendor"))
    }

    /// The reviewer's instructions. With a previous round, it first checks that round's findings,
    /// then reviews what changed since, and raises new problems in code that round already saw
    /// only at P0 or P1, so rounds converge instead of finding fresh edge cases each time.
    pub fn prompt(&self, base: &str, previous: Option<&PreviousRound>) -> String {
        let mut p = match previous {
            None => format!(
                "Review the changes on this branch against {base}. Run `git diff {base}...HEAD`, read the changed code and \
its callers, and look for correctness bugs and regressions. Skip style. Do not edit anything.\n"
            ),
            Some(prev) if prev.in_history => format!(
                "This branch was reviewed before, at commit {c}. Run `git diff {base}...HEAD` for the whole change and \
`git diff {c} HEAD` for what changed since that review. Read the changed code and its callers, and look for \
correctness bugs and regressions. Skip style. Do not edit anything.\n",
                c = prev.commit
            ),
            Some(prev) => format!(
                "This branch was reviewed before, at commit {c}, and has been rebased since, so review the whole change \
again. Run `git diff {base}...HEAD`, read the changed code and its callers, and look for correctness bugs and \
regressions. Skip style. Do not edit anything.\n",
                c = prev.commit
            ),
        };
        if let Some(prev) = previous.filter(|p| !p.findings.is_empty()) {
            p.push_str("\nFirst, check each finding of that review against the current code:\n");
            for (i, f) in prev.findings.iter().enumerate() {
                p.push_str(&format!("{}. [P{}] {}\n", i + 1, f.priority, f.text));
            }
            p.push_str(
                "Answer each on its own line as `PREVIOUS <n>: fixed` or `PREVIOUS <n>: open <file>:<line> `<code>``, \
where file, line and code are from the current branch and still show the problem. The code a finding cites may have \
moved or gone: check the problem itself, not the old line. If it no longer happens, it is fixed; an open answer that \
cites no current code is not counted as open. Do not list an open one again as a defect: it stays on record by itself.\n",
            );
        }
        if let Some(prev) = previous.filter(|p| p.in_history) {
            p.push_str(&format!(
                "\nThen review what changed since {c} and report any defect in it. The rest of the branch was already \
reviewed: raise something new there only if it is P0 or P1.\n",
                c = prev.commit
            ));
        }
        if !self.focus.trim().is_empty() {
            p.push_str(&format!("\n{}\n", self.focus.trim()));
        }
        p.push_str(
            "\nReport each defect on its own line as:\n- [P0] title: file, symbol: what goes wrong and when\n\
using P0 for data loss or security, P1 for a definite bug in normal use, P2 for an edge case, P3 for a minor issue.\n\n",
        );
        p.push_str(&format!("Then end your reply with exactly these {} lines:\nVERDICT: <findings or clean>\n", 1 + self.flags.len()));
        for f in &self.flags {
            p.push_str(&format!("{}: <yes or no>\n", f.name));
        }
        p.push_str("VERDICT is findings if you listed any new defect, otherwise clean.");
        for f in &self.flags {
            p.push_str(&format!(" {} is yes if {}", f.name, f.question.trim_end_matches('.')));
            p.push('.');
        }
        if !self.flags.is_empty() {
            p.push_str(" Answer yes when unsure.");
        }
        p
    }

    /// Parses a reviewer's reply. Fails when a closing line is missing, or when it claims findings
    /// but lists none (and marks no previous finding open): both mean the review did not finish.
    /// Previous findings it marks open, or does not mention, are carried into `findings` with their
    /// old priority, so the verdict still counts them.
    ///
    /// With `worktree`, a carried finding must point at code that is on the branch now: an open
    /// answer must cite a current file:line (and any code it quotes must be there), and a finding
    /// left unchecked must still name a file that exists. One that does not is `unconfirmed` and is
    /// carried at P2 at most, so a branch whose fix moved or removed the cited code can pass.
    pub fn parse(&self, reviewer: &str, text: &str, seconds: u64, previous: Option<&PreviousRound>, worktree: Option<&Path>) -> Result<Review> {
        let field = |name: &str, values: &[&str]| -> Option<String> {
            let prefix = format!("{}:", name.to_lowercase());
            text.lines()
                .filter_map(|l| {
                    let l = l.trim().to_lowercase();
                    l.strip_prefix(&prefix).map(|v| v.trim().trim_matches('*').trim().to_string())
                })
                .next_back()
                .filter(|v| values.contains(&v.as_str()))
        };
        let Some(verdict) = field("VERDICT", &["findings", "clean"]) else {
            bail!("the {reviewer} review did not end with a VERDICT line, so it is unfinished:\n{}", tail_lines(text));
        };
        let mut flagged = Vec::new();
        for f in &self.flags {
            match field(&f.name, &["yes", "no"]).as_deref() {
                Some("yes") => flagged.push(f.name.clone()),
                Some(_) => {}
                None => bail!("the {reviewer} review did not answer {}, so it is unfinished:\n{}", f.name, tail_lines(text)),
            }
        }
        let mut findings = parse_findings(text);
        let statuses = parse_previous(text);
        let code = worktree.map(CurrentCode::new);
        let previous: Vec<Recheck> = previous
            .map(|prev| {
                prev.findings
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let (status, said) = statuses.get(&(i + 1)).cloned().unwrap_or_else(|| ("unchecked".into(), String::new()));
                        // Open needs the reviewer's own pointer at current code; unchecked, the finding's.
                        let evidence = if status == "open" { said.as_str() } else { f.text.as_str() };
                        let status = match &code {
                            Some(code) if status != "fixed" && !code.confirms(evidence) => "unconfirmed".to_string(),
                            _ => status,
                        };
                        // Judged at the priority it was raised at, whatever an earlier round carried it at.
                        let text = f.text.strip_suffix(UNCONFIRMED).unwrap_or(&f.text).to_string();
                        Recheck { priority: f.raised_as.unwrap_or(f.priority), text, status }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if verdict == "findings" && findings.is_empty() && !previous.iter().any(|r| r.status == "open") {
            bail!("the {reviewer} review says it has findings but lists none as `- [Pn] ...`:\n{}", tail_lines(text));
        }
        for r in previous.iter().filter(|r| r.status != "fixed") {
            let finding = if r.status == "unconfirmed" {
                let priority = r.priority.max(2);
                Finding { priority, text: format!("{}{UNCONFIRMED}", r.text), carried: true, raised_as: (priority != r.priority).then_some(r.priority) }
            } else {
                Finding { priority: r.priority, text: r.text.clone(), carried: true, raised_as: None }
            };
            findings.push(finding);
        }
        let closing = |l: &str| {
            let l = l.trim().to_uppercase();
            l.starts_with("VERDICT:")
                || l.trim_start_matches(['-', '*', ' ']).starts_with("PREVIOUS ")
                || self.flags.iter().any(|f| l.starts_with(&format!("{}:", f.name)))
        };
        let summary = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("- [P") && !closing(l))
            .unwrap_or("")
            .to_string();
        let verdict = if findings.iter().any(|f| f.priority <= 1) { "changes" } else { "pass" };
        Ok(Review { reviewer: reviewer.into(), verdict: verdict.into(), findings, flagged, summary, seconds, previous, note: String::new() })
    }

    /// The human-facing reasons for the flags a review raised.
    pub fn flag_reasons(&self, review: &Review) -> Vec<String> {
        self.flags
            .iter()
            .filter(|f| review.flagged.contains(&f.name))
            .map(|f| format!("{} review: {}", review.reviewer, if f.reason.is_empty() { &f.question } else { &f.reason }))
            .collect()
    }

    /// Runs `reviewer` in `worktree` and parses its reply.
    pub fn run(&self, reviewer: &str, worktree: &Path, base: &str, previous: Option<&PreviousRound>) -> Result<Review> {
        let command = self
            .commands
            .get(reviewer)
            .filter(|c| !c.is_empty())
            .with_context(|| format!("the playbook has no command for the {reviewer} reviewer (add review.commands.{reviewer})"))?;
        let started = Instant::now();
        // Two reviews from one process in one second each get their own (NIT-116).
        let out_file = crate::paths::unique_temp("nitely-review", ".txt");
        let uses_file = command.iter().any(|a| a.contains("{output}"));
        let program = resolve_program(&command[0]);
        let codex_servers = if cli_name(&command[0]) == "codex" { codex_nitely_servers(&program, worktree) } else { vec![] };
        let command = without_nitely(command, &codex_servers);
        let args: Vec<String> = command[1..].iter().map(|a| a.replace("{output}", &out_file.to_string_lossy())).collect();

        let mut cmd = shell_free_command(&program);
        cmd.args(&args).current_dir(worktree).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        without_session_markers(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("starting the {reviewer} reviewer ({})", command[0]))?;
        let mut stdin = child.stdin.take().unwrap();
        let prompt = self.prompt(base, previous);
        let writer = std::thread::spawn(move || stdin.write_all(prompt.as_bytes()));
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out_reader = std::thread::spawn(move || {
            let mut s = String::new();
            stdout.read_to_string(&mut s).map(|_| s)
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            stderr.read_to_string(&mut s).map(|_| s)
        });

        let deadline = Duration::from_secs(self.timeout_minutes.max(1) * 60);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > deadline {
                child.kill().ok();
                child.wait().ok();
                bail!("the {reviewer} review took longer than {} minutes and was stopped", self.timeout_minutes);
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        writer.join().ok();
        let stdout = out_reader.join().ok().and_then(|r| r.ok()).unwrap_or_default();
        let stderr = err_reader.join().ok().and_then(|r| r.ok()).unwrap_or_default();
        let text = if uses_file { std::fs::read_to_string(&out_file).unwrap_or_default() } else { stdout.clone() };
        std::fs::remove_file(&out_file).ok();
        if !status.success() {
            bail!("the {reviewer} review failed ({status}):\n{}", tail(&format!("{stdout}{stderr}")));
        }
        self.parse(reviewer, &text, started.elapsed().as_secs(), previous, Some(worktree))
    }
}

/// The branch's code as it is now, to tell whether a finding still points at something.
struct CurrentCode {
    root: std::path::PathBuf,
    /// Tracked files, relative and with `/`, for citations that give only a file name.
    files: Vec<String>,
}

impl CurrentCode {
    fn new(root: &Path) -> CurrentCode {
        let files = crate::playbook::git(root, &["ls-files"]).map(|o| o.lines().map(str::to_string).collect()).unwrap_or_default();
        CurrentCode { root: root.to_path_buf(), files }
    }

    /// Whether `text` cites a file on the branch (at a line it has), and, when it quotes code in
    /// backticks, whether one of the cited files holds that code.
    fn confirms(&self, text: &str) -> bool {
        let cited: Vec<(String, Option<usize>)> = citations(text).into_iter().filter_map(|(p, line)| self.resolve(&p).map(|f| (f, line))).collect();
        let present: Vec<String> = cited
            .iter()
            .filter_map(|(f, line)| {
                let body = std::fs::read_to_string(self.root.join(f)).ok()?;
                line.is_none_or(|n| n >= 1 && n <= body.lines().count()).then_some(body)
            })
            .collect();
        if present.is_empty() {
            return false;
        }
        let squash = |s: &str| s.split_whitespace().collect::<String>();
        // A quote that is only a path is the citation itself, not code.
        let lone_path = |q: &str| q.split_whitespace().count() == 1 && !citations(q).is_empty();
        let quotes: Vec<String> = quoted(text).into_iter().filter(|q| !lone_path(q) && squash(q).len() >= 6).collect();
        quotes.is_empty() || quotes.iter().any(|q| present.iter().any(|body| squash(body).contains(&squash(q))))
    }

    /// The tracked file a cited path names: as given, or the only tracked file ending with it.
    fn resolve(&self, path: &str) -> Option<String> {
        let mut path = path.replace('\\', "/").trim_start_matches("./").to_string();
        // An absolute path inside the worktree, made relative to it.
        let root = crate::paths::normalize(&self.root.to_string_lossy());
        let lowered = crate::paths::normalize(&path);
        if let Some(rest) = lowered.strip_prefix(&format!("{root}/")) {
            // The same tail in the citation's own case; normalizing can spell an 8.3 short name
            // out, so the lengths need not match, and then the tracked file is found by name.
            path = match path.len().checked_sub(rest.len()).and_then(|i| path.get(i..)) {
                Some(tail) if crate::paths::normalize(tail) == rest => tail.to_string(),
                _ => self.files.iter().find(|f| crate::paths::normalize(f) == rest).cloned().unwrap_or_else(|| rest.to_string()),
            };
        }
        if self.files.contains(&path) || (self.files.is_empty() && self.root.join(&path).is_file()) {
            return Some(path);
        }
        let mut ends = self.files.iter().filter(|f| f.ends_with(&format!("/{path}")));
        match (ends.next(), ends.next()) {
            (Some(one), None) => Some(one.clone()),
            _ => None,
        }
    }
}

/// `path/to/file.rs:120` and `file.rs` mentions in a text, with their line when given.
fn citations(text: &str) -> Vec<(String, Option<usize>)> {
    let mut out = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || "`,;()[]\"'".contains(c)) {
        let word = word.trim_end_matches(['.', ':']);
        let (path, line) = match word.rsplit_once(':') {
            Some((p, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => (p, n.parse().ok()),
            _ => (word, None),
        };
        // A file name: a dot, then a short extension of letters; with a line number, any name
        // (Dockerfile:1, .gitignore:3); a dotfile or a path with a slash on its own.
        let has_ext = path.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && !stem.ends_with('/') && (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphabetic())
        });
        let named = path.chars().any(|c| c.is_ascii_alphabetic());
        let dotfile = path.starts_with('.') && path.len() > 1 && path[1..].chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c));
        let file = has_ext || dotfile || (named && (line.is_some() || path.contains('/')));
        if file && !path.contains("://") {
            out.push((path.to_string(), line));
        }
    }
    out
}

/// The `backticked` pieces of a text.
fn quoted(text: &str) -> Vec<String> {
    text.split('`').skip(1).step_by(2).map(str::to_string).collect()
}

/// `PREVIOUS <n>: fixed` and `PREVIOUS <n>: open ...` lines, by n, with what the line says after
/// the status (as written, for its citations); the last line for an n wins.
fn parse_previous(text: &str) -> BTreeMap<usize, (String, String)> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.replace("**", "");
        let l = l.trim().trim_start_matches(['-', '*', ' ', '`']);
        let Some(rest) = l.get(..9).filter(|p| p.eq_ignore_ascii_case("previous ")).map(|_| &l[9..]) else { continue };
        let Some((n, status)) = rest.split_once(':') else { continue };
        let Ok(n) = n.trim().trim_start_matches('#').parse::<usize>() else { continue };
        let status = status.trim().trim_start_matches('`');
        let word: String = status.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_lowercase();
        if matches!(word.as_str(), "fixed" | "open") {
            out.insert(n, (word.clone(), status[word.len()..].to_string()));
        }
    }
    out
}

/// `- [Pn] text` lines, each once.
pub fn parse_findings(text: &str) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix("- [P").or_else(|| l.strip_prefix("* [P")) else { continue };
        let mut chars = rest.chars();
        let (Some(d), Some(']')) = (chars.next(), chars.next()) else { continue };
        let Some(priority) = d.to_digit(10).filter(|p| *p <= 3) else { continue };
        let text = chars.as_str().trim().to_string();
        if !text.is_empty() && !out.iter().any(|f| f.text == text) {
            out.push(Finding { priority: priority as u8, text, carried: false, raised_as: None });
        }
    }
    out
}

fn tail_lines(text: &str) -> String {
    let t = text.trim();
    let start = t.char_indices().rev().nth(800).map_or(0, |(i, _)| i);
    t[start..].to_string()
}

/// A reviewer started from inside an agent session must not think it is that session.
fn without_session_markers(cmd: &mut std::process::Command) {
    for (name, _) in std::env::vars() {
        if is_session_marker(&name) {
            cmd.env_remove(name);
        }
    }
}

/// The CLI a command runs, e.g. `codex` for `C:\...\codex.exe`.
fn cli_name(program: &str) -> String {
    Path::new(program).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// The reviewer's command with Nitely's MCP server taken away: a reviewer reads the diff and
/// answers, and one that can report, note or move issues is a second actor on the board. Claude
/// starts with no MCP servers at all. Codex cannot drop its server table from the command line and
/// refuses to turn off a server it does not have, so each server in `codex_servers` (the Nitely
/// ones Codex has, from `codex_nitely_servers`) is turned off by name. Other commands are as given.
pub fn without_nitely(command: &[String], codex_servers: &[String]) -> Vec<String> {
    let mut out = command.to_vec();
    match cli_name(&command[0]).as_str() {
        "claude" if !command.iter().any(|a| a == "--strict-mcp-config") => out.insert(1, "--strict-mcp-config".into()),
        "codex" => {
            let at = command.iter().position(|a| a == "exec").map_or(1, |i| i + 1);
            for (i, name) in codex_servers.iter().enumerate() {
                out.splice(at + 2 * i..at + 2 * i, ["-c".to_string(), format!("mcp_servers.{name}.enabled=false")]);
            }
        }
        _ => {}
    }
    out
}

/// The MCP servers Codex would start in `worktree` (user and project config) that are Nitely's:
/// named `nitely`, or a nitely executable run with `mcp`. Empty when Codex cannot list them.
fn codex_nitely_servers(program: &str, worktree: &Path) -> Vec<String> {
    let mut cmd = shell_free_command(program);
    cmd.args(["mcp", "list", "--json"]).current_dir(worktree).stdin(Stdio::null()).stderr(Stdio::null());
    without_session_markers(&mut cmd);
    cmd.output().ok().filter(|o| o.status.success()).map(|o| nitely_servers(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

/// The Nitely servers in `codex mcp list --json` output. Only plain names are kept, since a name
/// goes into a `-c` key path.
fn nitely_servers(json: &str) -> Vec<String> {
    let Ok(serde_json::Value::Array(servers)) = serde_json::from_str(json) else { return vec![] };
    servers
        .iter()
        .filter(|s| s.get("enabled").and_then(serde_json::Value::as_bool) != Some(false))
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?;
            let transport = s.get("transport");
            let command = transport.and_then(|t| t.get("command")).and_then(serde_json::Value::as_str);
            let args = transport
                .and_then(|t| t.get("args"))
                .and_then(serde_json::Value::as_array)
                .and_then(|a| a.iter().map(serde_json::Value::as_str).collect::<Option<Vec<_>>>());
            let ours = name == "nitely" || crate::connect::is_nitely_mcp(command, args);
            let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            (ours && plain).then(|| name.to_string())
        })
        .collect()
}

/// Variables that make a CLI think it runs inside another agent's session.
pub(crate) fn is_session_marker(name: &str) -> bool {
    name == "CLAUDECODE"
        || name == "CLAUDE_PID"
        || name.starts_with("CLAUDE_CODE_SESSION")
        || name.starts_with("CLAUDE_CODE_HOST_SESSION")
        || name.starts_with("CLAUDE_CODE_CHILD_SESSION")
        || name.starts_with("CLAUDE_CODE_ENTRYPOINT")
        || name.starts_with("CLAUDE_CODE_EXECPATH")
        || name.starts_with("CLAUDE_CODE_SDK")
        || name.starts_with("CLAUDE_AGENT_SDK")
}

/// The vendors whose agents Nitely knows; a branch's first part names one of them.
const KNOWN_VENDORS: [&str; 3] = ["claude", "codex", "cursor"];

/// The program to start for a reviewer command's first word. On Windows a CLI installed with npm
/// is a `.cmd` file, which is started by its full path, and the Codex CLI that ships inside the
/// Codex Store app is not on PATH at all.
pub(crate) fn resolve_program(program: &str) -> String {
    #[cfg(windows)]
    if Path::new(program).extension().is_none() && Path::new(program).components().count() == 1 && which(program).is_none() {
        if let Some(script) = which_with(program, &["cmd", "bat"]) {
            return script.to_string_lossy().into_owned();
        }
        if program == "codex" {
            let out = shell_free_command("powershell")
                .args(["-NoProfile", "-Command", "(Get-AppxPackage -Name 'OpenAI.Codex*' | Select-Object -First 1).InstallLocation"])
                .output();
            if let Ok(out) = out {
                let exe = Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("app").join("resources").join("codex.exe");
                if exe.exists() {
                    return exe.to_string_lossy().into_owned();
                }
            }
        }
    }
    program.to_string()
}

/// Whether `program` can be started from `dir`: a path that exists, or a name found on PATH.
fn program_exists(program: &str, dir: &Path) -> bool {
    let path = &dir.join(program);
    if Path::new(program).components().count() > 1 {
        return runnable(path) || (cfg!(windows) && path.extension().is_none() && runnable(&path.with_extension("exe")));
    }
    which(program).is_some()
}

/// A file the OS can start: on Unix it needs an execute bit.
fn runnable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// `program` on PATH, as the OS would start it.
fn which(program: &str) -> Option<std::path::PathBuf> {
    if cfg!(windows) && Path::new(program).extension().is_none() {
        which_with(program, &["exe"])
    } else {
        which_with(program, &[""])
    }
}

fn which_with(program: &str, extensions: &[&str]) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        extensions
            .iter()
            .map(|ext| if ext.is_empty() { dir.join(program) } else { dir.join(format!("{program}.{ext}")) })
            .find(|p| runnable(p))
    })
}

/// "Codex (the `codex` CLI)", for the error that says what to install.
fn install_hint(reviewer: &str, rules: &ReviewRules) -> String {
    let program = rules.commands.get(reviewer).and_then(|c| c.first()).map(String::as_str).unwrap_or(reviewer);
    match cli_name(program).as_str() {
        "claude" => "Claude Code (the `claude` CLI)".to_string(),
        "codex" => "Codex (the `codex` CLI)".to_string(),
        _ => format!("{reviewer} (`{program}`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ReviewRules {
        ReviewRules {
            flags: vec![Flag { name: "MONEY_OR_RELEASE".into(), question: "the change alters billing".into(), reason: "money or release logic".into() }],
            ..Default::default()
        }
    }

    #[test]
    fn prompt_lists_every_closing_line() {
        let p = rules().prompt("main", None);
        assert!(p.contains("git diff main...HEAD"));
        assert!(p.contains("exactly these 2 lines:\nVERDICT: <findings or clean>\nMONEY_OR_RELEASE: <yes or no>"));
        assert!(!p.contains("PREVIOUS"));
    }

    fn previous(in_history: bool) -> PreviousRound {
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: false, raised_as: None };
        PreviousRound {
            commit: "abc1234".into(),
            findings: vec![f(1, "Drops the last row: src/a.rs, f"), f(2, "Empty name: src/b.rs, g"), f(2, "Slow loop: src/c.rs, h")],
            in_history,
        }
    }

    #[test]
    fn a_re_review_checks_the_previous_round_first() {
        let p = rules().prompt("main", Some(&previous(true)));
        assert!(p.contains("git diff abc1234 HEAD"), "{p}");
        assert!(p.contains("1. [P1] Drops the last row: src/a.rs, f\n2. [P2] Empty name"), "{p}");
        assert!(p.contains("`PREVIOUS <n>: fixed` or `PREVIOUS <n>: open <file>:<line>"), "{p}");
        assert!(p.contains("only if it is P0 or P1"), "{p}");
        assert!(p.contains("exactly these 2 lines:\nVERDICT: <findings or clean>\nMONEY_OR_RELEASE: <yes or no>"));

        // After a rebase there is no diff since that round: check the findings, review it all.
        let rebased = rules().prompt("main", Some(&previous(false)));
        assert!(rebased.contains("rebased since") && rebased.contains("PREVIOUS <n>"), "{rebased}");
        assert!(!rebased.contains("git diff abc1234") && !rebased.contains("only if it is P0 or P1"), "{rebased}");

        // A clean previous round leaves no checklist, but old code still only takes P0 or P1.
        let clean = PreviousRound { findings: vec![], ..previous(true) };
        let p = rules().prompt("main", Some(&clean));
        assert!(!p.contains("PREVIOUS") && p.contains("only if it is P0 or P1"), "{p}");
    }

    #[test]
    fn a_re_review_carries_what_is_not_fixed() {
        let prev = previous(true);
        let text = "Two of three fixed.\n**PREVIOUS 1:** fixed, now keeps the row\n- PREVIOUS 2: open - still accepts \"\"\n\
- [P3] New nit: src/d.rs\nVERDICT: findings\nMONEY_OR_RELEASE: no\n";
        let r = rules().parse("codex", text, 3, Some(&prev), None).unwrap();
        let statuses: Vec<&str> = r.previous.iter().map(|p| p.status.as_str()).collect();
        assert_eq!(statuses, ["fixed", "open", "unchecked"]);
        assert_eq!(r.summary, "Two of three fixed.");
        let carried: Vec<(u8, bool)> = r.findings.iter().map(|f| (f.priority, f.carried)).collect();
        assert_eq!(carried, [(3, false), (2, true), (2, true)]);
        assert_eq!(r.verdict, "pass");

        // An open P1 from the previous round still asks for changes, though nothing new was found.
        let text = "PREVIOUS 1: open\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed\nVERDICT: findings\nMONEY_OR_RELEASE: no";
        let r = rules().parse("codex", text, 3, Some(&prev), None).unwrap();
        assert_eq!(r.verdict, "changes");
        assert_eq!(r.findings, vec![Finding { priority: 1, text: "Drops the last row: src/a.rs, f".into(), carried: true, raised_as: None }]);

        // Everything fixed and nothing new is clean; an unfinished reply is still unfinished.
        let text = "PREVIOUS 1: fixed\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed\nVERDICT: clean\nMONEY_OR_RELEASE: no";
        let r = rules().parse("codex", text, 3, Some(&prev), None).unwrap();
        assert!(r.findings.is_empty() && r.verdict == "pass");
        assert!(rules().parse("codex", "PREVIOUS 1: fixed\nMONEY_OR_RELEASE: no", 1, Some(&prev), None).is_err());
    }

    #[test]
    fn an_open_finding_must_point_at_code_still_on_the_branch() {
        let dir = std::env::temp_dir().join(format!("nitely-recheck-{}-{}", std::process::id(), crate::db::now()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success());
        git(&["init", "-q"]);
        std::fs::write(dir.join("src/proposals.rs"), "fn apply() {\n    let closed = true;\n}\n").unwrap();
        git(&["add", "."]);
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: false, raised_as: None };
        let prev = PreviousRound {
            commit: "abc1234".into(),
            findings: vec![
                f(1, "Returns to Todo after a failed apply: crates/nitely-core/src/proposals.rs:488, apply"),
                f(2, "Schema v10 column: crates/nitely-core/src/db.rs, migrate"),
                f(1, "Apply is not atomic: src/proposals.rs, apply"),
            ],
            in_history: true,
        };
        let review = |text: &str| rules().parse("codex", &format!("{text}\nVERDICT: clean\nMONEY_OR_RELEASE: no"), 1, Some(&prev), Some(&dir)).unwrap();
        let statuses = |r: &Review| r.previous.iter().map(|p| p.status.clone()).collect::<Vec<_>>();

        // "Open" with no current code, at a line the file no longer has, or quoting code that is
        // not there, is unconfirmed: carried, but at P2, so the branch can pass.
        let r = review("PREVIOUS 1: open\nPREVIOUS 2: open proposals.rs:488 still returns to Todo\nPREVIOUS 3: open src/proposals.rs:2 `issue.state = Todo`");
        assert_eq!(statuses(&r), ["unconfirmed", "unconfirmed", "unconfirmed"]);
        assert_eq!(r.verdict, "pass");
        assert!(r.findings.iter().all(|f| f.priority == 2 && f.carried && f.text.ends_with("not on the branch now)")), "{:?}", r.findings);

        // Quoting the current code that still shows it keeps it open at its priority; a bare file
        // name finds the tracked file.
        let r = review("PREVIOUS 1: fixed\nPREVIOUS 2: fixed\nPREVIOUS 3: open proposals.rs:2 `let closed = true;` is set before the write");
        assert_eq!(statuses(&r), ["fixed", "fixed", "open"]);
        assert_eq!(r.verdict, "changes");

        // Not mentioned: carried as it was while the file it names exists, unconfirmed once it is gone.
        let r = review("`PREVIOUS 1: fixed`\nPREVIOUS 2: fixed");
        assert_eq!(statuses(&r), ["fixed", "fixed", "unchecked"], "a backticked answer counts");
        assert_eq!(r.findings[0].priority, 1);

        // Carried lower, a finding keeps the priority it was raised at: confirmed open later (here
        // by an absolute path), it blocks again at P1, without the unconfirmed mark.
        let first = review("PREVIOUS 1: open\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed");
        assert_eq!((first.findings[0].priority, first.findings[0].raised_as), (2, Some(1)));
        let next = PreviousRound { commit: "def5678".into(), findings: first.findings.clone(), in_history: true };
        let absolute = dir.join("src").join("proposals.rs");
        let text = format!("PREVIOUS 1: open {}:2 `let closed = true;`\nVERDICT: clean\nMONEY_OR_RELEASE: no", absolute.display());
        let r = rules().parse("codex", &text, 1, Some(&next), Some(&dir)).unwrap();
        assert_eq!(statuses(&r), ["open"]);
        assert_eq!((r.findings[0].priority, r.verdict.as_str()), (1, "changes"));
        assert!(!r.findings[0].text.contains("unconfirmed"), "{:?}", r.findings);

        // Files without an extension count when cited with a line.
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        git(&["add", "."]);
        assert!(CurrentCode::new(&dir).confirms("Dockerfile:1 `FROM scratch`"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_a_finished_review() {
        let text = "Looks mostly fine.\n- [P1] Off by one: src/a.rs, f: drops the last row\n- [P3] Typo: README\n- [P1] Off by one: src/a.rs, f: drops the last row\nVERDICT: findings\nMONEY_OR_RELEASE: **yes**\n";
        let r = rules().parse("codex", text, 5, None, None).unwrap();
        assert_eq!(r.verdict, "changes");
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.flagged, vec!["MONEY_OR_RELEASE"]);
        assert_eq!(r.summary, "Looks mostly fine.");
        assert_eq!(rules().flag_reasons(&r), vec!["codex review: money or release logic"]);

        let clean = rules().parse("claude", "Nothing found.\n- [P3] nit: x\nVERDICT: findings\nMONEY_OR_RELEASE: no", 1, None, None).unwrap();
        assert_eq!(clean.verdict, "pass");
    }

    #[test]
    fn unfinished_reviews_fail() {
        assert!(rules().parse("codex", "VERDICT: clean", 1, None, None).is_err(), "missing flag");
        assert!(rules().parse("codex", "MONEY_OR_RELEASE: no", 1, None, None).is_err(), "missing verdict");
        assert!(rules().parse("codex", "VERDICT: findings\nMONEY_OR_RELEASE: no", 1, None, None).is_err(), "findings but none listed");
    }

    #[test]
    fn picks_the_other_vendor() {
        let r = ReviewRules::default();
        assert_eq!(r.reviewer_for("claude"), Some("codex"));
        assert_eq!(r.reviewer_for("codex"), Some("claude"));
        assert_eq!(r.reviewer_for("cursor"), Some("claude"));
    }

    #[test]
    fn reviewers_start_without_nitely() {
        let commands = ReviewRules::default().commands;
        let claude = without_nitely(&commands["claude"], &[]);
        assert_eq!(claude[..3], ["claude", "--strict-mcp-config", "-p"]);
        assert_eq!(without_nitely(&claude, &[]), claude, "added once");

        let codex = without_nitely(&commands["codex"], &["nitely".into(), "nit_dev".into()]);
        assert_eq!(
            codex[..6],
            ["codex", "exec", "-c", "mcp_servers.nitely.enabled=false", "-c", "mcp_servers.nit_dev.enabled=false"]
        );
        assert_eq!(codex[6..], commands["codex"][2..]);
        // Codex refuses to turn off a server it does not have, so none means no override.
        assert_eq!(without_nitely(&commands["codex"], &[]), commands["codex"]);
        let path = vec!["/opt/Codex Tools/codex.exe".to_string(), "exec".into(), "-".into()];
        assert_eq!(without_nitely(&path, &["nitely".into()])[2..4], ["-c", "mcp_servers.nitely.enabled=false"]);
        let other = vec!["gemini".to_string(), "-p".into()];
        assert_eq!(without_nitely(&other, &["nitely".into()]), other);
    }

    #[test]
    fn finds_codexs_nitely_servers() {
        let json = r#"[
            {"name": "nitely", "enabled": true, "transport": {"type": "stdio", "command": "nitely", "args": ["mcp"]}},
            {"name": "board", "enabled": true, "transport": {"type": "stdio", "command": "/opt/Nitely Tools/nitely", "args": ["mcp"]}},
            {"name": "off", "enabled": false, "transport": {"type": "stdio", "command": "nitely", "args": ["mcp"]}},
            {"name": "docs", "enabled": true, "transport": {"type": "stdio", "command": "node", "args": ["docs.js"]}},
            {"name": "web", "enabled": true, "transport": {"type": "streamable_http", "url": "http://localhost:1"}}
        ]"#;
        assert_eq!(nitely_servers(json), ["nitely", "board"]);
        assert!(nitely_servers("not json").is_empty());
    }

    #[test]
    fn chooses_from_the_installed_reviewers() {
        let r = ReviewRules::default();
        let only = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        let choice = |vendor, names| r.choose(vendor, &only(names)).unwrap();
        let c = |reviewer: &str, independent, note: &str| Choice { reviewer: reviewer.into(), independent, note: note.into() };

        assert_eq!(choice("claude", &["claude", "codex"]), c("codex", true, ""));
        assert_eq!(choice("claude", &["claude"]), c("claude", false, "same vendor: codex not installed"));
        assert_eq!(choice("codex", &["codex"]), c("codex", false, "same vendor: claude not installed"));
        assert_eq!(choice("cursor", &["codex"]), c("codex", true, "claude not installed"));
        // A branch that names no vendor Nitely knows cannot show its review was independent.
        assert_eq!(choice("", &["claude", "codex"]), c("claude", false, "author's vendor unknown"));
        assert_eq!(choice("feature", &["codex"]), c("codex", false, "author's vendor unknown; claude not installed"));
        // Another vendor beats the author's own, whatever the map's order.
        let mut three = ReviewRules::default();
        three.commands.insert("gemini".into(), vec!["gemini".into()]);
        assert_eq!(three.choose("claude", &only(&["claude", "gemini"])).unwrap(), c("gemini", true, "codex not installed"));
        // A playbook that has an agent review its own vendor says so even when nothing is missing.
        let mut own = ReviewRules::default();
        own.reviewer.insert("claude".into(), "claude".into());
        assert_eq!(own.choose("claude", &only(&["claude", "codex"])).unwrap(), c("claude", false, "same vendor"));

        let none = r.choose("claude", &only(&[])).unwrap_err().to_string();
        assert!(none.contains("no reviewer can run") && none.contains("Codex (the `codex` CLI)") && none.contains("Claude Code"), "{none}");

        assert_eq!(r.independence_reason("claude", "claude", "same vendor"), None);
        let strict = ReviewRules { require_cross_vendor: true, ..ReviewRules::default() };
        assert!(strict.independence_reason("codex", "claude", "").is_none());
        let reason = strict.independence_reason("claude", "claude", "same vendor: codex not installed").unwrap();
        assert!(reason.contains("same vendor: codex not installed") && reason.contains("another vendor"), "{reason}");
        // From the branch alone, as for a review recorded before notes were kept.
        assert!(strict.independence_reason("claude", "claude", "").unwrap().contains("(same vendor)"));
        assert!(strict.independence_reason("claude", "", "").unwrap().contains("author's vendor unknown"));
        assert!(strict.independence_reason("codex", "feature", "").is_some());
    }

    #[test]
    fn finds_programs_on_path_or_by_path() {
        let dir = std::env::temp_dir().join(format!("nitely-which-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        let script = dir.join("scripts").join("review");
        std::fs::write(&script, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(!program_exists("./scripts/review", &dir), "not executable yet");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(program_exists(&crate::playbook::posix_sh(), &dir));
        assert!(!program_exists("nitely-no-such-reviewer", &dir));
        assert!(!program_exists("/nitely/no/such/reviewer", &dir));
        // A relative path counts from the worktree the reviewer runs in, not from Nitely's own.
        assert!(program_exists("./scripts/review", &dir) && program_exists("scripts/review", &dir));
        assert!(!program_exists("./scripts/review", &std::env::temp_dir()));
        let r = ReviewRules::default();
        assert!(!r.is_installed("gemini", &dir), "no command, not installed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn runs_a_reviewer_command() {
        let mut r = rules();
        r.commands.insert("fake".into(), vec![crate::playbook::posix_sh(), "-c".into(), "cat > /dev/null; printf 'ok\\nVERDICT: clean\\nMONEY_OR_RELEASE: no\\n' > \"$0\"".into(), "{output}".into()]);
        let review = r.run("fake", &std::env::temp_dir(), "main", None).unwrap();
        assert_eq!(review.verdict, "pass");
        assert!(review.flagged.is_empty());
    }
}
