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
    /// The person may turn automated review off for this repository. Gates still run.
    pub enabled: bool,
    /// Authoritative checkout to reread before new launches; never supplied by a playbook.
    #[serde(skip)]
    pub policy_root: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub yolo_guards: Vec<crate::project_yolo::ReviewGuard>,
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
    /// What this change is for, given by submit, not the playbook: its issue, the gate's result,
    /// the author's notes since the last round, what the person accepted elsewhere (NIT-282).
    #[serde(skip)]
    pub context: String,
    pub timeout_minutes: u64,
    /// A review by the author's own vendor (because no other reviewer is installed, or the
    /// `reviewer` map says so) sends the change to a human instead of approving it.
    pub require_cross_vendor: bool,
    /// Review the changes of a land queue batch in one reviewer call (docs/batching.md,
    /// review_batch.rs): `off` reviews each change at its submit; `small` and `all` both leave a
    /// change's review to its land batch. Set by the review mode (Fast); the playbook's own
    /// `[review] batch` is retired.
    pub batch: BatchMode,
    /// Retired (NIT-509): the land queue's batch is the review batch, so its `[land] batch_max`
    /// and window decide. Still read, so a playbook that sets them parses; ignored, with a notice
    /// (`project_modes::ignored_policy`).
    pub batch_window: String,
    pub batch_max: usize,
    pub small_lines: usize,
    pub small_files: usize,
    pub max_lines: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchMode {
    /// The retired `shadow` measurement reads as `off` (NIT-499).
    #[default]
    #[serde(alias = "shadow")]
    Off,
    Small,
    All,
}

pub const DISABLED_NOTE: &str = "Review disabled for this project";

/// A policy decision, not a failed reviewer: callers must not try another vendor.
#[derive(Debug)]
pub struct Disabled;
impl std::fmt::Display for Disabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(DISABLED_NOTE) }
}
impl std::error::Error for Disabled {}

impl ReviewRules {
    pub fn require_enabled(&self) -> Result<()> {
        for guard in &self.yolo_guards { guard.require_review()?; }
        let enabled = match &self.policy_root {
            Some(root) => crate::playbook::Playbook::load(root)?.review.enabled,
            None => self.enabled,
        };
        let mut enabled=enabled;
        for guard in &self.yolo_guards {if let Some(value)=guard.enabled_override()? {enabled=value;}}
        if !enabled { bail!(Disabled); }
        Ok(())
    }

    /// Review-dependent rules remain unanswered while review is off. The setting alone adds none.
    pub fn unevaluated_reasons(&self) -> Vec<String> {
        let mut reasons: Vec<String> = self.flags.iter().map(|f| format!("Unevaluated reviewer condition {}: {} (automated review is disabled)", f.name, f.question)).collect();
        if self.require_cross_vendor {
            reasons.push("Unevaluated cross-vendor review requirement (automated review is disabled)".into());
        }
        reasons
    }
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
            enabled: true,
            policy_root: None,
            yolo_guards: vec![],
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
            context: String::new(),
            timeout_minutes: 20,
            require_cross_vendor: false,
            batch: BatchMode::Off,
            batch_window: "5m".into(),
            batch_max: 3,
            small_lines: 100,
            small_files: 5,
            max_lines: 1500,
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
    /// For a carried finding the last round confirmed open: what the reviewer said then, the
    /// current code it cites and how that code still shows the problem. The text above stays as it
    /// was raised, so findings match across rounds; this says where it stands now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
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
    /// Findings the person accepted as they are (NIT-191), with their reasons: the reviewer does not
    /// raise them again. They are not in `findings`.
    pub accepted: Vec<(Finding, String)>,
    /// That review passed and the branch was rebased or rewritten since: the commit its change sat
    /// on, so the reviewer can compare the change with the one that passed (`git range-diff`)
    /// instead of reading the whole branch again (NIT-185).
    pub passed_on: Option<String>,
}

/// A previous round's finding and what this round says about it: `fixed`; `open`, with current code
/// that still shows it; `unchecked` when the reviewer did not say; or `unconfirmed` when it was
/// called open (or left unchecked) but nothing it points at is on the branch any more.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recheck {
    pub priority: u8,
    pub text: String,
    pub status: String,
    /// An open answer's citation and reason, as the reviewer wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
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
    /// The batch this change was reviewed in, with others in one reviewer call (NIT-216).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchInfo>,
    /// What the reviewer said the call used, when it says (Codex does); for a batched review, the
    /// batch's tokens shared evenly between its changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Not run again: the same patch passed review at this commit, and that pass carries over
    /// (NIT-185).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused: Option<String>,
    /// Findings set aside because everything they cite is code the base brought in when it was
    /// merged into the branch (NIT-646): its code is context for the review, never the change's
    /// problem. They count for nothing in the verdict, so nobody is asked to accept them; they are
    /// listed so a real one can be filed against the base.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outside: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchInfo {
    pub id: i64,
    /// The other changes' issues, in the batch's order.
    pub with: Vec<String>,
}

impl Review {
    /// "NIT-2, NIT-3": the changes a batched review reviewed this one with.
    pub fn batched_with(&self) -> Option<String> {
        self.batch.as_ref().filter(|b| !b.with.is_empty()).map(|b| b.with.join(", "))
    }
}

/// How severities are judged, word for word in a single and a batched review, so batching does not
/// change what a P1 is.
/// What is the change's to answer for and what is the base's (NIT-646). The second sentence is how
/// a finding names the changed code that causes it, so one that cites only code the base brought
/// in by a merge can be set aside without hiding a regression the change caused.
const BASE_RULES: &str = "A defect that shows in code the change does not touch, but that the change causes, also cites the changed \
line that causes it. A defect that is already on the base as it is, which this change neither causes nor worsens, is not \
this change's: report it as `- [BASE] title: file, symbol: what goes wrong` instead, with no priority, and it is listed to \
fix on the base without holding this change.";

const SEVERITIES: &str = "using P0 for data loss or security, P1 for a definite bug in normal use, P2 for an edge case, P3 for a minor issue. \
A P0 or P1 must name a concrete failure: the inputs or state that lead to a wrong result, data loss, a leak or a crash, and the \
file and line that cause it. A risk with no such failure (\"what if\", a missing guard nothing reaches) is P2 at most, however \
serious it sounds.";

/// What an answer about a previous finding must hold, after its format (NIT-214).
const PREVIOUS_RULES: &str = "where file, line and code are from the current branch, the code is at that line, and <how> says in a few words how \
that code still causes the problem the finding describes. The code a finding cites may have moved or gone, and a fix \
may have replaced it: check the problem itself, not the old line or its wording. If it no longer happens, it is \
fixed, even if code nearby could be better (raise that as a new defect if it is one). An open answer that quotes no \
code at the line it cites, or does not say how that code still causes the problem, is not counted as open. Do not \
list an open one again as a defect: it stays on record by itself.\n";

/// What the person accepted, before the list (NIT-191).
const ACCEPTED_INTRO: &str = "The person who owns this change accepted these findings as they are: a trade-off they decided, not a \
defect left to fix. Do not raise them again, or anything that only restates them, unless the code they cite has changed \
since in a way that makes them worse:\n";

/// The findings the person accepted, as a review is told them: not to be raised again.
pub(crate) fn accepted_told(accepted: &[(Finding, String)]) -> String {
    let mut p = ACCEPTED_INTRO.to_string();
    for (f, reason) in accepted {
        p.push_str(&format!("- [P{}] {} (accepted: {reason})\n", f.priority, f.text));
    }
    p
}

/// One change in a batched review.
#[derive(Debug, Clone)]
pub struct BatchChange {
    pub key: String,
    pub title: String,
    pub branch: String,
    /// The commits its diff runs between.
    pub base: String,
    pub head: String,
    /// Its own worktree, to check what its previous findings cite.
    pub worktree: std::path::PathBuf,
    /// The files its diff touches, to place a finding that names no change.
    pub files: Vec<String>,
    pub previous: Option<PreviousRound>,
}

/// A batched reply, split by change.
#[derive(Debug)]
pub struct BatchReply {
    /// In the batch's order: the change's review, or why it must be reviewed alone.
    pub reviews: Vec<std::result::Result<Review, String>>,
    /// `INTERACTION j,k` lines: the two changes (0-based, the first one first) and what goes wrong.
    pub interactions: Vec<(usize, usize, String)>,
}

/// A reviewer that could not run at all: it did not start, it stopped with an error, or it is not
/// signed in. Unlike a review that ran and found problems, another reviewer can take its place.
#[derive(Debug, Clone, PartialEq)]
pub struct CouldNotRun {
    pub reviewer: String,
    /// Short, for the review's note: "not signed in", "exited with code 1".
    pub why: String,
    /// What the person can do about it, and the tail of what the reviewer printed.
    pub detail: String,
}

impl std::fmt::Display for CouldNotRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the {} reviewer could not run ({}): {}", self.reviewer, self.why, self.detail)
    }
}

impl std::error::Error for CouldNotRun {}

