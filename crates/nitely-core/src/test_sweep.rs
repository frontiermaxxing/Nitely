//! Test hygiene evidence produces a human-edited plan, never automatic test removal.
use crate::goal_plans::{GoalPlan, PlanItem, PlanProposed};
use crate::{Actor, Db, NewIssue};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GUIDE: &str = include_str!("test_sweep_guide.md");
/// The analysis step of /nitely-test-optimize: classifying each flagged test (NIT-555).
pub const OPTIMIZE_GUIDE: &str = include_str!("test_optimize_guide.md");
pub const COLLECTOR: &str = include_str!("test_sweep.cjs");
pub const LABEL: &str = "test-sweep";
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schedule {
    /// off (default), nightly (UTC day), or weekly (UTC seven-day bucket). Requires running Crew.
    #[serde(default)]
    pub test_sweep: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Candidate {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub evidence: String,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub source_hash: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Report {
    pub version: u32,
    pub revision: String,
    pub generated_at: String,
    pub report_only: bool,
    /// Each run the evidence came from: its runner, run ID, revision, source (the machine or CI
    /// job), duration, and whether it was read whole (`partial`).
    #[serde(default)]
    pub runs: Vec<serde_json::Value>,
    pub observations: Vec<serde_json::Value>,
    pub tests: Vec<serde_json::Value>,
    pub candidates: Vec<Candidate>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}
impl Report {
    pub fn read(path: &Path) -> Result<Self> {
        if path.metadata()?.len() > 16 * 1024 * 1024 {
            bail!("sweep report exceeds 16 MiB");
        }
        let report: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        report.check()?;
        Ok(report)
    }
    pub fn check(&self) -> Result<()> {
        if self.version != 1
            || self.revision.len() != 40
            || !self.revision.chars().all(|c| c.is_ascii_hexdigit())
        {
            bail!("unsupported sweep report or revision");
        }
        if self.candidates.len() > 1000
            || self.tests.len() > 10000
            || self.observations.len() > 30000
        {
            bail!("sweep report exceeds bounded counts");
        }
        for c in &self.candidates {
            if !["fix", "speedup", "inspect", "delete"].contains(&c.kind.as_str())
                || c.evidence.len() > 12000
                || c.title.len() > 1000
                || c.id.len() > 80
            {
                bail!("invalid sweep candidate");
            }
            if let Some(p) = &c.path {
                if p.is_empty()
                    || p.len() > 500
                    || p.contains(['\\', ':', '\n', '\r', '`'])
                    || p.starts_with('/')
                    || p.split('/').any(|v| v == "..")
                {
                    bail!("candidate paths must be relative repo paths");
                }
            }
        }
        Ok(())
    }
    /// Grouped action lists remain editable before a human approves the normal goal plan.
    pub fn propose(
        &self,
        db: &mut Db,
        project: &str,
        planning_issue: Option<String>,
        actor: &Actor,
    ) -> Result<PlanProposed> {
        self.check()?;
        if let Some(key) = &planning_issue {
            let issue = db.issue(key)?.context("sweep issue missing")?;
            if issue.project.as_deref() != Some(project) {
                bail!("sweep issue belongs to another project");
            }
        }
        let mut items = Vec::new();
        for (kind, verb) in [
            ("delete", "Remove verified redundant tests"),
            ("fix", "Repair failing or flaky tests"),
            ("speedup", "Reduce test runtime"),
            (
                "inspect",
                "Inspect possible duplicate or implementation-coupled tests",
            ),
        ] {
            let candidates: Vec<_> = self.candidates.iter().filter(|c| c.kind == kind).collect();
            if candidates.is_empty() {
                continue;
            }
            let mut body = format!("Evidence revision: {}. {} candidates.\n\nOnly work on the candidates retained below by the human. Before editing, verify each source hash against the current test body; if it changed, reassess and propose again rather than applying stale removal. Preserve a concrete public contract or regression. Commit changes in the issue worktree and use normal submit/land. Inspection does not authorize deletion: propose any resulting removals for a separate human decision.\n\n", self.revision, candidates.len());
            let mut touches = Vec::new();
            for c in &candidates {
                if let Some(p) = &c.path {
                    if !touches.contains(p) {
                        touches.push(p.clone());
                    }
                }
                body.push_str(&format!(
                    "- [{}] {}\n  Location: {}:{}; source hash: {}\n  Evidence: {}\n\n",
                    c.id,
                    c.title,
                    c.path
                        .as_deref()
                        .unwrap_or("unresolved; locate before editing"),
                    c.line.unwrap_or(0),
                    c.source_hash.as_deref().unwrap_or("unknown; inspect first"),
                    c.evidence
                ));
            }
            if body.len() > 50000 {
                bail!("action list exceeds 50,000 bytes; narrow the report before proposing");
            }
            // Unknown locations need an investigative task, with no invented source path.
            let no_paths = touches.is_empty();
            items.push(PlanItem {
                local_ref: kind.into(),
                title: format!("{verb} ({})", candidates.len()),
                body,
                evidence: format!(
                    "Sweep at {}; {} observations; {} extracted test bodies; truncated: {}. {}",
                    self.revision,
                    self.observations.len(),
                    self.tests.len(),
                    self.truncated,
                    self.warnings.join(" ")
                ),
                kind: if no_paths {
                    "live-check"
                } else {
                    "improvement"
                }
                .into(),
                priority: 3,
                labels: vec!["test-hygiene".into()],
                touches,
                size: Some("M".into()),
                review_level: Some("standard".into()),
                ..Default::default()
            });
        }
        if items.is_empty() {
            bail!("no candidates: retain the report and note its unknowns; no empty approval is needed");
        }
        let proposed = db.propose_goal(project, &GoalPlan { sentence: "Review test hygiene evidence and choose the work worth doing".into(), title: "Test hygiene: proposed actions".into(), outcome: format!("Apply only the edited action lists, preserve meaningful coverage, and record before/after counts and measured runtime. Evidence revision {}. Untimed or unobserved tests remain unknown.", self.revision), issues: items, ..Default::default() }, planning_issue.as_deref(), actor)?;
        db.change_labels(&proposed.issue.key, &[LABEL.into()], &[], actor)?;
        Ok(proposed)
    }
}

/// A schedule only files the read-only sweep job. Human approval remains mandatory for its plan.
pub fn schedule(
    db: &mut Db,
    project: &str,
    rules: &Schedule,
    actor: &Actor,
    now: i64,
) -> Result<()> {
    let period = match rules.test_sweep.as_deref().unwrap_or("off") {
        "off" => return Ok(()),
        "nightly" => now / 86400,
        "weekly" => now / (86400 * 7),
        other => bail!("schedule.test_sweep must be off, nightly or weekly, not {other}"),
    };
    let bucket = format!("{}-{period}", rules.test_sweep.as_deref().unwrap());
    let reserved = db.conn.execute("INSERT INTO test_sweep_schedule(project,bucket,reserved_at) VALUES(?1,?2,?3) ON CONFLICT(project) DO UPDATE SET bucket=excluded.bucket,reserved_at=excluded.reserved_at,issue=NULL WHERE test_sweep_schedule.bucket != excluded.bucket OR (test_sweep_schedule.issue IS NULL AND test_sweep_schedule.reserved_at < ?3-3600)", rusqlite::params![project,bucket,now])?;
    if reserved == 0 {
        return Ok(());
    }
    let result = (|| -> Result<String> {
        let issues = db.list_issues(Some(project), true)?;
        let title = format!("Test hygiene sweep ({bucket})");
        if let Some(i) = issues
            .iter()
            .find(|i| i.title == title || (i.is_open() && i.labels.iter().any(|l| l == LABEL)))
        {
            return Ok(i.key.clone());
        }
        Ok(db.create_issue(&NewIssue { project: project.into(), title, body: GUIDE.into(), kind: "live-check".into(), source: "claude".into(), allow_similar: true, priority: 3, labels: vec![LABEL.into()], evidence: "Scheduled report-only sweep. Reuse existing logs first; no automatic deletions.".into(), ..Default::default() }, actor)?.key)
    })();
    match result {
        Ok(key) => {
            db.conn.execute(
                "UPDATE test_sweep_schedule SET issue=?2 WHERE project=?1 AND bucket=?3",
                rusqlite::params![project, key, bucket],
            )?;
        }
        Err(e) => {
            db.conn.execute(
                "DELETE FROM test_sweep_schedule WHERE project=?1 AND bucket=?2",
                rusqlite::params![project, bucket],
            )?;
            return Err(e).context("schedule test sweep");
        }
    }
    Ok(())
}

/// What a slow, flaky or flagged test is, as the agent finds it reading the code (NIT-555).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "class")]
pub enum Class {
    /// It checks one decision, which `module` makes: lift it into a pure function and test that.
    Decision { decision: String, module: String },
    /// It proves real I/O (a process, git, the network, a database) end to end: keep it, named.
    Scenario,
    /// It asserts that UI text or an element exists.
    Presence,
    /// It waits on the clock or on machine load.
    WallClock,
    /// Another test checks the same thing.
    Duplicate { of: String },
    /// It restates the implementation instead of checking a behaviour.
    Restates,
    /// It is worth what it costs as it is: a hint that led here was a false lead. Its `note` says
    /// why.
    Keep,
}

/// One test's class, keyed by the `test` a `flagged` entry names.
#[derive(Debug, Clone, Deserialize)]
pub struct Classified {
    pub test: String,
    #[serde(flatten)]
    pub class: Class,
    #[serde(default)]
    pub note: String,
}

/// A test the agent must classify, and why: slow, flaky or failing, or a mechanical hint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Flagged {
    /// `path::name` from the inventory, or an observation's id when no source was found.
    pub test: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    /// Its median time by the method that timed the most tests, when timed.
    pub seconds: Option<f64>,
    pub why: Vec<String>,
    pub hints: serde_json::Value,
}

impl Report {
    /// The tests to classify: slower than `slow_seconds`, flaky or failing at the report's
    /// revision, or whose body waits on time or asserts UI text or presence. Times are compared
    /// within one measuring method, as the collector compares them.
    pub fn flagged(&self, slow_seconds: f64) -> Vec<Flagged> {
        use std::collections::BTreeMap;
        let method_of = |o: &serde_json::Value| o["method"].as_str().unwrap_or("lane").to_string();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for o in self
            .observations
            .iter()
            .filter(|o| o["revision"] == self.revision.as_str() && o["duration_ms"].is_number())
        {
            *counts.entry(method_of(o)).or_default() += 1;
        }
        let method = counts.into_iter().max_by_key(|(_, n)| *n).map(|(m, _)| m);
        // Each run's outcome for the test: one workflow run's jobs and reports are one execution,
        // where a failure outweighs a pass, and a pass after a failed retry is both.
        #[derive(Default)]
        struct InRun {
            passed: bool,
            failed: bool,
        }
        // Each observed test at this revision: its name, and by run its times (by that method)
        // and outcome.
        struct Seen {
            name: String,
            times: BTreeMap<String, Vec<f64>>,
            runs: BTreeMap<String, InRun>,
        }
        let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
        for o in self
            .observations
            .iter()
            .filter(|o| o["revision"] == self.revision.as_str() && o["ambiguous"] != true)
        {
            let (Some(id), Some(name)) = (o["id"].as_str(), o["name"].as_str()) else {
                continue;
            };
            let run = o["run"].as_str().unwrap_or_default().to_string();
            let entry = seen.entry(id.to_string()).or_insert_with(|| Seen {
                name: name.to_string(),
                times: BTreeMap::new(),
                runs: BTreeMap::new(),
            });
            if Some(method_of(o)) == method {
                entry
                    .times
                    .entry(run.clone())
                    .or_default()
                    .extend(o["duration_ms"].as_f64().map(|ms| ms / 1000.0));
            }
            let state = entry.runs.entry(run).or_default();
            if o["outcome"] == "fail" {
                *state = InRun {
                    passed: false,
                    failed: true,
                };
            } else if o["outcome"] == "pass" && (state.passed || !state.failed) {
                state.passed = true;
                // A pass only after a failed attempt (a runner's retry) failed once too.
                state.failed |= o["flaky"] == true;
            }
        }
        let source = |name: &str| {
            let found: Vec<_> = self
                .tests
                .iter()
                .filter(|t| {
                    t["name"]
                        .as_str()
                        .is_some_and(|n| n == name || name.ends_with(&format!("::{n}")))
                })
                .collect();
            (found.len() == 1).then(|| found[0])
        };
        let key = |t: &serde_json::Value| {
            format!(
                "{}::{}",
                t["path"].as_str().unwrap_or_default(),
                t["name"].as_str().unwrap_or_default()
            )
        };
        let mut out: BTreeMap<String, Flagged> = BTreeMap::new();
        let mut flag =
            |test: String, t: Option<&serde_json::Value>, seconds: Option<f64>, why: String| {
                let entry = out.entry(test.clone()).or_insert_with(|| Flagged {
                    test,
                    path: t.and_then(|t| t["path"].as_str().map(String::from)),
                    line: t.and_then(|t| t["line"].as_u64()),
                    seconds,
                    why: vec![],
                    hints: t
                        .map(|t| t["hints"].clone())
                        .filter(|h| !h.is_null())
                        .unwrap_or_else(|| serde_json::json!({})),
                });
                entry.seconds = entry.seconds.or(seconds);
                entry.why.push(why);
            };
        for (
            id,
            Seen {
                name,
                times: timed,
                runs,
            },
        ) in seen
        {
            // One time per run: the longest any of its reports gave.
            let mut times: Vec<f64> = timed
                .values()
                .filter_map(|t| t.iter().copied().reduce(f64::max))
                .collect();
            let passed = runs.values().any(|r| r.passed);
            let failed = runs.values().any(|r| r.failed);
            let t = source(&name);
            let test = t.map(key).unwrap_or(id);
            times.sort_by(f64::total_cmp);
            let median = times.get(times.len() / 2).copied();
            if let Some(s) = median.filter(|s| *s > slow_seconds) {
                flag(
                    test.clone(),
                    t,
                    median,
                    format!(
                        "slow: {s:.1} s median by {}",
                        method.as_deref().unwrap_or("?")
                    ),
                );
            }
            if failed {
                flag(
                    test.clone(),
                    t,
                    median,
                    if passed {
                        "flaky: passed and failed at one revision".into()
                    } else {
                        "failing".into()
                    },
                );
            }
        }
        for t in &self.tests {
            for (hint, why) in [
                ("waits", "waits on time or load"),
                ("ui", "asserts UI text or presence"),
            ] {
                if let Some(n) = t["hints"][hint].as_u64() {
                    flag(key(t), Some(t), None, format!("{why} ({n} in its body)"));
                }
            }
        }
        // The collector's leads: a body the same as another's, heavy mocking.
        for c in self.candidates.iter().filter(|c| c.kind == "inspect") {
            let at = |t: &&serde_json::Value| {
                t["path"].as_str() == c.path.as_deref()
                    && t["line"].as_u64() == c.line.map(|l| l as u64)
            };
            if let Some(t) = self.tests.iter().find(at) {
                flag(key(t), Some(t), None, c.title.clone());
            }
        }
        out.into_values().collect()
    }
}

/// The map of flagged tests to their classes, as markdown for the person (docs/pure-cut.md was
/// NIT's): decisions by the module that makes them (the refactor cards), then the scenarios kept,
/// and the presence tests, clock waits, duplicates and restatements to delete or rewrite. Every
/// flagged test needs a class; a class for a test not flagged is refused, so the map stays the
/// evidence's.
pub fn render_map(revision: &str, flagged: &[Flagged], classes: &[Classified]) -> Result<String> {
    use std::collections::BTreeMap;
    let by_test: BTreeMap<&str, &Classified> =
        classes.iter().map(|c| (c.test.as_str(), c)).collect();
    if by_test.len() != classes.len() {
        bail!("a test is classified twice");
    }
    if let Some(c) = classes
        .iter()
        .find(|c| !flagged.iter().any(|f| f.test == c.test))
    {
        bail!(
            "{} is not among the flagged tests; classify only what the evidence flagged",
            c.test
        );
    }
    if let Some(c) = classes
        .iter()
        .find(|c| c.class == Class::Keep && c.note.trim().is_empty())
    {
        bail!("{} is kept as it is: say why in its note", c.test);
    }
    let missing: Vec<&str> = flagged
        .iter()
        .filter(|f| !by_test.contains_key(f.test.as_str()))
        .map(|f| f.test.as_str())
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} flagged tests have no class yet: {}",
            missing.len(),
            missing.join(", ")
        );
    }
    let time = |f: &Flagged| {
        f.seconds
            .map(|s| format!("{s:.1} s"))
            .unwrap_or_else(|| "not timed".into())
    };
    let row = |f: &Flagged, c: &Classified| {
        let note = if c.note.is_empty() {
            String::new()
        } else {
            format!(" {}", c.note)
        };
        format!(
            "- `{}` ({}; {}).{note}\n",
            f.test,
            time(f),
            f.why.join("; ")
        )
    };
    let mut modules: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut groups: [(&str, &str, Vec<String>); 6] = [
        ("Scenarios kept", "Real I/O end to end: keep each, named in the test budget's scenarios.", vec![]),
        ("Presence tests", "They assert that UI text or an element exists: delete, or check behaviour through state instead.", vec![]),
        ("Clock waits", "They wait on time or machine load: replace the wait with an event or an injected clock.", vec![]),
        ("Duplicates", "Another test checks the same thing: delete, naming what covers it.", vec![]),
        ("Restatements", "They restate the implementation: delete or rewrite against a behaviour.", vec![]),
        ("Kept as they are", "Flagged by a lead that was false: each is worth its cost, for the reason given.", vec![]),
    ];
    let mut saving: BTreeMap<&str, f64> = BTreeMap::new();
    for f in flagged {
        let c = by_test[f.test.as_str()];
        match &c.class {
            Class::Decision { decision, module } => {
                *saving.entry(module.as_str()).or_default() += f.seconds.unwrap_or(0.0);
                modules.entry(module.as_str()).or_default().push(format!(
                    "- {decision}: `{}` ({})\n",
                    f.test,
                    time(f)
                ));
            }
            Class::Scenario => groups[0].2.push(row(f, c)),
            Class::Presence => groups[1].2.push(row(f, c)),
            Class::WallClock => groups[2].2.push(row(f, c)),
            Class::Duplicate { of } => groups[3].2.push(format!(
                "- `{}` ({}): covered by `{of}`.\n",
                f.test,
                time(f)
            )),
            Class::Restates => groups[4].2.push(row(f, c)),
            Class::Keep => groups[5].2.push(row(f, c)),
        }
    }
    let mut md = format!("# Test map at {}\n\n{} tests flagged as slow, flaky or failing, or by a mechanical hint, each with its class. Times are medians by one measuring method; untimed tests are unknown, not fast.\n\n", &revision[..revision.len().min(12)], flagged.len());
    if !modules.is_empty() {
        md.push_str("## Decisions to lift, by module\n\nEach module is one refactor card: lift these decisions into pure functions, test those, and keep one or two scenarios.\n\n");
        for (module, lines) in &modules {
            md.push_str(&format!(
                "### {module} (up to {:.1} s measured)\n\n{}\n",
                saving[module],
                lines.concat()
            ));
        }
    }
    for (title, what, lines) in groups.iter().filter(|g| !g.2.is_empty()) {
        md.push_str(&format!("## {title}\n\n{what}\n\n{}\n", lines.concat()));
    }
    Ok(md)
}

