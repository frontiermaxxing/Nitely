//! Git repos for tests, built once per test run and copied for each test (NIT-208). Starting a git
//! process costs tens of milliseconds on Windows, more with the whole suite running, and most
//! fixtures make the same repo: `init`, a commit, a bare remote and a push.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

/// A new folder holding a copy of what `build` made in an empty folder, the first time a test
/// asked for `name`. Paths to the template in git's config files (a remote's URL) are changed to
/// the copy's.
pub(crate) fn copy(name: &str, build: fn(&Path)) -> PathBuf {
    static TEMPLATES: Mutex<Option<HashMap<String, Arc<OnceLock<PathBuf>>>>> = Mutex::new(None);
    let cell = TEMPLATES.lock().unwrap().get_or_insert_with(HashMap::new).entry(name.to_string()).or_default().clone();
    let template = cell.get_or_init(|| {
        forget_old_templates();
        let dir = crate::paths::unique_temp(&format!("nitely-template-{name}"), "");
        std::fs::create_dir_all(&dir).unwrap();
        build(&dir);
        dir
    });
    let base = crate::paths::unique_temp(&format!("nitely-{name}"), "");
    copy_dir(template, &base, template, &base);
    base
}

/// Removes templates made a day or more ago: a test run leaves its own behind, and none runs for
/// a day, so a run still using one is never left without it.
fn forget_old_templates() {
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    for entry in std::fs::read_dir(std::env::temp_dir()).into_iter().flatten().flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < day_ago);
        if old && entry.file_name().to_string_lossy().starts_with("nitely-template-") {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}

fn copy_dir(from: &Path, to: &Path, template: &Path, base: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&src, &dst, template, base);
        } else if entry.file_name() == "config" {
            let text = std::fs::read_to_string(&src).unwrap();
            std::fs::write(&dst, re_root(&text, template, base)).unwrap();
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// `text` with `template` in each spelling git may have written it (as given, with forward
/// slashes, or with its backslashes escaped) changed to `base` in the same spelling.
fn re_root(text: &str, template: &Path, base: &Path) -> String {
    let (t, b) = (template.to_string_lossy(), base.to_string_lossy());
    [(t.replace('\\', "\\\\"), b.replace('\\', "\\\\")), (t.to_string(), b.to_string()), (t.replace('\\', "/"), b.replace('\\', "/"))]
        .into_iter()
        .fold(text.to_string(), |text, (t, b)| text.replace(&t, &b))
}

/// Runs git in `dir` as user `t`, and returns its output; panics if it fails.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let o = crate::playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
    assert!(o.status.success(), "git {args:?}: {} {}", o.status, String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A repo at `dir` on `main` with an identity (land and crew commit in their own git calls) and
/// no background maintenance, its files committed: each `(path, content)` in `files`, or an empty
/// commit when there are none.
pub(crate) fn init(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    // No template: every `git init` would write a dozen hook samples for the virus scanner to read
    // (NIT-289).
    git(dir, &["init", "-q", "-b", "main", "--template="]);
    // Written straight into the config, not with `git config`: one process fewer for each.
    let config = dir.join(".git").join("config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[user]\n\tname = t\n\temail = t@t\n[maintenance]\n\tauto = false\n[gc]\n\tauto = 0\n");
    std::fs::write(&config, text).unwrap();
    for (path, content) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    if files.is_empty() {
        git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
    } else {
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }
}

/// A bare repo at `remote` that `repo` pushes `main` to as its `origin`.
pub(crate) fn add_remote(repo: &Path, remote: &Path) {
    crate::playbook::run_git(Command::new("git").args(["init", "-q", "--bare", "-b", "main"]).arg(remote)).unwrap();
    let config = remote.join("config");
    let text = std::fs::read_to_string(&config).unwrap() + "[receive]\n\tautogc = false\n[gc]\n\tauto = 0\n";
    std::fs::write(&config, text).unwrap();
    git(repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
    git(repo, &["push", "-q", "-u", "origin", "main"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_is_its_own_repo_with_its_own_remote() {
        let build = |dir: &Path| {
            init(&dir.join("repo"), &[("a.txt", "one\n")]);
            add_remote(&dir.join("repo"), &dir.join("remote.git"));
        };
        let (one, two) = (copy("copy-test", build), copy("copy-test", build));
        assert_ne!(one, two);
        for base in [&one, &two] {
            let url = git(&base.join("repo"), &["remote", "get-url", "origin"]);
            assert_eq!(crate::paths::normalize(&url), crate::paths::normalize(&base.join("remote.git").to_string_lossy()));
        }
        std::fs::write(one.join("repo").join("a.txt"), "two\n").unwrap();
        git(&one.join("repo"), &["commit", "-q", "-am", "two"]);
        git(&one.join("repo"), &["push", "-q", "origin", "main"]);
        assert_ne!(git(&one.join("remote.git"), &["rev-parse", "main"]), git(&two.join("remote.git"), &["rev-parse", "main"]), "pushed to its own remote");
        assert_eq!(git(&two.join("repo"), &["status", "--porcelain"]), "", "the other copy is untouched");
        for base in [one, two] {
            std::fs::remove_dir_all(base).ok();
        }
    }
}
