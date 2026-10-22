//! Test hygiene evidence produces a human-edited plan, never automatic test removal.
use crate::goal_plans::{GoalPlan, PlanItem, PlanProposed};
use crate::{Actor, Db, NewIssue};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The method of /nitely-test-optimize, from evidence to one goal; a scheduled health check
/// follows it as far as the report.
pub const GUIDE: &str = include_str!("test_optimize_method.md");
/// The analysis step of /nitely-test-optimize: classifying each flagged test (NIT-555).
pub const OPTIMIZE_GUIDE: &str = include_str!("test_optimize_guide.md");
pub const COLLECTOR: &str = include_str!("test_sweep.cjs");
pub const LABEL: &str = "test-sweep";
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schedule {
    /// The light, report-only health check: off (default), nightly (UTC day), or weekly (UTC
    /// seven-day bucket). Requires running Crew. `test_sweep`, its old name, still reads.
    #[serde(default, alias = "test_sweep")]
    pub test_health: Option<String>,
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
}

/// A schedule only files the read-only sweep job. Human approval remains mandatory for its plan.
pub fn schedule(
    db: &mut Db,
    project: &str,
    rules: &Schedule,
    actor: &Actor,
    now: i64,
) -> Result<()> {
    let period = match rules.test_health.as_deref().unwrap_or("off") {
        "off" => return Ok(()),
        "nightly" => now / 86400,
        "weekly" => now / (86400 * 7),
        other => bail!("schedule.test_health must be off, nightly or weekly, not {other}"),
    };
    let bucket = format!("{}-{period}", rules.test_health.as_deref().unwrap());
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
    /// `module` puts it on that module's refactor card as one of the scenarios kept.
    Scenario {
        #[serde(default)]
        module: Option<String>,
    },
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
    /// For a test to delete (presence, restates): the test or check that covers what it guarded.
    #[serde(default)]
    pub covered_by: Option<String>,
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
    /// The name its runner prints (what the test budget's scenarios name), when it ran.
    pub printed: Option<String>,
}

impl Report {
    /// The tests to classify: slower than `slow_seconds`, flaky or failing at the report's
    /// revision, or whose body waits on time or asserts UI text or presence; and those the
    /// project's gate `record` shows failing only ever for their own sake (NIT-565). Times are
    /// compared within one measuring method, as the collector compares them.
    pub fn flagged(
        &self,
        slow_seconds: f64,
        record: &[crate::test_catches::TestCatches],
    ) -> Vec<Flagged> {
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
        let mut flag = |test: String,
                        t: Option<&serde_json::Value>,
                        seconds: Option<f64>,
                        printed: Option<&str>,
                        why: String| {
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
                printed: None,
            });
            entry.seconds = entry.seconds.or(seconds);
            entry.printed = entry.printed.take().or(printed.map(String::from));
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
                    Some(&name),
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
                    Some(&name),
                    if passed {
                        "flaky: passed and failed at one revision".into()
                    } else {
                        "failing".into()
                    },
                );
            }
        }
        // The name a runner printed for an inventoried test, when exactly one observation names it:
        // what the budget's scenarios must match, whatever flagged the test.
        let printed_of = |t: &serde_json::Value| -> Option<String> {
            let n = t["name"].as_str()?;
            let names: std::collections::BTreeSet<&str> = self
                .observations
                .iter()
                .filter_map(|o| o["name"].as_str())
                .filter(|o| *o == n || o.ends_with(&format!("::{n}")))
                .collect();
            (names.len() == 1).then(|| names.into_iter().next().unwrap().to_string())
        };
        for t in &self.tests {
            for (hint, why) in [
                ("waits", "waits on time or load"),
                ("ui", "asserts UI text or presence"),
            ] {
                if let Some(n) = t["hints"][hint].as_u64() {
                    flag(
                        key(t),
                        Some(t),
                        None,
                        printed_of(t).as_deref(),
                        format!("{why} ({n} in its body)"),
                    );
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
                flag(
                    key(t),
                    Some(t),
                    None,
                    printed_of(t).as_deref(),
                    c.title.clone(),
                );
            }
        }
        // The gates' record, per test and step: failures that were only ever the test's own.
        for r in record
            .iter()
            .filter(|r| crate::test_catches::false_alarm_candidate(r))
        {
            let t = source(&r.test);
            flag(
                t.map(key).unwrap_or_else(|| r.test.clone()),
                t,
                None,
                Some(&r.test),
                format!(
                    "false alarms: its failures were its own {} times and never caught anything (gate record, `{}`)",
                    r.false_alarms, r.step
                ),
            );
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
    let by_test = check_classes(flagged, classes)?;
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
            Class::Scenario { .. } => groups[0].2.push(row(f, c)),
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

/// The classes file `map --classes` and `propose --classes` read.
pub fn read_classes(path: &Path) -> Result<Vec<Classified>> {
    if path.metadata()?.len() > 4 * 1024 * 1024 {
        bail!("the classes file exceeds 4 MiB");
    }
    serde_json::from_slice(&std::fs::read(path)?).context("classes must be a JSON array of {test, class, ...}: decision (with decision and module), scenario (with an optional module), presence, wall-clock, duplicate (with of) or restates (with covered_by)")
}

/// Every flagged test has exactly one class, and nothing else has one, so the map and the goal
/// stay the evidence's.
fn check_classes<'a>(
    flagged: &[Flagged],
    classes: &'a [Classified],
) -> Result<std::collections::BTreeMap<&'a str, &'a Classified>> {
    let by_test: std::collections::BTreeMap<&str, &Classified> =
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
    Ok(by_test)
}

/// A refactor card's title: the module and the decisions it lifts, short enough to read.
fn refactor_title<'a>(module: &str, decisions: impl Iterator<Item = &'a str>) -> String {
    let mut title = format!(
        "{module}: lift {}",
        decisions.collect::<Vec<_>>().join("; ")
    );
    if title.chars().count() > 120 {
        title = title.chars().take(117).collect::<String>() + "...";
    }
    title
}

/// Whether a gate running `command` prints the per-test times `test_budget::parse` reads.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetReads {
    Timed,
    /// It would with this flag.
    Add(String),
    Not,
}

/// `npm test` and `npm run <script>` are read through `package` (package.json) as the script
/// they run.
pub fn budget_reads(command: &str, package: Option<&serde_json::Value>) -> BudgetReads {
    reads_within(command, package, 0)
}

