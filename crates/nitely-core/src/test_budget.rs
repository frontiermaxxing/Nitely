//! The suite's time budget (NIT-522): each test's time, read from what `cargo test` and node:test
//! print, held to `.nitely/test-budget.toml`. Reading and checking are plain functions over the
//! log's text; the gate reads the log and says what came of it.
//!
//! Stable Rust prints no per-test times, so a lane runs `cargo test` with Nitely as cargo's test
//! runner (`runner_env`): `nitely test-runner` starts each test binary with `--report-time`, which
//! only the binary reads. Nothing about the build changes, so the lane's warm build stays warm.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where a repo keeps its budget.
pub const FILE: &str = ".nitely/test-budget.toml";

/// The limits a repo sets for its tests.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Seconds any one test may take, unless it is a named scenario.
    pub test_seconds: Option<f64>,
    /// Seconds a whole test binary may take, by the name cargo prints for it (`nitely_core`), or
    /// `node` for node:test.
    #[serde(default)]
    pub suites: BTreeMap<String, f64>,
    /// Tests allowed past `test_seconds`, by the name the runner prints, each with why.
    #[serde(default)]
    pub scenarios: BTreeMap<String, String>,
}

/// The budget in `dir`, if it has one.
pub fn load(dir: &Path) -> Result<Option<Budget>> {
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(text) => Ok(Some(toml::from_str(&text).map_err(|e| anyhow::anyhow!("{FILE}: {e}"))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// One test's time, in the suite (test binary, or `node`) that ran it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestTime {
    pub suite: String,
    pub name: String,
    pub seconds: f64,
}

/// A whole suite's time, as its runner reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuiteTime {
    pub suite: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Timings {
    pub tests: Vec<TestTime>,
    pub suites: Vec<SuiteTime>,
}

impl Timings {
    pub fn is_empty(&self) -> bool {
        self.tests.is_empty() && self.suites.is_empty()
    }

    pub fn extend(&mut self, other: Timings) {
        self.tests.extend(other.tests);
        self.suites.extend(other.suites);
    }
}

fn re(cell: &'static OnceLock<regex::Regex>, pattern: &str) -> &'static regex::Regex {
    cell.get_or_init(|| regex::Regex::new(pattern).unwrap())
}

/// A line without its colours (which `gh run view --log` spells `^[[1m`), and without what GitHub's job log puts before each one: the job
/// and step names and a timestamp (`gh run view --log`), or the timestamp alone.
pub(crate) fn plain_line(line: &str) -> std::borrow::Cow<'_, str> {
    static PREFIX: OnceLock<regex::Regex> = OnceLock::new();
    static COLOUR: OnceLock<regex::Regex> = OnceLock::new();
    let prefix = re(&PREFIX, r"^(?:[^\t]*\t[^\t]*\t)?\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z ?");
    let line = match prefix.find(line) {
        Some(m) => &line[m.end()..],
        None => line,
    };
    re(&COLOUR, r"(?:\x1b|\^\[)\[[0-9;]*m").replace_all(line, "")
}

/// Every test time and suite time in `log`: cargo's `test x ... ok <1.234s>` lines under each
/// `Running … (…/deps/<suite>-<hash>)`, its `test result: … finished in 4.5s`, and node:test's TAP
/// `duration_ms` for each test that has no subtests, and for the whole run.
pub fn parse(log: &str) -> Timings {
    static RUNNING: OnceLock<regex::Regex> = OnceLock::new();
    static DOC: OnceLock<regex::Regex> = OnceLock::new();
    static TEST: OnceLock<regex::Regex> = OnceLock::new();
    static RESULT: OnceLock<regex::Regex> = OnceLock::new();
    static TAP: OnceLock<regex::Regex> = OnceLock::new();
    static TAP_TIME: OnceLock<regex::Regex> = OnceLock::new();
    static TAP_TOTAL: OnceLock<regex::Regex> = OnceLock::new();
    let running = re(&RUNNING, r"^\s*Running .*\((?:.*[/\\])?([^/\\()]+?)-[0-9a-f]{6,}(?:\.exe)?\)\s*$");
    let doc = re(&DOC, r"^\s*Doc-tests (\S+)\s*$");
    let test = re(&TEST, r"^test (\S+) \.\.\. (?:ok|FAILED|ignored)\S* <(\d+(?:\.\d+)?)s>\s*$");
    let result = re(&RESULT, r"^test result: .*; finished in (\d+(?:\.\d+)?)s\s*$");
    let tap = re(&TAP, r"^( *)(?:not )?ok \d+ - (.+?)(?: # .*)?$");
    let tap_time = re(&TAP_TIME, r"^ *duration_ms: (\d+(?:\.\d+)?)\s*$");
    let tap_total = re(&TAP_TOTAL, r"^# duration_ms (\d+(?:\.\d+)?)\s*$");

    let mut out = Timings::default();
    let mut suite = String::from("cargo");
    // The TAP test whose YAML block may come next, and the indent of the last one: a test whose
    // line comes right after a deeper one's is the parent of those, and its time is theirs.
    let mut tap_last: Option<(String, bool)> = None;
    let mut tap_indent: Option<usize> = None;
    for line in log.lines() {
        let line = plain_line(line);
        let line = line.as_ref();
        if let Some(c) = running.captures(line) {
            suite = c[1].to_string();
        } else if let Some(c) = doc.captures(line) {
            suite = format!("{} (doc)", &c[1]);
        } else if let Some(c) = test.captures(line) {
            out.tests.push(TestTime { suite: suite.clone(), name: c[1].to_string(), seconds: c[2].parse().unwrap_or(0.0) });
        } else if let Some(c) = result.captures(line) {
            out.suites.push(SuiteTime { suite: suite.clone(), seconds: c[1].parse().unwrap_or(0.0) });
        } else if let Some(c) = tap.captures(line) {
            let indent = c[1].len();
            let parent = tap_indent.is_some_and(|last| last > indent);
            tap_indent = Some(indent);
            tap_last = Some((c[2].trim().to_string(), !parent));
        } else if let Some(c) = tap_time.captures(line) {
            if let Some((name, true)) = tap_last.take() {
                out.tests.push(TestTime { suite: "node".into(), name, seconds: c[1].parse::<f64>().unwrap_or(0.0) / 1000.0 });
            }
        } else if let Some(c) = tap_total.captures(line) {
            out.suites.push(SuiteTime { suite: "node".into(), seconds: c[1].parse::<f64>().unwrap_or(0.0) / 1000.0 });
        }
    }
    out
}

/// What the timings say against a budget.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    /// The slowest tests, slowest first, up to `SLOWEST`.
    pub slowest: Vec<TestTime>,
    /// Tests over the per-test limit that are not named scenarios, slowest first.
    pub over: Vec<TestTime>,
    /// Suites over their target, with the target.
    pub suites_over: Vec<(SuiteTime, f64)>,
    pub suites: Vec<SuiteTime>,
    pub tests: usize,
}

/// How many of the slowest tests a gate shows.
pub const SLOWEST: usize = 5;

/// How many tests over the limit a gate names before it counts the rest.
const NAMED: usize = 10;

fn slowest_first(tests: &mut [TestTime]) {
    tests.sort_by(|a, b| b.seconds.total_cmp(&a.seconds).then_with(|| a.name.cmp(&b.name)));
}

/// `timings` against `budget`.
pub fn check(budget: &Budget, timings: &Timings) -> Report {
    let mut all = timings.tests.clone();
    slowest_first(&mut all);
    let over = match budget.test_seconds {
        Some(limit) => all.iter().filter(|t| t.seconds > limit && !budget.scenarios.contains_key(&t.name)).cloned().collect(),
        None => vec![],
    };
    let suites_over = timings.suites.iter().filter_map(|s| budget.suites.get(&s.suite).filter(|target| s.seconds > **target).map(|target| (s.clone(), *target))).collect();
    Report { tests: all.len(), slowest: all.into_iter().take(SLOWEST).collect(), over, suites_over, suites: timings.suites.clone() }
}

fn secs(s: f64) -> String {
    if s >= 10.0 {
        format!("{s:.0} s")
    } else {
        format!("{s:.1} s")
    }
}

impl Report {
    /// A test took longer than the budget allows, or a suite did.
    pub fn breached(&self) -> bool {
        !self.over.is_empty() || !self.suites_over.is_empty()
    }

    /// The gate's line on test time: each suite's time (against its target), and the slowest tests.
    pub fn summary(&self, budget: &Budget) -> String {
        let suites = self
            .suites
            .iter()
            .filter(|s| s.seconds >= 0.05 || budget.suites.contains_key(&s.suite))
            .map(|s| match budget.suites.get(&s.suite) {
                Some(target) => format!("{} {} (target {})", s.suite, secs(s.seconds), secs(*target)),
                None => format!("{} {}", s.suite, secs(s.seconds)),
            })
            .collect::<Vec<_>>();
        let slowest = self.slowest.iter().map(|t| format!("{} {}", t.name, secs(t.seconds))).collect::<Vec<_>>();
        let mut line = format!("Test time, {} tests timed", self.tests);
        if !suites.is_empty() {
            line.push_str(&format!(": {}", suites.join(", ")));
        }
        if !slowest.is_empty() {
            line.push_str(&format!("; slowest: {}", slowest.join(", ")));
        }
        line
    }

    /// What broke the budget, in words, or None.
    pub fn breach(&self, budget: &Budget) -> Option<String> {
        if !self.breached() {
            return None;
        }
        let mut parts = Vec::new();
        for (s, target) in &self.suites_over {
            parts.push(format!("{} took {}, over its {} target", s.suite, secs(s.seconds), secs(*target)));
        }
        if !self.over.is_empty() {
            let limit = budget.test_seconds.unwrap_or_default();
            let named = self.over.iter().take(NAMED).map(|t| format!("{} ({})", t.name, secs(t.seconds))).collect::<Vec<_>>().join(", ");
            let more = self.over.len().saturating_sub(NAMED);
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            parts.push(format!("{} tests took over {} and are not named scenarios: {named}{more}", self.over.len(), secs(limit)));
        }
        Some(format!("Over the test budget in {FILE}: {}", parts.join("; ")))
    }
}

/// `cargo test` in a command, where cargo will start the test binaries.
fn runs_cargo_test(command: &str) -> bool {
    static CARGO_TEST: OnceLock<regex::Regex> = OnceLock::new();
    re(&CARGO_TEST, r"(^|[\s&|;(])cargo(?:\.exe)?(?:\s+\+\S+)?\s+test(\s|$)").is_match(command)
}

/// A repo whose test binaries may not take libtest's options: one with its own test harness, or
/// its own runner in cargo's config. Read from the manifests up to two folders down.
fn has_own_harness_or_runner(dir: &Path) -> bool {
    static HARNESS: OnceLock<regex::Regex> = OnceLock::new();
    let harness = re(&HARNESS, r"(?m)^\s*harness\s*=\s*false");
    let config = [".cargo/config.toml", ".cargo/config"].iter().any(|f| std::fs::read_to_string(dir.join(f)).is_ok_and(|t| t.contains("runner")));
    if config {
        return true;
    }
    fn walk(dir: &Path, depth: u32, harness: &regex::Regex) -> bool {
        if std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|t| harness.is_match(&t)) {
            return true;
        }
        if depth == 0 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return false };
        entries.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !(name.starts_with('.') || name == "target" || name == "node_modules") && walk(&e.path(), depth - 1, harness)
        })
    }
    walk(dir, 2, harness)
}

