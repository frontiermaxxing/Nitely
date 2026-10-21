use anyhow::{bail, Context, Result};
use clap::Subcommand;
use nitely_core::test_sweep::{self, Report};
use nitely_core::{Actor, Config, Db};
use std::path::PathBuf;
#[derive(Subcommand)]
pub enum Command {
    /// Read-only source inventory and imported logs by default. --execute uses a build lane.
    Collect {
        /// JSON array of {path,revision,run,source,runner?} evidence descriptors. No suites run by default.
        #[arg(long)]
        logs: Option<PathBuf>,
        /// Report on this commit, an ancestor of HEAD (main's, from `measure`): its tests are read
        /// from its own tree and its evidence compared at it.
        #[arg(long, conflicts_with = "execute")]
        at: Option<String>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        execute: bool,
        /// Measure stable Rust binaries one test at a time (includes process startup cost).
        #[arg(long, requires = "execute")]
        rust_timings: bool,
        /// JSON array of known failing test names/IDs; failures remain failures.
        #[arg(long)]
        known_failures: Option<PathBuf>,
        #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u32).range(1..=3))]
        runs: u32,
        #[arg(long, default_value_t=600, value_parser=clap::value_parser!(u32).range(1..=3600))]
        seconds: u32,
        #[arg(long)]
        issue: Option<String>,
    },
    /// The tests a report flags (slow, flaky, failing, a wait or a UI assertion in their body) as
    /// a template to classify; with --classes, the map of those classes as markdown.
    Map {
        file: PathBuf,
        /// A JSON array of {test, class, ...} for every flagged test (see `guide`).
        #[arg(long)]
        classes: Option<PathBuf>,
        /// Slower than this many seconds (median, one measuring method) is flagged.
        #[arg(long, default_value_t = 2.0)]
        slow: f64,
        /// Where to write the map; standard output when absent.
        #[arg(long, requires = "classes")]
        output: Option<PathBuf>,
    },
    /// Measures the gate's tests into an evidence folder for `collect --logs`: CI's logs at HEAD
    /// first; else, shown with its cost cap first, CI runs (--dispatch) or, only for a repo
    /// without CI, build-lane runs (--lane). Nothing runs on this machine's own checkout.
    Measure {
        /// An empty or new folder for the logs and evidence.json.
        #[arg(long)]
        out: PathBuf,
        /// Runs at the same revision: two tell a flaky test from a failing one.
        #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u32).range(1..=3))]
        runs: u32,
        /// The cost cap of each run, in minutes.
        #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u32).range(1..=120))]
        minutes: u32,
        #[arg(long, conflicts_with = "lane")]
        dispatch: bool,
        #[arg(long)]
        lane: bool,
        /// Finished CI runs at HEAD to read.
        #[arg(long, default_value_t=3, value_parser=clap::value_parser!(u32).range(1..=10))]
        limit: u32,
        #[arg(long)]
        issue: Option<String>,
    },
    /// Show existing evidence without executing tests.
    Report {
        file: PathBuf,
    },
    /// Proposes one goal from the classified map (see `map`): a refactor card per module, a
    /// clock-wait card, the test budget, one deletion card, and, where the repo has CI, a
    /// playbook proposal for ci_commands. Nothing is applied or approved.
    Propose {
        file: PathBuf,
        /// The classes of every flagged test, as `map --classes` takes them.
        #[arg(long)]
        classes: PathBuf,
        #[arg(long, default_value_t = 2.0)]
        slow: f64,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        issue: Option<String>,
    },
    Guide,
}
fn quoted(path: &std::path::Path) -> Result<String> {
    let text = path.to_str().context("path is not UTF-8")?;
    if text.contains(['"', '\n', '\r', '%', '!']) {
        bail!("unsupported shell metacharacter in collector path");
    }
    if cfg!(windows) {
        Ok(format!("\"{text}\""))
    } else {
        Ok(format!("'{}'", text.replace('\'', "'\\''")))
    }
}
/// The repo a sweep measures, its gate's test commands, and whether CI can take them.
struct Where {
    root: PathBuf,
    project: String,
    pb: nitely_core::playbook::Playbook,
    tests: Vec<String>,
    ci: nitely_core::executor::CiState,
    /// The playbook has a suite workflow to dispatch to (`[lanes] ci_commands`).
    workflow: bool,
}