/// Whether a failed reviewer's output says it is not signed in.
fn says_signed_out(output: &str) -> bool {
    let o = output.to_lowercase();
    ["not logged in", "not signed in", "please log in", "please run /login", "log in again", "login required", "oauth", "invalid api key", "api key", "unauthorized", "401", "authentication", "expired"]
        .iter()
        .any(|p| o.contains(p))
}

/// What the person runs to sign a reviewer's CLI in.
fn login_hint(reviewer: &str, rules: &ReviewRules) -> String {
    let program = rules.commands.get(reviewer).and_then(|c| c.first()).map(String::as_str).unwrap_or(reviewer);
    match cli_name(program).as_str() {
        "claude" => "run `claude` in a terminal and sign in with /login".to_string(),
        "codex" => "run `codex login`".to_string(),
        _ => format!("sign `{program}` in"),
    }
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
        self.choose_after(vendor, installed, &[])
    }

    /// `choose`, passing over the reviewers that already `failed` to run on this branch: the next
    /// candidate reviews, and the note says why the preferred one did not.
    pub fn choose_after(&self, vendor: &str, installed: &dyn Fn(&str) -> bool, failed: &[CouldNotRun]) -> Result<Choice> {
        let failed_one = |r: &str| failed.iter().find(|f| f.reviewer == r);
        let installed = |r: &str| installed(r) && failed_one(r).is_none();
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
            if !failed.is_empty() {
                let each: Vec<String> = failed
                    .iter()
                    .map(|f| if f.why == "not signed in" { format!("{} is not signed in ({})", f.reviewer, login_hint(&f.reviewer, self)) } else { f.to_string() })
                    .collect();
                bail!("no reviewer could run on this machine: {}", each.join("; "));
            }
            let wanted: Vec<String> = candidates.iter().map(|r| install_hint(r, self)).collect();
            bail!(
                "no reviewer can run on this machine: install {} (or point review.commands in the playbook at a reviewer you have)",
                wanted.join(" or ")
            );
        };
        let missing = self.reviewer_for(vendor).filter(|p| *p != reviewer).map(|m| match failed_one(m) {
            Some(f) => format!("{m} could not run: {}", f.why),
            None => format!("{m} not installed"),
        });
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
            Some(prev) if prev.passed_on.is_some() => format!(
                "This change passed review at commit {c}, and the branch has been rebased or rewritten since. Run `git range-diff \
{o}..{c} {base}..HEAD` to see how the change differs from the one that passed (`git diff {base}...HEAD` is the whole \
change). Review those differences and the code they reach, and look for correctness bugs and regressions, including \
ones the new base brings to the change. Skip style. Do not edit anything. The rest already passed review: raise \
something new there only if it is P0 or P1, and then say in the finding why that review missed it.\n",
                c = prev.commit,
                o = prev.passed_on.as_deref().unwrap_or_default()
            ),
            Some(prev) => format!(
                "This branch was reviewed before, at commit {c}, and has been rebased since, so review the whole change \
again. Run `git diff {base}...HEAD`, read the changed code and its callers, and look for correctness bugs and \
regressions. Skip style. Do not edit anything.\n",
                c = prev.commit
            ),
        };
        p.push_str(&self.prompt_rest(previous));
        p
    }

    /// The prompt for a branch reviewed before at `prev.commit` that has had `base` merged into it
    /// since (NIT-646): `git diff <commit> HEAD` would show everything the merge brought from the
    /// base as if the author wrote it, so the reviewer compares the change as reviewed with the
    /// change now, both against the base.
    pub fn prompt_after_merge(&self, base: &str, prev: &PreviousRound) -> String {
        let mut p = format!(
            "This branch was reviewed before, at commit {c}, and {base} has been merged into it since. Run `git diff \
{base}...HEAD` for the whole change: only its lines are this change's. Code the merge brought from {base} is context, \
not this change: raise nothing about it. To see what the author changed since that review, compare that diff with \
`git diff {base}...{c}`, the change as it was reviewed. Read the changed code and its callers, and look for correctness \
bugs and regressions. Skip style. Do not edit anything.\n",
            c = prev.commit
        );
        p.push_str(&self.prompt_rest(Some(prev)));
        p
    }

    /// The review prompt for small changes that already landed through the quick path (NIT-625):
    /// `changes` says which, and how to see each; the rest is as `prompt` asks.
    pub(crate) fn landed_prompt(&self, changes: &str) -> String {
        let mut p = format!(
            "These small changes already landed through the quick path, with no review before landing. Review each one: \
read its diff, the changed code and its callers, and look for correctness bugs and regressions. Skip style. Do not edit \
anything. Start each finding with the key of the change it is about.\n{changes}\n"
        );
        p.push_str(&self.prompt_rest(None));
        p
    }

    /// What every review prompt asks after saying what to review.
    fn prompt_rest(&self, previous: Option<&PreviousRound>) -> String {
        let mut p = String::new();
        p.push_str("\nTesting standard:\n");
        p.push_str(crate::instructions::TESTING_STANDARD);
        if !self.context.trim().is_empty() {
            p.push_str(&format!("\n{}\n", self.context.trim()));
        }
        if let Some(prev) = previous.filter(|p| !p.findings.is_empty()) {
            p.push_str("\nFirst, check each finding of that review against the current code:\n");
            for (i, f) in prev.findings.iter().enumerate() {
                p.push_str(&format!("{}. [P{}] {}\n", i + 1, f.priority, f.text));
                if let Some(now) = &f.now {
                    p.push_str(&format!("   Last round said it was still open: {now}\n"));
                }
            }
            p.push_str("Answer each on its own line as `PREVIOUS <n>: fixed` or `PREVIOUS <n>: open <file>:<line> `<code>` <how>`, ");
            p.push_str(PREVIOUS_RULES);
        }
        if let Some(prev) = previous.filter(|p| !p.accepted.is_empty()) {
            p.push('\n');
            p.push_str(&accepted_told(&prev.accepted));
        }
        if let Some(prev) = previous.filter(|p| p.in_history) {
            p.push_str(&format!(
                "\nThen review what changed since {c} and report any defect in it. The rest of the branch was already \
reviewed: raise something new there only if it is P0 or P1, and then say in the finding why that review missed it \
(for example, a change since made it reachable).\n",
                c = prev.commit
            ));
        }
        if !self.focus.trim().is_empty() {
            p.push_str(&format!("\n{}\n", self.focus.trim()));
        }
        p.push_str(&format!("\nReport each defect on its own line as:\n- [P0] title: file, symbol: what goes wrong and when\n{SEVERITIES}\n{BASE_RULES}\n\n"));
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
        let base_findings = parse_base_findings(text);
        let code = worktree.map(CurrentCode::new);
        // A P0 or P1 that points at no file names no concrete failure: it counts as P2, and keeps
        // the priority it was raised at, which a later round can restore by citing the code (NIT-282).
        // A file without an extension (`Makefile`) counts when the branch has it.
        for f in findings.iter_mut().filter(|f| f.priority <= 1 && !names_a_file(&f.text, code.as_ref())) {
            f.raised_as = Some(f.priority);
            f.priority = 2;
        }
        let statuses = parse_previous(text);
        let previous: Vec<Recheck> = previous
            .map(|prev| {
                prev.findings
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let (status, said) = statuses.get(&(i + 1)).cloned().unwrap_or_else(|| ("unchecked".into(), String::new()));
                        // Open needs the reviewer's own pointer at current code, and its reason (NIT-214);
                        // unchecked, the finding's own citation.
                        let confirmed = match (&code, status.as_str()) {
                            (_, "fixed") => true,
                            (Some(code), "open") => code.confirms_open(&said),
                            (Some(code), _) => code.confirms(&f.text),
                            (None, _) => true,
                        };
                        let status = if confirmed { status } else { "unconfirmed".to_string() };
                        let now = (status == "open").then(|| said.trim().trim_start_matches(['-', ':', ' ']).chars().take(400).collect::<String>()).filter(|s| !s.is_empty());
                        // Judged at the priority it was raised at, whatever an earlier round carried it at.
                        let text = f.text.strip_suffix(UNCONFIRMED).unwrap_or(&f.text).to_string();
                        Recheck { priority: f.raised_as.unwrap_or(f.priority), text, status, now }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if verdict == "findings" && findings.is_empty() && base_findings.is_empty() && !previous.iter().any(|r| r.status == "open") {
            bail!("the {reviewer} review says it has findings but lists none as `- [Pn] ...`:\n{}", tail_lines(text));
        }
        for r in previous.iter().filter(|r| r.status != "fixed") {
            let finding = if r.status == "unconfirmed" {
                let priority = r.priority.max(2);
                Finding { priority, text: format!("{}{UNCONFIRMED}", r.text), carried: true, raised_as: (priority != r.priority).then_some(r.priority), now: None }
            } else {
                Finding { priority: r.priority, text: r.text.clone(), carried: true, raised_as: None, now: r.now.clone() }
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
        Ok(Review { reviewer: reviewer.into(), verdict: verdict.into(), findings, flagged, summary, seconds, previous, note: String::new(), batch: None, tokens: None, reused: None, outside: base_findings })
    }

    /// The instructions for several changes reviewed in one call (docs/batching.md): one section
    /// per change, judged as if alone, with the single prompt's severities word for word.
    pub fn batch_prompt(&self, changes: &[BatchChange]) -> String {
        let mut p = format!(
            "Review these {n} changes to this repository. They are separate: each will land on its own, so judge each one \
as if it were reviewed alone. A defect belongs to the change whose diff introduces it. A problem that only appears when \
two of these changes meet goes on a line `INTERACTION <j>,<k>: <what goes wrong>`, not on either change. Your working \
directory has none of these changes checked out: read a file as change <k> has it with `git show <head>:<path>`. For each \
change, run its diff, read the changed code and its callers, and look for correctness bugs and regressions. Skip style. \
Do not edit anything.\n",
            n = changes.len()
        );
        p.push_str("\nTesting standard:\n");
        p.push_str(crate::instructions::TESTING_STANDARD);
        for (i, c) in changes.iter().enumerate() {
            let k = i + 1;
            p.push_str(&format!("\n## CHANGE {k}: {} \"{}\"\nbranch {}, base {}, head {}\n", c.key, c.title, c.branch, c.base, c.head));
            p.push_str(&format!("Run `git diff {}...{}` for the change.\n", c.base, c.head));
            match &c.previous {
                Some(prev) if prev.in_history && base_merged_since(&c.worktree, &c.base, &prev.commit, &c.head) => p.push_str(&format!(
                    "It was reviewed before, at commit {c0}, and {b} has been merged into it since: code the merge brought from {b} \
is context, not this change, so raise nothing about it. Compare the diff above with `git diff {b}...{c0}`, the change as \
it was reviewed, to see what changed since. The rest was already reviewed: raise something new there only if it is P0 or \
P1, and then say in the finding why that review missed it.\n",
                    c0 = prev.commit,
                    b = c.base
                )),
                Some(prev) if prev.in_history => p.push_str(&format!(
                    "It was reviewed before, at commit {c0}: `git diff {c0} {h}` is what changed since. The rest was already \
reviewed: raise something new there only if it is P0 or P1, and then say in the finding why that review missed it.\n",
                    c0 = prev.commit,
                    h = c.head
                )),
                Some(prev) => p.push_str(&format!("It was reviewed before, at commit {}, and rebased since: review the whole change again.\n", prev.commit)),
                None => {}
            }
            if let Some(prev) = c.previous.as_ref().filter(|p| !p.findings.is_empty()) {
                p.push_str("First, check each finding of its previous review against its current code:\n");
                for (j, f) in prev.findings.iter().enumerate() {
                    p.push_str(&format!("{k}.{}. [P{}] {}\n", j + 1, f.priority, f.text));
                    if let Some(now) = &f.now {
                        p.push_str(&format!("   Last round said it was still open: {now}\n"));
                    }
                }
            }
            if let Some(prev) = c.previous.as_ref().filter(|p| !p.accepted.is_empty()) {
                p.push_str(ACCEPTED_INTRO);
                for (f, reason) in &prev.accepted {
                    p.push_str(&format!("- [P{}] {} (accepted: {reason})\n", f.priority, f.text));
                }
            }
        }
        if changes.iter().any(|c| c.previous.as_ref().is_some_and(|p| !p.findings.is_empty())) {
            p.push_str("\nAnswer each previous finding on its own line as `PREVIOUS <k>.<n>: fixed` or `PREVIOUS <k>.<n>: open <file>:<line> `<code>` <how>`, ");
            p.push_str(PREVIOUS_RULES);
        }
        if !self.focus.trim().is_empty() {
            p.push_str(&format!("\n{}\n", self.focus.trim()));
        }
        p.push_str(&format!("\nReport each defect on its own line as:\n- [P0] CHANGE <k>: title: file, symbol: what goes wrong and when\n{SEVERITIES}\n{BASE_RULES} In a batch, `- [BASE] CHANGE <k>: ...`.\n\n"));
        p.push_str("Then end your reply with these lines for each change, change 1's first:\nVERDICT <k>: <findings or clean>\n");
        for f in &self.flags {
            p.push_str(&format!("{} <k>: <yes or no>\n", f.name));
        }
        p.push_str("VERDICT <k> is findings if you listed any new defect for change <k>, otherwise clean.");
        for f in &self.flags {
            p.push_str(&format!(" {} <k> is yes if, for change <k>, {}.", f.name, f.question.trim_end_matches('.')));
        }
        if !self.flags.is_empty() {
            p.push_str(" Answer yes when unsure.");
        }
        p
    }

    /// Splits a batched reply into one review per change, each parsed as a single review would be
    /// (carried and accepted findings, NIT-214's checks against that change's worktree). A change
    /// whose part of the reply cannot stand alone gets the reason it must be reviewed alone: no
    /// VERDICT for it, an unfinished part, or a finding that names no change and could be its.
    pub fn parse_batch(&self, reviewer: &str, text: &str, seconds: u64, changes: &[BatchChange]) -> BatchReply {
        let n = changes.len();
        let mut parts: Vec<Vec<String>> = vec![vec![]; n];
        let mut stray: Vec<String> = vec![];
        let mut interactions = vec![];
        let index = |k: &str| k.trim().trim_start_matches('#').parse::<usize>().ok().filter(|k| (1..=n).contains(k)).map(|k| k - 1);
        for line in text.lines() {
            let l = line.replace("**", "");
            let t = l.trim().trim_start_matches('`');
            let base_line = t.strip_prefix("- [BASE]").or_else(|| t.strip_prefix("* [BASE]")).map(|rest| ("BASE", rest));
            if let Some(rest) = base_line.map(|(_, r)| format!("BASE]{r}")).or_else(|| t.strip_prefix("- [P").or_else(|| t.strip_prefix("* [P")).map(str::to_string)) {
                let tag = if base_line.is_some() { "BASE" } else { "P" };
                let Some((priority, body)) = rest.split_once(']') else { continue };
                let priority = if tag == "BASE" { String::new() } else { priority.to_string() };
                let body = body.trim();
                match body.get(..7).filter(|w| w.eq_ignore_ascii_case("change ")).and_then(|_| body[7..].split_once(|c: char| !c.is_ascii_digit() && c != '#')) {
                    Some((k, said)) if index(k).is_some() => {
                        let said = said.trim_start_matches([':', ' ', '-', '\u{2013}', '\u{2014}']);
                        parts[index(k).unwrap()].push(format!("- [{tag}{priority}] {said}"));
                    }
                    _ => stray.push(body.to_string()),
                }
                continue;
            }
            let t = t.trim_start_matches(['-', '*', ' ']);
            let Some((head, value)) = t.split_once(':') else { continue };
            // The name, then what it is about, however it is spaced: `VERDICT 2`, `INTERACTION 1, 2`.
            let mut words = head.split_whitespace();
            let Some(name) = words.next() else { continue };
            let at: String = words.collect();
            if at.is_empty() {
                continue;
            }
            let at = at.as_str();
            let name = name.to_uppercase();
            if name == "PREVIOUS" {
                if let Some((k, finding)) = at.split_once('.') {
                    if let Some(k) = index(k) {
                        parts[k].push(format!("PREVIOUS {finding}:{value}"));
                    }
                }
            } else if name == "INTERACTION" {
                let pair: Vec<Option<usize>> = at.split(',').map(index).collect();
                if let [Some(a), Some(b)] = pair[..] {
                    if a != b {
                        interactions.push((a.min(b), a.max(b), value.trim().to_string()));
                    }
                }
            } else if name == "VERDICT" || self.flags.iter().any(|f| f.name.eq_ignore_ascii_case(&name)) {
                if let Some(k) = index(at) {
                    parts[k].push(format!("{name}: {}", value.trim()));
                }
            }
        }
        // A finding that names no change could be any change whose diff touches what it cites.
        let mut unattributed = vec![false; n];
        for s in &stray {
            let cited: Vec<String> = citations(s).into_iter().map(|(p, _)| p.replace('\\', "/")).collect();
            let touches = |c: &BatchChange| c.files.iter().any(|f| cited.iter().any(|p| f == p || f.ends_with(&format!("/{p}"))));
            let some: Vec<usize> = (0..n).filter(|&k| touches(&changes[k])).collect();
            for k in if some.is_empty() { (0..n).collect() } else { some } {
                unattributed[k] = true;
            }
        }
        let reviews = changes
            .iter()
            .enumerate()
            .map(|(k, c)| {
                if unattributed[k] {
                    return Err("the batched review listed a finding without saying which change it belongs to, and it could be this one's".to_string());
                }
                if !parts[k].iter().any(|l| l.starts_with("VERDICT:")) {
                    return Err(format!("the batched review gave no VERDICT for it (change {} of {n})", k + 1));
                }
                self.parse(reviewer, &parts[k].join("\n"), seconds, c.previous.as_ref(), Some(&c.worktree)).map_err(|e| format!("its part of the batched review was unfinished: {e:#}"))
            })
            .collect();
        BatchReply { reviews, interactions }
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
        self.run_checked(reviewer,worktree,base,previous,&mut ||self.require_enabled())
    }
    pub(crate) fn run_checked(&self,reviewer:&str,worktree:&Path,base:&str,previous:Option<&PreviousRound>,check:&mut dyn FnMut()->Result<()>)->Result<Review> {
        let prompt = match previous {
            Some(prev) if prev.in_history && base_merged_since(worktree, base, &prev.commit, "HEAD") => self.prompt_after_merge(base, prev),
            _ => self.prompt(base, previous),
        };
        let reply = self.run_prompt_checked(reviewer, worktree, &with_submodule_code(prompt, worktree, base),check)?;
        let mut review =
            self.parse(reviewer, &reply.text, reply.seconds, previous, Some(worktree)).map_err(|e| if reply.only_a_login_message { reply.signed_out(self) } else { e })?;
        review.tokens = reply.tokens;
        set_aside_outside(&mut review, worktree, base, "HEAD");
        Ok(review)
    }

    /// Runs `reviewer` in `worktree` with `prompt` on its stdin, and returns what it wrote. A
    /// reviewer that cannot run is a `CouldNotRun`; one that ran past its timeout, an error.
    pub fn run_prompt(&self, reviewer: &str, worktree: &Path, prompt: &str) -> Result<Reply> {
        self.run_prompt_checked(reviewer, worktree, prompt, &mut || Ok(()))
    }

    /// PR reviews obey the live review switch; other uses of the launcher, such as triage, do not.
    pub fn run_review_prompt(&self, reviewer: &str, worktree: &Path, prompt: &str) -> Result<Reply> {
        self.run_prompt_checked(reviewer, worktree, prompt, &mut || self.require_enabled())
    }

    pub(crate) fn run_prompt_checked(&self, reviewer: &str, worktree: &Path, prompt: &str, check: &mut dyn FnMut() -> Result<()>) -> Result<Reply> {
        check()?;
        let command = self
            .commands
            .get(reviewer)
            .filter(|c| !c.is_empty())
            .with_context(|| format!("the playbook has no command for the {reviewer} reviewer (add review.commands.{reviewer})"))?;
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
        let could_not = |why: &str, detail: String| anyhow::Error::new(CouldNotRun { reviewer: reviewer.to_string(), why: why.to_string(), detail });
        // A review counts against the machine as a build does (NIT-266): it starts only while the
        // machine has memory to spare, and it and what it runs (a reviewer may build and test) are
        // held to a run's cap, its share of cores and below-normal priority.
        crate::machine::admit_here_checked(check)?;
        check()?;
        // The review's own time starts once it may run.
        let started = Instant::now();
        let share = crate::lanes::share();
        crate::lanes::share_env(&mut cmd, share.jobs);
        let guard = crate::procs::TreeGuard::with(crate::procs::Contain { memory_cap: share.cap, low_priority: true, end_with_guard: true });
        let spawned = match &guard {
            Some(g) => g.spawn(&mut cmd),
            None => cmd.spawn(),
        };
        let mut child = spawned.map_err(|e| could_not("did not start", format!("{}: {e}", command[0])))?;
        let mut tracked = crate::performance_attribution::track_child(&child, "review");
        let mut stdin = child.stdin.take().unwrap();
        let prompt = prompt.to_string();
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
                if let Some(g) = &guard {
                    g.stop();
                }
                child.kill().ok();
                child.wait().ok();
                if let Some(t) = tracked.take() { t.finish(guard.as_ref().and_then(|g| g.accounting())); }
                bail!("the {reviewer} review took longer than {} minutes and was stopped", self.timeout_minutes);
            }
            if guard.as_ref().is_some_and(|g| g.over_memory()) {
                if let Some(g) = &guard {
                    g.stop();
                }
                child.kill().ok();
                child.wait().ok();
                if let Some(t) = tracked.take() { t.finish(guard.as_ref().and_then(|g| g.accounting())); }
                let cap = share.cap.map(crate::machine::gb).unwrap_or_default();
                return Err(could_not("ran out of memory", format!("it and what it started went over the {cap} memory cap a run has (Settings > Build lanes) and were stopped")));
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        if let Some(t) = tracked { t.finish(guard.as_ref().and_then(|g| g.accounting())); }
        writer.join().ok();
        let stdout = out_reader.join().ok().and_then(|r| r.ok()).unwrap_or_default();
        let stderr = err_reader.join().ok().and_then(|r| r.ok()).unwrap_or_default();
        let text = if uses_file { std::fs::read_to_string(&out_file).unwrap_or_default() } else { stdout.clone() };
        std::fs::remove_file(&out_file).ok();
        let printed = format!("{text}\n{stdout}{stderr}");
        let reply = Reply {
            reviewer: reviewer.to_string(),
            // A reviewer that is signed out may still exit 0, printing why instead of a review: a
            // short reply with no findings in it. A long or partial review that mentions logins is
            // unfinished.
            only_a_login_message: printed.len() < 1000 && !text.contains("- [P") && says_signed_out(&printed),
            tokens: tokens_used(&printed),
            text,
            printed,
            seconds: started.elapsed().as_secs(),
        };
        match exit_outcome(status.success(), &reply.printed) {
            Exit::Replied => Ok(reply),
            Exit::SignedOut => Err(reply.signed_out(self)),
            Exit::Failed => Err(could_not(&format!("exited with {status}"), tail(&reply.printed))),
        }
    }
}

/// What a reviewer's exit means, before its text is read (NIT-539).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Replied,
    SignedOut,
    Failed,
}

/// A reviewer that exited 0 replied (a signed-out one that still exits 0 is told apart by
/// `Reply::only_a_login_message`); one that failed and printed a sign-in message is signed out.
pub(crate) fn exit_outcome(success: bool, printed: &str) -> Exit {
    match (success, says_signed_out(printed)) {
        (true, _) => Exit::Replied,
        (false, true) => Exit::SignedOut,
        (false, false) => Exit::Failed,
    }
}

/// What a reviewer wrote.
pub struct Reply {
    reviewer: String,
    pub text: String,
    /// The reply and everything the reviewer printed, for its error.
    printed: String,
    pub seconds: u64,
    /// The tokens the reviewer said it used, when it says.
    pub tokens: Option<u64>,
    /// Short, with no findings, and about signing in: the reviewer could not run, whatever its
    /// exit code said.
    pub only_a_login_message: bool,
}

/// What a reviewer printed about its tokens: Codex ends with `tokens used` and the count, on that
/// line or the next (`4,755`).
fn tokens_used(printed: &str) -> Option<u64> {
    let lines: Vec<&str> = printed.lines().collect();
    let at = lines.iter().rposition(|l| l.to_lowercase().contains("tokens used"))?;
    let count = |l: &str| -> Option<u64> {
        let digits: String = l.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
        digits.parse().ok()
    };
    let after = lines[at].to_lowercase();
    let after = &after[after.find("tokens used").unwrap_or(0) + "tokens used".len()..];
    count(after).or_else(|| lines.get(at + 1).and_then(|l| count(l)))
}

impl Reply {
    pub fn signed_out(&self, rules: &ReviewRules) -> anyhow::Error {
        anyhow::Error::new(CouldNotRun {
            reviewer: self.reviewer.clone(),
            why: "not signed in".into(),
            detail: format!("{}.\n{}", login_hint(&self.reviewer, rules), tail(&self.printed)),
        })
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

    /// Whether an open answer holds up: it cites a file and line on the branch, quotes code that is
    /// at that line (or within three lines of it), and says in a few words how that code still
    /// causes the problem. A citation of code that merely exists, with no reason, is how a reviewer
    /// carried fixed findings forward by pointing at the fix (NIT-214).
    fn confirms_open(&self, said: &str) -> bool {
        let squash = |s: &str| s.split_whitespace().collect::<String>();
        let lone_path = |q: &str| q.split_whitespace().count() == 1 && !citations(q).is_empty();
        // Any length counts here: the quote must sit within a few lines of the cited one, so even
        // `n - 1` cannot match by chance the way it could anywhere in a file.
        let quotes: Vec<String> = quoted(said).into_iter().filter(|q| !lone_path(q) && !squash(q).is_empty()).collect();
        let at_line = citations(said).into_iter().filter_map(|(p, line)| Some((self.resolve(&p)?, line?))).any(|(f, line)| {
            let Ok(body) = std::fs::read_to_string(self.root.join(&f)) else { return false };
            let lines: Vec<&str> = body.lines().collect();
            if line == 0 || line > lines.len() {
                return false;
            }
            let near = squash(&lines[line.saturating_sub(4)..(line + 3).min(lines.len())].join("\n"));
            quotes.iter().any(|q| near.contains(&squash(q)))
        });
        // The reason: what is left once the citation and the quoted code are taken out.
        let mut rest = said.to_string();
        for q in quoted(said) {
            rest = rest.replacen(&format!("`{q}`"), " ", 1);
        }
        let words = rest
            .split_whitespace()
            .filter(|w| citations(w).is_empty())
            .filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 2)
            .count();
        at_line && words >= 3
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
        // Written with 8.3 short names (`src/FOOBAR~1.RS`): the tracked file at the same place once
        // short names are spelled out (NIT-133).
        if path.contains('~') {
            let place = crate::paths::normalize(&self.root.join(&path).to_string_lossy());
            if let Some(f) = self.files.iter().find(|f| crate::paths::normalize(&self.root.join(f).to_string_lossy()) == place) {
                return Some(f.clone());
            }
        }
        let mut ends = self.files.iter().filter(|f| f.ends_with(&format!("/{path}")));
        match (ends.next(), ends.next()) {
            (Some(one), None) => Some(one.clone()),
            _ => None,
        }
    }
}

/// Whether a finding names a file: a citation, or, where the branch can be read, the word after its
/// title (`title: Makefile, target: ...`) being one of its tracked files.
fn names_a_file(text: &str, code: Option<&CurrentCode>) -> bool {
    if !citations(text).is_empty() {
        return true;
    }
    let Some(code) = code else { return false };
    let place = text.split_once(": ").map(|(_, rest)| rest).unwrap_or(text);
    let word = place.split([',', ':', ' ']).next().unwrap_or("").trim_matches('`');
    !word.is_empty() && code.resolve(word).is_some()
}

/// Whether `base` came into the branch between `commit` and `head`: their merge bases with it
/// differ, as after `git merge main` into the branch (NIT-646). False when git cannot tell.
pub(crate) fn base_merged_since(worktree: &Path, base: &str, commit: &str, head: &str) -> bool {
    let merge_base = |c: &str| crate::playbook::git(worktree, &["merge-base", base, c]).ok().map(|o| o.trim().to_string());
    match (merge_base(commit), merge_base(head)) {
        (Some(then), Some(now)) => then != now,
        _ => false,
    }
}

/// The commits the base brought into the branch by merging it after the branch began: on the
/// base, after the commit the branch forked from (the parent of its oldest first-parent commit),
/// up to their merge base now. Empty when the base was never merged in; None when git cannot say.
fn brought_in_by_merges(worktree: &Path, base: &str, head: &str) -> Option<std::collections::HashSet<String>> {
    let git = |args: &[&str]| crate::playbook::git(worktree, args).ok().map(|o| o.trim().to_string());
    let merge_base = git(&["merge-base", base, head])?;
    let oldest = git(&["rev-list", "--first-parent", "--reverse", &format!("{base}..{head}")])?.lines().next()?.to_string();
    let fork = git(&["rev-parse", &format!("{oldest}^")])?;
    if fork == merge_base {
        return Some(Default::default());
    }
    Some(git(&["rev-list", &format!("{fork}..{merge_base}")])?.lines().map(str::to_string).collect())
}

/// Whether every line `text` cites was last written by one of `brought` (code the base brought in
/// by a merge, which the change did not touch since). A citation without a line, or of a file the
/// branch does not have, is about the change.
fn only_merged_code(text: &str, code: &CurrentCode, worktree: &Path, head: &str, brought: &std::collections::HashSet<String>) -> bool {
    let cited = citations(text);
    !cited.is_empty()
        && cited.iter().all(|(path, line)| {
            let (Some(file), Some(n)) = (code.resolve(path), line) else { return false };
            let range = format!("{n},{n}");
            crate::playbook::git(worktree, &["blame", "--porcelain", "-L", &range, head, "--", &file])
                .ok()
                .and_then(|o| o.split_whitespace().next().map(str::to_string))
                .is_some_and(|commit| brought.contains(&commit))
        })
}

/// Moves new findings that are entirely about code the base brought in by merging it into the
/// branch, which the change did not touch, to `review.outside`, and sets the verdict from what is
/// left (NIT-646). A branch that never merged its base has nothing to set aside, and a regression
/// the change causes in code it does not touch is kept unless that code itself came in by the
/// merge. Carried findings stay: the reviewer has already confirmed them against current code.
pub(crate) fn set_aside_outside(review: &mut Review, worktree: &Path, base: &str, head: &str) {
    let Some(brought) = brought_in_by_merges(worktree, base, head).filter(|b| !b.is_empty()) else { return };
    let code = CurrentCode::new(worktree);
    let (outside, inside): (Vec<Finding>, Vec<Finding>) =
        std::mem::take(&mut review.findings).into_iter().partition(|f| !f.carried && only_merged_code(&f.text, &code, worktree, head, &brought));
    review.findings = inside;
    review.outside.extend(outside);
    if review.reused.is_none() {
        review.verdict = if review.findings.iter().any(|f| f.priority <= 1) { "changes" } else { "pass" }.into();
    }
}

/// `path/to/file.rs:120` and `file.rs` mentions in a text, with their line when given.
pub(crate) fn citations(text: &str) -> Vec<(String, Option<usize>)> {
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
            out.push(Finding { priority: priority as u8, text, carried: false, raised_as: None, now: None });
        }
    }
    out
}

