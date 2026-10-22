//! Test evidence from any runner (NIT-553): one model of a run and its tests, read from JUnit XML,
//! the format most runners can write (pytest `--junitxml`, jest and vitest junit reporters, Go's
//! gotestsum, cargo-nextest, .NET's JUnit logger, Maven Surefire, Gradle), with Rust's libtest
//! output and node:test's TAP as two more adapters into the same model. Plain functions over the
//! text: reading a file or a CI log is the caller's.
//!
//! A test this reads is passed, failed, skipped or unknown. A file it cannot read whole leaves its
//! run incomplete: the tests it did read stand, and every test it did not is unknown, never passing.

use serde::Serialize;

/// The format of a piece of evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Junit,
    Libtest,
    Tap,
    /// pytest's console with `-v`.
    Pytest,
    /// `go test -v`.
    Gotest,
    /// `dotnet test --logger trx`.
    Trx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Fail,
    Skip,
}

/// One test as one run saw it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Observation {
    /// `suite::name`, or the name alone when the runner gives no suite.
    pub id: String,
    pub name: String,
    /// The test's group as its runner names it: a JUnit classname, a Rust test binary.
    pub suite: Option<String>,
    /// The source file, when the runner says.
    pub file: Option<String>,
    pub outcome: Outcome,
    pub seconds: Option<f64>,
    /// Failed and then passed in the same run (Surefire's flakyFailure, a rerun).
    pub flaky: bool,
}

/// What one piece of evidence holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub format: Option<Format>,
    /// The runner, when the evidence names it (`<testsuites name="pytest">`), else its format's:
    /// `rust` for libtest, `node` for TAP.
    pub runner: Option<String>,
    /// Whether the evidence was read whole: a cut or malformed file, or a binary that never
    /// reported its result, is not.
    pub complete: bool,
    /// The whole run's time, when the evidence says.
    pub seconds: Option<f64>,
    pub tests: Vec<Observation>,
    pub warnings: Vec<String>,
}

impl Run {
    fn unknown(why: &str) -> Run {
        Run {
            format: None,
            runner: None,
            complete: false,
            seconds: None,
            tests: vec![],
            warnings: vec![why.to_string()],
        }
    }
}

/// Which formats `text` holds: JUnit XML alone, or libtest and TAP, both in one log when a gate
/// ran `cargo test` and then `npm test`.
pub fn detect(text: &str) -> Vec<Format> {
    let head = text.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with('<') {
        return vec![Format::Junit];
    }
    let lines = || text.lines().map(|l| l.trim());
    let mut found = Vec::new();
    if lines().any(|l| {
        (l.starts_with("test ") && l.contains(" ... "))
            // A binary that started and died before its first result line.
            || running_line(l)
            || l.contains("\"type\": \"test\"")
            || l.contains("\"type\":\"test\"")
    }) {
        found.push(Format::Libtest);
    }
    if lines().any(|l| {
        l.starts_with("TAP version") || l.starts_with("ok 1 - ") || l.starts_with("not ok 1 - ")
    }) {
        found.push(Format::Tap);
    }
    static PYTEST: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pytest = PYTEST.get_or_init(|| {
        regex::Regex::new(
            r"^\S+::\S.* (?:PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS)(?:\s+\[\s*\d+%\])?$",
        )
        .unwrap()
    });
    if lines().any(|l| pytest.is_match(l)) {
        found.push(Format::Pytest);
    }
    if lines().any(|l| {
        l.starts_with("--- PASS: ") || l.starts_with("--- FAIL: ") || l.starts_with("--- SKIP: ")
    }) {
        found.push(Format::Gotest);
    }
    found
}

/// libtest's `running N tests` (or `running 1 test`), exactly.
fn running_line(l: &str) -> bool {
    l.strip_prefix("running ")
        .and_then(|rest| {
            rest.strip_suffix(" tests")
                .or_else(|| rest.strip_suffix(" test"))
        })
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The tests in `text`, in whichever format it is. A log with both libtest and TAP reads as one
/// run of both, complete only if each part is.
/// A CI job's log (`gh run view --log`) reads the same: its prefixes and colours are dropped first.
pub fn read(text: &str) -> Run {
    let plain: String;
    let text = if detect(text) == [Format::Junit] {
        text
    } else {
        plain = text
            .lines()
            .map(crate::test_budget::plain_line)
            .collect::<Vec<_>>()
            .join("\n");
        &plain
    };
    let mut parts = detect(text).into_iter().map(|f| match f {
        Format::Junit if text.contains("<TestRun") => trx(text),
        Format::Junit => junit(text),
        Format::Trx => trx(text),
        Format::Libtest => libtest(text),
        Format::Tap => tap(text),
        Format::Pytest => pytest(text),
        Format::Gotest => gotest(text),
    });
    let Some(mut run) = parts.next() else {
        return Run::unknown(
            "not JUnit XML, libtest, TAP, pytest -v or go test -v: its tests are unknown",
        );
    };
    for part in parts {
        let runners = [run.runner.take(), part.runner];
        run.runner = Some(runners.into_iter().flatten().collect::<Vec<_>>().join("+"));
        run.complete &= part.complete;
        run.seconds = run.seconds.zip(part.seconds).map(|(a, b)| a + b);
        run.tests.extend(part.tests);
        run.warnings.extend(part.warnings);
    }
    run
}

fn id(suite: Option<&str>, name: &str) -> String {
    match suite.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}::{name}"),
        None => name.to_string(),
    }
}

/// A number of seconds as JUnit writes it: "1.25", or with a thousands separator ("1,234.5").
fn seconds(text: &str) -> Option<f64> {
    text.trim()
        .replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
}

