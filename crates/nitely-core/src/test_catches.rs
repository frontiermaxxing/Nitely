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
/// `test_x.py`, `x.spec.js`, `x.e2e.mjs`, `XTest.java`, `x_spec.rb`). Rust's inline `mod tests`
/// lives in product files, so a fix there counts as product code.
pub fn is_test_path(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    let dirs = ["tests", "test", "__tests__", "spec", "specs", "e2e", "harness"];
    let (folders, file) = path.rsplit_once('/').unwrap_or(("", &path));
    if folders.split('/').any(|d| dirs.contains(&d)) {
        return true;
    }
    let stem = file.split('.').next().unwrap_or(file);
    file.contains(".test.") || file.contains(".spec.") || file.contains(".e2e.") || stem.ends_with("_test") || stem.ends_with("_spec") || stem.starts_with("test_") || stem.ends_with("test") && file.ends_with(".java")
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
        let names = if tests.is_empty() { vec![r.command.clone()] } else { tests };
        for n in names {
            if !out.iter().any(|f| f.test == n) {
                out.push(Failed { test: n, step: r.command.clone() });
            }
        }
    }
    out
}

/// The steps of a gate that ran and passed: where its tests were seen to pass.
pub fn passed_steps(results: &[CommandResult]) -> Vec<String> {
    results.iter().filter(|r| r.ok && !r.skipped).map(|r| r.command.clone()).collect()
}

/// A failed test and the revisions between which it was fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixSpan {
    pub test: String,
    pub failed: String,
    pub fixed: String,
}

/// Each test of the failures since the last pass (oldest first), from the revision where it last
/// failed to the first later gate where it was seen to pass: one where its step ran and passed, or
/// the passing gate (`passed`) that ends the run. A gate stops at its first failed step, so a test
/// merely absent from a later failure may not have run there, and says nothing. A test that failed
/// and passed on one revision was a flake, not fixed, and has no span.
pub fn fix_spans(failures: &[Failure], passed: &str) -> Vec<FixSpan> {
    let mut spans = Vec::new();
    for (i, failure) in failures.iter().enumerate() {
        let later = &failures[i + 1..];
        for f in &failure.tests {
            // A later failure of the same test holds its last failure; that record speaks for it.
            if later.iter().any(|l| l.tests.iter().any(|t| t.test == f.test)) {
                continue;
            }
            let fixed = later.iter().find(|l| l.passed_steps.contains(&f.step)).map(|l| l.commit.as_str()).unwrap_or(passed);
            if fixed != failure.commit {
                spans.push(FixSpan { test: f.test.clone(), failed: failure.commit.clone(), fixed: fixed.to_string() });
            }
        }
    }
    spans
}

/// One module's record: how often its failures caught something and how often they were the
/// tests' own.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModuleCatches {
    pub module: String,
    pub catches: i64,
    pub false_alarms: i64,
}

