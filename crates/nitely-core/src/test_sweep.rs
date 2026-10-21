//! Test hygiene evidence produces a human-edited plan, never automatic test removal.
use crate::goal_plans::{GoalPlan, PlanItem, PlanProposed};
use crate::{Actor, Db, NewIssue};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GUIDE: &str = include_str!("test_sweep_guide.md");
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