/// JUnit XML: `<testsuites>`, `<testsuite>`s (nested or not), and their `<testcase>`s with a
/// `<failure>`, `<error>`, `<skipped>`, or Surefire's `<flakyFailure>`/`<rerunFailure>`.
pub fn junit(text: &str) -> Run {
    use quick_xml::events::{BytesStart, Event};
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let attr = |e: &BytesStart, key: &str| -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == key)
            .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
            .map(|v| v.into_owned())
    };
    let mut run = Run {
        format: Some(Format::Junit),
        runner: None,
        complete: true,
        seconds: None,
        tests: vec![],
        warnings: vec![],
    };
    // The open suites' names and files, outermost first, and the test case being read.
    let mut suites: Vec<(Option<String>, Option<String>)> = Vec::new();
    let mut case: Option<Observation> = None;
    // The run's time: `<testsuites time>` when it is there, else the sum of the outermost suites'.
    let (mut root_time, mut suite_time) = (None, None::<f64>);
    // `<testsuites>` still open (a file cut between suites), and the names that may say the runner.
    let (mut root_open, mut root_name, mut suite_name) = (false, None, None);
    let case_from = |e: &BytesStart, suites: &[(Option<String>, Option<String>)]| {
        let name = attr(e, "name").unwrap_or_default();
        let suite = attr(e, "classname").or_else(|| suites.last().and_then(|s| s.0.clone()));
        let file = attr(e, "file").or_else(|| suites.iter().rev().find_map(|s| s.1.clone()));
        Observation {
            id: id(suite.as_deref(), &name),
            name,
            suite,
            file,
            outcome: Outcome::Pass,
            seconds: attr(e, "time").as_deref().and_then(seconds),
            flaky: false,
        }
    };
    loop {
        let (e, empty) = match reader.read_event() {
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::End(e)) => {
                match e.local_name().as_ref() {
                    "testcase" => run.tests.extend(case.take()),
                    "testsuite" => {
                        suites.pop();
                    }
                    "testsuites" => root_open = false,
                    _ => {}
                }
                continue;
            }
            Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(e) => {
                run.complete = false;
                run.warnings.push(format!(
                    "the XML stops being readable at byte {}: {e}; the tests after it are unknown",
                    reader.buffer_position()
                ));
                break;
            }
        };
        match e.local_name().as_ref() {
            "testsuites" => {
                root_time = attr(&e, "time").as_deref().and_then(seconds);
                root_name = attr(&e, "name");
                root_open = !empty;
            }
            "testsuite" => {
                if suites.is_empty() {
                    suite_name = suite_name.or_else(|| attr(&e, "name"));
                    if let Some(s) = attr(&e, "time").as_deref().and_then(seconds) {
                        suite_time = Some(suite_time.unwrap_or(0.0) + s);
                    }
                }
                if !empty {
                    suites.push((
                        attr(&e, "name"),
                        attr(&e, "file").or_else(|| attr(&e, "filepath")),
                    ));
                }
            }
            "testcase" if empty => run.tests.push(case_from(&e, &suites)),
            "testcase" => case = Some(case_from(&e, &suites)),
            "failure" | "error" => case.iter_mut().for_each(|c| c.outcome = Outcome::Fail),
            "skipped" => case.iter_mut().for_each(|c| c.outcome = Outcome::Skip),
            "flakyFailure" | "flakyError" | "rerunFailure" | "rerunError" => {
                case.iter_mut().for_each(|c| c.flaky = true)
            }
            _ => {}
        }
    }
    run.seconds = root_time.or(suite_time);
    run.runner = Some(runner_named([root_name, suite_name]));
    if case.is_some() || !suites.is_empty() || root_open {
        run.complete = false;
        run.warnings.push("the XML ends inside a test suite: the file was cut, and the tests after it are unknown".into());
    }
    if run.tests.is_empty() {
        run.complete = false;
        run.warnings
            .push("no test cases in the XML: its tests are unknown".into());
    }
    run
}

/// The runner a JUnit file names: `<testsuites name>` or the first `<testsuite name>` when it is
/// one of the runners that sign their files (pytest's `pytest`, jest's `jest tests`,
/// cargo-nextest's `nextest-run`, ...), else plain `junit`: Surefire, Gradle and .NET name suites
/// after classes, and the evidence's descriptor can say which it was.
fn runner_named(names: [Option<String>; 2]) -> String {
    const KNOWN: &[&str] = &[
        "pytest",
        "jest",
        "vitest",
        "nextest",
        "gotestsum",
        "mocha",
        "karma",
        "playwright",
        "cypress",
        "phpunit",
        "rspec",
    ];
    names
        .into_iter()
        .flatten()
        .find_map(|n| {
            let lower = n.to_lowercase();
            KNOWN
                .iter()
                .find(|k| lower.starts_with(*k))
                .map(|k| k.to_string())
        })
        .unwrap_or_else(|| "junit".into())
}

/// Rust's libtest, as `cargo test` prints it: `test name ... ok` (with ` <1.234s>` under
/// `--report-time`) under each `Running … (…/deps/<binary>-<hash>)`, or its `--format json` lines.
/// A binary that started (`running N tests`) and never printed its `test result:` was cut short.
pub fn libtest(text: &str) -> Run {
    static LINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RUNNING: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let line = LINE.get_or_init(|| {
        regex::Regex::new(
            r"^test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored)(?:, [^<]*)?(?: <(\d+(?:\.\d+)?)s>)?\s*$",
        )
        .unwrap()
    });
    let running = RUNNING.get_or_init(|| {
        regex::Regex::new(r"^\s*Running .*\((?:.*[/\\])?([^/\\()]+?)-[0-9a-f]{6,}(?:\.exe)?\)\s*$")
            .unwrap()
    });
    let mut run = Run {
        format: Some(Format::Libtest),
        runner: Some("rust".into()),
        complete: true,
        seconds: None,
        tests: vec![],
        warnings: vec![],
    };
    let mut suite: Option<String> = None;
    let mut open = 0usize;
    let mut total = 0.0;
    let mut timed = false;
    for raw in text.lines() {
        let l = raw.trim_end();
        if let Some(c) = running.captures(l) {
            suite = Some(c[1].to_string());
        } else if running_line(l) {
            open += 1;
        } else if let Some(rest) = l.strip_prefix("test result: ") {
            open = open.saturating_sub(1);
            if let Some(s) = rest
                .rsplit_once("finished in ")
                .and_then(|(_, s)| s.strip_suffix('s'))
                .and_then(seconds)
            {
                total += s;
                timed = true;
            }
        } else if let Some(c) = line.captures(l) {
            let outcome = match &c[2] {
                "ok" => Outcome::Pass,
                "FAILED" => Outcome::Fail,
                _ => Outcome::Skip,
            };
            let name = c[1].to_string();
            run.tests.push(Observation {
                id: id(suite.as_deref(), &name),
                name,
                suite: suite.clone(),
                file: None,
                outcome,
                seconds: c.get(3).and_then(|m| seconds(m.as_str())),
                flaky: false,
            });
        } else if l.starts_with('{') {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
                continue;
            };
            match (v["type"].as_str(), v["event"].as_str()) {
                (Some("suite"), Some("started")) => open += 1,
                (Some("suite"), Some(_)) => {
                    open = open.saturating_sub(1);
                    if let Some(s) = v["exec_time"].as_f64() {
                        total += s;
                        timed = true;
                    }
                }
                (Some("test"), Some(event @ ("ok" | "failed" | "ignored"))) => {
                    let name = v["name"].as_str().unwrap_or_default().to_string();
                    let outcome = match event {
                        "ok" => Outcome::Pass,
                        "failed" => Outcome::Fail,
                        _ => Outcome::Skip,
                    };
                    run.tests.push(Observation {
                        id: id(suite.as_deref(), &name),
                        name,
                        suite: suite.clone(),
                        file: None,
                        outcome,
                        seconds: v["exec_time"].as_f64(),
                        flaky: false,
                    });
                }
                _ => {}
            }
        }
    }
    run.seconds = timed.then_some(total);
    if open > 0 {
        run.complete = false;
        run.warnings.push(format!("{open} test binary(ies) started and never printed a result: their remaining tests are unknown"));
    }
    if run.tests.is_empty() {
        run.complete = false;
        run.warnings
            .push("no test lines: per-test outcomes are unknown".into());
    }
    run
}

