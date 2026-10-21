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
    /// Show existing evidence without executing tests.
    Report {
        file: PathBuf,
    },
    /// Create an editable human goal-plan proposal; never applies changes or approves it.
    Propose {
        file: PathBuf,
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
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Guide => println!("{}", test_sweep::GUIDE),
        Command::Report { file } => crate::print_json(&Report::read(&file)?)?,
        Command::Propose {
            file,
            project,
            issue,
        } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            let project =
                nitely_core::resolve_project(&config, &db, project.as_deref(), &crate::cwd()?)?;
            crate::print_json(&Report::read(&file)?.propose(
                &mut db,
                &project,
                issue,
                &Actor::Agent("cli".into()),
            )?)?;
        }
        Command::Collect {
            logs,
            output,
            execute,
            rust_timings,
            known_failures,
            runs,
            seconds,
            issue,
        } => {
            let cwd = std::env::current_dir()?;
            let revision = test_sweep::revision(&cwd)?;
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
                let (rows, run) = test_sweep::import(
                    &text,
                    log["run"].as_str().unwrap_or_default(),
                    log["revision"].as_str().unwrap_or_default(),
                    &source,
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
                    &serde_json::json!({"imported":{"observations":observations,"runs":imported_runs,"truncated":truncated},"output":output,"execute":execute,"runs":runs,"seconds":seconds,"revision":revision.trim(),"rust_timings":rust_timings,"known_failures":known_failures}),
                )?,
            )?;
            if execute {
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