impl Where {
    /// CI's state for `plan_measure`: a GitHub repo with no workflow files has no CI to measure in.
    fn ci_fact(&self) -> test_sweep::CiFact<'_> {
        use nitely_core::executor::CiState;
        let workflows = std::fs::read_dir(self.root.join(".github").join("workflows"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|f| {
                f.path()
                    .extension()
                    .is_some_and(|e| e == "yml" || e == "yaml")
            });
        // A repo with another CI has CI, which Nitely cannot dispatch to: nothing runs here.
        if !workflows {
            if let Some(why) = test_sweep::other_ci(&self.root) {
                return test_sweep::CiFact::Blocked(why);
            }
        }
        match &self.ci {
            // No workflow files: no CI, whether or not gh could reach GitHub.
            CiState::Ready(_) | CiState::Blocked(_) if !workflows => {
                test_sweep::CiFact::Absent("the repo has no CI workflows (.github/workflows)")
            }
            CiState::Ready(_) => test_sweep::CiFact::Ready {
                workflow: self.workflow,
            },
            // Workflows mean the repo has CI, even where this machine keeps it local or the
            // origin is elsewhere: CI that cannot be reached, never a reason to run here.
            CiState::Absent(why) if workflows => test_sweep::CiFact::Blocked(why),
            CiState::Absent(why) => test_sweep::CiFact::Absent(why),
            CiState::Blocked(why) => test_sweep::CiFact::Blocked(why),
        }
    }
}

impl Where {
    fn of(cwd: &std::path::Path) -> Result<Where> {
        use nitely_core::{executor, playbook};
        let config = Config::load()?;
        let dir = cwd.to_string_lossy();
        let root = playbook::repo_root(&config, &dir)
            .with_context(|| format!("{dir} is not inside a git repo"))?;
        let project = config
            .repo_for_dir(&root.to_string_lossy())
            .and_then(|r| r.project.clone())
            .unwrap_or_default();
        let pb = playbook::Playbook::load_with(&root, &config)?;
        let gate: Vec<String> = pb
            .gate
            .commands
            .iter()
            .cloned()
            .chain(pb.gate.steps.iter().map(|s| s.run.clone()))
            .collect();
        let tests = test_sweep::test_commands(&gate);
        let ci = executor::ci_state(&root, &project);
        let workflow = pb.lanes.as_ref().is_some_and(|l| !l.ci_commands.is_empty());
        Ok(Where {
            root,
            project,
            pb,
            tests,
            ci,
            workflow,
        })
    }
}