/// node:test's TAP (and other TAP producers): `ok N - name` / `not ok N - name`, `# SKIP` and
/// `# TODO`, YAML `duration_ms`, and subtests indented under their parent. A test whose line comes
/// right after a deeper one's is the parent of those and is not counted as a test of its own,
/// unless it failed while they did not show it (its own hook threw): then its failure is one. The
/// plan line (`1..N`) at the top level says how many to expect. Test ids are the names alone, as
/// the collector's `parseLog` writes them.
pub fn tap(text: &str) -> Run {
    static OK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let ok = OK.get_or_init(|| {
        regex::Regex::new(r"^( *)(not )?ok \d+(?: - (.+?))?(?: # (SKIP|TODO|skip|todo)\b.*)?$")
            .unwrap()
    });
    let mut run = Run {
        format: Some(Format::Tap),
        runner: Some("node".into()),
        complete: true,
        seconds: None,
        tests: vec![],
        warnings: vec![],
    };
    let mut last_indent: Option<usize> = None;
    let mut last_leaf: Option<usize> = None;
    let mut planned: Option<usize> = None;
    let mut top = 0usize;
    for raw in text.lines() {
        let l = raw.trim_end();
        if let Some(c) = ok.captures(l) {
            let indent = c[1].len();
            if indent == 0 {
                top += 1;
            }
            let parent = last_indent.is_some_and(|d| d > indent);
            last_indent = Some(indent);
            last_leaf = None;
            let failed = c.get(2).is_some() && c.get(4).is_none();
            if parent && !failed {
                continue;
            }
            let name = c
                .get(3)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let outcome = if c.get(4).is_some() {
                Outcome::Skip
            } else if c.get(2).is_some() {
                Outcome::Fail
            } else {
                Outcome::Pass
            };
            run.tests.push(Observation {
                id: name.clone(),
                name,
                suite: None,
                file: None,
                outcome,
                seconds: None,
                flaky: false,
            });
            last_leaf = Some(run.tests.len() - 1);
        } else if let Some(ms) = l
            .trim_start()
            .strip_prefix("duration_ms:")
            .and_then(seconds)
        {
            if let Some(i) = last_leaf.take() {
                run.tests[i].seconds = Some(ms / 1000.0);
            }
        } else if let Some(ms) = l.strip_prefix("# duration_ms ").and_then(seconds) {
            run.seconds = Some(ms / 1000.0);
        } else if let Some(n) = l
            .strip_prefix("1..")
            .and_then(|n| n.trim().parse::<usize>().ok())
        {
            planned = Some(n);
        } else if l.starts_with("Bail out!") {
            run.complete = false;
            run.warnings
                .push("the run bailed out: the tests after it are unknown".into());
        }
    }
    match planned {
        Some(n) if n != top => {
            run.complete = false;
            run.warnings.push(format!(
                "the plan says {n} tests and {top} reported: the missing ones are unknown"
            ));
        }
        None => {
            run.complete = false;
            run.warnings.push(
                "no plan line: the run may have been cut, and what it did not report is unknown"
                    .into(),
            );
        }
        _ => {}
    }
    if run.tests.is_empty() {
        run.complete = false;
        run.warnings
            .push("no test lines: per-test outcomes are unknown".into());
    }
    run
}

/// pytest's console with `-v`: `path::test PASSED [ 50%]`, FAILED, ERROR (a fixture failed),
/// SKIPPED, XFAIL (expected to fail: skipped) and XPASS, ended by its `= N passed in 1.2s =` line;
/// a run without that line was cut. `--durations=0` adds each test's setup, call and teardown
/// times (`0.52s call     path::test`), summed into its time.
pub fn pytest(text: &str) -> Run {
    static LINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SUMMARY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let line = LINE.get_or_init(|| {
        regex::Regex::new(
            r"^(\S+::\S.*?) (PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS)(?:\s+\[\s*\d+%\])?\s*$",
        )
        .unwrap()
    });
    let summary = SUMMARY.get_or_init(|| regex::Regex::new(r"^=+ .*\b(?:passed|failed|error|errors|skipped|deselected|no tests ran)\b.* in (\d+(?:\.\d+)?)s(?: \(.*\))? =+$").unwrap());
    let mut run = Run {
        format: Some(Format::Pytest),
        runner: Some("pytest".into()),
        complete: false,
        seconds: None,
        tests: vec![],
        warnings: vec![],
    };
    static DURATION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let duration = DURATION.get_or_init(|| {
        regex::Regex::new(r"^(\d+(?:\.\d+)?)s (?:setup|call|teardown)\s+(\S+::\S.*?)\s*$").unwrap()
    });
    let mut times: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for l in text.lines().map(str::trim_end) {
        if let Some(c) = duration.captures(l) {
            *times.entry(c[2].to_string()).or_default() += seconds(&c[1]).unwrap_or(0.0);
        } else if let Some(c) = line.captures(l) {
            let id = c[1].to_string();
            let (file, name) = match id.split_once("::") {
                Some((file, name)) => (Some(file.to_string()), name.to_string()),
                None => (None, id.clone()),
            };
            let outcome = match &c[2] {
                "PASSED" | "XPASS" => Outcome::Pass,
                "FAILED" | "ERROR" => Outcome::Fail,
                _ => Outcome::Skip,
            };
            // A test's phases report under one id (PASSED, then ERROR at teardown): one test,
            // failed if any phase failed.
            if let Some(seen) = run.tests.iter_mut().find(|t| t.id == id) {
                if outcome == Outcome::Fail {
                    seen.outcome = Outcome::Fail;
                }
                continue;
            }
            run.tests.push(Observation {
                id,
                name,
                suite: file.clone(),
                file,
                outcome,
                seconds: None,
                flaky: false,
            });
        } else if let Some(c) = summary.captures(l) {
            run.complete = true;
            run.seconds = seconds(&c[1]);
        }
    }
    for t in run.tests.iter_mut() {
        t.seconds = times.get(&t.id).copied();
    }
    if !run.complete {
        run.warnings.push(
            "no pytest summary line: the run was cut, and the tests after it are unknown".into(),
        );
    }
    if run.tests.is_empty() {
        run.complete = false;
        run.warnings.push(
            "no test lines (pytest prints them with -v): per-test outcomes are unknown".into(),
        );
    }
    run
}