/// Modules whose tests have only ever raised false alarms, at least twice, and caught nothing: the
/// tests to rewrite or remove first, by evidence (NIT-557).
pub fn false_alarm_modules(counts: &[ModuleCatches]) -> Vec<&ModuleCatches> {
    counts.iter().filter(|m| m.catches == 0 && m.false_alarms >= 2).collect()
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
        let failures: Vec<Failure> = {
            let mut stmt = self.conn.prepare("SELECT commit_sha, tests, passed_steps FROM gate_failure WHERE issue_key = ?1 AND resolved_at IS NULL ORDER BY id")?;
            let rows = stmt.query_map([issue], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|(commit, tests, steps)| Failure { commit, tests: serde_json::from_str(&tests).unwrap_or_default(), passed_steps: serde_json::from_str(&steps).unwrap_or_default() })
                .collect()
        };
        if failures.is_empty() {
            return Ok(());
        }
        // Every failure since the last pass is settled by this one.
        self.conn.execute("UPDATE gate_failure SET resolved_at = ?2 WHERE issue_key = ?1 AND resolved_at IS NULL", params![issue, now()])?;
        let mut diffs: std::collections::HashMap<(String, String), Option<Verdict>> = std::collections::HashMap::new();
        for span in fix_spans(&failures, commit) {
            let key = (span.failed.clone(), span.fixed.clone());
            let v = *diffs.entry(key).or_insert_with(|| {
                crate::playbook::git(worktree, &["diff", "--name-only", "--no-renames", &format!("{}..{}", span.failed, span.fixed)])
                    .ok()
                    .map(|d| verdict(&d.lines().filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>()))
            });
            let Some(v) = v else { continue }; // A revision git cannot diff records nothing.
            self.conn.execute(
                "INSERT INTO test_catch (issue_key, test, module, verdict, failed_commit, fixed_commit, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![issue, span.test, module_of(&span.test), v.as_str(), span.failed, span.fixed, now()],
            )?;
        }
        Ok(())
    }

    /// Catches and false alarms per module, most false alarms first.
    pub fn catches_by_module(&self) -> Result<Vec<ModuleCatches>> {
        let mut stmt = self.conn.prepare(
            "SELECT module, SUM(verdict = 'catch'), SUM(verdict = 'false_alarm') FROM test_catch GROUP BY module
             ORDER BY SUM(verdict = 'false_alarm') DESC, module",
        )?;
        let rows = stmt.query_map([], |r| Ok(ModuleCatches { module: r.get(0)?, catches: r.get(1)?, false_alarms: r.get(2)? }))?;
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
    }

    /// A fails, its fix exposes B, B is fixed: both are recorded, each against its own fix (NIT-557
    /// review). A test is fixed only where it was seen to pass: absent from a later failure whose
    /// gate stopped before its step ran, it waits for the pass.
    #[test]
    fn a_test_is_fixed_where_it_was_seen_to_pass() {
        let failure = |c: &str, tests: &[(&str, &str)], passed: &[&str]| Failure {
            commit: c.into(),
            tests: tests.iter().map(|(t, s)| Failed { test: t.to_string(), step: s.to_string() }).collect(),
            passed_steps: passed.iter().map(|s| s.to_string()).collect(),
        };
        let span = |t: &str, a: &str, b: &str| FixSpan { test: t.into(), failed: a.into(), fixed: b.into() };
        // A's step (cargo) ran and passed at c2, where npm failed on B.
        let seen = [failure("c1", &[("A", "cargo")], &["npm"]), failure("c2", &[("B", "npm")], &["cargo"])];
        assert_eq!(fix_spans(&seen, "c3"), vec![span("A", "c1", "c2"), span("B", "c2", "c3")]);
        // c2 failed on B in an earlier step, so A's step never ran there: A is judged up to the pass.
        let unseen = [failure("c1", &[("A", "cargo")], &["npm"]), failure("c2", &[("B", "npm")], &[])];
        assert_eq!(fix_spans(&unseen, "c3"), vec![span("A", "c1", "c3"), span("B", "c2", "c3")]);
        // Failing again later: only its last failure counts.
        let again = [failure("c1", &[("A", "cargo")], &[]), failure("c2", &[("A", "cargo")], &[])];
        assert_eq!(fix_spans(&again, "c3"), vec![span("A", "c2", "c3")]);
        assert_eq!(fix_spans(&[failure("c1", &[("A", "cargo")], &[])], "c1"), vec![], "the same revision passing is a flake");
    }

    #[test]
    fn only_modules_with_repeated_false_alarms_and_no_catch_are_flagged() {
        let m = |module: &str, catches: i64, false_alarms: i64| ModuleCatches { module: module.into(), catches, false_alarms };
        let counts = [m("lanes", 0, 3), m("land", 1, 4), m("notify", 0, 1)];
        let flagged: Vec<&str> = false_alarm_modules(&counts).iter().map(|m| m.module.as_str()).collect();
        assert_eq!(flagged, vec!["lanes"], "one catch keeps a module's tests; one false alarm is not a pattern");
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
        let r = |command: &str, ok: bool, output: &str| CommandResult { command: command.into(), ok, seconds: 1, output: output.into(), skipped: false, timed_out: false };
        let results = [
            r("npm test", true, ""),
            r("cargo test", false, "test lanes::tests::a ... FAILED\ntest lanes::tests::b ... ok\nfailures:\n    lanes::tests::a\n"),
            r("npm run build", false, "error TS2304"),
        ];
        let failed = |t: &str, step: &str| Failed { test: t.into(), step: step.into() };
        assert_eq!(failed_of(&results), vec![failed("lanes::tests::a", "cargo test"), failed("npm run build", "npm run build")]);
        assert_eq!(passed_steps(&results), vec!["npm test".to_string()]);
    }
}