/// One imported log (JUnit XML, libtest or TAP: `test_evidence::read`) as the collector's
/// observations and its run. A test named twice in one log is ambiguous, not a repeat; a log not
/// read whole is a partial run, and the tests it did not report stay unobserved.
pub fn import(
    text: &str,
    run: &str,
    revision: &str,
    source: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let evidence = crate::test_evidence::read(text);
    let runner = evidence.runner.clone().unwrap_or_else(|| "unknown".into());
    let mut seen = std::collections::HashMap::new();
    for t in &evidence.tests {
        *seen.entry(t.id.as_str()).or_insert(0) += 1;
    }
    let observations = evidence
        .tests
        .iter()
        .map(|t| {
            let mut o = serde_json::json!({"id": t.id, "name": t.name, "runner": runner, "outcome": t.outcome, "run": run, "revision": revision, "source": source, "ambiguous": seen[t.id.as_str()] > 1});
            for (key, value) in [("suite", t.suite.clone().map(Into::into)), ("file", t.file.clone().map(Into::into)), ("duration_ms", t.seconds.map(|s| (s * 1000.0).into())), ("flaky", t.flaky.then_some(true.into()))] {
                if let Some(v) = value {
                    o[key] = v;
                }
            }
            o
        })
        .collect();
    let summary = serde_json::json!({"runner": runner, "run": run, "revision": revision, "source": source, "format": evidence.format, "duration_ms": evidence.seconds.map(|s| s * 1000.0), "partial": !evidence.complete, "tests": evidence.tests.len(), "warnings": evidence.warnings});
    (observations, summary)
}