/// `go test -v`: `--- PASS: TestX (0.01s)`, FAIL and SKIP, subtests indented under their test.
/// A test that started (`=== RUN`) and never reported (a panic, a timeout) leaves the run cut.
pub fn gotest(text: &str) -> Run {
    static RESULT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| {
        regex::Regex::new(r"^\s*--- (PASS|FAIL|SKIP): (\S+) \((\d+(?:\.\d+)?)s\)\s*$").unwrap()
    });
    let mut run = Run {
        format: Some(Format::Gotest),
        runner: Some("go".into()),
        complete: true,
        seconds: None,
        tests: vec![],
        warnings: vec![],
    };
    let mut started = std::collections::BTreeSet::new();
    for l in text.lines().map(str::trim_end) {
        if let Some(name) = l.strip_prefix("=== RUN").map(str::trim) {
            started.insert(name.to_string());
        } else if let Some(c) = result.captures(l) {
            let name = c[2].to_string();
            started.remove(&name);
            let outcome = match &c[1] {
                "PASS" => Outcome::Pass,
                "FAIL" => Outcome::Fail,
                _ => Outcome::Skip,
            };
            run.tests.push(Observation {
                id: name.clone(),
                name,
                suite: None,
                file: None,
                outcome,
                seconds: seconds(&c[3]),
                flaky: false,
            });
        }
    }
    if !started.is_empty() {
        run.complete = false;
        run.warnings.push(format!(
            "{} test(s) started and never reported (a panic or a timeout): {}",
            started.len(),
            started.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if run.tests.is_empty() {
        run.complete = false;
        run.warnings.push(
            "no test lines (go test prints them with -v): per-test outcomes are unknown".into(),
        );
    }
    run
}

/// The command to dispatch for `command` so its console says each test's outcome, or why CI's
/// console cannot (NIT-554): cargo and node:test print libtest and TAP already, pytest and
/// `go test` do with `-v`; other runners only write JUnit files, which CI must upload as an
/// artifact for `measure` to read.
pub fn console_command(command: &str) -> std::result::Result<String, String> {
    if ["{main}", "{worktree}", "{base}", "{jobs}"]
        .iter()
        .any(|p| command.contains(p))
    {
        return Err(format!(
            "`{command}` names a local path or share and runs only in a lane"
        ));
    }
    readable_command(command)
}

/// `command` with what makes its console say each test's outcome and time, wherever it runs (a
/// lane as well as CI), or why its console cannot.
pub fn readable_command(command: &str) -> std::result::Result<String, String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let has = |w: &str| words.contains(&w);
    if words.first() == Some(&"cargo") && words.get(1) == Some(&"test") {
        return Ok(command.to_string());
    }
    if has("pytest") {
        // -q and -v cancel out: drop the quiet flags so each test gets its line.
        let kept: Vec<&str> = words
            .iter()
            .copied()
            .filter(|w| !matches!(*w, "-q" | "-qq" | "--quiet"))
            .collect();
        let verbose = kept
            .iter()
            .any(|w| matches!(*w, "-v" | "-vv" | "--verbose"));
        let timed = kept.iter().any(|w| w.starts_with("--durations"));
        let mut out = kept.join(" ");
        if !verbose {
            out.push_str(" -v");
        }
        if !timed {
            // Every test's time, not only the slowest few.
            out.push_str(" --durations=0 --durations-min=0");
        }
        return Ok(out);
    }
    if words.first() == Some(&"go") && words.get(1) == Some(&"test") {
        // -count=1: a measurement runs the tests, never replays Go's cached results.
        let mut out = command.to_string();
        if !has("-v") {
            out.push_str(" -v");
        }
        if !words.iter().any(|w| w.starts_with("-count")) {
            out.push_str(" -count=1");
        }
        return Ok(out);
    }
    if words.first().is_some_and(|w| *w == "node") && has("--test")
        || matches!(words.as_slice(), ["npm", "test", ..] | ["npm", "run", ..])
    {
        // node:test prints TAP when its output is not a terminal, as in CI; a script running
        // another runner shows up as a log with no tests, which `measure` reports.
        return Ok(command.to_string());
    }
    Err(format!("CI's console would not say each test's outcome for `{command}`: have CI write JUnit XML and upload it as an artifact named for tests, and `measure` reads it from CI's finished runs"))
}

/// `dotnet test --logger trx`'s report: each `<UnitTestResult testName duration outcome>`, its
/// `testName` the class-qualified method, ended by `<ResultSummary>`; a file without it was cut.
pub fn trx(text: &str) -> Run {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut run = Run {
        format: Some(Format::Trx),
        runner: Some("dotnet".into()),
        complete: false,
        seconds: None,
        tests: vec![],
        warnings: vec![],
    };
    loop {
        let e = match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(e) => {
                run.warnings.push(format!(
                    "the TRX stops being readable at byte {}: {e}; the tests after it are unknown",
                    reader.buffer_position()
                ));
                run.complete = false;
                return run;
            }
        };
        let attr = |key: &str| -> Option<String> {
            e.attributes()
                .flatten()
                .find(|a| a.key.as_ref() == key)
                .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                .map(|v| v.into_owned())
        };
        match e.local_name().as_ref() {
            "UnitTestResult" => {
                let id = attr("testName").unwrap_or_default();
                let (suite, name) = match id.rsplit_once('.') {
                    Some((suite, name)) => (Some(suite.to_string()), name.to_string()),
                    None => (None, id.clone()),
                };
                let outcome = match attr("outcome").as_deref() {
                    Some("Passed") => Outcome::Pass,
                    Some("Failed" | "Error" | "Timeout" | "Aborted") => Outcome::Fail,
                    _ => Outcome::Skip,
                };
                run.tests.push(Observation {
                    id,
                    name,
                    suite,
                    file: None,
                    outcome,
                    seconds: attr("duration").as_deref().and_then(trx_seconds),
                    flaky: false,
                });
            }
            "ResultSummary" => run.complete = true,
            _ => {}
        }
    }
    if !run.complete {
        run.warnings.push(
            "no ResultSummary in the TRX: the file was cut, and the tests after it are unknown"
                .into(),
        );
    }
    if run.tests.is_empty() {
        run.complete = false;
        run.warnings
            .push("no test results in the TRX: its tests are unknown".into());
    }
    run
}

