//! The listener for issue links that starts at login (NIT-229): `nitely links serve`, a small
//! process with no window that owns 127.0.0.1:7423 (links.rs). A link it gets goes to the app the
//! way `nitely://` does, so the OS starts the app with it when it is closed. That is what makes an
//! issue link pasted in a chat or a pull request open whenever the PC is on.
//!
//! Registered per user, with no admin rights: a value under the user's Run key on Windows, a
//! LaunchAgent on macOS, a systemd user unit on Linux (an XDG autostart entry where there is no
//! systemd). It is on once the person connects a repo (Agent setup proposes it, and connect applies
//! it), and off when they turn it off in Settings or with `nitely links uninstall`.
//!
//! One process per person, which later could also keep the crew and the async submit runner; for
//! now it only answers links. It never changes anything: it opens views.

use crate::links::{self, Role};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The value under the user's Run key on Windows.
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "NitelyLinks";
/// The LaunchAgent's label on macOS.
const LABEL: &str = "net.nitely.links";
/// The systemd user unit, and the XDG autostart entry where there is no systemd, on Linux.
const UNIT: &str = "nitely-links.service";
const DESKTOP: &str = "nitely-links.desktop";

/// Whether this process may touch the person's login items: never under the test harness, and
/// never while an agents' home other than theirs stands in (tests, the harness).
pub fn allowed() -> bool {
    !cfg!(test) && std::env::var_os("NITELY_HARNESS").is_none() && std::env::var_os("NITELY_AGENT_HOME").is_none()
}

/// Where and how the listener is registered on one OS.
#[derive(Debug, Clone, PartialEq)]
pub struct Registration {
    /// Where it shows: the Run value, the plist or the unit.
    pub place: String,
    /// What runs at login.
    pub command: String,
    /// Files to write (macOS and Linux).
    pub files: Vec<(PathBuf, String)>,
    /// Commands that register it (and start it where the OS does so).
    pub on: Vec<Vec<String>>,
    /// Commands that take it off again.
    pub off: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    /// Linux; `true` with systemd's user manager.
    Linux(bool),
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux(Command::new("systemctl").args(["--user", "--version"]).output().is_ok_and(|o| o.status.success()))
        }
    }
}

