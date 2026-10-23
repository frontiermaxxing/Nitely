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
pub fn copy(name: &str, build: fn(&Path)) -> PathBuf {
    let base = crate::paths::unique_temp(&format!("nitely-{name}"), "");
    copy_to(name, &base, &|dir: &Path| build(dir));
    base
}

/// The template `name` (made by `build` in an empty folder the first time a test asks for it in
/// this test binary) copied to `to`: its git objects hardlinked, since git never changes one in
/// place, and everything else copied (NIT-527).
fn copy_to(name: &str, to: &Path, build: &dyn Fn(&Path)) {
    static TEMPLATES: Mutex<Option<HashMap<String, Arc<OnceLock<PathBuf>>>>> = Mutex::new(None);
    let cell = TEMPLATES.lock().unwrap().get_or_insert_with(HashMap::new).entry(name.to_string()).or_default().clone();
    let template = cell.get_or_init(|| {
        forget_old_templates();
        let dir = crate::paths::unique_temp(&format!("nitely-template-{name}"), "");
        std::fs::create_dir_all(&dir).unwrap();
        build(&dir);
        dir
    });
    copy_dir(template, to, template, to, false);
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

/// `objects` once inside a git directory's `objects` (a folder beside its `HEAD`): those files are
/// hardlinked, falling back to a copy where the file system cannot link.
fn copy_dir(from: &Path, to: &Path, template: &Path, base: &Path, objects: bool) {
    std::fs::create_dir_all(to).unwrap();
    let git_dir = from.join("HEAD").is_file();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&src, &dst, template, base, objects || (git_dir && entry.file_name() == "objects"));
        } else if objects {
            if std::fs::hard_link(&src, &dst).is_err() {
                std::fs::copy(&src, &dst).unwrap();
            }
        } else if entry.file_name() == "config" {
            let text = std::fs::read_to_string(&src).unwrap();
            std::fs::write(&dst, re_root(&text, template, base)).unwrap();
        } else {
            std::fs::copy(&src, &dst).unwrap_or_else(|error| panic!("copy fixture {} to {}: {error}", src.display(), dst.display()));
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
pub fn git(dir: &Path, args: &[&str]) -> String {
    let o = crate::playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
    assert!(o.status.success(), "git {args:?}: {} {}", o.status, String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A repo at `dir` on `main` with an identity (land and crew commit in their own git calls) and
/// no background maintenance, its files committed: each `(path, content)` in `files`, or an empty
/// commit when there are none.
/// The same files make the same repo, so it is built once per test binary and copied (NIT-527):
/// three git processes and a commit fewer for each test after the first. A folder that already
/// holds files is set up in place, since its files are committed too.
pub fn init(dir: &Path, files: &[(&str, &str)]) {
    if std::fs::read_dir(dir).map_or(true, |mut d| d.next().is_none()) {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        files.hash(&mut hash);
        let owned: Vec<(String, String)> = files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect();
        copy_to(&format!("init-{:016x}", hash.finish()), dir, &move |at: &Path| {
            let files: Vec<(&str, &str)> = owned.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
            init_here(at, &files)
        });
        return;
    }
    init_here(dir, files)
}

fn init_here(dir: &Path, files: &[(&str, &str)]) {
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

/// What a submit that passed with no person needed leaves for land: a passing review of the
/// branch's real patch at its head, and the issue In Review. For tests about what happens after
/// submit; a submit runs a gate and a reviewer, tens of git processes each (NIT-498).
pub(crate) fn reviewed(db: &mut crate::db::Db, worktree: &Path, key: &str, actor: &crate::issues::Actor) {
    let patch = crate::submit::patch_id(worktree, "main").unwrap();
    let head = git(worktree, &["rev-parse", "HEAD"]);
    let branch = git(worktree, &["branch", "--show-current"]);
    db.conn
        .execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, branch, reviewer, verdict, approver, findings, ts) VALUES (?1, ?2, ?3, ?4, 'fake', 'pass', 'agent', '[]', ?5)",
            rusqlite::params![key, patch, head, branch, crate::db::now()],
        )
        .unwrap();
    if db.issue(key).unwrap().is_some_and(|i| i.state != "In Review") {
        db.move_issue(key, "In Review", None, None, actor).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copys_paths_are_rewritten_in_every_spelling_git_writes() {
        let (template, base) = (Path::new(r"C:\t\tpl"), Path::new(r"C:\b\copy"));
        let text = r#"gitdir: C:/t/tpl/.git
url = C:\\t\\tpl\\remote
path C:\t\tpl\x"#;
        assert_eq!(re_root(text, template, base), r#"gitdir: C:/b/copy/.git
url = C:\\b\\copy\\remote
path C:\b\copy\x"#);
    }

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

    /// Repos from one template share their object files by hardlink, so a commit in one must
    /// leave the other as it was; a folder that already holds files commits them (NIT-527).
    #[test]
    fn repos_from_one_template_are_independent_and_a_full_folder_commits_its_files() {
        let base = crate::paths::unique_temp("nitely-init-template", "");
        let (a, b) = (base.join("a"), base.join("b"));
        init(&a, &[("x.txt", "one\n")]);
        init(&b, &[("x.txt", "one\n")]);
        let before = git(&b, &["rev-parse", "HEAD"]);
        std::fs::write(a.join("x.txt"), "two\n").unwrap();
        git(&a, &["commit", "-q", "-am", "two"]);
        git(&a, &["gc", "-q", "--prune=now"]);
        assert_eq!(git(&b, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(&b, &["status", "--porcelain"]), "");
        assert_eq!(git(&b, &["show", "HEAD:x.txt"]), "one");
        let full = base.join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("mine.txt"), "m").unwrap();
        init(&full, &[("x.txt", "one\n")]);
        assert_eq!(git(&full, &["ls-files"]), "mine.txt\nx.txt");
        std::fs::remove_dir_all(base).ok();
    }
}