/// A TRX duration, `hh:mm:ss.fffffff`, in seconds.
fn trx_seconds(d: &str) -> Option<f64> {
    let mut parts = d.split(':');
    let (h, m, s) = (
        parts.next()?.parse::<f64>().ok()?,
        parts.next()?.parse::<f64>().ok()?,
        parts.next()?.parse::<f64>().ok()?,
    );
    Some(h * 3600.0 + m * 60.0 + s)
}

/// Where a dispatched command's machine-readable report lands: the suite workflow sets
/// `$NITELY_JUNIT_DIR` and uploads it with Maven's and Gradle's own report folders (NIT-567).
pub const JUNIT_DIR: &str = "$NITELY_JUNIT_DIR";

/// `command` as dispatched to CI so it writes a report `measure` reads (JUnit XML or TRX), for
/// runners whose console says too little: pytest, vitest, jest (with jest-junit), dotnet, Maven
/// and Gradle (their default reports). `npm test` and `npm run <script>` are resolved through
/// `package` (package.json) to the runner their script calls. None when the command has no such
/// report, with why.
pub fn report_command(
    command: &str,
    package: Option<&serde_json::Value>,
) -> std::result::Result<String, String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let script = match words.as_slice() {
        ["npm", "test" | "t", ..] => Some("test"),
        ["npm", "run" | "run-script", name, ..] => Some(*name),
        _ => None,
    };
    if let Some(name) = script {
        let body = package
            .and_then(|p| p["scripts"][name].as_str())
            .ok_or_else(|| format!("`{command}`: package.json has no script `{name}`"))?;
        let (env, flags) = report_flags(body, package)?;
        if flags.is_empty() {
            return Ok(format!("{env}{command}"));
        }
        // npm passes what follows `--` to the script's runner.
        let sep = if words.contains(&"--") { "" } else { " --" };
        return Ok(format!("{env}{command}{sep} {flags}"));
    }
    let (env, flags) = report_flags(command, package)?;
    Ok(if flags.is_empty() {
        format!("{env}{command}")
    } else {
        format!("{env}{command} {flags}")
    })
}