/// The commands of a compound command line, split at `&&`, `||`, `|` and `;` outside quotes: a
/// quoted `'TestFoo|TestBar'` is one argument, not a pipe.
fn command_parts(command: &str) -> Vec<&str> {
    let (mut parts, mut start, mut quote, mut escaped) = (Vec::new(), 0, None, false);
    for (i, c) in command.char_indices() {
        if std::mem::take(&mut escaped) {
            continue;
        }
        match (quote, c) {
            // A backslash escapes the next character, except inside single quotes (`it\'s`).
            (None | Some('"'), '\\') => escaped = true,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '&' | '|' | ';') => {
                parts.push(&command[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&command[start..]);
    parts.into_iter().map(str::trim).filter(|p| !p.is_empty()).collect()
}

/// `budget_reads` of each part of a compound command (`npm run build && node --test ...`), its
/// npm scripts followed `depth` deep at most: timed if any part is, else the first flag that would
/// make one so.
fn reads_within(command: &str, package: Option<&serde_json::Value>, depth: u32) -> BudgetReads {
    let mut best = BudgetReads::Not;
    for part in command_parts(command) {
        let words: Vec<&str> = part.split_whitespace().collect();
        let script = match words.as_slice() {
            ["npm", "test" | "t", ..] => Some("test"),
            ["npm", "run" | "run-script", name, ..] => Some(*name),
            _ => None,
        };
        let reads = match script {
            Some(name) if depth < 4 => match package.and_then(|p| p["scripts"][name].as_str()) {
                Some(body) => reads_within(body, package, depth + 1),
                None => BudgetReads::Not,
            },
            Some(_) => BudgetReads::Not,
            None => runner_reads(&words),
        };
        match reads {
            BudgetReads::Timed => return BudgetReads::Timed,
            BudgetReads::Add(_) if best == BudgetReads::Not => best = reads,
            _ => {}
        }
    }
    best
}

/// `budget_reads` of one command, its words.
fn runner_reads(words: &[&str]) -> BudgetReads {
    let has = |w: &str| words.contains(&w);
    if words.first() == Some(&"cargo") && words.get(1) == Some(&"test")
        || has("node") && has("--test")
    {
        return BudgetReads::Timed;
    }
    if has("pytest") {
        return if words.iter().any(|w| w.starts_with("--durations")) {
            BudgetReads::Timed
        } else {
            // The parser reads pytest's verbose lines: -v, and no -q to cancel it.
            BudgetReads::Add("-v --durations=0 --durations-min=0, without -q".into())
        };
    }
    if words.first() == Some(&"go") && words.get(1) == Some(&"test") {
        return if has("-v") {
            BudgetReads::Timed
        } else {
            BudgetReads::Add("-v".into())
        };
    }
    BudgetReads::Not
}

/// What the goal is planned from besides the map: the budget's per-test limit, the gate's test
/// commands, and the commands CI already runs, when the repo has CI.
pub struct OptimizeFacts<'a> {
    pub slow_seconds: f64,
    pub tests: &'a [String],
    pub ci_commands: &'a [String],
    pub has_ci: bool,
    /// The playbook has `[lanes]`, where `ci_commands` lives; a repo without build lanes gets no
    /// CI-routing proposal, which would turn lanes on.
    pub has_lanes: bool,
    /// The repo's playbook, for the CI-routing proposal; none in a repo Nitely does not keep.
    pub playbook: Option<&'a crate::proposals::Target>,
    /// The project's gate record per test (`Db::catches_by_test`), which flags the tests whose
    /// failures were only ever their own.
    pub record: &'a [crate::test_catches::TestCatches],
    /// The repo's package.json, to read `npm test` as the script it runs.
    pub package: Option<&'a serde_json::Value>,
    /// The repo's test budget, when it has one: the plan then names only the scenarios it lacks.
    pub budget: Option<&'a crate::test_budget::Budget>,
}

/// `n` and the word for it: "1 test", "2 tests".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What the goal says goes, in seconds measured on the tests it would cut (NIT-607): a sum over
/// the timed ones, or "no measured saving" when none was timed, and the untimed ones by name.
pub fn saving_words(seconds: &[Option<f64>], names: &[&str]) -> String {
    let timed: Vec<f64> = seconds.iter().flatten().copied().collect();
    let untimed: Vec<&str> = seconds.iter().zip(names).filter(|(s, _)| s.is_none()).map(|(_, n)| *n).collect();
    let sum: f64 = timed.iter().sum();
    let said = if timed.is_empty() {
        "no measured saving".to_string()
    } else {
        format!("up to {} of measured test time goes", if sum < 10.0 { format!("{sum:.1} s") } else { format!("{sum:.0} s") })
    };
    match untimed.as_slice() {
        [] => said,
        some if some.len() <= 3 => format!("{said} (no times for {})", some.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")),
        some => format!("{said} ({} have no times)", count(some.len(), "test", "tests")),
    }
}

/// The budget card's title and TOML (NIT-607): the whole budget for a repo without one; for a repo
/// that has one, only the scenarios it does not name yet; None when it names them all.
pub fn budget_card(existing: Option<&crate::test_budget::Budget>, slow_seconds: f64, scenarios: &[String]) -> Option<(String, String)> {
    let missing: Vec<&String> = scenarios.iter().filter(|n| existing.is_none_or(|b| !b.scenarios.contains_key(*n))).collect();
    let mut toml = String::new();
    if existing.is_none() {
        toml.push_str(&format!("test_seconds = {slow_seconds}\n"));
    } else if missing.is_empty() {
        return None;
    }
    if !missing.is_empty() {
        toml.push_str("\n[scenarios]\n");
        for name in &missing {
            toml.push_str(&format!("{} = {}\n", serde_json::to_string(name).ok()?, serde_json::to_string("real I/O end to end (test map)").ok()?));
        }
    }
    let title = if existing.is_none() {
        "Set the test time budget and name the scenarios".to_string()
    } else {
        format!("Name {} in the test budget", count(missing.len(), "more scenario", "more scenarios"))
    };
    Some((title, toml))
}

/// The goal a classified map proposes (NIT-556), and the `ci_commands` the playbook should list
/// when CI does not run every test command yet.
#[derive(Debug, Clone)]
pub struct OptimizePlan {
    pub plan: GoalPlan,
    pub ci_commands: Option<Vec<String>>,
}

/// One goal from the map, as goal 20 was: a refactor card per module, each with the decisions to
/// lift, the scenarios it keeps and the time measured on its tests; a card for clock waits; the
/// test budget naming the scenarios; and one deletion card, after the refactors, naming for every
/// test what covers it. Nothing is deleted or refactored before the person approves the goal,
/// and the deletions wait for one more yes.
pub fn optimize_plan(
    revision: &str,
    flagged: &[Flagged],
    classes: &[Classified],
    facts: &OptimizeFacts,
) -> Result<OptimizePlan> {
    use std::collections::BTreeMap;
    let by_test = check_classes(flagged, classes)?;
    if flagged.is_empty() {
        bail!("nothing is flagged: keep the report and its unknowns; no goal is needed");
    }
    let short = &revision[..revision.len().min(12)];
    let time = |f: &Flagged| {
        f.seconds
            .map(|s| format!("{s:.1} s"))
            .unwrap_or_else(|| "not timed".into())
    };
    let paths = |fs: &[&Flagged]| {
        let mut p: Vec<String> = fs.iter().filter_map(|f| f.path.clone()).collect();
        p.sort();
        p.dedup();
        p
    };
    let size = |n: usize| {
        Some(
            if n <= 3 {
                "S"
            } else if n <= 10 {
                "M"
            } else {
                "L"
            }
            .to_string(),
        )
    };
    let evidence = format!("Test map at {short}: {} flagged tests, each classified by reading its code. Times are medians by one measuring method; untimed tests are unknown.", flagged.len());
    let item = |local_ref: String,
                title: String,
                body: String,
                touches: Vec<String>,
                n: usize,
                blocked_by: Vec<String>| PlanItem {
        local_ref,
        title,
        body,
        evidence: evidence.clone(),
        kind: if touches.is_empty() {
            "live-check"
        } else {
            "improvement"
        }
        .into(),
        priority: 3,
        labels: vec!["test-hygiene".into()],
        blocked_by,
        touches,
        size: size(n),
        review_level: Some("standard".into()),
        ..Default::default()
    };
    let mut modules: BTreeMap<&str, Vec<(&str, &Flagged)>> = BTreeMap::new();
    let mut scenarios: BTreeMap<Option<&str>, Vec<&Flagged>> = BTreeMap::new();
    let (mut waits, mut deletions) = (vec![], vec![]);
    let mut uncovered = vec![];
    for f in flagged {
        let c = by_test[f.test.as_str()];
        match &c.class {
            Class::Decision { decision, module } => modules
                .entry(module.as_str())
                .or_default()
                .push((decision.as_str(), f)),
            Class::Scenario { module } => scenarios.entry(module.as_deref()).or_default().push(f),
            Class::WallClock => waits.push(f),
            Class::Duplicate { of } => deletions.push((f, of.clone())),
            Class::Presence => deletions.push((
                f,
                c.covered_by.clone().unwrap_or_else(|| {
                    "nothing it should keep: it asserts only that UI text or an element exists"
                        .into()
                }),
            )),
            Class::Restates => match &c.covered_by {
                Some(by) => deletions.push((f, by.clone())),
                None => uncovered.push(f.test.as_str()),
            },
            // Worth its cost as it is: no card.
            Class::Keep => {}
        }
    }
    if !uncovered.is_empty() {
        bail!("a test that restates the implementation needs `covered_by`, the behaviour test that covers it, before its deletion is proposed: {}", uncovered.join(", "));
    }
    let mut items = Vec::new();
    // Failing and flaky tests that stay (scenarios, kept tests, decisions until their refactor)
    // are repaired, whatever else is planned for them.
    let failing: Vec<&Flagged> = flagged
        .iter()
        .filter(|f| {
            f.why
                .iter()
                .any(|w| w.starts_with("failing") || w.starts_with("flaky"))
        })
        .filter(|f| {
            !matches!(
                by_test[f.test.as_str()].class,
                Class::Duplicate { .. } | Class::Presence | Class::Restates
            )
        })
        .collect();
    if !failing.is_empty() {
        let mut body = "These tests fail, or pass and fail at one revision. Find why and fix the cause; a failure is a finding, never a reason to delete:\n\n".to_string();
        for f in &failing {
            body.push_str(&format!("- `{}` ({})\n", f.test, f.why.join("; ")));
        }
        items.push(item(
            "repair".into(),
            format!("Repair {} failing or flaky", count(failing.len(), "test", "tests")),
            body,
            paths(&failing),
            failing.len(),
            vec![],
        ));
    }
    for (n, (module, decisions)) in modules.iter().enumerate() {
        let measured: f64 = decisions.iter().filter_map(|(_, f)| f.seconds).sum();
        let timed = decisions
            .iter()
            .filter(|(_, f)| f.seconds.is_some())
            .count();
        let mut body = format!("Lift these decisions out of {module} into plain functions over plain data, and test each without I/O:\n\n");
        for (decision, f) in decisions {
            body.push_str(&format!(
                "- {decision}: now checked by `{}` ({})\n",
                f.test,
                time(f)
            ));
        }
        let kept = scenarios
            .get(&Some(*module))
            .map(Vec::as_slice)
            .unwrap_or_default();
        if !kept.is_empty() {
            body.push_str("\nKeep these scenarios (real I/O, named in the test budget):\n\n");
            for f in kept {
                body.push_str(&format!("- `{}` ({})\n", f.test, time(f)));
            }
        }
        body.push_str(&format!("\nEstimated saving: up to {measured:.1} s of measured time across {timed} of {} tests (untimed ones unknown).\n", decisions.len()));
        body.push_str("\nKeep the old tests: the deletion card removes them, in one approval, once this lands; list in your handoff the pure test that covers each.\n");
        for (decision, f) in decisions {
            deletions.push((*f, format!("the pure test of \"{decision}\" from refactor-{} ({module}), if that card landed", n + 1)));
        }
        let fs: Vec<&Flagged> = decisions
            .iter()
            .map(|(_, f)| *f)
            .chain(kept.iter().copied())
            .collect();
        items.push(item(
            format!("refactor-{}", n + 1),
            refactor_title(module, decisions.iter().map(|(d, _)| *d)),
            body,
            paths(&fs),
            decisions.len(),
            vec![],
        ));
    }
    if !waits.is_empty() {
        let mut body = "These tests wait on the clock or on machine load. Wait on the event instead, or inject the clock, so each passes as fast as the machine allows and never fails under load:\n\n".to_string();
        for f in &waits {
            body.push_str(&format!(
                "- `{}` ({}; {})\n",
                f.test,
                time(f),
                f.why.join("; ")
            ));
        }
        items.push(item(
            "clock-waits".into(),
            format!("Replace {} with events or an injected clock", count(waits.len(), "clock wait", "clock waits")),
            body,
            paths(&waits),
            waits.len(),
            vec![],
        ));
    }
    let named: Vec<&Flagged> = scenarios.values().flatten().copied().collect();
    // One key per printed name: two binaries' `tests::roundtrip` are one entry.
    let mut names: Vec<String> = Vec::new();
    for f in &named {
        let name = f.printed.clone().unwrap_or_else(|| f.test.rsplit("::").next().unwrap_or(&f.test).to_string());
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let card = budget_card(facts.budget, facts.slow_seconds, &names);
    // The budget works only where a gate's output carries per-test times: say which commands do,
    // what to add where a flag would, and leave the card out when none can.
    let reads: Vec<(&String, BudgetReads)> = facts
        .tests
        .iter()
        .map(|t| (t, budget_reads(t, facts.package)))
        .collect();
    if let (Some((title, budget)), true) = (card, reads.iter().any(|(_, r)| !matches!(r, BudgetReads::Not))) {
        let mut how = String::new();
        let mut touches = vec![".nitely/test-budget.toml".to_string()];
        for (t, r) in &reads {
            how.push_str(&match r {
                BudgetReads::Timed => format!("- `{t}`: gates read its per-test times.\n"),
                BudgetReads::Add(flag) => format!("- `{t}`: add `{flag}` to it in the playbook's gate, or gates see no per-test times.\n"),
                BudgetReads::Not => format!("- `{t}`: prints no per-test times a gate reads (its runner writes them only to JUnit or TRX), so the budget cannot flag its tests.\n"),
            });
            if matches!(r, BudgetReads::Add(_))
                && !touches.iter().any(|p| p == ".nitely/playbook.toml")
            {
                touches.push(".nitely/playbook.toml".into());
            }
        }
        let what = if facts.budget.is_some() { "Add these scenarios to `.nitely/test-budget.toml`, so a gate does not flag them" } else { "Write `.nitely/test-budget.toml` so a gate reports test times and flags a test past the limit unless it is a named scenario" };
        items.push(item("budget".into(), title, format!("{what}:\n\n```toml\n{budget}```\n\nNames are as the runner prints them; check each against a gate's \"Test time\" line. What the gate's test commands give the budget:\n\n{how}\nA playbook change goes through `propose_playbook_change`."), touches, 1, vec![]));
    }
    if !deletions.is_empty() {
        let mut body = "After the cards above land, ask the person once (`ask`) with this list, and delete only what they approve. Every test names what covers it. A decision's old test goes only if its refactor card landed: a card the person dropped keeps its tests, so leave them off the list:\n\n".to_string();
        for (f, by) in &deletions {
            body.push_str(&format!("- `{}` ({}): covered by {by}\n", f.test, time(f)));
        }
        let fs: Vec<&Flagged> = deletions.iter().map(|(f, _)| *f).collect();
        let blockers = items
            .iter()
            .filter(|i| i.local_ref != "budget" && i.local_ref != "repair")
            .map(|i| i.local_ref.clone())
            .collect();
        items.push(item(
            "delete".into(),
            format!("Delete {} other tests cover (one approval)", count(deletions.len(), "test", "tests")),
            body,
            paths(&fs),
            deletions.len(),
            blockers,
        ));
    }
    // CI runs every test command where the repo has CI: the full suites leave the person's machine.
    // A command naming a local path or share runs only in a lane, so CI never takes it.
    let missing: Vec<String> = facts
        .tests
        .iter()
        .filter(|t| !facts.ci_commands.contains(t))
        .filter(|t| {
            !["{main}", "{worktree}", "{base}", "{jobs}"]
                .iter()
                .any(|p| t.contains(p))
        })
        .cloned()
        .collect();
    let ci_commands = (facts.has_ci && facts.has_lanes && !missing.is_empty()).then(|| {
        let mut all = facts.ci_commands.to_vec();
        all.extend(missing);
        all
    });
    let cut: Vec<&Flagged> = flagged
        .iter()
        .filter(|f| matches!(by_test[f.test.as_str()].class, Class::Decision { .. } | Class::WallClock))
        .collect();
    let saving = saving_words(&cut.iter().map(|f| f.seconds).collect::<Vec<_>>(), &cut.iter().map(|f| f.test.as_str()).collect::<Vec<_>>());
    let plan = GoalPlan {
        sentence: "Make this repo's tests cost what they're worth".into(),
        title: "Tests that cost what they're worth".into(),
        outcome: format!(
            "Decisions are tested as pure functions and real I/O by a few named scenarios; no test waits on the clock or checks only that UI text exists; every test stays under {} s unless named; {}{saving}. Approve the refactors module by module; the deletions wait for one more yes. Measured at {short}.",
            facts.slow_seconds,
            if ci_commands.is_some() { "the full suites run in CI; " } else { "" }
        ),
        issues: items,
        ..Default::default()
    };
    Ok(OptimizePlan { plan, ci_commands })
}

/// A proposed optimize goal, and the playbook proposal for CI routing when there is one.
#[derive(Debug, Clone, Serialize)]
pub struct OptimizeProposed {
    pub goal: PlanProposed,
    pub playbook: Option<crate::proposals::Proposed>,
}

impl Report {
    /// Proposes the goal `optimize_plan` builds, labelled as a sweep, and, given the repo's
    /// playbook, the `ci_commands` change it needs. The person approves both; nothing is applied.
    pub fn propose(
        &self,
        db: &mut Db,
        project: &str,
        planning_issue: Option<String>,
        classes: &[Classified],
        facts: &OptimizeFacts,
        actor: &Actor,
    ) -> Result<OptimizeProposed> {
        self.check()?;
        if let Some(key) = &planning_issue {
            let issue = db.issue(key)?.context("sweep issue missing")?;
            if issue.project.as_deref() != Some(project) {
                bail!("sweep issue belongs to another project");
            }
        }
        let planned = optimize_plan(
            &self.revision,
            &self.flagged(facts.slow_seconds, facts.record),
            classes,
            facts,
        )?;
        let goal = db.propose_goal(project, &planned.plan, planning_issue.as_deref(), actor)?;
        db.change_labels(&goal.issue.key, &[LABEL.into()], &[], actor)?;
        let playbook = match (planned.ci_commands, facts.playbook) {
            (Some(commands), Some(target)) => Some(db.propose_playbook_change(
                project,
                target,
                crate::proposals::Mode::Merge,
                &format!("[lanes]\nci_commands = {}\n", serde_json::to_string(&commands)?),
                &format!("Run every test command in CI so full suites never run on the person's machine (test goal {}).", goal.issue.key),
                actor,
            )?),
            _ => None,
        };
        Ok(OptimizeProposed { goal, playbook })
    }
}

/// One imported log (JUnit XML, libtest or TAP: `test_evidence::read`) as the collector's
/// observations and its run. A test named twice in one log is ambiguous, not a repeat; a log not
/// read whole is a partial run, and the tests it did not report stay unobserved.
pub fn import(
    text: &str,
    run: &str,
    revision: &str,
    source: &str,
    method: Method,
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
            let mut o = serde_json::json!({"id": t.id, "name": t.name, "runner": runner, "outcome": t.outcome, "run": run, "revision": revision, "source": source, "method": method, "ambiguous": seen[t.id.as_str()] > 1});
            for (key, value) in [("suite", t.suite.clone().map(Into::into)), ("file", t.file.clone().map(Into::into)), ("duration_ms", t.seconds.map(|s| (s * 1000.0).into())), ("flaky", t.flaky.then_some(true.into()))] {
                if let Some(v) = value {
                    o[key] = v;
                }
            }
            o
        })
        .collect();
    let summary = serde_json::json!({"runner": runner, "run": run, "revision": revision, "source": source, "method": method, "format": evidence.format, "duration_ms": evidence.seconds.map(|s| s * 1000.0), "partial": !evidence.complete, "tests": evidence.tests.len(), "warnings": evidence.warnings});
    (observations, summary)
}

/// How a run's evidence was measured (NIT-554). Times from different methods are never compared:
/// a CI runner, a build lane and a log of either differ in machine and load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// A finished CI run's logs or artifacts, read and nothing run.
    CiLog,
    /// A CI run dispatched for the sweep.
    CiRun,
    /// A capped run in a build lane, for a repo without CI.
    Lane,
    /// A log the person supplied.
    Imported,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::CiLog => "ci-log",
            Method::CiRun => "ci-run",
            Method::Lane => "lane",
            Method::Imported => "imported",
        }
    }
}

