use anyhow::Result;
use clap::ValueEnum;
use nitely_core::{release_verification as release, Db};
use std::path::PathBuf;

#[derive(Clone, ValueEnum)]
enum Phase { Gate, Review, Check }

#[derive(clap::Args)]
pub struct Args {
    #[arg(value_enum)] phase: Phase,
    /// Trusted integration ref containing release policy (fetch it before verifying).
    #[arg(long, default_value = "main")] integration_ref: String,
    /// Original tagged source commit, before version stamping. Required for stamped candidates.
    #[arg(long, default_value = "HEAD")] source: String,
    /// Vendor that authored the release; used to enforce review independence.
    #[arg(long)] author_vendor: String,
    #[arg(long, default_value = ".")] worktree: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    let db = Db::open_default()?;
    let request = release::Request { worktree: &args.worktree, integration_ref: &args.integration_ref, source: &args.source, author_vendor: &args.author_vendor };
    let identity = match args.phase {
        Phase::Gate => release::gate(&db, &request)?,
        Phase::Review => release::review(&db, &request)?,
        Phase::Check => release::check(&db, &request)?,
    };
    crate::print_json(&identity)
}