/// The environment prefix and flags that make the runner in `command` write its report into
/// `$NITELY_JUNIT_DIR`.
fn report_flags(
    command: &str,
    package: Option<&serde_json::Value>,
) -> std::result::Result<(String, String), String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let has = |w: &str| {
        words
            .iter()
            .any(|x| *x == w || x.ends_with(&format!("/{w}")))
    };
    let depends = |dep: &str| {
        package.is_some_and(|p| {
            ["dependencies", "devDependencies"]
                .iter()
                .any(|k| p[k][dep].is_string())
        })
    };
    if has("vitest") {
        return Ok((
            String::new(),
            format!(
                "--reporter=default --reporter=junit --outputFile.junit={JUNIT_DIR}/vitest.xml"
            ),
        ));
    }
    if has("jest") {
        if !depends("jest-junit") {
            return Err(format!("`{command}` runs jest, which writes JUnit only with jest-junit: add it as a dev dependency"));
        }
        return Ok((
            format!("JEST_JUNIT_OUTPUT_DIR={JUNIT_DIR} JEST_JUNIT_OUTPUT_NAME=jest.xml "),
            "--reporters=default --reporters=jest-junit".into(),
        ));
    }
    if has("pytest") {
        return Ok((String::new(), format!("--junitxml={JUNIT_DIR}/pytest.xml")));
    }
    if words.first() == Some(&"dotnet") && words.get(1) == Some(&"test") {
        return Ok((
            String::new(),
            format!("--logger trx --results-directory {JUNIT_DIR}"),
        ));
    }
    // Maven Surefire and Gradle write JUnit XML into their own report folders by default.
    if (has("mvn") || has("mvnw"))
        && words
            .iter()
            .any(|w| matches!(*w, "test" | "verify" | "install" | "package"))
        || (has("gradle") || has("gradlew"))
            && words
                .iter()
                .any(|w| w.ends_with("test") || *w == "check" || *w == "build")
    {
        return Ok((String::new(), String::new()));
    }
    Err(format!(
        "`{command}`: no report Nitely reads is known for its runner"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(run: &Run) -> Vec<(String, Outcome, Option<f64>)> {
        run.tests
            .iter()
            .map(|t| (t.id.clone(), t.outcome, t.seconds))
            .collect()
    }

    /// One small file from each runner that writes JUnit, as each writes it: every test's id,
    /// outcome and time is read the same way whatever wrote it.
    #[test]
    fn junit_from_each_runner_reads_into_one_model() {
        let pytest = r#"<?xml version="1.0" encoding="utf-8"?><testsuites><testsuite name="pytest" errors="0" failures="1" skipped="1" tests="3" time="1.204" timestamp="2026-10-21T10:00:00" hostname="ci"><testcase classname="tests.test_cart" name="test_total" time="0.011" /><testcase classname="tests.test_cart" name="test_tax" time="1.010"><failure message="assert 3 == 4">tests/test_cart.py:12: AssertionError</failure></testcase><testcase classname="tests.test_cart" name="test_slow" time="0.000"><skipped type="pytest.skip" message="slow">skip</skipped></testcase></testsuite></testsuites>"#;
        let r = junit(pytest);
        assert!(r.complete, "{:?}", r.warnings);
        assert_eq!(r.seconds, Some(1.204));
        // pytest signs its one <testsuite>, not <testsuites>.
        assert_eq!(r.runner.as_deref(), Some("pytest"));
        assert_eq!(
            summary(&r),
            [
                (
                    "tests.test_cart::test_total".to_string(),
                    Outcome::Pass,
                    Some(0.011)
                ),
                (
                    "tests.test_cart::test_tax".into(),
                    Outcome::Fail,
                    Some(1.01)
                ),
                (
                    "tests.test_cart::test_slow".into(),
                    Outcome::Skip,
                    Some(0.0)
                )
            ]
        );

        // jest-junit: suites named by describe, a file attribute, times on cases.
        let jest = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="jest tests" tests="2" failures="0" errors="0" time="2.5">
  <testsuite name="Cart" errors="0" failures="0" skipped="0" timestamp="2026-10-21T10:00:00" time="2.4" tests="2">
    <testcase classname="Cart adds an item" name="Cart adds an item" time="0.004" file="src/cart.test.ts">
    </testcase>
    <testcase classname="Cart shows the total" name="Cart shows the total" time="2.3" file="src/cart.test.ts">
    </testcase>
  </testsuite>
</testsuites>"#;
        let r = junit(jest);
        assert_eq!(r.tests[1].file.as_deref(), Some("src/cart.test.ts"));
        assert_eq!(r.tests[1].seconds, Some(2.3));
        assert_eq!(r.seconds, Some(2.5));

        // vitest: the classname is the file.
        let vitest = r#"<?xml version="1.0" encoding="UTF-8" ?><testsuites name="vitest tests" tests="1" failures="1" errors="0" time="0.3"><testsuite name="src/math.test.ts" timestamp="2026-10-21T10:00:00" hostname="ci" tests="1" failures="1" errors="0" skipped="0" time="0.2"><testcase classname="src/math.test.ts" name="adds &gt; carries" time="0.1"><failure message="expected 2 to be 3" type="AssertionError">AssertionError: expected 2 to be 3</failure></testcase></testsuite></testsuites>"#;
        assert_eq!(
            summary(&junit(vitest)),
            [(
                "src/math.test.ts::adds > carries".to_string(),
                Outcome::Fail,
                Some(0.1)
            )]
        );

        // gotestsum: one suite per package, classname the package.
        let go = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites tests="2" failures="0" errors="0" time="0.812">
	<testsuite tests="2" failures="0" time="0.810" name="example.com/shop/cart" timestamp="2026-10-21T10:00:00Z">
		<properties><property name="go.version" value="go1.23.0"></property></properties>
		<testcase classname="example.com/shop/cart" name="TestTotal" time="0.800"></testcase>
		<testcase classname="example.com/shop/cart" name="TestTotal/empty" time="0.000"></testcase>
	</testsuite>
</testsuites>"#;
        let r = junit(go);
        assert_eq!(r.tests.len(), 2);
        assert_eq!(r.tests[0].id, "example.com/shop/cart::TestTotal");

        // cargo-nextest: a suite per binary, retries as flakyFailure.
        let nextest = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="2" failures="0" errors="0" uuid="u" timestamp="2026-10-21T10:00:00Z" time="3.1">
    <testsuite name="shop::lib" tests="2" disabled="0" errors="0" failures="0">
        <testcase name="cart::tests::adds" classname="shop::lib" timestamp="2026-10-21T10:00:00Z" time="0.010">
        </testcase>
        <testcase name="cart::tests::flaky" classname="shop::lib" timestamp="2026-10-21T10:00:00Z" time="3.000">
            <flakyFailure timestamp="2026-10-21T10:00:01Z" time="1.0" type="test failure">thread panicked</flakyFailure>
        </testcase>
    </testsuite>
</testsuites>"#;
        let r = junit(nextest);
        assert!(
            r.tests[1].flaky && r.tests[1].outcome == Outcome::Pass,
            "{:?}",
            r.tests[1]
        );

        // .NET (JunitXml.TestLogger): a bare <testsuites>, a filepath on the suite.
        let dotnet = r#"<?xml version="1.0" encoding="utf-8"?><testsuites><testsuite name="Shop.Tests.dll" tests="1" skipped="0" failures="0" errors="0" time="0.5" filepath="/src/Shop.Tests/bin/Shop.Tests.dll" id="0"><testcase classname="Shop.Tests.CartTests" name="Total_IsSum" time="0.0420000" /></testsuite></testsuites>"#;
        let r = junit(dotnet);
        assert_eq!(
            summary(&r),
            [(
                "Shop.Tests.CartTests::Total_IsSum".to_string(),
                Outcome::Pass,
                Some(0.042)
            )]
        );
        assert_eq!(
            r.tests[0].file.as_deref(),
            Some("/src/Shop.Tests/bin/Shop.Tests.dll")
        );

        // Maven Surefire: one <testsuite> root, an <error>, a rerun.
        let surefire = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" name="com.example.CartTest" time="1,234.5" tests="2" errors="1" skipped="0" failures="0">
  <properties/>
  <testcase name="total" classname="com.example.CartTest" time="0.004"/>
  <testcase name="checkout" classname="com.example.CartTest" time="1234.4">
    <error message="boom" type="java.lang.IllegalStateException">java.lang.IllegalStateException: boom</error>
    <rerunError message="boom" type="java.lang.IllegalStateException"/>
  </testcase>
</testsuite>"#;
        let r = junit(surefire);
        assert_eq!(r.seconds, Some(1234.5));
        // Surefire names suites after classes: the runner is plain JUnit unless the descriptor says.
        assert_eq!(r.runner.as_deref(), Some("junit"));
        assert_eq!(
            (r.tests[1].outcome, r.tests[1].flaky),
            (Outcome::Fail, true)
        );

        // Gradle: one file per class, <skipped/> as an empty element.
        let gradle = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.example.CartTest" tests="2" skipped="1" failures="0" errors="0" timestamp="2026-10-21T10:00:00" hostname="ci" time="0.05">
  <properties/>
  <testcase name="total()" classname="com.example.CartTest" time="0.01"/>
  <testcase name="later()" classname="com.example.CartTest" time="0.0">
    <skipped/>
  </testcase>
  <system-out><![CDATA[]]></system-out>
</testsuite>"#;
        assert_eq!(junit(gradle).tests[1].outcome, Outcome::Skip);
    }

    /// A cut or broken file keeps what it read and says the rest is unknown: never a pass.
    #[test]
    fn a_cut_or_broken_junit_file_is_incomplete_and_its_missing_tests_unknown() {
        let cut = r#"<testsuites><testsuite name="s"><testcase classname="s" name="a" time="0.1"/><testcase classname="s" name="b" time="0.2">"#;
        let r = junit(cut);
        assert!(!r.complete);
        assert_eq!(r.tests.len(), 1, "only the test read whole stands");
        assert!(
            r.warnings.iter().any(|w| w.contains("cut")),
            "{:?}",
            r.warnings
        );
        let broken = "<testsuites><testsuite name=\"s\"><testcase name=\"a\" time=\"1\"></testsuite></testcase>";
        assert!(!junit(broken).complete);
        // Cut between suites: every suite closed, <testsuites> not.
        let between = r#"<testsuites><testsuite name="s"><testcase name="a"/></testsuite>"#;
        assert!(!junit(between).complete);
        let empty = "<testsuites></testsuites>";
        assert!(!junit(empty).complete);
        assert!(!read("Build failed: no tests ran").complete);
    }

    /// Rust's libtest, in its plain, timed and JSON forms, and a binary cut short.
    #[test]
    fn libtest_reads_plain_timed_and_json_output_and_a_binary_cut_short() {
        let plain = "     Running unittests src/lib.rs (target/debug/deps/shop-0a1b2c3d4e5f6789)\n\nrunning 3 tests\ntest cart::adds ... ok\ntest cart::tax ... FAILED <1.250s>\ntest cart::slow ... ignored, needs a db\n\ntest result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.30s\n";
        let r = read(plain);
        assert_eq!(r.format, Some(Format::Libtest));
        assert!(r.complete);
        assert_eq!(
            summary(&r),
            [
                ("shop::cart::adds".to_string(), Outcome::Pass, None),
                ("shop::cart::tax".into(), Outcome::Fail, Some(1.25)),
                ("shop::cart::slow".into(), Outcome::Skip, None)
            ]
        );
        assert_eq!(r.seconds, Some(1.3));

        let json = "{ \"type\": \"suite\", \"event\": \"started\", \"test_count\": 2 }\n{ \"type\": \"test\", \"event\": \"started\", \"name\": \"a\" }\n{ \"type\": \"test\", \"name\": \"a\", \"event\": \"ok\", \"exec_time\": 0.5 }\n{ \"type\": \"test\", \"name\": \"b\", \"event\": \"failed\", \"exec_time\": 2.0 }\n{ \"type\": \"suite\", \"event\": \"failed\", \"passed\": 1, \"failed\": 1, \"exec_time\": 2.6 }\n";
        let r = read(json);
        assert_eq!(
            summary(&r),
            [
                ("a".to_string(), Outcome::Pass, Some(0.5)),
                ("b".into(), Outcome::Fail, Some(2.0))
            ]
        );
        assert!(r.complete);

        let cut = "running 5 tests\ntest a ... ok\ntest b ... ok\n";
        let r = read(cut);
        assert!(!r.complete && r.tests.len() == 2, "{r:?}");
        // A #[should_panic] test prints its name with ` - should panic`.
        let panics = "running 1 test\ntest rejects - should panic ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; finished in 0.01s\n";
        assert_eq!(
            summary(&read(panics)),
            [("rejects".to_string(), Outcome::Fail, None)]
        );
    }

    /// node:test's TAP: leaves only, skips, times, the run's time, and a run short of its plan.
    #[test]
    fn tap_reads_leaf_tests_skips_and_times_and_a_run_short_of_its_plan() {
        let text = "TAP version 13\n# Subtest: board\n    # Subtest: sorts\n    ok 1 - sorts\n      ---\n      duration_ms: 3100.5\n      ...\n    ok 2 - keeps order # SKIP not yet\n      ---\n      duration_ms: 0.2\n      ...\n    1..2\nok 1 - board\n  ---\n  duration_ms: 3101.2\n  ...\nnot ok 2 - inbox opens\n  ---\n  duration_ms: 12.0\n  ...\n1..2\n# duration_ms 3400.25\n";
        let r = read(text);
        assert_eq!(r.format, Some(Format::Tap));
        assert!(r.complete, "{:?}", r.warnings);
        assert_eq!(
            summary(&r),
            [
                ("sorts".to_string(), Outcome::Pass, Some(3.1005)),
                ("keeps order".into(), Outcome::Skip, Some(0.0002)),
                ("inbox opens".into(), Outcome::Fail, Some(0.012))
            ]
        );
        assert_eq!(r.seconds, Some(3.40025));
        let short = "TAP version 13\nok 1 - a\n1..3\n";
        assert!(!read(short).complete);
        // A parent whose own hook threw fails though its subtests passed: that failure stands.
        let hook = "TAP version 13\n    ok 1 - child\n    1..1\nnot ok 1 - parent\n1..1\n";
        let r = read(hook);
        assert_eq!(
            r.tests
                .iter()
                .map(|t| (t.id.as_str(), t.outcome))
                .collect::<Vec<_>>(),
            [("child", Outcome::Pass), ("parent", Outcome::Fail)]
        );
    }

    /// A CI job's log (`gh run view --log`) of a gate that ran `cargo test` and then `npm test`:
    /// each line's job, step and timestamp go, and both parts read as one run.
    #[test]
    fn a_ci_log_with_cargo_and_node_tests_reads_as_one_run() {
        let log = [
            "running 1 test",
            "test cart::adds ... ok <0.250s>",
            "test result: ok. 1 passed; 0 failed; finished in 0.30s",
            "TAP version 13",
            "ok 1 - board opens",
            "1..1",
        ]
        .map(|l| format!("suite\tRun tests\t2026-10-21T10:00:00.1234567Z {l}"))
        .join("\n");
        let r = read(&log);
        assert!(r.complete, "{:?}", r.warnings);
        assert_eq!(r.runner.as_deref(), Some("rust+node"));
        assert_eq!(
            r.tests.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["cart::adds", "board opens"]
        );
        assert_eq!(r.tests[0].seconds, Some(0.25));
        // Node finished, Rust started and died before its first result: the run is not whole.
        let died = read("TAP version 13\nok 1 - js\n1..1\nrunning 1 test\n");
        assert!(!died.complete, "{died:?}");
        assert!(read("TAP version 13\nok 1 - js\n1..1\nrunning setup\n").complete);
    }

    /// pytest -v and go test -v as CI's console prints them, and what a dispatch adds so it does.
    #[test]
    fn pytest_and_go_consoles_read_and_a_dispatch_asks_for_them() {
        let py = "tests/test_cart.py::test_total PASSED                          [ 50%]\ntests/test_cart.py::test_tax[2] FAILED                         [100%]\n=================== 1 failed, 1 passed in 0.12s ===================\n";
        let r = read(py);
        assert_eq!(r.format, Some(Format::Pytest));
        assert!(r.complete, "{:?}", r.warnings);
        assert_eq!(
            summary(&r),
            [
                (
                    "tests/test_cart.py::test_total".to_string(),
                    Outcome::Pass,
                    None
                ),
                (
                    "tests/test_cart.py::test_tax[2]".into(),
                    Outcome::Fail,
                    None
                )
            ]
        );
        assert!(
            !read("tests/test_cart.py::test_total PASSED [ 50%]\n").complete,
            "no summary: cut"
        );
        // A teardown error after a pass is the same test, failed.
        let teardown = "t.py::test_a PASSED [ 50%]\nt.py::test_a ERROR [ 50%]\n===== 1 passed, 1 error in 0.10s =====\n";
        assert_eq!(
            summary(&read(teardown)),
            [("t.py::test_a".to_string(), Outcome::Fail, None)]
        );
        let go = "=== RUN   TestTotal\n=== RUN   TestTotal/empty\n    --- PASS: TestTotal/empty (0.00s)\n--- PASS: TestTotal (0.81s)\n=== RUN   TestHangs\n";
        let r = read(go);
        assert_eq!(r.format, Some(Format::Gotest));
        assert_eq!(
            summary(&r),
            [
                ("TestTotal/empty".to_string(), Outcome::Pass, Some(0.0)),
                ("TestTotal".into(), Outcome::Pass, Some(0.81))
            ]
        );
        assert!(
            !r.complete && r.warnings[0].contains("TestHangs"),
            "{:?}",
            r.warnings
        );
        assert_eq!(
            console_command("pytest -q").as_deref(),
            Ok("pytest -v --durations=0 --durations-min=0")
        );
        assert_eq!(
            console_command("python -m pytest -q tests -v --durations=5").as_deref(),
            Ok("python -m pytest tests -v --durations=5")
        );
        let timed = "tests/t.py::test_a PASSED [100%]\n============ slowest durations ============\n1.50s call     tests/t.py::test_a\n0.25s setup    tests/t.py::test_a\n============ 1 passed in 1.80s ============\n";
        assert_eq!(
            summary(&read(timed)),
            [("tests/t.py::test_a".to_string(), Outcome::Pass, Some(1.75))]
        );
        assert!(console_command("cargo test --manifest-path {worktree}/Cargo.toml").is_err());
        assert_eq!(
            console_command("go test ./...").as_deref(),
            Ok("go test ./... -v -count=1")
        );
        assert_eq!(
            console_command("cargo test --workspace").as_deref(),
            Ok("cargo test --workspace")
        );
        assert!(console_command("npx jest --ci")
            .unwrap_err()
            .contains("JUnit"));
    }

    /// A dispatched command writes its runner's report where CI uploads it; npm scripts resolve
    /// to their runner; jest needs jest-junit.
    #[test]
    fn dispatched_commands_write_each_runners_report() {
        let none = None;
        assert_eq!(
            report_command("pytest -q", none).as_deref(),
            Ok("pytest -q --junitxml=$NITELY_JUNIT_DIR/pytest.xml")
        );
        assert_eq!(report_command("npx vitest run", none).as_deref(), Ok("npx vitest run --reporter=default --reporter=junit --outputFile.junit=$NITELY_JUNIT_DIR/vitest.xml"));
        assert_eq!(
            report_command("dotnet test Shop.sln", none).as_deref(),
            Ok("dotnet test Shop.sln --logger trx --results-directory $NITELY_JUNIT_DIR")
        );
        assert_eq!(
            report_command("./mvnw -q verify", none).as_deref(),
            Ok("./mvnw -q verify")
        );
        assert_eq!(
            report_command("./gradlew test", none).as_deref(),
            Ok("./gradlew test")
        );
        assert!(report_command("cargo test", none).is_err());
        let with_jest = serde_json::json!({"scripts": {"test": "jest --ci", "unit": "vitest run"}, "devDependencies": {"jest-junit": "^16"}});
        assert_eq!(report_command("npm test", Some(&with_jest)).as_deref(), Ok("JEST_JUNIT_OUTPUT_DIR=$NITELY_JUNIT_DIR JEST_JUNIT_OUTPUT_NAME=jest.xml npm test -- --reporters=default --reporters=jest-junit"));
        assert!(report_command("npm run unit", Some(&with_jest))
            .unwrap()
            .starts_with("npm run unit -- --reporter=default --reporter=junit"));
        let bare_jest = serde_json::json!({"scripts": {"test": "jest"}});
        assert!(report_command("npm test", Some(&bare_jest))
            .unwrap_err()
            .contains("jest-junit"));
    }

    /// dotnet's TRX: each result, its duration and outcome, and a cut file.
    #[test]
    fn trx_reads_each_result_and_a_cut_file() {
        let trx_text = r#"<?xml version="1.0" encoding="utf-8"?><TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010"><Results><UnitTestResult testName="Shop.Tests.CartTests.Total" duration="00:00:01.2500000" outcome="Passed" /><UnitTestResult testName="Shop.Tests.CartTests.Tax" duration="00:00:00.0100000" outcome="Failed"><Output /></UnitTestResult></Results><ResultSummary outcome="Failed"><Counters total="2" /></ResultSummary></TestRun>"#;
        let r = read(trx_text);
        assert_eq!(r.format, Some(Format::Trx));
        assert!(r.complete, "{:?}", r.warnings);
        assert_eq!(
            summary(&r),
            [
                (
                    "Shop.Tests.CartTests.Total".to_string(),
                    Outcome::Pass,
                    Some(1.25)
                ),
                ("Shop.Tests.CartTests.Tax".into(), Outcome::Fail, Some(0.01))
            ]
        );
        assert!(!read(&trx_text[..trx_text.find("<ResultSummary").unwrap()]).complete);
    }
}
