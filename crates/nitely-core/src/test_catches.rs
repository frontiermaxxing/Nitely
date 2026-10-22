//! Whether a failed gate caught something (NIT-557). When an issue's gate fails and its next
//! passing gate differs from the failed revision only in test files, the failed tests were false
//! alarms: the code was right and the tests were changed to agree. When the fix changed product
//! code, they caught a defect. Counted per test and per module, it is evidence for the test
//! optimizer: a test with repeated false alarms and no catches is a candidate to rewrite or delete.
//!
//! The decisions are pure over file lists and test names; `record` only stores and reads.

use crate::db::{now, Db};
use crate::playbook::CommandResult;
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The fix changed product code: the failure caught a defect.
    Catch,
    /// The fix changed only tests: the failure was the test's, not the product's.
    FalseAlarm,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Catch => "catch",
            Verdict::FalseAlarm => "false_alarm",
        }
    }
}

/// Whether `path` is a test file in common layouts across languages: a `tests`, `test`,
/// `__tests__`, `spec` or `e2e` folder, or a test-named file (`x.test.ts`, `x_test.go`,
/// `test_x.py`, `x.spec.js`, `x.e2e.mjs`, `XTest.java`, `x_spec.rb`). A `test_` prefix marks a
/// test only in Python, its convention: `test_budget.rs` is product code. Rust's inline
/// `mod tests` lives in product files, so a fix there counts as product code.
pub fn is_test_path(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    let dirs = ["tests", "test", "__tests__", "spec", "specs", "e2e", "harness"];
    let (folders, file) = path.rsplit_once('/').unwrap_or(("", &path));
    if folders.split('/').any(|d| dirs.contains(&d)) {
        return true;
    }
    let stem = file.split('.').next().unwrap_or(file);
    file.contains(".test.") || file.contains(".spec.") || file.contains(".e2e.") || stem.ends_with("_test") || stem.ends_with("_spec") || (stem.starts_with("test_") && file.ends_with(".py")) || (stem.ends_with("test") && file.ends_with(".java"))
}

/// The files `git diff --name-only -z` names, as they are spelled: NUL-separated, so git quotes
/// none, and a test file named outside ASCII (`tests/café.rs`) is still seen as a test (NIT-569).
pub fn changed_files(out: &str) -> Vec<String> {
    out.split('\0').map(|f| f.trim_matches(['\r', '\n'])).filter(|f| !f.is_empty()).map(str::to_string).collect()
}

/// A fix that changed any product file caught something; one that changed only tests (or nothing)
/// was a false alarm.
pub fn verdict(changed: &[String]) -> Verdict {
    if changed.iter().any(|f| !is_test_path(f)) {
        Verdict::Catch
    } else {
        Verdict::FalseAlarm
    }
}

/// The module a test belongs to: a Rust test's path up to `tests` (`lanes::tests::x` is `lanes`),
/// a file's path for a test named by its file, else the name before its first separator.
pub fn module_of(test: &str) -> String {
    let parts: Vec<&str> = test.split("::").collect();
    // pytest's `path/to/test_x.py::test_name`: the file is the module.
    if parts.len() > 1 && (parts[0].contains('/') || parts[0].contains('.')) {
        return parts[0].replace('\\', "/");
    }
    if parts.len() > 1 {
        let upto: Vec<&str> = parts.iter().take_while(|p| **p != "tests").copied().collect();
        return if upto.is_empty() { parts[0].to_string() } else { upto.join("::") };
    }
    if test.contains('/') || test.contains('\\') {
        return test.replace('\\', "/");
    }
    test.split(['.', ' ']).next().unwrap_or(test).to_string()
}

/// A failed test and the gate step that ran it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Failed {
    pub test: String,
    pub step: String,
}

/// One failed gate: its revision, its failed tests, and the steps that ran and passed there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub commit: String,
    pub tests: Vec<Failed>,
    pub passed_steps: Vec<String>,
}

/// The tests a gate's failed steps name, each with its step; a failed step that names none counts
/// as its command.
pub fn failed_of(results: &[CommandResult]) -> Vec<Failed> {
    let mut out: Vec<Failed> = Vec::new();
    for r in results.iter().filter(|r| !r.ok && !r.skipped) {
        let tests = crate::playbook::failed_tests(&r.output);
        let names = if tests.is_empty() { vec![r.step_id().to_string()] } else { tests };
        for n in names {
            if !out.iter().any(|f| f.test == n && f.step == r.step_id()) {
                out.push(Failed { test: n, step: r.step_id().to_string() });
            }
        }
    }
    out
}