/// Measures the gate's tests without running them on this machine when it can (NIT-554): logs of
/// finished CI runs at HEAD first, then capped CI runs (`--dispatch`), and a capped build-lane run
/// (`--lane`) only for a repo without CI. Each run's plan and cap are shown before it starts.
/// Writes the logs and an evidence list for `collect --logs` into `out`.
fn measure(
    out: PathBuf,
    runs: u32,
    minutes: u32,
    dispatch: bool,
    lane: bool,
    limit: u32,
    issue: Option<String>,
) -> Result<()> {
    use nitely_core::{executor, lanes, test_sweep::Method};
    let cwd = std::env::current_dir()?;
    let revision = test_sweep::revision(&cwd)?;
    let db = Db::open_default()?;
    let here = Where::of(&cwd)?;
    let ci_fact = here.ci_fact();
    let Where {
        root,
        project,
        pb,
        tests,
        ci,
        ..
    } = &here;
    let out = if out.is_absolute() {
        out
    } else {
        cwd.join(out)
    };
    if out.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        bail!(
            "{} is not empty; choose a new folder for the evidence",
            out.display()
        );
    }
    std::fs::create_dir_all(&out)?;
    // Logs at HEAD from runs that finished, kept only when they hold tests.
    let (mut found, mut tested_main, mut from_main) = (Vec::new(), None, Vec::new());
    if let (executor::CiState::Ready(slug), false, false) = (ci, dispatch, lane) {
        for log in executor::ci_logs(slug, &revision, limit, &out.join("artifacts"), 64 << 20)? {
            if !nitely_core::test_evidence::read(&log.text).tests.is_empty() {
                found.push(log);
            }
        }
        if found.is_empty() {
            tested_main = executor::tested_commit(slug, pb.integration_branch())
                .filter(|sha| *sha != revision);
            if let Some(sha) = &tested_main {
                for log in executor::ci_logs(slug, sha, limit, &out.join("artifacts"), 64 << 20)? {
                    if !nitely_core::test_evidence::read(&log.text).tests.is_empty() {
                        from_main.push(log);
                    }
                }
            }
        }
    }
    let plan = test_sweep::plan_measure(&test_sweep::MeasureFacts {
        revision: &revision,
        ci_logs: found.len(),
        tested_main: tested_main.as_deref().map(|sha| (sha, from_main.len())),
        ci: ci_fact,
        tests,
        runs,
        minutes,
        dispatch,
        lane,
    })?;
    eprintln!("{}", plan.says);
    if !plan.start {
        return crate::print_json(&serde_json::json!({ "plan": plan, "started": false }));
    }
    if plan.method != Method::CiLog {
        // What runs is HEAD's tree and nothing else: no untracked test rides along under its
        // name, and the evidence written between runs stays out of the snapshots.
        if test_sweep::measured_revision(&cwd, &out)? != revision {
            bail!("HEAD moved while measuring was being planned; start again");
        }
    }
    let mut evidence = Vec::new();
    let mut empty = Vec::new();
    let mut stopped: Option<String> = None;
    // The evidence's commit: HEAD, or main's when the plan reads main's logs.
    let at = plan.at.clone().unwrap_or_else(|| revision.clone());
    let list = out.join("evidence.json");
    let mut keep = |name: String,
                    text: &str,
                    run: String,
                    source: String,
                    method: Method|
     -> Result<()> {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = out.join(safe);
        std::fs::write(&path, text)?;
        if nitely_core::test_evidence::read(text).tests.is_empty() {
            empty.push(format!("{source}: no test outcomes in its log"));
        }
        evidence.push(serde_json::json!({ "path": path, "revision": at, "run": run, "source": source, "method": method }));
        // Written after each log, so a run that fails later leaves what came before usable.
        std::fs::write(&list, serde_json::to_vec_pretty(&evidence)?)?;
        Ok(())
    };
    let actor = Actor::Agent("cli".into());
    match plan.method {
        Method::CiLog => {
            let logs = if found.is_empty() { &from_main } else { &found };
            for (n, log) in logs.iter().enumerate() {
                // One workflow run attempt is one execution: its jobs and reports share a run, where
                // the collector keeps a failure over a pass.
                let run = test_sweep::run_id(&[
                    "ci",
                    &log.workflow_run.to_string(),
                    &log.attempt.to_string(),
                ]);
                keep(
                    format!("{run}-{n}.log"),
                    &log.text,
                    run,
                    format!("{} ({})", log.job, log.url),
                    Method::CiLog,
                )?;
            }
        }
        Method::CiRun => {
            // One snapshot for every run: what is compared across runs is one source.
            let snapshot = lanes::snapshot(&cwd)?;
            for n in 0..plan.runs {
                let req = executor::SuiteRequest {
                    project,
                    issue: issue.as_deref(),
                    kind: lanes::Kind::Run,
                    snapshot: &snapshot,
                    commands: &plan.commands,
                    workflow_ref: None,
                };
                let pending = match executor::dispatch(&db, root, pb, &req, &actor)? {
                    executor::Dispatch::Started(p) => p.cap(plan.minutes.into()).every_log(),
                    executor::Dispatch::Unavailable(why) => bail!("CI cannot take the run: {why}"),
                };
                let ran = match executor::wait(&db, pending) {
                    Ok(ran) => ran,
                    Err(e) => {
                        stopped = Some(format!("run {} of {} ended without results ({e:#}); the evidence holds the runs before it", n + 1, plan.runs));
                        break;
                    }
                };
                for (j, (command, log)) in ran.logs.iter().enumerate() {
                    let run =
                        test_sweep::run_id(&["ci-run", &ran.workflow_run.to_string(), command]);
                    keep(
                        format!("{run}-{n}-{j}.log"),
                        log,
                        run,
                        format!("{command} ({})", ran.url),
                        Method::CiRun,
                    )?;
                }
            }
        }
        Method::Lane => {
            // Capped, and kept in the lanes: nothing here goes to CI.
            let mut capped = pb.clone();
            if let Some(rules) = capped.lanes.as_mut() {
                rules.ci_commands.clear();
            }
            let limit = capped.lanes.as_ref().map_or(30, |l| l.timeout_minutes);
            let snapshot = lanes::snapshot(&cwd)?;
            'runs: for n in 0..plan.runs {
                // Named by its first lane run, so runs of separate invocations never merge.
                let mut run = None;
                // The run's cap is one deadline for all its commands, setup included.
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(u64::from(plan.minutes) * 60);
                for command in &plan.commands {
                    // Whole minutes left, a last part-minute counted as one; under half a
                    // minute is too little to start a command.
                    let secs = deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .as_secs();
                    let left = (secs / 60).max(1);
                    if secs < 30 {
                        stopped = Some(format!(
                            "run {} reached its {}-minute cap before `{command}`",
                            n + 1,
                            plan.minutes
                        ));
                        break 'runs;
                    }
                    if let Some(rules) = capped.lanes.as_mut() {
                        rules.timeout_minutes = limit.min(left);
                    }
                    let req = lanes::Request {
                        project,
                        issue: issue.as_deref(),
                        kind: lanes::Kind::Run,
                        snapshot: &snapshot,
                        command,
                    };
                    let ran = lanes::run(&db, root, &capped, &req, &actor, &mut |_| {})?;
                    let text = match ran.log.as_deref() {
                        Some(log) => std::fs::read_to_string(log)?,
                        None => ran.output.clone(),
                    };
                    let run = run
                        .get_or_insert_with(|| format!("lane-{}", ran.id))
                        .clone();
                    keep(
                        format!("lane-{}-{n}.log", ran.id),
                        &text,
                        run,
                        format!("{command} in lane {}", ran.lane.unwrap_or(0)),
                        Method::Lane,
                    )?;
                }
            }
        }
        Method::Imported => unreachable!("measure never plans an import"),
    }
    std::fs::write(&list, serde_json::to_vec_pretty(&evidence)?)?;
    let at_flag = plan
        .at
        .as_ref()
        .map(|sha| format!(" --at {sha}"))
        .unwrap_or_default();
    crate::print_json(
        &serde_json::json!({ "plan": plan, "started": true, "logs": evidence.len(), "evidence": list, "warnings": empty, "stopped": stopped, "next": format!("nitely test-sweep collect --logs {}{at_flag} --output report.json", list.display()) }),
    )
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Guide => println!("{}\n{}", test_sweep::GUIDE, test_sweep::OPTIMIZE_GUIDE),
        Command::Map {
            file,
            classes,
            slow,
            output,
        } => {
            let report = Report::read(&file)?;
            let flagged = report.flagged(slow);
            let Some(classes) = classes else {
                // The template: each flagged test, its evidence, and an empty class to fill.
                let template: Vec<_> = flagged
                    .iter()
                    .map(|f| serde_json::json!({ "test": f.test, "class": "", "why": f.why, "seconds": f.seconds, "path": f.path, "line": f.line, "hints": f.hints }))
                    .collect();
                return crate::print_json(&template);
            };
            let classes = test_sweep::read_classes(&classes)?;
            let map = test_sweep::render_map(&report.revision, &flagged, &classes)?;
            match output {
                Some(path) => std::fs::write(path, map)?,
                None => print!("{map}"),
            }
        }
        Command::Measure {
            out,
            runs,
            minutes,
            dispatch,
            lane,
            limit,
            issue,
        } => measure(out, runs, minutes, dispatch, lane, limit, issue)?,
        Command::Report { file } => crate::print_json(&Report::read(&file)?)?,
        Command::Propose {
            file,
            classes,
            slow,
            project,
            issue,
        } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            let cwd = crate::cwd()?;
            let project = nitely_core::resolve_project(&config, &db, project.as_deref(), &cwd)?;
            let root = nitely_core::playbook::repo_root(&config, &cwd)
                .with_context(|| format!("{cwd} is not inside a git repo"))?;
            let classes = test_sweep::read_classes(&classes)?;
            let repo = test_sweep::repo_facts(&config, &root)?;
            let facts = test_sweep::OptimizeFacts {
                slow_seconds: slow,
                tests: &repo.tests,
                ci_commands: &repo.ci_commands,
                has_ci: repo.has_ci,
                has_lanes: repo.has_lanes,
                playbook: repo.target.as_ref(),
            };
            crate::print_json(&Report::read(&file)?.propose(
                &mut db,
                &project,
                issue,
                &classes,
                &facts,
                &Actor::Agent("cli".into()),
            )?)?;
        }
        Command::Collect {
            logs,
            at,
            output,
            execute,
            rust_timings,
            known_failures,
            runs,
            seconds,
            issue,
        } => {
            let cwd = std::env::current_dir()?;
            let head = test_sweep::revision(&cwd)?;
            let revision = match at {
                Some(at) => test_sweep::ancestor_revision(&cwd, &at)?,
                None => head,
            };
            let output = if output.is_absolute() {
                output
            } else {
                cwd.join(output)
            };
            if output.exists() {
                bail!("output already exists; choose a new report path");
            }
            let logs: serde_json::Value = match logs {
                Some(file) => {
                    if file.metadata()?.len() > 1024 * 1024 {
                        bail!("log descriptor file exceeds 1 MiB");
                    }
                    serde_json::from_slice(&std::fs::read(file)?)?
                }
                None => serde_json::json!([]),
            };
            let known_failures: Vec<String> = match known_failures {
                Some(file) => {
                    if file.metadata()?.len() > 1024 * 1024 {
                        bail!("known-failure list exceeds 1 MiB");
                    }
                    serde_json::from_slice(&std::fs::read(file)?)?
                }
                None => Vec::new(),
            };
            let descriptors = logs.as_array().context("logs must be a JSON array")?;
            if descriptors.len() > 100 {
                bail!("at most 100 evidence logs");
            }
            for log in descriptors {
                let file = log["path"].as_str().context("log path missing")?;
                if !std::path::Path::new(file).is_absolute()
                    || log["run"].as_str().unwrap_or("").is_empty()
                    || log["revision"]
                        .as_str()
                        .is_none_or(|s| s.len() != 40 || !s.chars().all(|c| c.is_ascii_hexdigit()))
                {
                    bail!("each log needs an absolute path, distinct run ID, and actual 40-character revision");
                }
            }
            // Every log is read here, whatever its runner wrote (JUnit XML, libtest, TAP), into
            // the observations the collector classifies (NIT-553).
            let (mut observations, mut imported_runs, mut bytes, mut truncated) =
                (vec![], vec![], 0, false);
            for log in descriptors {
                let file = std::path::Path::new(log["path"].as_str().unwrap_or_default());
                bytes += file.metadata()?.len();
                if bytes > 16 * 1024 * 1024 || observations.len() >= 30000 {
                    truncated = true;
                    break;
                }
                let source = log["source"].as_str().map(String::from).unwrap_or_else(|| {
                    file.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                let text = String::from_utf8_lossy(&std::fs::read(file)?).into_owned();
                let method = serde_json::from_value(log["method"].clone())
                    .unwrap_or(test_sweep::Method::Imported);
                let (rows, run) = test_sweep::import(
                    &text,
                    log["run"].as_str().unwrap_or_default(),
                    log["revision"].as_str().unwrap_or_default(),
                    &source,
                    method,
                );
                // The descriptor may name the runner a JUnit file does not (Surefire, Gradle, .NET).
                let (mut rows, mut run) = (rows, run);
                if let Some(runner) = log["runner"].as_str() {
                    for row in rows.iter_mut().chain(std::iter::once(&mut run)) {
                        row["runner"] = runner.into();
                    }
                }
                observations.extend(rows);
                imported_runs.push(run);
            }
            let scratch = nitely_core::paths::unique_temp("nitely-test-sweep", "");
            std::fs::create_dir_all(&scratch)?;
            let script = scratch.join("collector.cjs");
            let options = scratch.join("options.json");
            std::fs::write(&script, test_sweep::COLLECTOR)?;
            std::fs::write(
                &options,
                serde_json::to_vec(
                    &serde_json::json!({"imported":{"observations":observations,"runs":imported_runs,"truncated":truncated},"output":output,"execute":execute,"runs":runs,"seconds":seconds,"revision":revision.trim(),"at":revision.trim(),"rust_timings":rust_timings,"known_failures":known_failures}),
                )?,
            )?;
            if execute {
                // Suites run on this machine's lanes only for a repo without CI (NIT-554).
                let here = Where::of(&cwd)?;
                if !matches!(here.ci_fact(), test_sweep::CiFact::Absent(_)) {
                    bail!("this repo has CI: measure there with `nitely test-sweep measure` (CI's logs, or --dispatch), not with --execute on this machine's lanes");
                }
                let config = Config::load()?;
                let db = Db::open_default()?;
                let command = format!("node {} {}", quoted(&script)?, quoted(&options)?);
                let result = nitely_core::lanes::run_in(
                    &config,
                    &db,
                    &cwd.to_string_lossy(),
                    issue.as_deref(),
                    &command,
                    &Actor::Agent("cli".into()),
                    &mut |_| {},
                )?;
                if result.exit_code != Some(0) {
                    bail!("collector lane {} failed: {}", result.id, result.output);
                }
            } else {
                let status = std::process::Command::new("node")
                    .arg(&script)
                    .arg(&options)
                    .current_dir(&cwd)
                    .status()?;
                if !status.success() {
                    bail!("report-only collector failed");
                }
            }
            let report = Report::read(&output)?;
            crate::print_json(
                &serde_json::json!({"report":output,"revision":report.revision,"observations":report.observations.len(),"tests":report.tests.len(),"candidates":report.candidates.len(),"truncated":report.truncated,"warnings":report.warnings}),
            )?;
            // Scratch contains only this invocation's generated inputs; preserve reports elsewhere.
            let _ = std::fs::remove_file(&script);
            let _ = std::fs::remove_file(&options);
            let _ = std::fs::remove_dir(&scratch);
        }
    }
    Ok(())
}