/// The `nitely` this process can start: itself, or the one beside it (the app's).
fn nitely_exe() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    if me.file_stem().is_some_and(|s| s == "nitely") {
        return Some(me);
    }
    let beside = me.with_file_name(format!("nitely{}", std::env::consts::EXE_SUFFIX));
    beside.is_file().then_some(beside)
}

/// The environment that makes cargo start each test binary through `nitely test-runner`, for a
/// `command` that runs `cargo test` in `dir`. None when it does not, when the repo has a harness or
/// runner of its own, when a runner is set already, or when no `nitely` is at hand: the command
/// then runs as it is, without per-test times.
pub fn runner_env(command: &str, dir: &Path) -> Option<Vec<(String, OsString)>> {
    if !runs_cargo_test(command) || command.contains("--report-time") || command.contains("--format") {
        return None;
    }
    let var = format!("CARGO_TARGET_{}_RUNNER", env!("NITELY_HOST_TARGET").to_uppercase().replace(['-', '.'], "_"));
    if std::env::var_os(&var).is_some() || has_own_harness_or_runner(dir) {
        return None;
    }
    let exe = nitely_exe()?;
    let spelled = exe.to_string_lossy();
    // Cargo splits the setting at spaces, so a path with one goes on PATH and is named bare.
    if !spelled.contains(char::is_whitespace) {
        return Some(vec![(var, format!("{spelled} test-runner").into())]);
    }
    let folder = exe.parent()?.to_path_buf();
    let mut path = vec![folder];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    Some(vec![(var, "nitely test-runner".into()), ("PATH".into(), std::env::join_paths(path).ok()?)])
}