/// The steps of a gate that ran and passed: where its tests were seen to pass.
/// A step answered from an earlier pass on the same tree (the lane's record, or a reduced gate's
/// validated pass) is marked skipped but passed all the same: it counts (NIT-557 review). A step
/// skipped for any other reason (only some paths changed) proves nothing.
pub fn passed_steps(results: &[CommandResult]) -> Vec<String> {
    results.iter().filter(|r| r.ok && (!r.skipped || r.output.contains("not run again"))).map(|r| r.step_id().to_string()).collect()
}

/// A failed test and the revisions between which it was fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixSpan {
    pub test: String,
    /// The gate step that ran it: with the name, what tells two packages' tests of one name apart.
    pub step: String,
    pub failed: String,
    pub fixed: String,
}

/// What a passing gate settles of the failures still open (oldest first): the fix spans to record,
/// and which failures (by index) are closed. A test is settled by the first later gate that says
/// something about it: a failure of it there starts a new episode (no span here), its step having
/// run and passed there ends this one (a span). The passing gate counts only for the steps it ran
/// (`passed_steps`): a reduced gate that skipped a test's step proves nothing about it, and the
/// failure stays open for a gate that does run it. A failure closes once every test in it is
/// settled, and only then are its spans recorded, so a failure is never recorded twice. A test
/// that passed on the revision it failed on was a flake: settled, with no span.
///
/// A pass that left failures open is kept among them as a row with no tests (NIT-565): it settles
/// the tests whose steps it ran for the failures before it, as a later failure's passed steps do,
/// and closes only once every row before it is closed.
pub fn settle(failures: &[Failure], passed: &str, passed_steps: &[String]) -> (Vec<FixSpan>, Vec<usize>) {
    let (mut spans, mut closed) = (Vec::new(), Vec::new());
    for (i, failure) in failures.iter().enumerate() {
        let later = &failures[i + 1..];
        let mut found = Vec::new();
        let mut all = true;
        for f in &failure.tests {
            let next = later.iter().find(|l| l.tests.contains(f) || l.passed_steps.contains(&f.step));
            let fixed = match next {
                // The same test of the same step: two steps may each have a test of that name.
                Some(l) if l.tests.contains(f) => continue,
                Some(l) => l.commit.as_str(),
                None if passed_steps.contains(&f.step) => passed,
                None => {
                    all = false;
                    continue;
                }
            };
            if fixed != failure.commit {
                found.push(FixSpan { test: f.test.clone(), step: f.step.clone(), failed: failure.commit.clone(), fixed: fixed.to_string() });
            }
        }
        // A kept pass waits for every row before it: a later pass still needs it to date their fixes.
        let pass_waits = failure.tests.is_empty() && closed.len() < i;
        if all && !pass_waits {
            spans.extend(found);
            closed.push(i);
        }
    }
    (spans, closed)
}

/// Whether a pass is kept as a row of its own (NIT-565): it ran some steps and left failures open,
/// which a later pass will settle, and some of those it may have seen pass already.
pub fn keep_pass(open_after: usize, passed_steps: &[String]) -> bool {
    open_after > 0 && !passed_steps.is_empty()
}

/// A test's record over a project's gates: how often its failure caught something and how often
/// it was the test's own.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TestCatches {
    pub test: String,
    pub step: String,
    pub catches: i64,
    pub false_alarms: i64,
}

/// Whether the record alone makes a test a candidate to rewrite or delete (NIT-565): its failures
/// were the test's own more than once and never caught anything. Per test, not per module: one
/// test that once caught something does not vouch for its neighbours. A step that named no test
/// (its command stands in for it) is no test to judge.
pub fn false_alarm_candidate(t: &TestCatches) -> bool {
    t.false_alarms >= 2 && t.catches == 0 && t.test != t.step
}

/// One module's record: how often its failures caught something and how often they were the
/// tests' own.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModuleCatches {
    pub module: String,
    pub catches: i64,
    pub false_alarms: i64,
}