/// A run's id from its parts (`ci`, the workflow run, its attempt), safe in a file name. A
/// workflow run's jobs share one: they are one execution, where the collector keeps a failure over
/// a pass.
pub fn run_id(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// The gate's commands that run tests: `cargo test`, `npm test`, `pytest`, `go test`,
/// `dotnet test`, `mvn test`, `gradle test`, and the like. Builds and lints are not measured.
pub fn test_commands(gate: &[String]) -> Vec<String> {
    gate.iter()
        .filter(|c| {
            let words: Vec<&str> = c.split_whitespace().collect();
            words.iter().any(|w| {
                matches!(
                    *w,
                    "test"
                        | "tests"
                        | "--test"
                        | "pytest"
                        | "vitest"
                        | "jest"
                        | "nextest"
                        | "gotestsum"
                )
            }) || words.iter().any(|w| w.starts_with("test:"))
        })
        .cloned()
        .collect()
}

/// What `measure` does, in order of preference: read CI's logs at the revision when there are
/// any; else dispatch CI runs of the test commands CI takes; else, only when the repo has no CI,
/// run them in a build lane. A run is named, with its cost cap, before it starts: `start` says the
/// person asked for that method, and without it the plan is only shown.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeasurePlan {
    pub method: Method,
    pub commands: Vec<String>,
    pub runs: u32,
    pub minutes: u32,
    /// Shown to the person before anything runs.
    pub says: String,
    /// Whether to run it now.
    pub start: bool,
    /// The commit the evidence is for when it is not HEAD: main's newest commit CI tested.
    /// `collect --at` builds its report from that commit's tree.
    pub at: Option<String>,
}