/// How `nitely` at `nitely` is registered on `os` for the person whose home is `home`.
pub fn registration(nitely: &Path, home: &Path, os: Os) -> Registration {
    let exe = nitely.to_string_lossy().into_owned();
    match os {
        Os::Windows => {
            // conhost's headless mode runs the console program with no window at all, at every login.
            let command = format!("conhost.exe --headless \"{exe}\" links serve");
            Registration {
                place: format!(r"{RUN_KEY}\{RUN_VALUE}"),
                command: command.clone(),
                files: vec![],
                on: vec![sv(&["reg", "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &command, "/f"])],
                off: vec![sv(&["reg", "delete", RUN_KEY, "/v", RUN_VALUE, "/f"])],
            }
        }
        Os::Mac => {
            let plist = home.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"));
            let text = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array><string>{}</string><string>links</string><string>serve</string></array>\n\
  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict>\n</plist>\n",
                xml(&exe)
            );
            let at = plist.to_string_lossy().into_owned();
            Registration {
                place: at.clone(),
                command: format!("{exe} links serve"),
                files: vec![(plist, text)],
                on: vec![sv(&["launchctl", "load", "-w", &at])],
                off: vec![sv(&["launchctl", "unload", "-w", &at])],
            }
        }
        Os::Linux(true) => {
            let unit = home.join(".config").join("systemd").join("user").join(UNIT);
            let text = format!(
                "[Unit]\nDescription=Nitely issue links (http://127.0.0.1:{}/i/KEY)\n\n[Service]\nExecStart=\"{exe}\" links serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
                links::PORT
            );
            Registration {
                place: unit.to_string_lossy().into_owned(),
                command: format!("\"{exe}\" links serve"),
                files: vec![(unit, text)],
                on: vec![sv(&["systemctl", "--user", "daemon-reload"]), sv(&["systemctl", "--user", "enable", "--now", UNIT])],
                off: vec![sv(&["systemctl", "--user", "disable", "--now", UNIT])],
            }
        }
        Os::Linux(false) => {
            let entry = home.join(".config").join("autostart").join(DESKTOP);
            let text = format!("[Desktop Entry]\nType=Application\nName=Nitely issue links\nExec=\"{exe}\" links serve\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n");
            Registration { place: entry.to_string_lossy().into_owned(), command: format!("\"{exe}\" links serve"), files: vec![(entry, text)], on: vec![], off: vec![] }
        }
    }
}

fn sv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Runs a helper (reg, launchctl, systemctl) with no window, even from the app.
fn quiet(words: &[String]) -> Result<String> {
    let mut cmd = Command::new(&words[0]);
    cmd.args(&words[1..]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().with_context(|| format!("running {}", words[0]))?;
    if !out.status.success() {
        bail!("{} failed: {}", words.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What runs at login now, if anything is registered.
pub fn registered_command(os: Os, reg: &Registration) -> Option<String> {
    match os {
        Os::Windows => {
            let out = quiet(&sv(&["reg", "query", RUN_KEY, "/v", RUN_VALUE])).ok()?;
            out.lines().find_map(|l| l.split_once("REG_SZ").map(|(_, v)| v.trim().to_string()))
        }
        _ => {
            let (path, _) = reg.files.first()?;
            std::fs::read_to_string(path).ok().map(|text| if reg.files.iter().all(|(_, want)| *want == text) { reg.command.clone() } else { text })
        }
    }
}

/// Where the listener stands, for the app's Settings, Agent setup and `nitely links status`.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// False under the harness or a stand-in agents' home: nothing here touches login items.
    pub allowed: bool,
    /// The person turned it off.
    pub off: bool,
    /// Something is registered to run at login.
    pub registered: bool,
    /// It is registered with this nitely's own command.
    pub current: bool,
    /// Who answers on the port now: "app", "service", or none.
    pub answering: Option<&'static str>,
    pub place: String,
    pub command: String,
    pub port: u16,
}

/// The listener's status for `nitely` (the path that should run at login).
pub fn status(config: &crate::Config, nitely: &Path) -> Status {
    let os = Os::current();
    let reg = registration(nitely, &crate::connect::agent_home(), os);
    let now = if allowed() { registered_command(os, &reg) } else { None };
    Status {
        allowed: allowed(),
        off: config.links_service == Some(false),
        registered: now.is_some(),
        current: now.as_deref() == Some(reg.command.as_str()),
        answering: links::probe().map(|r| if r == Role::App { "app" } else { "service" }),
        place: reg.place,
        command: reg.command,
        port: links::PORT,
    }
}

/// Whether `nitely` has the `links` command: one built before it would fail at every login.
pub fn has_links(nitely: &Path) -> bool {
    quiet(&[nitely.to_string_lossy().into_owned(), "links".into(), "--help".into()]).is_ok()
}

fn older(nitely: &Path) -> String {
    format!(
        "the nitely at {} is older than this app and has no `links` command; install the current one (`cargo run --release -p nitely-cli -- install` in a Nitely checkout) and try again",
        nitely.display()
    )
}

/// Registers the listener for `nitely` and starts it now, and records that the person wants it.
pub fn install(nitely: &Path) -> Result<Status> {
    if !allowed() {
        bail!("issue links are never registered at login under the test harness or a stand-in agents' home");
    }
    if !has_links(nitely) {
        bail!("{}", older(nitely));
    }
    let os = Os::current();
    let reg = registration(nitely, &crate::connect::agent_home(), os);
    // A registration from an older path goes first, so only one listener runs at login.
    if os != Os::Windows {
        for c in &reg.off {
            quiet(c).ok();
        }
    }
    for (path, text) in &reg.files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    for c in &reg.on {
        quiet(c)?;
    }
    let mut config = crate::Config::load()?;
    if config.links_service.is_some() {
        config.links_service = None;
        config.save()?;
    }
    // Windows and the XDG entry run it only at the next login: start it now too.
    if matches!(os, Os::Windows | Os::Linux(false)) && links::probe() != Some(Role::Service) {
        start(nitely)?;
    }
    forget_cached();
    Ok(status(&config, nitely))
}

/// Takes the listener off at login and records that the person does not want it; the one running
/// sees that and stops within a minute, and the app takes the port again.
pub fn uninstall(nitely: &Path) -> Result<Status> {
    if !allowed() {
        bail!("nothing to take off: issue links are never registered under the test harness or a stand-in agents' home");
    }
    let os = Os::current();
    let reg = registration(nitely, &crate::connect::agent_home(), os);
    // Taking off what is not there fails too, so a failure counts only when something stays registered.
    let mut failed = None;
    for c in &reg.off {
        if let Err(e) = quiet(c) {
            failed.get_or_insert(e);
        }
    }
    for (path, _) in &reg.files {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                failed.get_or_insert(anyhow::Error::new(e).context(format!("removing {}", path.display())));
            }
        }
    }
    forget_cached();
    if registered_command(os, &reg).is_some() {
        return Err(failed.unwrap_or_else(|| anyhow::anyhow!("still registered")).context(format!("could not take the issue links listener off at {}", reg.place)));
    }
    let mut config = crate::Config::load()?;
    config.links_service = Some(false);
    config.save()?;
    forget_cached();
    Ok(status(&config, nitely))
}

/// Starts `nitely links serve` now, detached and with no window.
pub fn start(nitely: &Path) -> Result<()> {
    if !allowed() {
        bail!("not under the test harness or a stand-in agents' home");
    }
    let mut cmd = Command::new(nitely);
    cmd.args(["links", "serve"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000 | 0x0000_0200 | 0x0000_0008); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {} links serve", nitely.display()))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

static REGISTERED: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Whether the listener is registered at login, asked of the OS at most every 30 seconds. Whichever
/// nitely it runs: the app yields the port to it without having to find a nitely itself.
pub fn registered() -> bool {
    if !allowed() {
        return false;
    }
    let mut cache = REGISTERED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, yes)) = *cache {
        if at.elapsed() < Duration::from_secs(30) {
            return yes;
        }
    }
    let os = Os::current();
    let yes = registered_command(os, &registration(Path::new("nitely"), &crate::connect::agent_home(), os)).is_some();
    *cache = Some((Instant::now(), yes));
    yes
}

fn forget_cached() {
    *REGISTERED.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Opens `nitely://issue/KEY` the way the OS opens any link: in the running app, or by starting it.
pub fn open_in_app(key: &str) {
    let url = crate::issues::issue_link(key);
    let words = match Os::current() {
        Os::Windows => sv(&["rundll32", "url.dll,FileProtocolHandler", &url]),
        Os::Mac => sv(&["open", &url]),
        Os::Linux(_) => sv(&["xdg-open", &url]),
    };
    if let Err(e) = quiet(&words) {
        eprintln!("issue links: could not open {url}: {e:#}");
    }
}

/// `nitely links serve`: answers issue links on the port while it is registered (or for as long as
/// it runs, when started by hand), each one opening the app. Another listener already on the port
/// means this one is not needed; something else holding it is waited out.
pub fn serve() -> Result<()> {
    if !allowed() {
        bail!("issue links are not served at login under the test harness or a stand-in agents' home");
    }
    // One listener at a time: the app starts one whenever nothing answers, and a second one would
    // wait beside the first for the port (NIT-257).
    let Some(_one) = only_one(&crate::paths::data_dir())? else { return Ok(()) };
    let by_login = registered();
    let keep = || !by_login || registered();
    loop {
        match links::bind() {
            Ok(listener) => {
                links::serve(listener, Role::Service, |key| open_in_app(&key), keep);
                return Ok(());
            }
            Err(_) if links::probe() == Some(Role::Service) => return Ok(()),
            Err(_) => {
                if !keep() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(15));
            }
        }
    }
}

/// The listener's lock in `dir`, or None while another listener holds it. An OS file lock, so a
/// listener that dies lets go of it.
fn only_one(dir: &Path) -> Result<Option<std::fs::File>> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("links-serve.lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// Agent setup's change for issue links: what `nitely links install` would do, when it is not done
/// and the person has not turned it off; nothing otherwise.
pub fn setup_change(config: &crate::Config, nitely: &str) -> Option<crate::connect::Change> {
    if !allowed() || config.links_service == Some(false) {
        return None;
    }
    let path = crate::connect::resolve_nitely(nitely)?;
    let os = Os::current();
    let reg = registration(&path, &crate::connect::agent_home(), os);
    let now = registered_command(os, &reg);
    let mut action = if now.as_deref() == Some(reg.command.as_str()) { "unchanged" } else { "run" };
    // Running `links install` with a nitely that has no such command fails with clap's words alone.
    if action == "run" && !has_links(&path) {
        action = "manual";
    }
    Some(crate::connect::Change {
        target: reg.place,
        action: action.into(),
        why: format!(
            "a small listener that starts at login, so issue links (http://127.0.0.1:{}/i/KEY) open Nitely from chats and pull requests even when the app is closed; Settings turns it off",
            links::PORT
        ),
        before: now,
        after: if action == "manual" { older(&path) } else { format!("\"{}\" links install", path.display()) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_per_user_on_every_os_without_admin() {
        let home = Path::new("/home/you");
        let nitely = Path::new("C:/Users/you/.cargo/bin/nitely.exe");
        let w = registration(nitely, home, Os::Windows);
        assert_eq!(w.place, r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run\NitelyLinks");
        assert_eq!(w.command, "conhost.exe --headless \"C:/Users/you/.cargo/bin/nitely.exe\" links serve");
        assert!(w.on[0].starts_with(&sv(&["reg", "add", RUN_KEY])) && w.on[0].contains(&w.command), "{:?}", w.on);
        assert!(w.on.iter().chain(&w.off).all(|c| !c.iter().any(|a| a.contains("HKLM"))), "the user's own key only");

        let nitely = Path::new("/usr/local/bin/nitely");
        let m = registration(nitely, home, Os::Mac);
        assert_eq!(PathBuf::from(&m.place), home.join("Library").join("LaunchAgents").join("net.nitely.links.plist"));
        assert!(m.files[0].1.contains("<string>/usr/local/bin/nitely</string><string>links</string><string>serve</string>") && m.files[0].1.contains("<key>RunAtLoad</key><true/>"));
        let l = registration(nitely, home, Os::Linux(true));
        assert!(PathBuf::from(&l.place) == home.join(".config").join("systemd").join("user").join("nitely-links.service") && l.files[0].1.contains("ExecStart=\"/usr/local/bin/nitely\" links serve"));
        assert!(l.on.iter().any(|c| c.join(" ") == "systemctl --user enable --now nitely-links.service"));
        let x = registration(nitely, home, Os::Linux(false));
        assert!(PathBuf::from(&x.place) == home.join(".config").join("autostart").join("nitely-links.desktop") && x.files[0].1.contains("Exec=\"/usr/local/bin/nitely\" links serve"));
    }

    #[test]
    fn one_listener_at_a_time() {
        let dir = crate::paths::unique_temp("nitely-links-lock", "");
        let first = only_one(&dir).unwrap();
        assert!(first.is_some(), "the first listener takes the lock");
        assert!(only_one(&dir).unwrap().is_none(), "a second one finds it held and leaves");
        drop(first);
        assert!(only_one(&dir).unwrap().is_some(), "free again once the first is gone");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_nitely_without_links_is_named_not_run() {
        let missing = Path::new("C:/no/such/nitely.exe");
        assert!(!has_links(missing));
        let why = older(missing);
        assert!(why.contains("older than this app") && why.contains("nitely-cli -- install"), "{why}");
    }

    #[test]
    fn never_touches_login_items_from_tests() {
        // Tests (and the harness, and a stand-in agents' home) never register anything at login.
        assert!(!allowed());
        let nitely = Path::new("nitely");
        assert!(install(nitely).unwrap_err().to_string().contains("never registered"));
        assert!(uninstall(nitely).is_err() && serve().is_err() && start(nitely).is_err());
        assert!(!registered());
        assert!(setup_change(&crate::Config::default(), "nitely").is_none());
    }
}