impl Db {
    /// Records a gate's outcome for its issue: a failure's tests wait for the fix; a pass after a
    /// failure diffs the two revisions in `worktree` and records each failed test's verdict. Best
    /// effort: a revision git cannot diff records nothing.
    pub fn record_gate_catches(&self, issue: &str, worktree: &Path, commit: &str, results: &[CommandResult], passed: bool) -> Result<()> {
        if !passed {
            let tests = failed_of(results);
            if !tests.is_empty() {
                self.conn.execute(
                    "INSERT INTO gate_failure (issue_key, commit_sha, tests, passed_steps, failed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![issue, commit, serde_json::to_string(&tests)?, serde_json::to_string(&passed_steps(results))?, now()],
                )?;
            }
            return Ok(());
        }
        let (ids, failures): (Vec<i64>, Vec<Failure>) = {
            let mut stmt = self.conn.prepare("SELECT id, commit_sha, tests, passed_steps FROM gate_failure WHERE issue_key = ?1 AND resolved_at IS NULL ORDER BY id")?;
            let rows = stmt.query_map([issue], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|(id, commit, tests, steps)| (id, Failure { commit, tests: serde_json::from_str(&tests).unwrap_or_default(), passed_steps: serde_json::from_str(&steps).unwrap_or_default() }))
                .unzip()
        };
        if failures.is_empty() {
            return Ok(());
        }
        // Only what this gate ran settles anything; the rest stays open for a gate that runs it.
        let steps = passed_steps(results);
        let (spans, closed) = settle(&failures, commit, &steps);
        let open_after = failures.len() - closed.len();
        for &i in &closed {
            self.conn.execute("UPDATE gate_failure SET resolved_at = ?2 WHERE id = ?1", params![ids[i], now()])?;
        }
        // A pass of only some steps that left failures open: kept, so a later full pass dates
        // only the fixes this one did not see (NIT-565).
        if keep_pass(open_after, &steps) {
            self.conn.execute(
                "INSERT INTO gate_failure (issue_key, commit_sha, tests, passed_steps, failed_at) VALUES (?1, ?2, '[]', ?3, ?4)",
                params![issue, commit, serde_json::to_string(&steps)?, now()],
            )?;
        }
        let mut diffs: std::collections::HashMap<(String, String), Option<Verdict>> = std::collections::HashMap::new();
        for span in spans {
            let key = (span.failed.clone(), span.fixed.clone());
            let v = *diffs.entry(key).or_insert_with(|| {
                crate::playbook::git(worktree, &["diff", "--name-only", "-z", "--no-renames", &format!("{}..{}", span.failed, span.fixed)])
                    .ok()
                    .map(|d| verdict(&changed_files(&d)))
            });
            let Some(v) = v else { continue }; // A revision git cannot diff records nothing.
            self.conn.execute(
                "INSERT INTO test_catch (issue_key, test, module, verdict, failed_commit, fixed_commit, at, step) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![issue, span.test, module_of(&span.test), v.as_str(), span.failed, span.fixed, now(), span.step],
            )?;
        }
        Ok(())
    }

