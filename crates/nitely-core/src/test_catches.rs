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
use rusqlite::{params, OptionalExtension};
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

/// The tests a gate's failed steps name; a failed step that names none counts as its command.
pub fn failed_of(results: &[CommandResult]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in results.iter().filter(|r| !r.ok && !r.skipped) {
        let tests = crate::playbook::failed_tests(&r.output);
        let names = if tests.is_empty() { vec![r.command.clone()] } else { tests };
        for n in names {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
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
                    "INSERT INTO gate_failure (issue_key, commit_sha, tests, failed_at) VALUES (?1, ?2, ?3, ?4)",
                    params![issue, commit, serde_json::to_string(&tests)?, now()],
                )?;
            }
            return Ok(());
        }
        let open: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT commit_sha, tests FROM gate_failure WHERE issue_key = ?1 AND resolved_at IS NULL ORDER BY id DESC LIMIT 1",
                [issue],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((failed, tests)) = open else { return Ok(()) };
        // Every failure since the last pass is settled by this one.
        self.conn.execute("UPDATE gate_failure SET resolved_at = ?2 WHERE issue_key = ?1 AND resolved_at IS NULL", params![issue, now()])?;
        if failed == commit {
            return Ok(()); // The same revision passed: a flake, which the gate records itself.
        }
        let Ok(diff) = crate::playbook::git(worktree, &["diff", "--name-only", "--no-renames", &format!("{failed}..{commit}")]) else { return Ok(()) };
        let changed: Vec<String> = diff.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
        let v = verdict(&changed);
        let tests: Vec<String> = serde_json::from_str(&tests).unwrap_or_default();
        for t in tests {
            self.conn.execute(
                "INSERT INTO test_catch (issue_key, test, module, verdict, failed_commit, fixed_commit, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![issue, t, module_of(&t), v.as_str(), failed, commit, now()],
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
        assert_eq!(failed_of(&results), vec!["lanes::tests::a".to_string(), "npm run build".to_string()]);
    }
}
