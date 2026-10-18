//! Times the board's git read on this machine's own repos (NIT-339): a cold read, a read after it
//! with nothing moved, a read after one worktree's index moved, and a whole snapshot. It reads
//! git and the database only; with a worktree given, it stages and unstages one scratch file there.
//!
//!   cargo run --release -p nitely-core --example board_timing -- [worktree to touch]

use std::process::Command;
use std::time::Instant;

use nitely_core::{snapshot, Config, Db};

fn main() -> anyhow::Result<()> {
    let config = Config::load_read_only()?;
    let db = Db::open_read_only(&nitely_core::paths::db_path())?;
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let idle = |_: &str| false;

    let t = Instant::now();
    let board = snapshot::snapshot(&config, &db)?;
    let full = ms(t);
    let worktrees: usize = board.repos.iter().map(|r| r.rows.len()).sum();

    let t = Instant::now();
    let cold = snapshot::read_git_from(&config, None, &idle);
    let cold_ms = ms(t);
    let t = Instant::now();
    let warm = snapshot::read_git_from(&config, Some(&cold), &idle);
    let warm_ms = ms(t);
    println!("{} repos, {worktrees} worktrees", config.repos.len());
    println!("snapshot, git read again:          {full:7.0} ms");
    let said = |s: &snapshot::GitState| format!("({} repos listed, {} worktrees counted)", s.relisted, s.recounted);
    println!("git read, cold:                    {cold_ms:7.0} ms {}", said(&cold));
    println!("git read, nothing moved:           {warm_ms:7.0} ms {}", said(&warm));

    if let Some(wt) = std::env::args().nth(1) {
        // A file of its own: created new, so nothing already there is touched.
        let name = format!("board-timing-{}-{}.txt", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
        let file = std::path::Path::new(&wt).join(&name);
        std::io::Write::write_all(&mut std::fs::File::create_new(&file)?, b"x")?;
        let git = |args: &[&str]| nitely_core::playbook::run_git(Command::new("git").arg("-C").arg(&wt).args(args));
        git(&["add", "--", &name])?;
        let t = Instant::now();
        let after = snapshot::read_git_from(&config, Some(&warm), &idle);
        println!("git read, one worktree's index moved: {:4.0} ms {}", ms(t), said(&after));
        git(&["rm", "-q", "--cached", "--", &name])?;
        std::fs::remove_file(&file).ok();
        drop(after);
    }
    Ok(())
}
