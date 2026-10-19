use anyhow::Result;
use clap::Subcommand;
use nitely_core::performance::{self, Store};

#[derive(Subcommand)]
pub enum Command {
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
        Command::Record { seconds } => {
            let _recording = performance::background(dir);
            if let Some(seconds) = seconds { std::thread::sleep(std::time::Duration::from_secs(seconds)); }
            else { loop { std::thread::park(); } }
        }
        Command::Status => {
            let store = Store::read(&dir)?;
            let settings = store.as_ref().map(Store::settings).transpose()?.unwrap_or_default();
            let samples = store.as_ref().map(|s| s.samples(i64::MIN, i64::MAX, 1)).transpose()?.unwrap_or_default();
            let error = std::fs::read_to_string(dir.join("last-error.txt")).ok();
            crate::print_json(&serde_json::json!({ "settings": settings, "latest": samples.first(), "last_error": error, "cadence_ms": performance::CADENCE_MS }))?;
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
