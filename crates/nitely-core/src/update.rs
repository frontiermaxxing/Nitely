//! What the app needs to update itself safely (docs/releasing.md): which channel this build came
//! from and which one the person follows, where each channel's manifest is, and whether a land is
//! running, so an update never lands in the middle of one. The download and the install are the
//! Tauri updater's (src-tauri/src/update.rs).

use std::path::Path;

/// The channel this build was released on: the release workflow sets `NITELY_CHANNEL` when it
/// builds. Anything else (`cargo build`, `npm run tauri dev`) is "dev", and a dev build never
/// checks for updates.
pub const BUILD_CHANNEL: &str = match option_env!("NITELY_CHANNEL") {
    Some(c) => c,
    None => "dev",
};

/// This build's version, the one the release workflow stamped.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The channels a person can follow, from the most settled to the newest.
pub const CHANNELS: [&str; 3] = ["stable", "beta", "nightly"];

const REPO: &str = "frontiermaxxing/Nitely";

/// The channel a person follows when they have not chosen one: the build's own, or stable.
pub fn default_channel() -> &'static str {
    if CHANNELS.contains(&BUILD_CHANNEL) {
        BUILD_CHANNEL
    } else {
        "stable"
    }
}

/// Where a channel's update manifest is (the release workflow keeps it current).
pub fn manifest_url(channel: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/updates/{channel}.json")
}

/// The release page for `version` on `channel`, for "what's new".
pub fn release_url(channel: &str, version: &str) -> String {
    if channel == "nightly" {
        format!("https://github.com/{REPO}/releases/tag/nightly")
    } else {
        format!("https://github.com/{REPO}/releases/tag/v{version}")
    }
}

/// Said before someone moves to a more settled channel than the build they run: its next update
/// may be an older version, which refuses a database this build already upgraded.
pub fn older_channel_warning(from: &str, to: &str) -> Option<String> {
    let rank = |c: &str| CHANNELS.iter().position(|x| *x == c);
    match (rank(from), rank(to)) {
        (Some(f), Some(t)) if t < f => Some(format!(
            "You run a {from} build. The {to} channel's newest version can be older than this one; Nitely updates to it only once it is newer. An older build than the one that last opened your data would refuse to start, so stay on {from} until {to} catches up if you need that."
        )),
        _ => None,
    }
}

/// The watched repos with a land running now: an update waits for them.
pub fn lands_running(config: &crate::Config) -> Vec<String> {
    config.repos.iter().filter(|r| land_running_in(Path::new(&r.root))).map(|r| r.name.clone()).collect()
}

fn land_running_in(root: &Path) -> bool {
    crate::playbook::git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .map(|dir| Path::new(dir.trim()).join(crate::land::LOCK_FILE).exists())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_urls_and_the_warning_for_an_older_channel() {
        assert_eq!(BUILD_CHANNEL, "dev", "tests are dev builds, which never check for updates");
        assert_eq!(default_channel(), "stable");
        assert_eq!(manifest_url("beta"), "https://github.com/frontiermaxxing/Nitely/releases/download/updates/beta.json");
        assert_eq!(release_url("stable", "0.2.0"), "https://github.com/frontiermaxxing/Nitely/releases/tag/v0.2.0");
        assert!(release_url("nightly", "0.2.0-nightly.20261016.g1a2b3c4").ends_with("/tag/nightly"));
        assert!(older_channel_warning("nightly", "stable").unwrap().contains("refuse to start"));
        assert!(older_channel_warning("beta", "stable").is_some());
        assert_eq!(older_channel_warning("stable", "nightly"), None, "a newer channel needs no warning");
        assert_eq!(older_channel_warning("dev", "stable"), None);
    }

    #[test]
    fn a_held_land_lock_means_a_land_is_running() {
        let repo = crate::paths::unique_temp("nitely-update", "");
        std::fs::create_dir_all(&repo).unwrap();
        crate::playbook::run_git(std::process::Command::new("git").args(["init", "-q", "-b", "main"]).arg(&repo)).unwrap();
        let config = crate::Config { repos: vec![crate::RepoConfig { name: "app".into(), root: repo.to_string_lossy().into_owned(), integration_branch: "main".into(), project: None }], ..Default::default() };
        assert!(lands_running(&config).is_empty());
        std::fs::write(repo.join(".git").join(crate::land::LOCK_FILE), "APP-1").unwrap();
        assert_eq!(lands_running(&config), ["app"]);
        std::fs::remove_dir_all(&repo).ok();
    }
}