/// `nitely test-runner <binary> <args>`: starts a test binary as cargo asked, with libtest's
/// per-test times on, and exits as it did. A listing, a benchmark, or a run that sets its own
/// output format runs unchanged.
pub fn run_test_binary(args: &[OsString]) -> i32 {
    let Some((binary, rest)) = args.split_first() else {
        eprintln!("nitely test-runner: no test binary given");
        return 2;
    };
    let plain = rest.iter().any(|a| matches!(a.to_str(), Some("--list" | "--bench" | "--report-time" | "--format" | "-q" | "--quiet")) || a.to_str().is_some_and(|a| a.starts_with("--format=")));
    let mut cmd = std::process::Command::new(binary);
    cmd.args(rest);
    if !plain {
        // libtest takes its unstable options on a stable compiler when RUSTC_BOOTSTRAP is set
        // where it runs; the build never sees it.
        cmd.args(["-Z", "unstable-options", "--report-time"]).env("RUSTC_BOOTSTRAP", "1");
    }
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(101),
        Err(e) => {
            eprintln!("nitely test-runner: could not start {}: {e}", Path::new(binary).display());
            101
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cargo_and_node_times_from_a_lane_log_and_a_ci_log() {
        let cargo = "\
$ cargo test --workspace -- --test-threads=12
   Compiling nitely-core v0.1.0
     Running unittests src/lib.rs (target\\debug\\deps\\nitely_core-0a1b2c3d4e5f6789.exe)

running 3 tests
test land::tests::quick ... ok <0.012s>
test land_queue::tests::bisects ... FAILED <61.250s>
test lanes::tests::slow_one ... ignored
test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 61.30s

     Running tests/cli.rs (target/debug/deps/cli-89abcdef01234567)
test runs_help ... ok <2.500s>
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.51s

   Doc-tests nitely_core
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let t = parse(cargo);
        let names: Vec<_> = t.tests.iter().map(|t| (t.suite.as_str(), t.name.as_str(), t.seconds)).collect();
        assert_eq!(names, [("nitely_core", "land::tests::quick", 0.012), ("nitely_core", "land_queue::tests::bisects", 61.25), ("cli", "runs_help", 2.5)]);
        let suites: Vec<_> = t.suites.iter().map(|s| (s.suite.as_str(), s.seconds)).collect();
        assert_eq!(suites, [("nitely_core", 61.3), ("cli", 2.51), ("nitely_core (doc)", 0.0)]);

        // GitHub's job log puts the job, the step and a timestamp before each line, and cargo
        // colours its own lines there.
        let ci = cargo.replace("     Running", "^[[1m^[[92m     Running^[[0m").replace("   Doc-tests", "\x1b[1m   Doc-tests\x1b[0m").lines().map(|l| format!("cargo test\tRun\t2026-10-21T12:00:01.1234567Z {l}")).collect::<Vec<_>>().join("\n");
        assert_eq!(parse(&ci), t);

        // node:test's TAP: a test with subtests is timed by them, not counted as one.
        let node = "\
TAP version 13
# Subtest: board
    # Subtest: sorts by urgency
    ok 1 - sorts by urgency
      ---
      duration_ms: 3100.5
      ...
    ok 2 - keeps the order # SKIP
      ---
      duration_ms: 0.2
      ...
ok 1 - board
  ---
  duration_ms: 3101.2
  type: 'suite'
  ...
not ok 2 - inbox opens
  ---
  duration_ms: 12.0
  ...
# duration_ms 3400.25
";
        let t = parse(node);
        let names: Vec<_> = t.tests.iter().map(|t| (t.name.as_str(), t.seconds)).collect();
        assert_eq!(names, [("sorts by urgency", 3.1005), ("keeps the order", 0.0002), ("inbox opens", 0.012)]);
        assert_eq!(t.suites, [SuiteTime { suite: "node".into(), seconds: 3.40025 }]);
    }

    #[test]
    fn a_budget_flags_slow_tests_unless_named_and_suites_over_their_target() {
        let budget: Budget = toml::from_str(
            r#"
test_seconds = 2
[suites]
nitely_core = 60
[scenarios]
"land_queue::tests::bisects" = "the bisection scenario"
"#,
        )
        .unwrap();
        let t = |name: &str, seconds: f64| TestTime { suite: "nitely_core".into(), name: name.into(), seconds };
        let timings = Timings {
            tests: vec![t("a::fast", 0.1), t("land_queue::tests::bisects", 40.0), t("b::slow", 3.0), t("c::slower", 9.0), t("d::at_the_limit", 2.0)],
            suites: vec![SuiteTime { suite: "nitely_core".into(), seconds: 75.0 }, SuiteTime { suite: "cli".into(), seconds: 1.0 }],
        };
        let report = check(&budget, &timings);
        assert!(report.breached());
        assert_eq!(report.over.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["c::slower", "b::slow"]);
        assert_eq!(report.slowest[0].name, "land_queue::tests::bisects");
        assert_eq!(report.slowest.len(), SLOWEST);
        assert_eq!(
            report.breach(&budget).unwrap(),
            "Over the test budget in .nitely/test-budget.toml: nitely_core took 75 s, over its 60 s target; 2 tests took over 2.0 s and are not named scenarios: c::slower (9.0 s), b::slow (3.0 s)"
        );
        assert!(report.summary(&budget).starts_with("Test time, 5 tests timed: nitely_core 75 s (target 60 s), cli 1.0 s; slowest: land_queue::tests::bisects 40 s, c::slower 9.0 s"));

        let within = Timings { tests: vec![t("a::fast", 0.1)], suites: vec![SuiteTime { suite: "nitely_core".into(), seconds: 30.0 }] };
        assert_eq!(check(&budget, &within).breach(&budget), None);
        // No limits set: times are shown, nothing is flagged.
        assert!(!check(&Budget::default(), &timings).breached());
    }

    #[test]
    fn only_cargo_test_gets_the_runner() {
        assert!(runs_cargo_test("cargo test --workspace -- --test-threads=12"));
        assert!(runs_cargo_test("npm run build && cargo +stable test -p nitely-core"));
        assert!(!runs_cargo_test("cargo clippy --workspace -- -D warnings"));
        assert!(!runs_cargo_test("cargo testify"));
        assert!(!runs_cargo_test("npm test"));
        let dir = crate::paths::unique_temp("nitely-budget", "");
        std::fs::create_dir_all(dir.join("crates/bench")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").unwrap();
        assert!(!has_own_harness_or_runner(&dir));
        std::fs::write(dir.join("crates/bench/Cargo.toml"), "[[test]]\nname = \"e2e\"\nharness = false\n").unwrap();
        assert!(has_own_harness_or_runner(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
