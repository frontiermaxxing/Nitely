use anyhow::Result;
use clap::{Args, Subcommand};
use nitely_core::performance::{self, Store};
use nitely_crew::performance_query::{self, Query, View};

#[derive(Args)]
pub struct Range {
    /// Inclusive UTC Unix milliseconds.
    #[arg(long)] from_ms: Option<i64>,
    #[arg(long)] to_ms: Option<i64>,
    /// Ten minutes ending at the previous boot's last retained sample.
    #[arg(long)] before_last_restart: bool,
    #[arg(long, default_value_t = 1000)] limit: usize,
    /// Recorded project key for attribution/events, including historical keys before a rename.
    #[arg(long)] project: Option<String>,
}
impl Range {
    fn query(self, view: View) -> Query { Query { view, from_ms: self.from_ms, to_ms: self.to_ms, before_last_restart: self.before_last_restart, limit: self.limit, project: self.project } }
}

#[derive(Subcommand)]
pub enum Command {
    /// Current cached health and recent pressure. Does not start a collector.
    Now { #[arg(long)] project: Option<String> },
    /// Bounded retained history, with explicit gaps, units and freshness.
    History { #[command(flatten)] range: Range },
    /// Sustained sampled pressure, separated from inferred causes.
    Episodes { #[command(flatten)] range: Range },
    /// Compact diagnostic JSON. Writes a new file only; existing files are never overwritten.
    Export { #[command(flatten)] range: Range, #[arg(long)] output: Option<std::path::PathBuf> },
    /// Keep recording independently of the desktop. With no duration, runs until interrupted.
    Record { #[arg(long)] seconds: Option<u64> },
    /// Read the latest saved sample and recording settings; does not start a collector.
    Status,
    /// Pause/resume recording and set bounded local retention.
    Configure { #[arg(long)] enabled: Option<bool>, #[arg(long)] detailed_hours: Option<u32>, #[arg(long)] summary_days: Option<u32> },
    /// Delete history after recording is paused. Settings are retained.
    Clear,
}
pub fn run(command: Command) -> Result<()> {
    let dir = performance::directory();
    match command {
        Command::Now { project } => crate::print_json(&performance_query::query(&dir, &Query { project, ..Query::default() })?)?,
        Command::History { range } => crate::print_json(&performance_query::query(&dir, &range.query(View::History))?)?,
        Command::Episodes { range } => crate::print_json(&performance_query::query(&dir, &range.query(View::Episodes))?)?,
        Command::Export { range, output } => {
            let report = performance_query::query(&dir, &range.query(View::History))?;
            if let Some(output) = output {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&output)?;
                file.write_all(serde_json::to_string(&report)?.as_bytes())?;
                file.sync_all()?;
                crate::print_json(&serde_json::json!({"exported": output}))?;
            } else { println!("{}", serde_json::to_string(&report)?); }
        }
        Command::Record { seconds } => {
            // The handler only signals; normal stack unwinding joins the collector and saves
            // its clean-stop marker. Install it before recording can acquire ownership.
            let (stop, stopped) = std::sync::mpsc::sync_channel(1);
            ctrlc::set_handler(move || { let _ = stop.try_send(()); })?;
            let _recording = performance::background(dir);
            if let Some(seconds) = seconds { let _ = stopped.recv_timeout(std::time::Duration::from_secs(seconds)); }
            else { let _ = stopped.recv(); }
        }
        Command::Status => {
            let store = Store::read(&dir)?;
            let settings = store.as_ref().map(Store::settings).transpose()?.unwrap_or_default();
            let samples = store.as_ref().map(|s| s.samples(i64::MIN, i64::MAX, 1)).transpose()?.unwrap_or_default();
            let session = store.as_ref().map(Store::sessions).transpose()?.unwrap_or_default().into_iter()
                .find(|session| samples.first().is_some_and(|sample| sample.session == session.session));
            let error = std::fs::read_to_string(dir.join("last-error.txt")).ok();
            crate::print_json(&serde_json::json!({ "settings": settings, "latest": samples.first(), "session": session, "last_error": error, "cadence_ms": performance::CADENCE_MS }))?;
        }
        Command::Configure { enabled, detailed_hours, summary_days } => {
            let store = Store::open(&dir)?;
            let mut settings = store.settings()?;
            if let Some(value) = enabled { settings.enabled = value; }
            if let Some(value) = detailed_hours { settings.detailed_hours = value; }
            if let Some(value) = summary_days { settings.summary_days = value; }
            store.configure(&settings)?;
            crate::print_json(&settings)?;
        }
        Command::Clear => {
            performance::clear_history(&dir)?;
            crate::print_json(&serde_json::json!({ "cleared": true }))?;
        }
    }
    Ok(())
}
