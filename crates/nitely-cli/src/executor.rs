use anyhow::{bail, Result};
use clap::Subcommand;
use std::io::Read;
#[derive(Subcommand)]
pub enum Command {
    /// Read one execution request from stdin. Requires receive_projects in executors.json.
    Receive {
        #[arg(long)]
        project: String,
    },
    /// Read a completed receipt after a disconnect, without executing again.
    Result {
        #[arg(long)]
        project: String,
        id: String,
    },
}
pub fn run(command: Command) -> Result<()> {
    let project = match command {
        Command::Receive { project } => project,
        Command::Result { project, id } => {
            let reply = nitely_core::executor::result(
                &nitely_core::Db::open_default()?,
                &nitely_core::executor::Settings::load()?,
                &project,
                &id,
            )?;
            println!("{}", serde_json::to_string(&reply)?);
            return Ok(());
        }
    };
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(nitely_core::executor::MAX_REQUEST + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > nitely_core::executor::MAX_REQUEST {
        bail!("execution request exceeds 12 MiB");
    }
    let reply = nitely_core::executor::receive(
        &nitely_core::Config::load_read_only()?,
        &nitely_core::Db::open_default()?,
        &nitely_core::executor::Settings::load()?,
        &project,
        &bytes,
    )?;
    println!("{}", serde_json::to_string(&reply)?);
    Ok(())
}