/// A clean source revision keeps imported outcomes and extracted bodies comparable.
pub fn revision(root: &Path) -> Result<String> {
    if !crate::playbook::git(root, &["status", "--porcelain", "--untracked-files=no"])?
        .trim()
        .is_empty()
    {
        bail!("commit tracked changes before collecting revision-bound sweep evidence");
    }
    Ok(crate::playbook::git(root, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sweep_schedule_is_idempotent_and_does_not_overlap_open_sweeps() {
        let mut db = Db::open(&crate::paths::unique_temp("nitely-sweep", "").join("t.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        let actor = Actor::Agent("codex:sweep".into());
        let rules = Schedule {
            test_sweep: Some("nightly".into()),
        };
        schedule(&mut db, "APP", &rules, &actor, 100000).unwrap();
        schedule(&mut db, "APP", &rules, &actor, 100001).unwrap();
        schedule(&mut db, "APP", &rules, &actor, 200000).unwrap();
        let issues = db.list_issues(Some("APP"), true).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].owner, "agent");
        assert!(issues[0].body.contains("do not edit or remove"));
        db.move_issue(
            &issues[0].key,
            "Done",
            Some("Report complete"),
            None,
            &actor,
        )
        .unwrap();
        schedule(&mut db, "APP", &rules, &actor, 300000).unwrap();
        assert_eq!(db.list_issues(Some("APP"), true).unwrap().len(), 2);
    }
    /// The tests to classify come from the evidence (slow by one method, flaky, a hint, a
    /// candidate), and the map needs a class for each of them and for nothing else.
    #[test]
    fn flagged_tests_need_a_class_each_and_the_map_groups_decisions_by_module() {
        let rev = "c".repeat(40);
        let obs = |id: &str, run: &str, outcome: &str, ms: f64, method: &str| serde_json::json!({"id": id, "name": id.rsplit("::").next().unwrap(), "outcome": outcome, "run": run, "revision": rev, "source": "CI", "method": method, "ambiguous": false, "duration_ms": ms});
        let report = Report {
            version: 1,
            revision: rev.clone(),
            generated_at: "now".into(),
            report_only: true,
            runs: vec![],
            observations: vec![
                obs("core::land::batches", "ci-1", "pass", 9000.0, "ci-log"),
                obs("core::land::batches", "ci-2", "pass", 8000.0, "ci-log"),
                obs("core::cart::adds", "ci-1", "pass", 10.0, "ci-log"),
                obs("core::cart::adds", "ci-2", "fail", 12.0, "ci-log"),
                // Timed only in a lane: not compared with CI's times, so not slow.
                obs("core::sync::pulls", "lane-1", "pass", 60000.0, "lane"),
            ],
            tests: vec![
                serde_json::json!({"name": "batches", "path": "src/land.rs", "line": 10, "hints": {"process": 2, "git": 4}}),
                serde_json::json!({"name": "adds", "path": "src/cart.rs", "line": 3, "hints": {}}),
                serde_json::json!({"name": "shows_title", "path": "web/page.test.tsx", "line": 7, "hints": {"ui": 2}}),
                serde_json::json!({"name": "pulls", "path": "src/sync.rs", "line": 5, "hints": {}}),
            ],
            candidates: vec![],
            truncated: false,
            warnings: vec![],
        };
        let flagged = report.flagged(2.0);
        let names: Vec<(&str, &str)> = flagged
            .iter()
            .map(|f| (f.test.as_str(), f.why[0].as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (
                    "src/cart.rs::adds",
                    "flaky: passed and failed at one revision"
                ),
                ("src/land.rs::batches", "slow: 9.0 s median by ci-log"),
                (
                    "web/page.test.tsx::shows_title",
                    "asserts UI text or presence (2 in its body)"
                )
            ]
        );
        assert_eq!(flagged[1].hints["git"], 4);
        // One run failing on one platform and passing on another is failing, not flaky.
        let matrix = Report {
            observations: vec![
                obs("core::cart::adds", "ci-7", "fail", 10.0, "ci-log"),
                obs("core::cart::adds", "ci-7", "pass", 10.0, "ci-log"),
            ],
            ..report.clone()
        };
        assert_eq!(
            matrix
                .flagged(2.0)
                .iter()
                .find(|f| f.test == "src/cart.rs::adds")
                .unwrap()
                .why,
            ["failing"]
        );

        let classes: Vec<Classified> = serde_json::from_value(serde_json::json!([
            {"test": "src/land.rs::batches", "class": "decision", "decision": "which changes land in one batch", "module": "land_rules"},
            {"test": "src/cart.rs::adds", "class": "wall-clock", "note": "waits for a debounce"},
        ]))
        .unwrap();
        let err = render_map(&rev, &flagged, &classes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("web/page.test.tsx::shows_title"), "{err}");
        let mut all = classes.clone();
        all.push(
            serde_json::from_value(
                serde_json::json!({"test": "web/page.test.tsx::shows_title", "class": "presence"}),
            )
            .unwrap(),
        );
        let map = render_map(&rev, &flagged, &all).unwrap();
        assert!(map.contains("### land_rules (up to 9.0 s measured)\n\n- which changes land in one batch: `src/land.rs::batches` (9.0 s)"), "{map}");
        assert!(
            map.contains("## Presence tests")
                && map.contains("## Clock waits")
                && !map.contains("## Scenarios kept"),
            "{map}"
        );
        all.push(
            serde_json::from_value(
                serde_json::json!({"test": "src/sync.rs::pulls", "class": "scenario"}),
            )
            .unwrap(),
        );
        assert!(
            render_map(&rev, &flagged, &all).is_err(),
            "a class for a test the evidence did not flag is refused"
        );
        // A false lead is kept as it is, with its reason.
        all.pop();
        all[2] = serde_json::from_value(
            serde_json::json!({"test": "web/page.test.tsx::shows_title", "class": "keep"}),
        )
        .unwrap();
        assert!(render_map(&rev, &flagged, &all)
            .unwrap_err()
            .to_string()
            .contains("say why"));
        all[2].note = "checks the rendered total, not that a label exists".into();
        assert!(render_map(&rev, &flagged, &all)
            .unwrap()
            .contains("## Kept as they are"));
    }

    /// A JUnit file imports as the same observations the collector classifies from libtest and
    /// TAP; a cut one is a partial run.
    #[test]
    fn imported_junit_becomes_observations_and_a_cut_file_a_partial_run() {
        let xml = r#"<testsuites name="pytest" time="1.5"><testsuite name="pytest"><testcase classname="tests.test_cart" name="test_total" time="1.25"/><testcase classname="tests.test_cart" name="test_tax"><failure/></testcase></testsuite></testsuites>"#;
        let (rows, run) = import(xml, "ci-1", "abc", "CI job 7");
        assert_eq!(rows[0]["id"], "tests.test_cart::test_total");
        assert_eq!(rows[0]["duration_ms"], 1250.0);
        assert_eq!(
            (rows[1]["outcome"].as_str(), rows[1]["runner"].as_str()),
            (Some("fail"), Some("pytest"))
        );
        assert_eq!(
            (run["partial"].as_bool(), run["duration_ms"].as_f64()),
            (Some(false), Some(1500.0))
        );
        let (rows, run) = import(&xml[..xml.len() - 40], "ci-2", "abc", "CI job 8");
        assert_eq!(rows.len(), 1);
        assert_eq!(run["partial"], true);
    }

    #[test]
    fn sweep_action_lists_require_human_approval_and_keep_edits() {
        let mut db =
            Db::open(&crate::paths::unique_temp("nitely-sweep-plan", "").join("t.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        let actor = Actor::Agent("codex:sweep".into());
        let report = Report {
            version: 1,
            revision: "a".repeat(40),
            generated_at: "now".into(),
            report_only: true,
            runs: vec![],
            observations: vec![],
            tests: vec![],
            truncated: false,
            warnings: vec![],
            candidates: vec![Candidate {
                id: "one".into(),
                kind: "fix".into(),
                title: "Flaky parser scenario".into(),
                evidence: "Failed then passed at the same revision".into(),
                path: Some("tests/parser.rs".into()),
                line: Some(10),
                source_hash: Some("b".repeat(64)),
            }],
        };
        let proposed = report.propose(&mut db, "APP", None, &actor).unwrap();
        assert_eq!(db.list_issues(Some("APP"), true).unwrap().len(), 1);
        let edit = crate::goal_plans::PlanEdit {
            bodies: std::collections::BTreeMap::from([(
                "fix".into(),
                "Repair only the parser scenario; preserve the roundtrip contract.".into(),
            )]),
            ..Default::default()
        };
        assert!(db
            .approve_goal_plan(&proposed.issue.key, &edit, &actor)
            .is_err());
        let approved = db
            .approve_goal_plan(&proposed.issue.key, &edit, &Actor::Human)
            .unwrap();
        let filed = db.issue(&approved.filed.keys["fix"]).unwrap().unwrap();
        assert!(filed.body.starts_with("Repair only the parser scenario"));
        assert_eq!(
            db.goal_plan_for_issue(&proposed.issue.key)
                .unwrap()
                .unwrap()
                .plan
                .issues[0]
                .body,
            edit.bodies["fix"]
        );
    }
}
