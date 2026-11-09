//! Which build lands approved changes (NIT-752). The desktop app lands them itself, as
//! `nitely:auto-land`, every few seconds (`land::land_approved`). Land installs a newer `nitely`
//! after each change to Nitely's code, but a running app keeps its own code: an app started a day
//! earlier landed on day-old rules, a fixed check included, and nothing said so. So between rounds
//! the app compares its build with the installed one, and when the installed one is newer each round
//! runs there (`nitely land-approved`), the way a running MCP server forwards its calls (NIT-327).

use crate::{Actor, Db};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The commit a build was made from, and that commit's time (seconds): what `nitely build-info`
/// prints, and what the app has built in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub commit: String,
    pub committed_at: i64,
}

impl Build {
    /// A build as its build script stamped it; None when git could not say (an unknown build is
    /// never called newer or older).
    pub fn stamped(commit: &str, committed_at: &str) -> Option<Build> {
        let committed_at = committed_at.trim().parse().ok()?;
        let commit = commit.trim();
        (!commit.is_empty() && committed_at > 0).then(|| Build { commit: commit.to_string(), committed_at })
    }

    fn short(&self) -> &str {
        &self.commit[..self.commit.len().min(7)]
    }
}

/// Whether the installed build is newer than the app's: another commit, made later. Land rebases
/// a change onto the integration branch before it fast-forwards, so a later commit there has a
/// later time. A build the app cannot read (an old `nitely` without `build-info`, one still being
/// copied in) is not newer: the app lands itself, as before.
pub fn installed_is_newer(app: Option<&Build>, installed: Option<&Build>) -> bool {
    match (app, installed) {
        (Some(app), Some(installed)) => installed.commit != app.commit && installed.committed_at > app.committed_at,
        _ => false,
    }
}

/// What one round sends to `nitely land-approved` on stdin and gets back on stdout: the last error
/// noted per issue, kept by the app across rounds, so a lasting one still goes on the issue once.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Round {
    pub reported: HashMap<String, String>,
    /// The issues this round landed.
    #[serde(default)]
    pub landed: Vec<String>,
}

/// `nitely land-approved`: one round of `land_approved` on `input` (a `Round`), the round after it
/// as JSON.
pub fn run_round(db: &mut Db, config: &crate::Config, input: &str) -> Result<String> {
    let mut round: Round = if input.trim().is_empty() { Round::default() } else { serde_json::from_str(input).context("reading the round")? };
    let landed = crate::land::land_approved(db, config, &mut round.reported)?;
    round.landed = landed.into_iter().filter(|l| l.landed).map(|l| l.issue).collect();
    Ok(serde_json::to_string(&round)?)
}

/// `exe` started with no window of its own (the app is a windowed program).
pub fn command(exe: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// The build `exe` says it is; None when it cannot say.
pub fn build_of(mut cmd: Command) -> Option<Build> {
    let out = cmd.arg("build-info").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| serde_json::from_slice(&out.stdout).ok()).flatten()
}

/// Runs one round in the build `cmd` starts, carrying `reported` through it.
pub fn round_with(mut cmd: Command, reported: &mut HashMap<String, String>) -> Result<Vec<String>> {
    use std::io::Write;
    let mut child = cmd.arg("land-approved").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let input = serde_json::to_string(&Round { reported: reported.clone(), landed: Vec::new() })?;
    child.stdin.take().context("no stdin")?.write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let round: Round = serde_json::from_slice(&out.stdout).context("reading the installed build's round")?;
    *reported = round.reported;
    Ok(round.landed)
}

/// An installed build as last read: its path, its file's stamp, and what it said it is.
type Seen = (PathBuf, Option<(u64, std::time::SystemTime)>, Option<Build>);

/// The app's lander: lands in-process, or in the installed build once that is newer.
pub struct AutoLander {
    app: Option<Build>,
    /// The installed build last read, by its path and file stamp, so it is asked again only once
    /// an install replaced it.
    seen: Option<Seen>,
    /// The installed build rounds were last handed to; said once per build.
    handed_to: Option<String>,
    pub reported: HashMap<String, String>,
}