/// Where CI stands, as `plan_measure` needs it (the shell reads `executor::ci_state`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CiFact<'a> {
    /// CI is reachable, and `workflow` says whether it has Nitely's suite workflow
    /// (`[lanes] ci_commands` set up) to dispatch to.
    Ready { workflow: bool },
    /// The repo has no CI Nitely can use, or this machine keeps it local: a lane may measure.
    Absent(&'a str),
    /// The repo has CI that cannot be reached from here.
    Blocked(&'a str),
}

/// The inputs `plan_measure` decides on, gathered by the shell.
pub struct MeasureFacts<'a> {
    pub revision: &'a str,
    /// Logs found from finished CI runs that tested `revision`.
    pub ci_logs: usize,
    /// When HEAD has none, the newest integration-branch commit CI tested and the logs with tests
    /// found for it: that commit's evidence, filed under it and never under HEAD's name.
    pub tested_main: Option<(&'a str, usize)>,
    pub ci: CiFact<'a>,
    /// The gate's test commands.
    pub tests: &'a [String],
    /// The repo's package.json, to find the runner behind `npm test` (NIT-567).
    pub package: Option<&'a serde_json::Value>,
    pub runs: u32,
    pub minutes: u32,
    /// The person asked for a CI run (`--dispatch`) or a lane run (`--lane`).
    pub dispatch: bool,
    pub lane: bool,
}

/// What CI runs for test command `t` so its results come back per test (NIT-567): the command
/// writing its runner's report (JUnit XML or TRX) where the suite workflow uploads it, else one
/// whose console says each outcome (cargo, node:test, go test -v). An npm script whose runner
/// needs a report it cannot write is skipped, with why, rather than run for nothing.
pub fn dispatched_command(
    t: &str,
    package: Option<&serde_json::Value>,
) -> std::result::Result<String, String> {
    if ["{main}", "{worktree}", "{base}", "{jobs}"]
        .iter()
        .any(|p| t.contains(p))
    {
        return crate::test_evidence::console_command(t);
    }
    match crate::test_evidence::report_command(t, package) {
        Ok(c) => Ok(c),
        Err(why) if why.contains("jest-junit") || why.contains("more than one command") => Err(why),
        Err(_) => crate::test_evidence::console_command(t),
    }
}

/// Which CI other than GitHub Actions the repo at `root` is set up for, as why `measure` may
/// neither dispatch to it nor run its suite here: Nitely's executor reaches GitHub Actions only.
pub fn other_ci(root: &Path) -> Option<&'static str> {
    const KNOWN: &[(&str, &str)] = &[
        ("bitbucket-pipelines.yml", "the repo runs Bitbucket Pipelines (bitbucket-pipelines.yml), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
        (".gitlab-ci.yml", "the repo runs GitLab CI (.gitlab-ci.yml), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
        (".circleci", "the repo runs CircleCI (.circleci), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
        ("azure-pipelines.yml", "the repo runs Azure Pipelines (azure-pipelines.yml), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
        ("Jenkinsfile", "the repo runs Jenkins (Jenkinsfile), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
        (".travis.yml", "the repo runs Travis CI (.travis.yml), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
        (".buildkite", "the repo runs Buildkite (.buildkite), which Nitely cannot dispatch to; read its test logs or JUnit with `collect --logs`"),
    ];
    KNOWN
        .iter()
        .find(|(file, _)| root.join(file).exists())
        .map(|(_, why)| *why)
}

pub fn plan_measure(f: &MeasureFacts) -> Result<MeasurePlan> {
    let short = &f.revision[..f.revision.len().min(12)];
    let plan = |method, commands: Vec<String>, says: String, start| MeasurePlan {
        method,
        commands,
        runs: f.runs,
        minutes: f.minutes,
        says,
        start,
        at: None,
    };
    if f.ci_logs > 0 && !f.dispatch && !f.lane {
        return Ok(plan(
            Method::CiLog,
            vec![],
            format!("{} CI logs at {short}: read them, nothing runs", f.ci_logs),
            true,
        ));
    }
    if let (Some((sha, logs)), false, false) = (f.tested_main, f.dispatch, f.lane) {
        if logs > 0 {
            let main = &sha[..sha.len().min(12)];
            return Ok(MeasurePlan {
                at: Some(sha.to_string()),
                ..plan(Method::CiLog, vec![], format!("No CI logs at {short}. {logs} CI logs of {main}, the newest main commit CI tested: read them as {main}'s evidence (collect it with --at {main}), nothing runs"), true)
            });
        }
    }
    if f.tests.is_empty() {
        bail!(
            "the playbook's gate has no test command to measure; supply logs to `collect --logs`"
        );
    }
    let ask = |says: String, flag: &str, start: bool| {
        if start {
            says
        } else {
            format!("{says}. Start it with {flag}.")
        }
    };
    match f.ci {
        CiFact::Blocked(why) => bail!("the repo has CI, which cannot be reached from here ({why}); fix that and measure again: nothing runs on this machine while the repo has CI"),
        CiFact::Ready { .. } if f.lane => bail!("this repo has CI: measure there (--dispatch), not on this machine"),
        CiFact::Ready { workflow: false } => bail!("CI has no Nitely suite workflow to dispatch to ([lanes] ci_commands is empty): set one up (docs/build-lanes.md), or have CI write JUnit and upload it as an artifact named for tests, and measure reads it from CI's finished runs"),
        CiFact::Ready { workflow: true } => {
            let (mut commands, mut skipped) = (vec![], vec![]);
            for t in f.tests {
                match dispatched_command(t, f.package) {
                    Ok(c) => commands.push(c),
                    Err(why) => skipped.push(why),
                }
            }
            if commands.is_empty() {
                bail!("{}", skipped.join("; "));
            }
            let mut says = format!("No CI logs at {short}. CI would run {} run(s) of {} at {short}, each run capped at {} minutes, queue included", f.runs, commands.join(" and "), f.minutes);
            if f.runs > 1 {
                says.push_str("; two runs at one revision tell a flaky test from a failing one");
            }
            if !skipped.is_empty() {
                says.push_str(&format!(". Not dispatched: {}", skipped.join("; ")));
            }
            Ok(plan(Method::CiRun, commands, ask(says, "--dispatch", f.dispatch), f.dispatch))
        }
        CiFact::Absent(why) => {
            if f.dispatch {
                bail!("CI cannot take the test commands: {why}");
            }
            // The same readable consoles as a dispatch: pytest -v, go test -v -count=1.
            let (mut commands, mut skipped) = (vec![], vec![]);
            for t in f.tests {
                match crate::test_evidence::readable_command(t) {
                    Ok(c) => commands.push(c),
                    Err(why) => skipped.push(why),
                }
            }
            if commands.is_empty() {
                bail!("{}", skipped.join("; "));
            }
            if (f.minutes as usize) < commands.len() {
                bail!("a lane run's cap is shared by its {} commands: give it at least {} minutes (--minutes)", commands.len(), commands.len());
            }
            let mut says = format!("No CI ({why}). A build lane would run {} run(s) of {} at {short}, each run's commands capped at {} minutes in all (a lane's dependency setup, when it reruns, comes before and is not counted)", f.runs, commands.join(" and "), f.minutes);
            if !skipped.is_empty() {
                says.push_str(&format!(". Not run: {}", skipped.join("; ")));
            }
            Ok(plan(Method::Lane, commands, ask(says, "--lane", f.lane), f.lane))
        }
    }
}

/// HEAD of the checkout at `dir`, for measuring by running tests there (NIT-554): untracked files
/// count as changes too, since its CI or lane snapshot would run them while the evidence names
/// HEAD, and the evidence folder `out` must be outside it for the same reason.
pub fn measured_revision(dir: &Path, out: &Path) -> Result<String> {
    let top = crate::playbook::git(dir, &["rev-parse", "--show-toplevel"])?;
    // Both resolved (`..`, links, case), so a folder beside the checkout is told from one in it.
    let top = Path::new(top.trim()).canonicalize()?;
    let out = out.canonicalize()?;
    if out.starts_with(&top) {
        bail!("write the evidence outside the checkout when tests run (--out), so no snapshot picks it up");
    }
    let status = crate::playbook::git(dir, &["status", "--porcelain", "--untracked-files=all"])?;
    if !status.trim().is_empty() {
        bail!("commit or remove changed and untracked files before measuring: a snapshot would run them under HEAD's name");
    }
    Ok(crate::playbook::git(dir, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// The full hash of commit `at`, for a report on that commit: its tests are read from its own
/// tree, so it need not be HEAD's ancestor (main moves on after a task branch forks). A commit not
/// here yet is fetched from origin first.
pub fn ancestor_revision(dir: &Path, at: &str) -> Result<String> {
    if at.is_empty() || at.len() > 40 || !at.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("--at takes a commit hash");
    }
    let commit = format!("{at}^{{commit}}");
    if crate::playbook::git(dir, &["rev-parse", "--verify", "--quiet", &commit]).is_err() {
        crate::playbook::git(dir, &["fetch", "--quiet", "origin", at])
            .with_context(|| format!("{at} is not here and could not be fetched from origin"))?;
    }
    Ok(
        crate::playbook::git(dir, &["rev-parse", "--verify", &commit])?
            .trim()
            .to_string(),
    )
}

/// The repo facts `Report::propose` plans from: its gate's test commands, the commands CI already
/// runs, whether CI is reachable from here, and the playbook a CI-routing proposal changes.
pub struct RepoFacts {
    pub project: String,
    /// The repo's package.json, when it has one.
    pub package: Option<serde_json::Value>,
    pub tests: Vec<String>,
    pub ci_commands: Vec<String>,
    pub has_ci: bool,
    pub has_lanes: bool,
    pub target: Option<crate::proposals::Target>,
    /// The repo's test budget, when it has one.
    pub budget: Option<crate::test_budget::Budget>,
}

pub fn repo_facts(config: &crate::Config, root: &Path) -> Result<RepoFacts> {
    let pb = crate::playbook::Playbook::load_with(root, config)?;
    let gate: Vec<String> = pb
        .gate
        .commands
        .iter()
        .cloned()
        .chain(pb.gate.steps.iter().map(|s| s.run.clone()))
        .collect();
    let normalized = crate::paths::normalize(&root.to_string_lossy());
    let repo = config
        .repos
        .iter()
        .find(|r| crate::paths::normalize(&r.root) == normalized);
    let project = repo.and_then(|r| r.project.clone()).unwrap_or_default();
    Ok(RepoFacts {
        tests: test_commands(&gate),
        ci_commands: pb
            .lanes
            .as_ref()
            .map(|l| l.ci_commands.clone())
            .unwrap_or_default(),
        // CI that can run ci_commands: a GitHub repo holding the playbook's suite workflow.
        // CI that can run ci_commands: reachable, with the playbook's suite workflow, which takes
        // Nitely's dispatch inputs (an ordinary ci.yml does not).
        has_ci: matches!(
            crate::executor::ci_state(root, &project),
            crate::executor::CiState::Ready(_)
        ) && std::fs::read_to_string(
            root.join(".github").join("workflows").join(
                pb.lanes
                    .as_ref()
                    .map(|l| l.ci_workflow.as_str())
                    .unwrap_or("ci.yml"),
            ),
        )
        .is_ok_and(|w| w.contains("nitely_commands")),
        has_lanes: pb.lanes.is_some(),
        target: repo.map(|r| crate::proposals::Target::of(&r.name, root)),
        package: std::fs::read(root.join("package.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok()),
        budget: crate::test_budget::load(root).ok().flatten(),
        project,
    })
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
        // An old playbook's `test_sweep` still schedules the health check (NIT-558).
        let rules: Schedule = toml::from_str("test_sweep = \"nightly\"").unwrap();
        assert_eq!(rules.test_health.as_deref(), Some("nightly"));
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
    /// A test the project's gates record failing only for its own sake is flagged, by itself
    /// (NIT-565).
    #[test]
    fn the_gate_record_flags_a_test_whose_failures_were_only_its_own() {
        use crate::test_catches::TestCatches;
        let report = Report {
            version: 1,
            revision: "c".repeat(40),
            generated_at: "now".into(),
            report_only: true,
            runs: vec![],
            observations: vec![],
            tests: vec![
                serde_json::json!({"name": "roundtrip", "path": "src/store.rs", "line": 4, "hints": {}}),
                serde_json::json!({"name": "loads", "path": "src/store.rs", "line": 9, "hints": {}}),
            ],
            candidates: vec![],
            truncated: false,
            warnings: vec![],
        };
        let rec = |test: &str, catches: i64, false_alarms: i64| TestCatches {
            test: test.into(),
            step: "cargo test".into(),
            catches,
            false_alarms,
        };
        // Its neighbour once caught something; that does not vouch for it.
        let record = [
            rec("store::tests::roundtrip", 0, 3),
            rec("store::tests::loads", 1, 0),
        ];
        let flagged = report.flagged(2.0, &record);
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        assert_eq!(
            (flagged[0].test.as_str(), flagged[0].printed.as_deref()),
            ("src/store.rs::roundtrip", Some("store::tests::roundtrip"))
        );
        assert!(
            flagged[0].why[0].starts_with("false alarms"),
            "{:?}",
            flagged[0].why
        );
        assert!(
            report.flagged(2.0, &[]).is_empty(),
            "without the record, nothing flags it"
        );
    }

    /// NIT-607: a goal says what goes only as measured, and proposes only the budget the repo lacks.
    #[test]
    fn the_goal_claims_only_measured_savings_and_only_a_missing_budget() {
        assert_eq!(saving_words(&[None], &["a::slow"]), "no measured saving (no times for `a::slow`)");
        assert_eq!(saving_words(&[Some(2.5), Some(1.25)], &["a", "b"]), "up to 3.8 s of measured test time goes");
        assert_eq!(saving_words(&[Some(42.0), None], &["a", "b"]), "up to 42 s of measured test time goes (no times for `b`)");
        assert_eq!(saving_words(&[None; 4], &["a", "b", "c", "d"]), "no measured saving (4 tests have no times)");
        assert_eq!(saving_words(&[], &[]), "no measured saving");
        let scenarios = vec!["land::tests::lands".to_string(), "mcp_stale::waits".to_string()];
        let (title, toml) = budget_card(None, 2.0, &scenarios).unwrap();
        assert_eq!(title, "Set the test time budget and name the scenarios");
        assert!(toml.starts_with("test_seconds = 2\n\n[scenarios]\n\"land::tests::lands\""), "{toml}");
        let has: crate::test_budget::Budget = toml::from_str("test_seconds = 2\n[scenarios]\n\"land::tests::lands\" = \"a land\"\n").unwrap();
        let (title, toml) = budget_card(Some(&has), 2.0, &scenarios).unwrap();
        assert_eq!(title, "Name 1 more scenario in the test budget");
        assert!(!toml.contains("test_seconds") && !toml.contains("land::tests::lands") && toml.contains("mcp_stale::waits"), "{toml}");
        assert_eq!(budget_card(Some(&has), 2.0, &scenarios[..1]), None, "every scenario named: no card");
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
        let flagged = report.flagged(2.0, &[]);
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
                .flagged(2.0, &[])
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

    /// Measuring prefers what CI already wrote, then a capped CI run named before it starts, and
    /// runs on this machine's lanes only for a repo without CI.
    #[test]
    fn measure_reads_ci_logs_first_then_a_capped_ci_run_and_a_lane_only_without_ci() {
        let gate = [
            "npm run build",
            "npm test",
            "cargo test --workspace",
            "cargo clippy --workspace",
            "pytest -q",
        ]
        .map(String::from);
        let tests = test_commands(&gate);
        assert_eq!(tests, ["npm test", "cargo test --workspace", "pytest -q"]);
        // Run ids are safe in file names; only push and scheduled runs tested their commit.
        assert_ne!(
            run_id(&["ci", "7", "1", "Gate (windows-latest)"]),
            run_id(&["ci", "7", "1", "Gate (ubuntu-latest)"])
        );
        assert_eq!(
            run_id(&["ci", "7", "1", "Gate (windows-latest)"]),
            "ci-7-1-Gate--windows-latest-"
        );
        assert!(
            crate::executor::tested_its_commit("push")
                && !crate::executor::tested_its_commit("workflow_dispatch")
                && !crate::executor::tested_its_commit("pull_request")
        );
        let rev = "a".repeat(40);
        let facts = |ci_logs, ci, dispatch, lane| MeasureFacts {
            revision: &rev,
            ci_logs,
            tested_main: None,
            ci,
            tests: &tests,
            package: None,
            runs: 2,
            minutes: 20,
            dispatch,
            lane,
        };
        let ready = CiFact::Ready { workflow: true };
        let p = plan_measure(&facts(3, ready, false, false)).unwrap();
        assert_eq!((p.method, p.start), (Method::CiLog, true));
        // No logs: the CI run is shown with its cap and starts only when asked, with pytest -v
        // so CI's console says each test's outcome.
        let p = plan_measure(&facts(0, ready, false, false)).unwrap();
        assert_eq!((p.method, p.start), (Method::CiRun, false));
        assert_eq!(
            p.commands,
            [
                "npm test",
                "cargo test --workspace",
                "pytest -q --junitxml=$NITELY_JUNIT_DIR/pytest.xml"
            ]
        );
        assert!(
            p.says.contains("capped at 20 minutes") && p.says.contains("--dispatch"),
            "{}",
            p.says
        );
        assert!(plan_measure(&facts(0, ready, true, false)).unwrap().start);
        // A repo with CI never runs the suite here: not with --lane, not with CI unreachable or
        // without a suite workflow.
        assert!(plan_measure(&facts(0, ready, false, true)).is_err());
        assert!(
            plan_measure(&facts(0, CiFact::Blocked("gh is signed out"), false, true))
                .unwrap_err()
                .to_string()
                .contains("signed out")
        );
        assert!(plan_measure(&facts(0, CiFact::Ready { workflow: false }, false, true)).is_err());
        assert!(
            plan_measure(&facts(0, CiFact::Ready { workflow: false }, true, false))
                .unwrap_err()
                .to_string()
                .contains("JUnit")
        );
        // No CI: a lane, shown first, each command capped.
        let p = plan_measure(&facts(
            0,
            CiFact::Absent("the repo's origin is not on GitHub"),
            false,
            false,
        ))
        .unwrap();
        assert_eq!(
            (p.method, p.start, p.commands.len()),
            (Method::Lane, false, 3)
        );
        assert!(
            p.says.contains("not on GitHub") && p.says.contains("capped at 20 minutes in all"),
            "{}",
            p.says
        );
        assert!(
            plan_measure(&facts(0, CiFact::Absent("kept local"), false, true))
                .unwrap()
                .start
        );
        assert!(plan_measure(&facts(0, CiFact::Absent("kept local"), true, false)).is_err());
        // A lane run's cap is shared: three commands need three minutes at least.
        let tight = MeasureFacts {
            minutes: 2,
            ..facts(0, CiFact::Absent("kept local"), false, true)
        };
        assert!(plan_measure(&tight)
            .unwrap_err()
            .to_string()
            .contains("at least 3 minutes"));
        // No logs at HEAD: main's newest tested commit is read as that commit's evidence, even
        // where CI has no suite workflow to dispatch to; a dispatch still runs HEAD.
        // Another CI's config means CI is there: a lane is refused, never run.
        let repo = crate::paths::unique_temp("nitely-other-ci", "");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(other_ci(&repo).is_none());
        std::fs::write(repo.join("bitbucket-pipelines.yml"), "pipelines: {}").unwrap();
        let why = other_ci(&repo).unwrap();
        assert!(plan_measure(&facts(0, CiFact::Blocked(why), false, true))
            .unwrap_err()
            .to_string()
            .contains("Bitbucket Pipelines"));
        let main = "b".repeat(40);
        let from_main = |n, ci, dispatch| MeasureFacts {
            tested_main: Some((&main, n)),
            ..facts(0, ci, dispatch, false)
        };
        let p = plan_measure(&from_main(2, CiFact::Ready { workflow: false }, false)).unwrap();
        assert_eq!(
            (p.method, p.start, p.at.as_deref()),
            (Method::CiLog, true, Some(main.as_str()))
        );
        assert!(p.says.contains("--at bbbbbbbbbbbb"), "{}", p.says);
        assert_eq!(
            plan_measure(&from_main(2, ready, true)).unwrap().method,
            Method::CiRun
        );
        assert_eq!(
            plan_measure(&from_main(0, ready, false)).unwrap().method,
            Method::CiRun
        );
    }

    /// A JUnit file imports as the same observations the collector classifies from libtest and
    /// TAP; a cut one is a partial run.
    #[test]
    fn imported_junit_becomes_observations_and_a_cut_file_a_partial_run() {
        let xml = r#"<testsuites name="pytest" time="1.5"><testsuite name="pytest"><testcase classname="tests.test_cart" name="test_total" time="1.25"/><testcase classname="tests.test_cart" name="test_tax"><failure/></testcase></testsuite></testsuites>"#;
        let (rows, run) = import(xml, "ci-1", "abc", "CI job 7", Method::CiLog);
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
        let (rows, run) = import(
            &xml[..xml.len() - 40],
            "ci-2",
            "abc",
            "CI job 8",
            Method::CiLog,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(run["partial"], true);
    }

    /// One plan's cards are filed as written, even two that read alike: the person approved them
    /// together.
    #[test]
    fn a_plans_cards_that_read_alike_are_both_filed() {
        let mut db =
            Db::open(&crate::paths::unique_temp("nitely-sweep-alike", "").join("t.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        let card = |r: &str, module: &str| PlanItem {
            local_ref: r.into(),
            title: format!("{module}: lift whether input is valid"),
            body: format!("Lift the check out of {module}."),
            evidence: "Test map".into(),
            kind: "improvement".into(),
            priority: 3,
            touches: vec![format!("src/{module}.rs")],
            ..Default::default()
        };
        let plan = GoalPlan {
            sentence: "s".into(),
            title: "Tests".into(),
            outcome: "o".into(),
            issues: vec![card("refactor-1", "parser"), card("refactor-2", "router")],
            ..Default::default()
        };
        let proposed = db
            .propose_goal("APP", &plan, None, &Actor::Agent("codex:sweep".into()))
            .unwrap();
        let approved = db
            .approve_goal_plan(&proposed.issue.key, &Default::default(), &Actor::Human)
            .unwrap();
        assert_ne!(
            approved.filed.keys["refactor-1"],
            approved.filed.keys["refactor-2"]
        );
    }

    /// Two modules' refactor cards are filed as two issues, not one as a duplicate.
    #[test]
    fn refactor_cards_of_two_modules_are_not_duplicates() {
        let parser = refactor_title("parser", ["where a quoted token ends"].into_iter());
        let router = refactor_title("router", ["which route a path matches"].into_iter());
        fn text(t: &str) -> crate::similar::Text<'_> {
            crate::similar::Text {
                title: t,
                body: "",
                evidence: "",
            }
        }
        assert!(
            !crate::similar::compare(&text(&parser), &text(&router)).is_duplicate(),
            "{parser} / {router}"
        );
    }

    #[test]
    fn optimize_goal_has_a_card_per_module_one_deletion_card_and_needs_a_person() {
        let mut db =
            Db::open(&crate::paths::unique_temp("nitely-sweep-plan", "").join("t.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        let actor = Actor::Agent("codex:sweep".into());
        let rev = "a".repeat(40);
        let obs = |id: &str, ms: f64| serde_json::json!({"id": id, "name": id.split_once("::").unwrap().1, "outcome": "pass", "run": "ci-1", "revision": rev, "source": "CI", "method": "ci-log", "ambiguous": false, "duration_ms": ms});
        let report = Report {
            version: 1,
            revision: rev.clone(),
            generated_at: "now".into(),
            report_only: true,
            runs: vec![],
            observations: vec![
                obs("core::land::batches", 9000.0),
                obs("core::land::pushes", 4000.0),
            ],
            tests: vec![
                serde_json::json!({"name": "batches", "path": "src/land.rs", "line": 10, "hints": {}}),
                serde_json::json!({"name": "pushes", "path": "src/land.rs", "line": 40, "hints": {"git": 3}}),
                serde_json::json!({"name": "shows_title", "path": "web/page.test.tsx", "line": 7, "hints": {"ui": 1}}),
                serde_json::json!({"name": "counts", "path": "web/count.test.ts", "line": 2, "hints": {"waits": 1}}),
            ],
            truncated: false,
            warnings: vec![],
            candidates: vec![],
        };
        let class = |v: serde_json::Value| -> Classified { serde_json::from_value(v).unwrap() };
        let mut classes = vec![
            class(
                serde_json::json!({"test": "src/land.rs::batches", "class": "decision", "decision": "which changes land together", "module": "land_rules"}),
            ),
            class(
                serde_json::json!({"test": "src/land.rs::pushes", "class": "scenario", "module": "land_rules"}),
            ),
            class(
                serde_json::json!({"test": "web/page.test.tsx::shows_title", "class": "presence"}),
            ),
            class(serde_json::json!({"test": "web/count.test.ts::counts", "class": "restates"})),
        ];
        let facts = OptimizeFacts {
            slow_seconds: 2.0,
            tests: &[
                "cargo test".into(),
                "npm test".into(),
                "cargo test --manifest-path {worktree}/x/Cargo.toml".into(),
            ],
            ci_commands: &["cargo test".into()],
            has_ci: true,
            has_lanes: true,
            playbook: None,
            record: &[],
            package: None,
            budget: None,
        };
        // A deletion must name what covers the test.
        let err = report
            .propose(&mut db, "APP", None, &classes, &facts, &actor)
            .unwrap_err()
            .to_string();
        assert!(err.contains("web/count.test.ts::counts"), "{err}");
        classes[3].covered_by = Some("`web/count.test.ts::adds_up`".into());

        let planned = optimize_plan(&rev, &report.flagged(2.0, &[]), &classes, &facts).unwrap();
        let refs: Vec<(&str, &[String])> = planned
            .plan
            .issues
            .iter()
            .map(|i| (i.local_ref.as_str(), i.blocked_by.as_slice()))
            .collect();
        assert_eq!(
            refs,
            [
                ("refactor-1", &[][..]),
                ("budget", &[][..]),
                ("delete", &["refactor-1".to_string()][..])
            ]
        );
        let card = &planned.plan.issues[0];
        assert_eq!(card.title, "land_rules: lift which changes land together");
        assert!(
            card.body.contains("Keep these scenarios")
                && card.body.contains("`src/land.rs::pushes` (4.0 s)"),
            "{}",
            card.body
        );
        assert!(
            planned.plan.issues[1].body.contains("\"land::pushes\" = "),
            "the budget names the scenario as the runner prints it"
        );
        assert!(
            planned.plan.issues[2]
                .body
                .contains("covered by nothing it should keep"),
            "a presence test is covered by nothing it should keep"
        );
        // A decision's old test is on the one deletion list, gone only if its module's card landed.
        let deletion = &planned.plan.issues[2].body;
        assert!(deletion.contains("`src/land.rs::batches` (9.0 s): covered by the pure test of \"which changes land together\" from refactor-1 (land_rules), if that card landed"), "{deletion}");
        assert!(!card.body.contains("ask the person once"));
        // The budget says which test commands gates time.
        assert!(
            planned.plan.issues[1]
                .body
                .contains("`cargo test`: gates read its per-test times"),
            "{}",
            planned.plan.issues[1].body
        );
        assert_eq!(
            budget_reads("pytest -q", None),
            BudgetReads::Add("-v --durations=0 --durations-min=0, without -q".into())
        );
        assert_eq!(budget_reads("dotnet test", None), BudgetReads::Not);
        let jest = serde_json::json!({"scripts": {"test": "jest --ci"}});
        let node = serde_json::json!({"scripts": {"test": "tsc && node --test dist"}});
        assert_eq!(budget_reads("npm test", Some(&jest)), BudgetReads::Not);
        assert_eq!(budget_reads("npm test", Some(&node)), BudgetReads::Timed);
        // A script that builds with npm first is read part by part; a script calling itself stops.
        let built = serde_json::json!({"scripts": {"test": "npm run build && node --test dist/tests", "build": "tsc", "loop": "npm run loop"}});
        assert_eq!(budget_reads("npm test", Some(&built)), BudgetReads::Timed);
        assert_eq!(budget_reads("npm run loop", Some(&built)), BudgetReads::Not);
        // A quoted pipe is an argument, not a second command (NIT-601 review).
        assert_eq!(command_parts("go test ./... -run 'TestFoo|TestBar' -v && echo \"a;b\""), ["go test ./... -run 'TestFoo|TestBar' -v", "echo \"a;b\""]);
        assert_eq!(budget_reads("go test ./... -run 'TestFoo|TestBar' -v", None), budget_reads("go test ./... -v", None));
        assert_eq!(command_parts(r"echo it\'s-ready && go test ./... -v"), [r"echo it\'s-ready", "go test ./... -v"], "an escaped quote opens nothing");
        // A failing scenario gets a repair card even when nothing else is planned for it.
        let mut broken = report.clone();
        broken.observations = vec![obs("core::land::pushes", 4000.0)];
        broken.observations[0]["outcome"] = "fail".into();
        let only = [class(
            serde_json::json!({"test": "src/land.rs::pushes", "class": "scenario", "module": "land_rules"}),
        )];
        let flagged_broken: Vec<Flagged> = broken
            .flagged(2.0, &[])
            .into_iter()
            .filter(|f| f.test == "src/land.rs::pushes")
            .collect();
        let repair = optimize_plan(&rev, &flagged_broken, &only, &facts).unwrap();
        assert_eq!(repair.plan.issues[0].local_ref, "repair");
        assert!(
            repair.plan.issues[0]
                .body
                .contains("`src/land.rs::pushes` (slow: 4.0 s median by ci-log; failing)"),
            "{}",
            repair.plan.issues[0].body
        );
        assert_eq!(
            planned.ci_commands,
            Some(vec!["cargo test".to_string(), "npm test".to_string()])
        );

        let proposed = report
            .propose(&mut db, "APP", None, &classes, &facts, &actor)
            .unwrap();
        assert!(
            proposed.playbook.is_none(),
            "no playbook proposal without the repo's playbook"
        );
        let key = proposed.goal.issue.key;
        let edit = crate::goal_plans::PlanEdit {
            bodies: std::collections::BTreeMap::from([(
                "refactor-1".into(),
                "Lift only the batching decision; keep the push scenario.".into(),
            )]),
            ..Default::default()
        };
        assert!(db.approve_goal_plan(&key, &edit, &actor).is_err());
        let approved = db.approve_goal_plan(&key, &edit, &Actor::Human).unwrap();
        let filed = db
            .issue(&approved.filed.keys["refactor-1"])
            .unwrap()
            .unwrap();
        assert!(filed.body.starts_with("Lift only the batching decision"));
    }
}