    /// A project's catches and false alarms per module, most false alarms first: modules of two
    /// projects that share a name are counted apart.
    pub fn catches_by_module(&self, project: &str) -> Result<Vec<ModuleCatches>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.module, SUM(c.verdict = 'catch'), SUM(c.verdict = 'false_alarm') FROM test_catch c
             JOIN issue i ON i.key = c.issue_key COLLATE NOCASE WHERE i.project = ?1 COLLATE NOCASE
             GROUP BY c.module ORDER BY SUM(c.verdict = 'false_alarm') DESC, c.module",
        )?;
        let rows = stmt.query_map([project], |r| Ok(ModuleCatches { module: r.get(0)?, catches: r.get(1)?, false_alarms: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A project's catches and false alarms per test and step (NIT-565), most false alarms first:
    /// what the sweep reads to name a test whose failures were only ever its own.
    pub fn catches_by_test(&self, project: &str) -> Result<Vec<TestCatches>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.test, c.step, SUM(c.verdict = 'catch'), SUM(c.verdict = 'false_alarm') FROM test_catch c
             JOIN issue i ON i.key = c.issue_key COLLATE NOCASE WHERE i.project = ?1 COLLATE NOCASE
             GROUP BY c.test, c.step ORDER BY SUM(c.verdict = 'false_alarm') DESC, c.test, c.step",
        )?;
        let rows = stmt.query_map([project], |r| Ok(TestCatches { test: r.get(0)?, step: r.get(1)?, catches: r.get(2)?, false_alarms: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fix_to_tests_alone_is_a_false_alarm_and_any_product_change_a_catch() {
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(verdict(&files(&["crates/app/tests/flow.rs", "src/notify.test.ts", "scripts/harness/notify.e2e.mjs"])), Verdict::FalseAlarm);
        assert_eq!(verdict(&files(&["pkg/store/store_test.go", "tests/test_api.py", "spec/models/user_spec.rb", "src/test/java/AppTest.java"])), Verdict::FalseAlarm);
        assert_eq!(verdict(&files(&["src/notify.test.ts", "src/notify.ts"])), Verdict::Catch);
        assert_eq!(verdict(&files(&["crates/nitely-core/src/lanes.rs"])), Verdict::Catch, "a Rust inline test module lives in product code");
        assert_eq!(verdict(&[]), Verdict::FalseAlarm, "nothing changed: the test passed on its own");
        assert!(!is_test_path("src/latest.rs") && !is_test_path("src/contest/ranking.ts"), "a word containing test is not a test");
        assert!(!is_test_path("crates/nitely-core/src/test_budget.rs") && is_test_path("app/test_api.py"), "test_ marks a test only in Python");
        // NIT-569: read unquoted, a fix only to a test named outside ASCII is still the test's own.
        assert_eq!(changed_files("tests/café.rs\0web/naïve.test.ts\0"), ["tests/café.rs", "web/naïve.test.ts"]);
        assert_eq!(verdict(&changed_files("tests/café.rs\0")), Verdict::FalseAlarm);
        assert_eq!(verdict(&changed_files("tests/café.rs\0src/café.rs\0")), Verdict::Catch);
        assert!(changed_files("").is_empty());
    }

    /// A test is settled where its step ran: a later gate that passed its step, a later failure of
    /// it (a new episode), or the passing gate only if that gate ran its step (NIT-557 reviews).
    #[test]
    fn a_failed_test_is_settled_only_by_a_gate_that_ran_its_step() {
        let failure = |c: &str, tests: &[(&str, &str)], passed: &[&str]| Failure {
            commit: c.into(),
            tests: tests.iter().map(|(t, s)| Failed { test: t.to_string(), step: s.to_string() }).collect(),
            passed_steps: passed.iter().map(|s| s.to_string()).collect(),
        };
        let span = |t: &str, a: &str, b: &str| FixSpan { test: t.into(), step: if t == "B" { "npm".into() } else { "cargo".into() }, failed: a.into(), fixed: b.into() };
        let all = ["cargo".to_string(), "npm".to_string()];
        // A's step passed at c2, where npm failed on B; the pass ran both.
        let seen = [failure("c1", &[("A", "cargo")], &["npm"]), failure("c2", &[("B", "npm")], &["cargo"])];
        assert_eq!(settle(&seen, "c3", &all), (vec![span("A", "c1", "c2"), span("B", "c2", "c3")], vec![0, 1]));
        // c2 stopped before A's step: A is judged up to the pass.
        let unseen = [failure("c1", &[("A", "cargo")], &["npm"]), failure("c2", &[("B", "npm")], &[])];
        assert_eq!(settle(&unseen, "c3", &all), (vec![span("A", "c1", "c3"), span("B", "c2", "c3")], vec![0, 1]));
        // A reduced gate that ran only npm settles B, not A: A's failure stays open.
        assert_eq!(settle(&unseen, "c3", &["npm".to_string()]), (vec![span("B", "c2", "c3")], vec![1]));
        // Two episodes of A: each keeps its own fix.
        let twice = [failure("c1", &[("A", "cargo")], &[]), failure("c2", &[("B", "npm")], &["cargo"]), failure("c3", &[("A", "cargo")], &["npm"])];
        assert_eq!(settle(&twice, "c4", &all).0, vec![span("A", "c1", "c2"), span("B", "c2", "c3"), span("A", "c3", "c4")]);
        // Two steps with a test of one name are two tests: beta's failure is not alpha's episode.
        let named = [failure("c1", &[("tests::roundtrip", "cargo test -p alpha")], &[]), failure("c2", &[("tests::roundtrip", "cargo test -p beta")], &["cargo test -p alpha"])];
        let at = |step: &str, a: &str, b: &str| FixSpan { test: "tests::roundtrip".into(), step: step.into(), failed: a.into(), fixed: b.into() };
        assert_eq!(
            settle(&named, "c3", &["cargo test -p alpha".to_string(), "cargo test -p beta".to_string()]).0,
            vec![at("cargo test -p alpha", "c1", "c2"), at("cargo test -p beta", "c2", "c3")],
            "each span keeps its step, so the record counts them apart"
        );
        // The same revision passing is a flake: settled, no span.
        assert_eq!(settle(&[failure("c1", &[("A", "cargo")], &[])], "c1", &all), (vec![], vec![0]));
    }

    /// NIT-565: a pass that ran only npm fixed B at c3 and left A open. Kept, it dates B's fix at
    /// c3 when a full pass at c4 settles A, and closes with them.
    #[test]
    fn a_partial_pass_keeps_the_fixes_it_saw() {
        let failure = |c: &str, tests: &[(&str, &str)], passed: &[&str]| Failure {
            commit: c.into(),
            tests: tests.iter().map(|(t, s)| Failed { test: t.to_string(), step: s.to_string() }).collect(),
            passed_steps: passed.iter().map(|s| s.to_string()).collect(),
        };
        let span = |t: &str, s: &str, a: &str, b: &str| FixSpan { test: t.into(), step: s.into(), failed: a.into(), fixed: b.into() };
        let both = failure("c1", &[("A", "cargo"), ("B", "npm")], &[]);
        let npm = ["npm".to_string()];
        let (spans, closed) = settle(std::slice::from_ref(&both), "c3", &npm);
        assert!(spans.is_empty() && closed.is_empty(), "A is open, so nothing of c1 is recorded yet");
        assert!(keep_pass(1, &npm), "so the pass at c3 is kept");
        assert!(!keep_pass(0, &npm) && !keep_pass(1, &[]), "not when nothing is open, nor a pass that ran nothing");
        let kept = failure("c3", &[], &["npm"]);
        let (spans, closed) = settle(&[both.clone(), kept.clone()], "c4", &["cargo".to_string(), "npm".to_string()]);
        assert_eq!(spans, vec![span("A", "cargo", "c1", "c4"), span("B", "npm", "c1", "c3")], "B was fixed by c3, not c4");
        assert_eq!(closed, vec![0, 1], "the kept pass closes once the failure before it does");
        // While A stays open, a pass of npm only again leaves both rows open.
        assert_eq!(settle(&[both, kept], "c4", &npm).1, Vec::<usize>::new());
    }

    #[test]
    fn a_test_whose_failures_were_only_its_own_is_a_candidate() {
        let t = |catches: i64, false_alarms: i64| TestCatches { test: "store::tests::roundtrip".into(), step: "cargo test".into(), catches, false_alarms };
        assert!(false_alarm_candidate(&t(0, 2)), "twice its own, never a catch");
        assert!(!false_alarm_candidate(&t(0, 1)), "once is not a pattern");
        assert!(!false_alarm_candidate(&t(1, 5)), "it caught something once");
        let command = TestCatches { test: "npm run build".into(), step: "npm run build".into(), catches: 0, false_alarms: 3 };
        assert!(!false_alarm_candidate(&command), "a step that named no test is no test to judge");
    }

    #[test]
    fn a_test_belongs_to_its_module() {
        assert_eq!(module_of("lanes::tests::a_run_waits"), "lanes");
        assert_eq!(module_of("verification_backstop::tests::x"), "verification_backstop");
        assert_eq!(module_of("tests/test_api.py::test_login"), "tests/test_api.py");
        assert_eq!(module_of("scripts/harness/notify.e2e.mjs"), "scripts/harness/notify.e2e.mjs");
        assert_eq!(module_of("a workflow keeps runs apart"), "a");
    }

    #[test]
    fn a_failed_step_names_its_tests_or_else_its_command() {
        let r = |command: &str, ok: bool, output: &str| CommandResult { step: None, command: command.into(), ok, seconds: 1, output: output.into(), skipped: false, timed_out: false };
        let results = [
            r("npm test", true, ""),
            r("cargo test", false, "test lanes::tests::a ... FAILED\ntest lanes::tests::b ... ok\nfailures:\n    lanes::tests::a\n"),
            r("npm run build", false, "error TS2304"),
        ];
        let failed = |t: &str, step: &str| Failed { test: t.into(), step: step.into() };
        assert_eq!(failed_of(&results), vec![failed("lanes::tests::a", "cargo test"), failed("npm run build", "npm run build")]);
        assert_eq!(passed_steps(&results), vec!["npm test".to_string()]);
        // A gate answered from an earlier pass on the same tree passed its steps; a step skipped by
        // the changed paths did not run.
        let skipped = |command: &str, why: &str| CommandResult { step: None, command: command.into(), ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true, timed_out: false };
        let reused = [skipped("cargo test", "this tree passed it in lane run 7; not run again"), skipped("cargo clippy", "only paths matching ^src/ changed")];
        assert_eq!(passed_steps(&reused), vec!["cargo test".to_string()]);
        // A step's identity is its template: an executed run (expanded) and a cached pass match.
        let ran = CommandResult { step: Some("cargo test --jobs {jobs}".into()), command: "cargo test --jobs 6".into(), ok: false, seconds: 9, output: "test a::tests::x ... FAILED".into(), skipped: false, timed_out: false };
        let cached = CommandResult { step: None, command: "cargo test --jobs {jobs}".into(), ok: true, seconds: 0, output: "skipped: this tree passed it in lane run 9; not run again".into(), skipped: true, timed_out: false };
        assert_eq!(failed_of(std::slice::from_ref(&ran))[0].step, passed_steps(&[cached])[0]);
    }
}