/// Where a round ran.
#[derive(Debug, PartialEq)]
pub enum Ran {
    InProcess,
    Installed(PathBuf),
}

impl AutoLander {
    pub fn new(app: Option<Build>) -> Self {
        AutoLander { app, seen: None, handed_to: None, reported: HashMap::new() }
    }

    /// The installed build at `exe`, read again only when its file changed.
    fn installed(&mut self, exe: &Path, start: impl Fn(&Path) -> Command) -> Option<Build> {
        let stamp = crate::install::stamp(exe);
        if let Some((path, was, build)) = &self.seen {
            if path == exe && *was == stamp {
                return build.clone();
            }
        }
        let build = build_of(start(exe));
        self.seen = Some((exe.to_path_buf(), stamp, build.clone()));
        build
    }

    /// Where this round should run: the installed build at `installed` when it is newer than the
    /// app, else here. The first round handed to a build says so on the board.
    pub fn choose(&mut self, db: &Db, installed: Option<&Path>, start: impl Fn(&Path) -> Command) -> Result<Ran> {
        let Some(exe) = installed else { return Ok(Ran::InProcess) };
        let build = self.installed(exe, start);
        if !installed_is_newer(self.app.as_ref(), build.as_ref()) {
            if self.handed_to.take().is_some() {
                db.log_event(&Actor::Agent(crate::land::AUTO_LANDER.into()), None, None, "status", "The app lands approved changes itself again: the installed nitely is no newer than the app")?;
            }
            return Ok(Ran::InProcess);
        }
        let (app, build) = (self.app.as_ref().expect("newer than a known build"), build.expect("newer"));
        if self.handed_to.as_deref() != Some(build.commit.as_str()) {
            let line = format!(
                "The app is older than the installed nitely ({} against {}): approved changes now land with the installed build, at {}. Restart the app to run the new build itself",
                app.short(),
                build.short(),
                exe.display()
            );
            eprintln!("{line}");
            db.log_event(&Actor::Agent(crate::land::AUTO_LANDER.into()), None, None, "status", &line)?;
            self.handed_to = Some(build.commit.clone());
        }
        Ok(Ran::Installed(exe.to_path_buf()))
    }

    /// One round of landing approved changes, in the newest build at hand. A newer installed build
    /// that cannot run the round (it failed to start) lands nothing this round: the old code here
    /// is the one known to be behind, so it does not step in.
    pub fn round(&mut self, config: &crate::Config, installed: Option<&Path>) -> Result<Ran> {
        let mut db = Db::open_default()?;
        let ran = self.choose(&db, installed, command)?;
        match &ran {
            Ran::InProcess => {
                crate::land::land_approved(&mut db, config, &mut self.reported)?;
            }
            Ran::Installed(exe) => {
                drop(db);
                round_with(command(exe), &mut self.reported).with_context(|| format!("landing approved changes with {}", exe.display()))?;
            }
        }
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(commit: &str, at: i64) -> Option<Build> {
        Some(Build { commit: commit.into(), committed_at: at })
    }

    #[test]
    fn only_a_later_commit_installed_takes_the_lands() {
        // The app a day behind what land installed since (the case NIT-752 met).
        assert!(installed_is_newer(build("aaa", 100).as_ref(), build("bbb", 200).as_ref()));
        // The same build (the harness's nitely beside its app), an older install (an app run from
        // a newer checkout), or a build either side cannot name: the app lands itself.
        assert!(!installed_is_newer(build("aaa", 100).as_ref(), build("aaa", 100).as_ref()));
        assert!(!installed_is_newer(build("bbb", 200).as_ref(), build("aaa", 100).as_ref()));
        assert!(!installed_is_newer(None, build("bbb", 200).as_ref()));
        assert!(!installed_is_newer(build("aaa", 100).as_ref(), None));
        assert_eq!(Build::stamped("", "100"), None);
        assert_eq!(Build::stamped("abc", ""), None);
    }
}