/// The `- [BASE] ...` lines: defects the reviewer says are the base's own, not this change's
/// (NIT-646). They hold nothing; P2 marks them as not blocking wherever they are shown.
pub fn parse_base_findings(text: &str) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix("- [BASE]").or_else(|| l.strip_prefix("* [BASE]")) else { continue };
        let text = rest.trim().to_string();
        if !text.is_empty() && !out.iter().any(|f| f.text == text) {
            out.push(Finding { priority: 2, text, carried: false, raised_as: None, now: None });
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
pub fn is_session_marker(name: &str) -> bool {
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
pub fn resolve_program(program: &str) -> String {
    #[cfg(windows)]
    if Path::new(program).extension().is_none() && Path::new(program).components().count() == 1 && which(program).is_none() {
        if let Some(script) = which_with(program, &["cmd", "bat"]) {
            return script.to_string_lossy().into_owned();
        }
        if program == "codex" {
            // Asked of PowerShell once per process: the crew resolves its agents on every tick, and
            // a PowerShell start each time is a second of CPU (NIT-289). A Store update moves the
            // folder, so a found path is checked again on each use.
            static STORE_CODEX: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
            let mut found = STORE_CODEX.lock().unwrap_or_else(|e| e.into_inner());
            if found.as_deref().is_none_or(|exe| !Path::new(exe).exists()) {
                let out = shell_free_command("powershell")
                    .args(["-NoProfile", "-Command", "(Get-AppxPackage -Name 'OpenAI.Codex*' | Select-Object -First 1).InstallLocation"])
                    .output();
                *found = out.ok().map(|out| Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("app").join("resources").join("codex.exe")).filter(|exe| exe.exists()).map(|exe| exe.to_string_lossy().into_owned());
            }
            if let Some(exe) = found.clone() {
                return exe;
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

/// A prompt whose change moves a submodule asks for its diff with the submodule's code in it
/// (`git diff --submodule=diff`), so the reviewer reads the code, not a one-line pointer change.
fn with_submodule_code(prompt: String, worktree: &Path, base: &str) -> String {
    let moves = crate::playbook::git(worktree, &["diff", "--raw", "--no-renames", &format!("{base}...HEAD")])
        .is_ok_and(|raw| raw.lines().any(|l| l.split_whitespace().take(2).any(|mode| mode.trim_start_matches(':') == "160000")));
    if moves {
        prompt.replace("`git diff ", "`git diff --submodule=diff ")
    } else {
        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit rules `runs_a_reviewer_command` checks by starting seven reviewers.
    #[test]
    fn a_reviewers_exit_says_whether_it_replied_is_signed_out_or_failed() {
        assert_eq!(exit_outcome(true, "- [P1] something"), Exit::Replied);
        assert_eq!(exit_outcome(true, "Please log in"), Exit::Replied, "exit 0 is a reply; its text is judged later");
        assert_eq!(exit_outcome(false, "Error: not logged in. Run codex login"), Exit::SignedOut);
        assert_eq!(exit_outcome(false, "panicked at main.rs"), Exit::Failed);
    }

    fn rules() -> ReviewRules {
        ReviewRules {
            flags: vec![Flag { name: "MONEY_OR_RELEASE".into(), question: "the change alters billing".into(), reason: "money or release logic".into() }],
            ..Default::default()
        }
    }

    #[test]
    fn disabled_policy_is_default_on_and_reloaded_for_new_launches_only() {
        assert!(crate::playbook::Playbook::parse("").unwrap().review.enabled);
        let root = crate::paths::unique_temp("nitely-review-policy", "");
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        let file = root.join(crate::playbook::PLAYBOOK_PATH);
        std::fs::write(&file, "[review]\nenabled = true\n").unwrap();
        let rules = ReviewRules { policy_root: Some(root.clone()), ..Default::default() };
        rules.require_enabled().unwrap();
        std::fs::write(&file, "[review]\nenabled = false\n").unwrap();
        assert!(rules.run_review_prompt("not-installed", &root, "unused").err().unwrap().is::<Disabled>());
        assert!(ReviewRules::default().require_enabled().is_ok(), "another project's policy is isolated");
        std::fs::write(&file, "[review]\nenabled = true\n").unwrap();
        rules.require_enabled().unwrap();
        let mut running = rules.clone();
        running.commands.insert("fake".into(), vec![crate::playbook::posix_sh(), "-c".into(),
            "cat >/dev/null; printf '[review]\\nenabled = false\\n' > .nitely/playbook.toml; printf 'VERDICT: clean\\n'".into()]);
        let reply = running.run_review_prompt("fake", &root, "read this").unwrap();
        assert!(reply.text.contains("VERDICT: clean"), "an already-running reviewer keeps its actual result");
        assert!(running.require_enabled().unwrap_err().is::<Disabled>(), "the next launch is disabled");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_lists_every_closing_line() {
        let p = rules().prompt("main", None);
        assert!(p.contains("git diff main...HEAD"));
        assert!(p.contains("exactly these 2 lines:\nVERDICT: <findings or clean>\nMONEY_OR_RELEASE: <yes or no>"));
        assert!(!p.contains("PREVIOUS"));
    }

    fn previous(in_history: bool) -> PreviousRound {
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: false, raised_as: None, now: None };
        PreviousRound {
            passed_on: None,
            commit: "abc1234".into(),
            findings: vec![f(1, "Drops the last row: src/a.rs, f"), f(2, "Empty name: src/b.rs, g"), f(2, "Slow loop: src/c.rs, h")],
            in_history,
            accepted: vec![],
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
        // Findings the person accepted are told apart, not checked again.
        let accepted = PreviousRound {
            passed_on: None,
            findings: vec![],
            accepted: vec![(Finding { priority: 1, text: "No migration: src/k.rs, load".into(), carried: false, raised_as: None, now: None }, "no outside users yet".into())],
            ..previous(true)
        };
        let p = rules().prompt("main", Some(&accepted));
        assert!(p.contains("accepted these findings") && p.contains("- [P1] No migration: src/k.rs, load (accepted: no outside users yet)"), "{p}");
        assert!(!p.contains("PREVIOUS <n>"), "{p}");
        let clean = PreviousRound { passed_on: None, findings: vec![], ..previous(true) };
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
        assert_eq!(r.findings, vec![Finding { priority: 1, text: "Drops the last row: src/a.rs, f".into(), carried: true, raised_as: None, now: None }]);

        // Everything fixed and nothing new is clean; an unfinished reply is still unfinished.
        let text = "PREVIOUS 1: fixed\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed\nVERDICT: clean\nMONEY_OR_RELEASE: no";
        let r = rules().parse("codex", text, 3, Some(&prev), None).unwrap();
        assert!(r.findings.is_empty() && r.verdict == "pass");
        assert!(rules().parse("codex", "PREVIOUS 1: fixed\nMONEY_OR_RELEASE: no", 1, Some(&prev), None).is_err());
    }

    #[test]
    fn a_citation_in_short_names_finds_the_tracked_file() {
        // New files get no 8.3 name where the volume makes none, but Program Files usually has one
        // (PROGRA~1); treat the drive as the worktree and Program Files as a tracked path (NIT-133).
        let Some(program_files) = std::env::var_os("ProgramFiles").map(|p| p.to_string_lossy().into_owned()) else { return };
        let short = crate::paths::short_path(&program_files);
        if !cfg!(windows) || short == program_files {
            return;
        }
        let root = Path::new(&program_files).parent().unwrap().to_path_buf();
        let name = |p: &str| Path::new(p).file_name().unwrap().to_string_lossy().into_owned();
        let code = CurrentCode { root: root.clone(), files: vec![format!("{}/Common Files", name(&program_files))] };
        let tracked = Some(code.files[0].clone());
        assert_eq!(code.resolve(&format!("{}/Common Files", name(&short))), tracked, "relative, short folder");
        assert_eq!(code.resolve(&format!(r"{short}\Common Files")), tracked, "absolute, short folder");
        assert_eq!(code.resolve(&format!("{}/Other Files", name(&short))), None, "a file that is not tracked");
    }

    /// NIT-646: main merged into a branch after its review brings main's code along. That code is
    /// context, so the next round compares the change with itself instead of reading the merge,
    /// and a finding about main's own lines is set aside: it neither holds the land nor reaches
    /// the person to accept.
    #[test]
    fn after_main_is_merged_in_a_finding_on_mains_own_code_is_set_aside() {
        let dir = crate::paths::unique_temp("nitely-review-outside", "");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            let out = crate::playbook::run_git(std::process::Command::new("git").arg("-C").arg(&dir).args(args)).unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let write = |path: &str, body: String| std::fs::write(dir.join(path), body).unwrap();
        let lines = |n: usize, what: &str| (1..=n).map(|i| format!("{what} {i}\n")).collect::<String>();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        write("src/goal.rs", lines(20, "goal"));
        write("src/quick.rs", lines(20, "quick"));
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        git(&["checkout", "-qb", "goal"]);
        // The change: line 10 of goal.rs.
        write("src/goal.rs", lines(20, "goal").replace("goal 10\n", "goal ten, changed\n"));
        git(&["commit", "-qam", "the change"]);
        let reviewed = git(&["rev-parse", "HEAD"]);
        // Main moves on in another file, and in goal.rs far from the change, and comes in by a merge.
        git(&["checkout", "-q", "main"]);
        write("src/quick.rs", lines(20, "quick").replace("quick 5\n", "quick five, from main\n"));
        write("src/goal.rs", lines(20, "goal").replace("goal 19\n", "goal nineteen, from main\n"));
        git(&["commit", "-qam", "main moves on"]);
        git(&["checkout", "-q", "goal"]);
        git(&["merge", "-q", "--no-edit", "main"]);

        assert!(base_merged_since(&dir, "main", &reviewed, "HEAD"), "main came in since that review");
        assert!(!base_merged_since(&dir, "main", "HEAD", "HEAD"));
        let prev = PreviousRound { passed_on: None, commit: reviewed.clone(), findings: vec![], in_history: true, accepted: vec![] };
        let prompt = rules().prompt_after_merge("main", &prev);
        assert!(prompt.contains("git diff main...HEAD") && prompt.contains(&format!("git diff main...{reviewed}")), "{prompt}");
        assert!(!prompt.contains(&format!("git diff {reviewed} HEAD")), "never the diff that holds the merge: {prompt}");

        let reply = "Two problems.\n\
- [P1] Quick start loses the flag: src/quick.rs:5, start\n\
- [P1] Main's tail is wrong: src/goal.rs:19, tail\n\
- [P1] The changed line breaks parsing: src/goal.rs:10, parse\n\
- [P1] A caller the change breaks: src/quick.rs:2, unwrap\n\
- [P2] Off by one next to the change: goal.rs:11\n\
VERDICT: findings\nMONEY_OR_RELEASE: no";
        let texts = |fs: &[Finding]| fs.iter().map(|f| f.text.split(':').next().unwrap().to_string()).collect::<Vec<_>>();
        // Before the merge nothing came in from main, so nothing is set aside, whatever it cites.
        let mut before = rules().parse("codex", reply, 1, None, Some(&dir)).unwrap();
        set_aside_outside(&mut before, &dir, "main", &reviewed);
        assert!(before.outside.is_empty(), "{:?}", before.outside);

        let mut review = rules().parse("codex", reply, 1, None, Some(&dir)).unwrap();
        set_aside_outside(&mut review, &dir, "main", "HEAD");
        assert_eq!(texts(&review.findings), ["The changed line breaks parsing", "A caller the change breaks", "Off by one next to the change"]);
        assert_eq!(texts(&review.outside), ["Quick start loses the flag", "Main's tail is wrong"]);
        assert_eq!(review.verdict, "changes", "the change's own P1 still holds it");

        // The reviewer can say a defect is the base's own, whenever it came in: it holds nothing.
        let r = rules().parse("codex", "- [BASE] Old overflow: src/quick.rs:2, parse\nVERDICT: findings\nMONEY_OR_RELEASE: no", 1, None, Some(&dir)).unwrap();
        assert_eq!((r.verdict.as_str(), r.findings.len(), texts(&r.outside)), ("pass", 0, vec!["Old overflow".to_string()]));

        // A carried finding the reviewer confirmed open stays, whatever its first citation says now.
        let carried = PreviousRound {
            findings: vec![Finding { priority: 1, text: "Quick start loses the flag: src/quick.rs:5, start".into(), carried: false, raised_as: None, now: None }],
            ..prev.clone()
        };
        let mut review = rules().parse("codex", "PREVIOUS 1: open src/quick.rs:5 `quick five, from main` still drops it\nVERDICT: clean\nMONEY_OR_RELEASE: no", 1, Some(&carried), Some(&dir)).unwrap();
        set_aside_outside(&mut review, &dir, "main", "HEAD");
        assert_eq!((review.findings.len(), review.outside.len(), review.verdict.as_str()), (1, 0, "changes"));

        // With only main's code to object to, the change passes.
        let mut review = rules().parse("codex", "- [P1] Quick start loses the flag: src/quick.rs:5, start\nVERDICT: findings\nMONEY_OR_RELEASE: no", 1, None, Some(&dir)).unwrap();
        set_aside_outside(&mut review, &dir, "main", "HEAD");
        assert_eq!((review.verdict.as_str(), review.findings.len(), review.outside.len()), ("pass", 0, 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_open_finding_must_point_at_code_still_on_the_branch() {
        let dir = crate::paths::unique_temp("nitely-recheck", "");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| assert!(crate::playbook::run_git(std::process::Command::new("git").arg("-C").arg(&dir).args(args)).unwrap().status.success());
        git(&["init", "-q"]);
        std::fs::write(dir.join("src/proposals.rs"), "fn apply() {\n    let closed = true;\n}\n".to_string() + &"// more\n".repeat(9)).unwrap();
        git(&["add", "."]);
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: false, raised_as: None, now: None };
        let prev = PreviousRound {
            passed_on: None,
            commit: "abc1234".into(),
            findings: vec![
                f(1, "Returns to Todo after a failed apply: crates/nitely-core/src/proposals.rs:488, apply"),
                f(2, "Schema v10 column: crates/nitely-core/src/db.rs, migrate"),
                f(1, "Apply is not atomic: src/proposals.rs, apply"),
            ],
            in_history: true,
            accepted: vec![],
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
        // The reviewer's reason travels with the carried finding; its text stays as it was raised.
        let carried = r.findings.iter().find(|f| f.carried).unwrap();
        assert_eq!(carried.text, "Apply is not atomic: src/proposals.rs, apply");
        assert_eq!(carried.now.as_deref(), Some("proposals.rs:2 `let closed = true;` is set before the write"));
        let again = PreviousRound { passed_on: None, commit: "abc9999".into(), findings: r.findings.clone(), in_history: true, accepted: vec![] };
        assert!(rules().prompt("main", Some(&again)).contains("Last round said it was still open: proposals.rs:2"));

        // Code that is in the file but not at the cited line, or a citation with no reason, is not
        // an open answer.
        let r = review("PREVIOUS 1: fixed\nPREVIOUS 2: fixed\nPREVIOUS 3: open proposals.rs:12 `let closed = true;` is set before the write");
        assert_eq!(statuses(&r), ["fixed", "fixed", "unconfirmed"], "the quote is on line 2, not near 12");
        let r = review("PREVIOUS 1: fixed\nPREVIOUS 2: fixed\nPREVIOUS 3: open proposals.rs:2 `let closed = true;`");
        assert_eq!(statuses(&r), ["fixed", "fixed", "unconfirmed"], "no reason given");
        // A short quote at its line is enough.
        let r = review("PREVIOUS 1: fixed
PREVIOUS 2: fixed
PREVIOUS 3: open proposals.rs:2 `true` still closes before the write");
        assert_eq!(statuses(&r), ["fixed", "fixed", "open"]);

        // Not mentioned: carried as it was while the file it names exists, unconfirmed once it is gone.
        let r = review("`PREVIOUS 1: fixed`\nPREVIOUS 2: fixed");
        assert_eq!(statuses(&r), ["fixed", "fixed", "unchecked"], "a backticked answer counts");
        assert_eq!(r.findings[0].priority, 1);

        // Carried lower, a finding keeps the priority it was raised at: confirmed open later (here
        // by an absolute path), it blocks again at P1, without the unconfirmed mark.
        let first = review("PREVIOUS 1: open\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed");
        assert_eq!((first.findings[0].priority, first.findings[0].raised_as), (2, Some(1)));
        let next = PreviousRound { passed_on: None, commit: "def5678".into(), findings: first.findings.clone(), in_history: true, accepted: vec![] };
        let absolute = dir.join("src").join("proposals.rs");
        let text = format!("PREVIOUS 1: open {}:2 `let closed = true;` still closes before the write\nVERDICT: clean\nMONEY_OR_RELEASE: no", absolute.display());
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

    /// NIT-197, rounds 2 and 3 (2026-10-17): the reviewer called two fixed findings open by citing
    /// the code that fixed them, with no reason, and both were carried at their priority, which sent
    /// the issue to a person as "not converging".
    #[test]
    fn a_citation_of_the_fix_without_a_reason_does_not_keep_a_finding_open() {
        let dir = crate::paths::unique_temp("nitely-recheck-197", "");
        std::fs::create_dir_all(dir.join("crates/nitely-cli/src")).unwrap();
        std::fs::create_dir_all(dir.join("crates/nitely-core/src")).unwrap();
        let git = |args: &[&str]| assert!(crate::playbook::run_git(std::process::Command::new("git").arg("-C").arg(&dir).args(args)).unwrap().status.success());
        git(&["init", "-q"]);
        // The fixed code, at the lines the reviewer cited (139 and 529).
        let cli: String = (1..=138).map(|i| format!("// line {i}\n")).collect::<String>() + "                out.push_str(&\"\\\\\".repeat(backslashes * 2 + 1));\n";
        std::fs::write(dir.join("crates/nitely-cli/src/lanes.rs"), cli).unwrap();
        let core: String = (1..=528).map(|i| format!("// line {i}\n")).collect::<String>()
            + "    let same = !command.trim().is_empty() && crate::procs::command_line(pid).is_some_and(|line| line.contains(command.trim()));\n";
        std::fs::write(dir.join("crates/nitely-core/src/lanes.rs"), core).unwrap();
        git(&["add", "."]);
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: true, raised_as: None, now: None };
        let prev = PreviousRound {
            passed_on: None,
            commit: "4567a11".into(),
            findings: vec![
                f(2, "Stale PID recovery can kill unrelated processes: crates/nitely-core/src/lanes.rs:518, `stop_leftovers`: after a requester crashes and its child exits, Windows can reuse the recorded PID. Checking only `pid_alive` then calling `stop_tree` forcibly terminates the unrelated process and its descendants without verifying their identity."),
                f(1, "CLI drops argument quoting: crates/nitely-cli/src/lanes.rs:78, `run`: joining arguments with spaces and reparsing them through a shell breaks quoted arguments such as `nitely run -- node -e \"console.log('ok')\"` and interprets literal shell metacharacters as commands."),
            ],
            in_history: true,
            accepted: vec![],
        };
        // The reviewer's reply on 2463af7, word for word.
        let reply = "PREVIOUS 1: open crates/nitely-core/src/lanes.rs:529 `let same = !command.trim().is_empty() && crate::procs::command_line(pid).is_some_and(|line| line.contains(command.trim()));`\n\
PREVIOUS 2: open crates/nitely-cli/src/lanes.rs:139 `out.push_str(&\"\\\\\".repeat(backslashes * 2 + 1));`\nVERDICT: clean\nMONEY_OR_RELEASE: no";
        let r = rules().parse("codex", reply, 1, Some(&prev), Some(&dir)).unwrap();
        assert_eq!(r.previous.iter().map(|p| p.status.as_str()).collect::<Vec<_>>(), ["unconfirmed", "unconfirmed"]);
        assert_eq!(r.verdict, "pass", "no P1 is carried on an answer that only points at the fix: {:?}", r.findings);
        assert!(r.findings.iter().all(|f| f.priority >= 2 && f.now.is_none()));
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

        // One that could not run is passed over, and the note says why, so a playbook that requires
        // another vendor still sends the review to a person.
        let out = |reviewer: &str, why: &str| CouldNotRun { reviewer: reviewer.into(), why: why.into(), detail: String::new() };
        let after = |names, failed: &[CouldNotRun]| r.choose_after("claude", &only(names), failed);
        assert_eq!(after(&["claude", "codex"], &[out("codex", "not signed in")]).unwrap(), c("claude", false, "same vendor: codex could not run: not signed in"));
        let both = after(&["claude", "codex"], &[out("codex", "not signed in"), out("claude", "exited with exit code: 1")]).unwrap_err().to_string();
        assert!(both.starts_with("no reviewer could run on this machine: codex is not signed in (run `codex login`); the claude reviewer could not run (exited"), "{both}");

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
        let dir = crate::paths::unique_temp("nitely-which", "");
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

        // A reviewer that is signed out, whether it exits with an error or with 0, could not run;
        // one that fails otherwise could not run either; one that ran but did not finish its
        // review is an ordinary error.
        let sh = |script: &str| vec![crate::playbook::posix_sh(), "-c".into(), format!("cat > /dev/null; {script}")];
        let could_not = |script: &str| {
            let mut r = rules();
            r.commands.insert("fake".into(), sh(script));
            r.run("fake", &std::env::temp_dir(), "main", None).unwrap_err().downcast::<CouldNotRun>()
        };
        let out = could_not("echo 'Error: OAuth session expired and cannot be refreshed' >&2; exit 1").unwrap();
        assert_eq!(out.why, "not signed in");
        assert!(out.detail.contains("sign `") && out.detail.contains("OAuth session expired"), "{}", out.detail);
        assert_eq!(could_not("echo 'Invalid API key - Please run /login'").unwrap().why, "not signed in");
        assert!(could_not("echo 'disk full' >&2; exit 3").unwrap().why.starts_with("exited with"));
        assert!(could_not("echo 'I looked around but ran out of time'").is_err(), "an unfinished review is not a reviewer that could not run");
        assert!(
            could_not("echo '- [P1] Leaks the token: auth.rs, login: the API key is logged on a 401'").is_err(),
            "an unfinished review about logins is not a reviewer that is signed out"
        );
        let mut r = rules();
        r.commands.insert("gone".into(), vec!["nitely-test-no-such-reviewer".into()]);
        assert_eq!(r.run("gone", &std::env::temp_dir(), "main", None).unwrap_err().downcast::<CouldNotRun>().unwrap().why, "did not start");
    }

    /// Two changes in one reviewer call: each finding, previous answer, verdict and flag goes to the
    /// change it names, even when both touch one file (NIT-216).
    #[test]
    fn a_batched_reply_is_split_by_change() {
        let dir = crate::paths::unique_temp("nitely-batch-parse", "");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shared.rs"), "let total = rows.len();\nlet edge = 2;\n".to_string() + &"// more\n".repeat(9)).unwrap();
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: false, raised_as: None, now: None };
        let change = |k: &str, files: &[&str], previous: Option<PreviousRound>| BatchChange {
            key: k.into(),
            title: format!("{k} title"),
            branch: format!("claude/{}-x", k.to_lowercase()),
            base: "b0".into(),
            head: format!("h{k}"),
            worktree: dir.clone(),
            files: files.iter().map(|f| f.to_string()).collect(),
            previous,
        };
        let earlier = PreviousRound {
            passed_on: None,
            commit: "c0".into(),
            findings: vec![f(2, "Edge: shared.rs:2, the empty line")],
            in_history: true,
            accepted: vec![(f(2, "Slow: shared.rs"), "fine for now".into())],
        };
        let changes = vec![change("NIT-1", &["shared.rs"], Some(earlier)), change("NIT-2", &["shared.rs", "other.rs"], None)];
        let rules = rules();

        // The prompt: a section per change, its previous findings numbered by change, what was
        // accepted, the single prompt's severities word for word, and the closing lines per change.
        let p = rules.batch_prompt(&changes);
        assert!(p.contains("## CHANGE 1: NIT-1 \"NIT-1 title\"\nbranch claude/nit-1-x, base b0, head hNIT-1\nRun `git diff b0...hNIT-1`"), "{p}");
        assert!(p.contains("`git diff c0 hNIT-1`") && p.contains("1.1. [P2] Edge: shared.rs:2, the empty line"), "{p}");
        assert!(p.contains("- [P2] Slow: shared.rs (accepted: fine for now)") && p.contains("## CHANGE 2: NIT-2"), "{p}");
        assert!(p.contains(SEVERITIES) && rules.prompt("main", None).contains(SEVERITIES));
        assert!(p.contains("`PREVIOUS <k>.<n>: fixed`") && p.contains("VERDICT <k>: <findings or clean>\nMONEY_OR_RELEASE <k>: <yes or no>"), "{p}");

        let reply = "Two small changes.\n\
- [P1] **CHANGE 2**: Off by one: shared.rs:1, total: skips the last row\n\
- [P2] CHANGE 1: Naming: shared.rs:1, total\n\
PREVIOUS 1.1: fixed\n\
INTERACTION 2,1: both change `total`\n\
VERDICT 1: findings\nMONEY_OR_RELEASE 1: no\nVERDICT 2: findings\nMONEY_OR_RELEASE 2: yes\n";
        let split = rules.parse_batch("codex", reply, 70, &changes);
        let one = split.reviews[0].as_ref().unwrap();
        assert_eq!((one.verdict.as_str(), one.findings.len(), one.findings[0].text.as_str()), ("pass", 1, "Naming: shared.rs:1, total"));
        assert_eq!((one.previous[0].status.as_str(), one.seconds, one.flagged.is_empty()), ("fixed", 70, true));
        let two = split.reviews[1].as_ref().unwrap();
        assert_eq!((two.verdict.as_str(), two.findings[0].priority, two.flagged.clone()), ("changes", 1, vec!["MONEY_OR_RELEASE".to_string()]));
        assert_eq!(split.interactions, [(0, 1, "both change `total`".to_string())]);

        // A finding that names no change goes to every change whose diff touches what it cites, or
        // to all of them when it cites nothing they touch; a change without its VERDICT goes alone.
        let stray = |line: &str| {
            rules.parse_batch("codex", &format!("{line}\nVERDICT 1: clean\nMONEY_OR_RELEASE 1: no\nVERDICT 2: findings\nMONEY_OR_RELEASE 2: no\n"), 1, &changes)
        };
        let only_two = stray("- [P1] Leak: other.rs:4, keeps the token");
        assert!(only_two.reviews[0].is_ok() && only_two.reviews[1].as_ref().unwrap_err().contains("without saying which change"), "{only_two:?}");
        let both = stray("- [P1] CHANGE 7: Leak: nowhere");
        assert!(both.reviews.iter().all(|r| r.is_err()), "{both:?}");
        // A defect the reviewer says is the base's own goes to its change, and holds nothing (NIT-646).
        let base = stray("- [BASE] CHANGE 2: Old leak: other.rs:4, keeps the token");
        let two = base.reviews[1].as_ref().unwrap();
        assert_eq!((two.verdict.as_str(), two.findings.len(), two.outside.len()), ("pass", 0, 1), "{base:?}");
        let no_verdict = rules.parse_batch("codex", "VERDICT 1: clean\nMONEY_OR_RELEASE 1: no\n", 1, &changes);
        assert!(no_verdict.reviews[0].is_ok() && no_verdict.reviews[1].as_ref().unwrap_err().contains("no VERDICT"), "{no_verdict:?}");
        // Its part unfinished (a flag unanswered) is the same.
        let unanswered = rules.parse_batch("codex", "VERDICT 1: clean\nVERDICT 2: clean\nMONEY_OR_RELEASE 2: no\n", 1, &changes);
        assert!(unanswered.reviews[0].as_ref().unwrap_err().contains("unfinished") && unanswered.reviews[1].is_ok(), "{unanswered:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_reviewers_token_count_is_read_from_what_it_printed() {
        assert_eq!(tokens_used("codex\nok\ntokens used\n4,755\nok\n"), Some(4755));
        assert_eq!(tokens_used("[2026-10-17T21:42:00] tokens used: 12345\n"), Some(12345));
        assert_eq!(tokens_used("Fine.\nVERDICT: clean\n"), None);
    }

    #[test]
    fn a_rebased_pass_is_reviewed_from_how_it_differs() {
        let prev = PreviousRound { passed_on: Some("aaa1111".into()), findings: vec![], ..previous(false) };
        let p = rules().prompt("origin/main", Some(&prev));
        assert!(p.contains("passed review at commit abc1234") && p.contains("`git range-diff aaa1111..abc1234 origin/main..HEAD`"), "{p}");
        assert!(p.contains("raise something new there only if it is P0 or P1"), "{p}");
        // Rebased after a review that asked for changes: the whole change, as before.
        let p = rules().prompt("origin/main", Some(&previous(false)));
        assert!(p.contains("so review the whole change again") && !p.contains("range-diff"), "{p}");
    }

    #[test]
    fn a_p1_that_points_at_no_code_counts_as_p2_and_the_change_s_purpose_comes_first() {
        let text = "- [P1] Could break someday: if the config ever grows, loading may slow down\n\
- [P1] Drops a row: src/a.rs, f: the last row of every page\nVERDICT: findings\nMONEY_OR_RELEASE: no\n";
        let r = rules().parse("codex", text, 1, None, None).unwrap();
        assert_eq!((r.findings[0].priority, r.findings[0].raised_as), (2, Some(1)), "no file, no concrete failure: P2 at most");
        assert_eq!((r.findings[1].priority, r.findings[1].raised_as), (1, None));
        assert_eq!(r.verdict, "changes");
        // A file without an extension counts when the branch has it.
        let dir = crate::paths::unique_temp("nitely-rubric", "");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Makefile"), "all:
").unwrap();
        let r = rules().parse("codex", "- [P1] Wrong target: Makefile, all: builds nothing
VERDICT: findings
MONEY_OR_RELEASE: no
", 1, None, Some(&dir)).unwrap();
        assert_eq!((r.findings[0].priority, r.verdict.as_str()), (1, "changes"));
        std::fs::remove_dir_all(dir).ok();
        // The rubric is in the prompt, and what the change is for comes before the line-level work.
        let mut with = rules();
        with.context = "What the change is for, its issue APP-1 \"Footer\":\nShow the footer.".into();
        let p = with.prompt("main", None);
        assert!(p.contains("must name a concrete failure") && p.contains("is P2 at most"), "{p}");
        let (purpose, findings) = (p.find("What the change is for").unwrap(), p.find("Report each defect").unwrap());
        assert!(purpose < findings, "{p}");
    }
}
