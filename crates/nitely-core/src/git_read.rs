//! Git reads answered in this process, without starting git (NIT-518). Starting a program can
//! cost hundreds of milliseconds on a machine whose security software hooks process creation, and
//! a land, a queue run or a board read asks git hundreds of questions. `playbook::git` asks here
//! first; a question this module does not know, or cannot answer exactly as git would, gets None
//! and goes to the git CLI as before. Writes always go to the CLI.
//!
//! Every answer is the text git prints for those arguments, so callers cannot tell the difference.
//! Anything unusual (an option this module does not know, a revision with `@`, `:` or `..`, a path
//! git would quote, a merge with several bases, a repository gix cannot open) falls back.

use anyhow::Result;
use std::path::Path;

/// git's answer to `args` in `dir`, read in process, or None to ask the git CLI.
pub(crate) fn answer(dir: &Path, args: &[&str]) -> Option<Result<String>> {
    if env_redirects_git() {
        return None;
    }
    let question = Question::parse(args)?;
    let repo = open(dir)?;
    // Below the top of a checkout, ls-tree lists that folder's part of the tree.
    if matches!(question, Question::LsTree(_)) && repo.workdir().is_some() && !dir.join(".git").exists() {
        return None;
    }
    question.answer(&repo, args)
}

/// What this module answers, by the arguments git was given.
#[derive(Debug, PartialEq)]
enum Question<'a> {
    /// `rev-parse [--verify] [--quiet] REV...`: each REV's object id on its own line.
    RevParse { revs: Vec<&'a str>, verify: bool, quiet: bool },
    /// `merge-base A B`.
    MergeBase(&'a str, &'a str),
    /// `cat-file -s OBJ`, `cat-file -t OBJ`, `cat-file -e OBJ`, `cat-file blob OBJ`.
    CatFile(CatFile, &'a str),
    /// `merge-base --is-ancestor A B`: exit 0 or 1, saying nothing.
    IsAncestor(&'a str, &'a str),
    /// `ls-tree -r -z TREE`: every file below TREE, depth first in tree order.
    LsTree(&'a str),
    /// `rev-list --count A..B`.
    CountRange(&'a str, &'a str),
    /// `branch --show-current`.
    CurrentBranch,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum CatFile {
    Size,
    Type,
    Exists,
    Blob,
}

impl<'a> Question<'a> {
    fn parse(args: &[&'a str]) -> Option<Question<'a>> {
        match args {
            ["rev-parse", rest @ ..] => {
                let mut verify = false;
                let mut quiet = false;
                let mut revs = Vec::new();
                for a in rest {
                    match *a {
                        "--verify" => verify = true,
                        "--quiet" | "-q" => quiet = true,
                        r if plain_rev(r) => revs.push(r),
                        _ => return None,
                    }
                }
                // Without --verify, git checks --quiet only with --verify; with it, one revision.
                if revs.is_empty() || (verify && revs.len() != 1) || (quiet && !verify) {
                    return None;
                }
                Some(Question::RevParse { revs, verify, quiet })
            }
            ["merge-base", a, b] if plain_rev(a) && plain_rev(b) => Some(Question::MergeBase(a, b)),
            ["merge-base", "--is-ancestor", a, b] if plain_rev(a) && plain_rev(b) => Some(Question::IsAncestor(a, b)),
            ["ls-tree", "-r", "-z", t] if plain_rev(t) => Some(Question::LsTree(t)),
            ["rev-list", "--count", range] => {
                let (a, b) = range.split_once("..")?;
                (plain_rev(a) && plain_rev(b)).then_some(Question::CountRange(a, b))
            }
            ["branch", "--show-current"] => Some(Question::CurrentBranch),
            ["cat-file", how, obj] if object_name(obj) => {
                let how = match *how {
                    "-s" => CatFile::Size,
                    "-t" => CatFile::Type,
                    "-e" => CatFile::Exists,
                    "blob" => CatFile::Blob,
                    _ => return None,
                };
                Some(Question::CatFile(how, obj))
            }
            _ => None,
        }
    }

    fn answer(&self, repo: &gix::Repository, args: &[&str]) -> Option<Result<String>> {
        match self {
            Question::RevParse { revs, verify, quiet } => {
                let mut out = String::new();
                for rev in revs {
                    // A whole id is taken as it is written, whether or not the object is there.
                    if let Ok(id) = gix::ObjectId::from_hex(rev.as_bytes()) {
                        out.push_str(&id.to_string());
                        out.push('\n');
                        continue;
                    }
                    match resolve(repo, rev)? {
                        Some(id) => {
                            out.push_str(&id.to_string());
                            out.push('\n');
                        }
                        // A name that is surely nothing: --verify --quiet says so with exit 1 and
                        // no words; anything else is git's own message, so git says it.
                        None if *verify && *quiet => return Some(Err(said_nothing(args, 1))),
                        None => return None,
                    }
                }
                Some(Ok(out))
            }
            Question::MergeBase(a, b) => {
                let a = resolve(repo, &format!("{a}^{{commit}}"))??;
                let b = resolve(repo, &format!("{b}^{{commit}}"))??;
                let bases = repo.merge_bases_many(a, &[b.detach()]).ok()?;
                // git picks one of several bases by its own order; leave that to git.
                match bases.as_slice() {
                    [one] => Some(Ok(format!("{one}\n"))),
                    [] => Some(Err(said_nothing(args, 1))),
                    _ => None,
                }
            }
            Question::IsAncestor(a, b) => {
                let a = resolve(repo, &format!("{a}^{{commit}}"))??.detach();
                let b = resolve(repo, &format!("{b}^{{commit}}"))??.detach();
                let yes = a == b || repo.merge_bases_many(a, &[b]).ok()?.iter().any(|m| m.detach() == a);
                Some(if yes { Ok(String::new()) } else { Err(said_nothing(args, 1)) })
            }
            Question::LsTree(t) => {
                let tree = resolve(repo, &format!("{t}^{{tree}}"))??.detach();
                let mut out = Vec::new();
                list_files(repo, tree, &mut Vec::new(), &mut out)?;
                Some(Ok(String::from_utf8_lossy(&out).into_owned()))
            }
            Question::CountRange(a, b) => {
                let a = resolve(repo, &format!("{a}^{{commit}}"))??.detach();
                let b = resolve(repo, &format!("{b}^{{commit}}"))??.detach();
                Some(Ok(format!("{}\n", count(repo, b, a)?)))
            }
            Question::CurrentBranch => match repo.head_name().ok()? {
                Some(name) => {
                    let branch = name.as_bstr().strip_prefix(b"refs/heads/")?;
                    Some(Ok(format!("{}\n", std::str::from_utf8(branch).ok()?)))
                }
                None => Some(Ok(String::new())),
            },
            Question::CatFile(how, obj) => {
                let id = match gix::ObjectId::from_hex(obj.as_bytes()) {
                    Ok(id) => id,
                    Err(_) => resolve(repo, obj)??.detach(),
                };
                let object = repo.find_object(id).ok();
                match (how, object) {
                    (CatFile::Exists, Some(_)) => Some(Ok(String::new())),
                    // Only an id that names nothing is a quiet exit 1; a name that does not
                    // resolve is git's fatal message.
                    (CatFile::Exists, None) if gix::ObjectId::from_hex(obj.as_bytes()).is_ok() => Some(Err(said_nothing(args, 1))),
                    (_, None) => None,
                    (CatFile::Size, Some(o)) => Some(Ok(format!("{}\n", o.data.len()))),
                    (CatFile::Type, Some(o)) => Some(Ok(format!("{}\n", o.kind))),
                    (CatFile::Blob, Some(o)) if o.kind == gix::object::Kind::Blob => Some(Ok(String::from_utf8_lossy(&o.data).into_owned())),
                    (CatFile::Blob, Some(_)) => None,
                }
            }
        }
    }
}

/// Appends `ls-tree -r -z`'s entries for the files below `tree`, whose path is `prefix`. None for a
/// mode git would print differently from the usual five.
fn list_files(repo: &gix::Repository, tree: gix::ObjectId, prefix: &mut Vec<u8>, out: &mut Vec<u8>) -> Option<()> {
    use gix::object::tree::EntryKind;
    let object = repo.find_object(tree).ok()?;
    let decoded = gix::objs::TreeRef::from_bytes(&object.data, repo.object_hash()).ok()?;
    let entries: Vec<_> = decoded.entries.iter().map(|e| (e.mode, e.filename.to_vec(), e.oid.to_owned())).collect();
    drop(object);
    for (mode, name, oid) in entries {
        let kind = mode.kind();
        let octal = kind.as_octal_str().to_string();
        if u32::from(mode.value()) != u32::from_str_radix(&octal, 8).ok()? {
            return None;
        }
        let len = prefix.len();
        if len > 0 {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(&name);
        if kind == EntryKind::Tree {
            list_files(repo, oid, prefix, out)?;
        } else {
            let what = if kind == EntryKind::Commit { "commit" } else { "blob" };
            out.extend_from_slice(format!("{octal} {what} {oid}\t").as_bytes());
            out.extend_from_slice(prefix);
            out.push(0);
        }
        prefix.truncate(len);
    }
    Some(())
}

/// The object `spec` names: Some(None) when it surely names nothing (a full ref name, `refs/...`,
/// that does not exist), None when gix could not say for sure.
fn resolve<'r>(repo: &'r gix::Repository, spec: &str) -> Option<Option<gix::Id<'r>>> {
    if let Ok(id) = repo.rev_parse_single(spec) {
        return Some(Some(id));
    }
    let name = spec.strip_suffix("^{commit}").or_else(|| spec.strip_suffix("^{tree}")).unwrap_or(spec);
    if name.starts_with("refs/") && !name.contains(['~', '^']) {
        if let Ok(None) = repo.try_find_reference(name) {
            return Some(None);
        }
    }
    None
}

/// A revision this module resolves: HEAD, a ref or branch name, or an object id, with at most
/// `^{commit}`, `^{tree}`, `^`, `~N` after it. Nothing with `@`, `:`, `..`, a leading `-`, or a
/// short id, whose meaning depends on what else the repository holds.
fn plain_rev(rev: &str) -> bool {
    let mut base = rev;
    for suffix in ["^{commit}", "^{tree}"] {
        if let Some(b) = base.strip_suffix(suffix) {
            base = b;
            break;
        }
    }
    while let Some((b, n)) = base.rsplit_once('~') {
        if !n.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        base = b;
    }
    let base = base.trim_end_matches('^');
    if base.is_empty() || base.starts_with('-') || base.contains("..") || base.ends_with('/') || base.ends_with(".lock") {
        return false;
    }
    let hex = base.bytes().all(|b| b.is_ascii_hexdigit());
    if hex && base.len() != 40 && base.len() != 64 {
        return false;
    }
    base.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.'))
}

/// An object `cat-file` names: a plain revision, or `REV:path` for a path git takes as written.
fn object_name(name: &str) -> bool {
    match name.split_once(':') {
        Some((rev, path)) => {
            plain_rev(rev)
                && !rev.ends_with("^{tree}")
                && !path.is_empty()
                && !path.starts_with('/')
                && !path.split('/').any(|c| c.is_empty() || c == "." || c == "..")
                && !path.contains('\\')
        }
        None => plain_rev(name),
    }
}

/// The error playbook::git gives when git exits with `code` having printed nothing.
fn said_nothing(args: &[&str], code: i32) -> anyhow::Error {
    let code = if cfg!(windows) { format!("{code} (0x{:08x})", code as u32) } else { code.to_string() };
    anyhow::anyhow!("git {}: exited with {code} and said nothing", args.join(" "))
}

/// Environment that points git somewhere other than `-C dir` would find: the CLI follows it, and
/// this module does not try to.
fn env_redirects_git() -> bool {
    const VARS: [&str; 7] = ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE", "GIT_REPLACE_REF_BASE"];
    VARS.iter().any(|v| std::env::var_os(v).is_some())
}

/// The repository `dir` is in, opened as git -C would find it. Opened again on every question, so a
/// ref, a checkout or a new pack written since is always seen; that costs well under a millisecond.
/// Isolated from global and system config, which no read here depends on and whose discovery
/// would start git itself on Windows.
fn open(dir: &Path) -> Option<gix::Repository> {
    let opts = gix::open::Options::isolated();
    let mapping = gix::sec::trust::Mapping { full: opts.clone(), reduced: opts };
    let repo = gix::ThreadSafeRepository::discover_opts(dir, Default::default(), mapping).ok()?;
    let repo = repo.to_thread_local();
    // Replace refs change what an id reads as; leave a repository that has them to git.
    if repo.references().ok()?.prefixed("refs/replace/").ok()?.next().is_some() {
        return None;
    }
    Some(repo)
}

/// `git worktree list --porcelain` in the main checkout `root`, as `git::parse_worktrees` reads
/// it, or None to ask git: a bare repository, a common folder not named `.git`, a worktree that
/// git records by a relative path, a branch not born yet, or anything gix cannot read.
pub(crate) fn worktrees(root: &Path) -> Option<Vec<crate::git::Worktree>> {
    if env_redirects_git() || !root.join(".git").is_dir() {
        return None;
    }
    let repo = open(root)?;
    let common = repo.common_dir();
    if common.file_name()? != ".git" {
        return None;
    }
    let main = realpath(common.parent()?)?;
    let mut out = vec![worktree_entry(&repo, main, &common.join("HEAD"), true, false, false)?];
    let linked = common.join("worktrees");
    let Ok(ids) = std::fs::read_dir(&linked) else { return Some(out) };
    // In the order the folder lists them, as git reads them.
    for id in ids {
        let dir = id.ok()?.path();
        if !dir.is_dir() {
            return None;
        }
        let gitdir = std::fs::read_to_string(dir.join("gitdir")).ok()?;
        let gitdir = gitdir.trim_end();
        let path = gitdir.strip_suffix("/.git").unwrap_or(gitdir);
        if !Path::new(path).is_absolute() {
            return None;
        }
        let locked = dir.join("locked").exists();
        let prunable = !locked && !Path::new(gitdir).exists();
        out.push(worktree_entry(&repo, path.to_string(), &dir.join("HEAD"), false, locked, prunable)?);
    }
    Some(out)
}

/// One worktree's entry: its HEAD file names a branch (`ref: refs/heads/...`) or holds an id.
fn worktree_entry(repo: &gix::Repository, path: String, head_file: &Path, is_main: bool, locked: bool, prunable: bool) -> Option<crate::git::Worktree> {
    let head = std::fs::read_to_string(head_file).ok()?;
    let head = head.trim_end();
    let (head, branch) = match head.strip_prefix("ref: ") {
        Some(name) => {
            let branch = name.strip_prefix("refs/heads/")?;
            let id = repo.try_find_reference(name).ok()??.into_fully_peeled_id().ok()?;
            (id.to_string(), Some(branch.to_string()))
        }
        None => (gix::ObjectId::from_hex(head.as_bytes()).ok()?.to_string(), None),
    };
    Some(crate::git::Worktree { path, head, branch, is_main, locked, prunable })
}

/// A folder as git prints it: its real path, with forward slashes on Windows.
fn realpath(p: &Path) -> Option<String> {
    let real = std::fs::canonicalize(p).ok()?;
    let real = real.to_str()?;
    if cfg!(windows) {
        // Only a plain drive path; a share or a device path is git's to spell.
        let plain = real.strip_prefix(r"\\?\").filter(|r| r.as_bytes().get(1) == Some(&b':'))?;
        Some(plain.replace('\\', "/"))
    } else {
        Some(real.to_string())
    }
}

/// `git::list_branches(root, integration)` read in process: every local branch, most recently
/// committed first (then by name), with its counts against `integration`, or None to ask git: an
/// integration branch that names no commit, a branch whose short name git would spell longer
/// because another ref shares it, a branch not on a commit, or a message that is not UTF-8.
pub(crate) fn branches(root: &Path, integration: &str) -> Option<Vec<crate::git::BranchRef>> {
    if env_redirects_git() || !plain_rev(integration) {
        return None;
    }
    let repo = open(root)?;
    let base = resolve(&repo, &format!("{integration}^{{commit}}"))??.detach();
    let mut listed = Vec::new();
    for reference in repo.references().ok()?.local_branches().ok()? {
        let mut reference = reference.ok()?;
        let full = reference.name().as_bstr().to_string();
        let short = full.strip_prefix("refs/heads/")?.to_string();
        let shared = [format!("refs/{short}"), format!("refs/tags/{short}"), format!("refs/remotes/{short}"), format!("refs/remotes/{short}/HEAD")];
        if shared.iter().any(|n| !matches!(repo.try_find_reference(n.as_str()), Ok(None))) || repo.git_dir().join(&short).exists() {
            return None;
        }
        let commit = reference.peel_to_id().ok()?.object().ok()?.try_into_commit().ok()?;
        let at = commit.committer().ok()?.seconds();
        let message = commit.message_raw().ok()?;
        let subject = std::str::from_utf8(gix::objs::commit::MessageRef::from_bytes(message).summary().as_ref()).ok()?.trim().to_string();
        let tip = commit.id;
        let ahead = count(&repo, tip, base)?;
        let behind = count(&repo, base, tip)?;
        listed.push((full, crate::git::BranchRef { name: short, ahead: Some(ahead), behind: Some(behind), last_commit_subject: (!subject.is_empty()).then_some(subject), last_commit_at: Some(at) }));
    }
    listed.sort_by(|(an, a), (bn, b)| b.last_commit_at.cmp(&a.last_commit_at).then_with(|| an.cmp(bn)));
    Some(listed.into_iter().map(|(_, b)| b).collect())
}

/// How many commits `from` reaches that `not` does not.
fn count(repo: &gix::Repository, from: gix::ObjectId, not: gix::ObjectId) -> Option<u32> {
    let mut n = 0u32;
    for commit in repo.rev_walk([from]).with_hidden([not]).all().ok()? {
        commit.ok()?;
        n += 1;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_only_questions_it_can_answer_as_git_would() {
        let rev = |args: &[&'static str]| Question::parse(args);
        assert_eq!(rev(&["rev-parse", "HEAD"]), Some(Question::RevParse { revs: vec!["HEAD"], verify: false, quiet: false }));
        assert_eq!(
            rev(&["rev-parse", "--verify", "--quiet", "refs/heads/claude/nit-3-x^{commit}"]),
            Some(Question::RevParse { revs: vec!["refs/heads/claude/nit-3-x^{commit}"], verify: true, quiet: true })
        );
        assert!(rev(&["rev-parse", "HEAD~2", "main^{tree}", "HEAD^"]).is_some());
        assert_eq!(rev(&["merge-base", "main", "HEAD"]), Some(Question::MergeBase("main", "HEAD")));
        assert_eq!(rev(&["cat-file", "-s", "HEAD:src/a b.rs"]), Some(Question::CatFile(CatFile::Size, "HEAD:src/a b.rs")));
        // Left to git: what depends on more than the name (short ids, reflogs, searches, ranges,
        // upstreams), options this module does not know, and --verify with several names.
        for args in [
            &["rev-parse", "abc1234"][..],
            &["rev-parse", "HEAD@{1}"],
            &["rev-parse", ":/Port the change"],
            &["rev-parse", "main..HEAD"],
            &["rev-parse", "--short", "HEAD"],
            &["rev-parse", "--abbrev-ref", "HEAD"],
            &["rev-parse", "--show-toplevel"],
            &["rev-parse", "--verify", "HEAD", "main"],
            &["rev-parse", "--quiet", "HEAD"],
            &["rev-parse", "-q"],
            &["cat-file", "-p", "HEAD"],
            &["cat-file", "-s", "HEAD:../x"],
            &["cat-file", "-s", "HEAD:/x"],
            &["merge-base", "--all", "a", "b"],
            &["merge-base", "--is-ancestor", "a", "b"],
        ] {
            assert_eq!(Question::parse(args), None, "{args:?}");
        }
    }

    #[test]
    fn answers_as_the_git_cli_does_in_a_repo_with_branches_tags_merges_packed_refs_and_a_worktree() {
        use crate::test_repo::{git, init};
        let dir = crate::paths::unique_temp("nitely-git-read", "");
        let files = [("a.txt", "one\n"), ("src/b.rs", "fn b() {}\n"), ("src/deep/c d.txt", "x\n"), ("src-x/e.txt", "e\n"), ("src.txt", "s\n")];
        init(&dir, &files);
        git(&dir, &["update-index", "--chmod=+x", "a.txt"]);
        git(&dir, &["commit", "-qm", "exec"]);
        git(&dir, &["tag", "-a", "v1", "-m", "v1"]);
        git(&dir, &["switch", "-q", "-c", "claude/nit-3-x"]);
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        git(&dir, &["commit", "-qam", "side"]);
        git(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("c.txt"), "c\n").unwrap();
        git(&dir, &["add", "c.txt"]);
        git(&dir, &["commit", "-qm", "main moves"]);
        git(&dir, &["pack-refs", "--all"]);
        git(&dir, &["merge", "-q", "--no-ff", "-m", "merge", "claude/nit-3-x"]);
        let wt = dir.with_extension("wt");
        git(&dir, &["worktree", "add", "-q", &wt.to_string_lossy(), "claude/nit-3-x"]);
        let blob = git(&dir, &["rev-parse", "HEAD:a.txt"]);
        let missing = "0123456789012345678901234567890123456789";

        let questions: Vec<Vec<&str>> = vec![
            vec!["rev-parse", "HEAD"],
            vec!["rev-parse", "main"],
            vec!["rev-parse", "HEAD", "HEAD^{tree}", "HEAD^", "HEAD~1", "main^{commit}"],
            vec!["rev-parse", "v1"],
            vec!["rev-parse", "v1^{commit}"],
            vec!["rev-parse", "refs/heads/claude/nit-3-x"],
            vec!["rev-parse", "claude/nit-3-x^{tree}"],
            vec!["rev-parse", missing],
            vec!["rev-parse", "--verify", "--quiet", "refs/heads/nope"],
            vec!["rev-parse", "--verify", "--quiet", "refs/heads/nope^{commit}"],
            vec!["rev-parse", "--verify", "refs/heads/main^{commit}"],
            vec!["merge-base", "main", "claude/nit-3-x"],
            vec!["merge-base", "HEAD^", "v1"],
            vec!["merge-base", "--is-ancestor", "v1", "HEAD"],
            vec!["merge-base", "--is-ancestor", "main", "claude/nit-3-x"],
            vec!["merge-base", "--is-ancestor", "HEAD", "HEAD"],
            vec!["ls-tree", "-r", "-z", "HEAD"],
            vec!["ls-tree", "-r", "-z", "main^{tree}"],
            vec!["rev-list", "--count", "v1..HEAD"],
            vec!["rev-list", "--count", "claude/nit-3-x..main"],
            vec!["rev-list", "--count", "HEAD..v1"],
            vec!["branch", "--show-current"],
            vec!["cat-file", "-s", "HEAD:a.txt"],
            vec!["cat-file", "-s", "HEAD:src/deep/c d.txt"],
            vec!["cat-file", "-t", "v1"],
            vec!["cat-file", "-t", "HEAD^{tree}"],
            vec!["cat-file", "blob", "HEAD:a.txt"],
            vec!["cat-file", "blob", &blob],
            vec!["cat-file", "-e", "main:c.txt"],
            vec!["cat-file", "-e", missing],
        ];
        let same = |at: &Path, args: &[&str]| {
            let ours = answer(at, args).unwrap_or_else(|| panic!("{args:?} in {at:?} went to git"));
            let o = crate::playbook::run_git(crate::playbook::shell_free_command("git").arg("-C").arg(at).args(args)).unwrap();
            match ours {
                Ok(text) => {
                    assert!(o.status.success(), "{args:?} in {at:?}: git failed, we said {text:?}");
                    assert_eq!(text, String::from_utf8_lossy(&o.stdout), "{args:?} in {at:?}");
                }
                Err(e) => {
                    assert!(!o.status.success() && o.stdout.is_empty() && o.stderr.is_empty(), "{args:?} in {at:?}: we said {e}, git said {o:?}");
                    let code = o.status.code().unwrap();
                    let code = if cfg!(windows) { format!("{code} (0x{code:08x})") } else { code.to_string() };
                    assert_eq!(e.to_string(), format!("git {}: exited with {code} and said nothing", args.join(" ")));
                }
            }
        };
        for at in [&dir, &wt, &dir.join("src")] {
            for args in &questions {
                if at.ends_with("src") && args[0] == "ls-tree" {
                    // Relative to the folder there: left to git.
                    assert!(answer(at, args).is_none());
                    continue;
                }
                same(at, args);
            }
        }
        // Detached, there is no current branch.
        git(&wt, &["switch", "-q", "--detach"]);
        same(&wt, &["branch", "--show-current"]);
        // What surely names nothing only through git's own message goes to git.
        assert!(answer(&dir, &["rev-parse", "--verify", "nope"]).is_none());
        assert!(answer(&dir, &["cat-file", "-s", "HEAD:nope.txt"]).is_none());
        std::fs::remove_dir_all(&wt).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lists_worktrees_and_branches_as_the_board_reads_them_from_git() {
        use crate::test_repo::{git, init};
        let dir = crate::paths::unique_temp("nitely-git-read-board", "");
        let root = dir.join("app");
        init(&root, &[("a.txt", "a\n")]);
        let cli = |args: &[&str]| {
            let o = crate::playbook::run_git(crate::playbook::shell_free_command("git").arg("-C").arg(&root).args(args)).unwrap();
            assert!(o.status.success(), "{args:?}");
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let as_git = || {
            let counted = "--format=%(refname:short)%00%(committerdate:unix)%00%(ahead-behind:main)%00%(subject)";
            (crate::git::parse_worktrees(&cli(&["worktree", "list", "--porcelain"])), crate::git::parse_branch_refs(&cli(&["for-each-ref", "--sort=-committerdate", counted, "refs/heads"])))
        };
        // Worktree ids listed in another order than their paths; one locked, one detached, one
        // whose folder is gone.
        let add = |name: &str, args: &[&str]| {
            let path = dir.join(name).to_string_lossy().into_owned();
            let (commit, options) = args.split_last().unwrap();
            let mut all = vec!["worktree", "add", "-q"];
            all.extend_from_slice(options);
            all.extend([path.as_str(), commit]);
            git(&root, &all);
        };
        add("z-first", &["-b", "claude/app-2-y", "main"]);
        add("a-second", &["-b", "claude/app-1-x", "main"]);
        add("detached", &["--detach", "main"]);
        add("gone", &["-b", "claude/app-3-gone", "main"]);
        git(&root, &["worktree", "lock", &dir.join("a-second").to_string_lossy()]);
        std::fs::remove_dir_all(dir.join("gone")).unwrap();
        // Branches ahead, behind, both, committed in the same second, with a two-line title.
        let x = dir.join("a-second");
        std::fs::write(x.join("b.txt"), "b\n").unwrap();
        git(&x, &["add", "b.txt"]);
        git(&x, &["commit", "-q", "-m", "Two lines\nof title\n\nAnd a body"]);
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        git(&root, &["add", "c.txt"]);
        git(&root, &["commit", "-q", "-m", "main moves"]);
        git(&root, &["branch", "same-second-b", "main"]);
        git(&root, &["branch", "same-second-a", "main"]);

        let (worktrees, branches) = as_git();
        assert_eq!(worktrees.len(), 5);
        assert!(worktrees.iter().any(|w| w.locked) && worktrees.iter().any(|w| w.prunable) && worktrees.iter().any(|w| w.branch.is_none()));
        assert_eq!(super::worktrees(&root), Some(worktrees));
        assert!(branches.iter().any(|b| b.ahead == Some(1) && b.behind == Some(1)));
        assert_eq!(super::branches(&root, "main"), Some(branches));

        // A tag that shares a branch's name makes git spell the branch longer: left to git.
        git(&root, &["tag", "same-second-a"]);
        assert_eq!(super::branches(&root, "main"), None);
        // No such integration branch: git's listing without counts answers.
        assert_eq!(super::branches(&root, "develop"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
