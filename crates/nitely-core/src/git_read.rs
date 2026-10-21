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
                let mut n = 0usize;
                for commit in repo.rev_walk([b]).with_hidden([a]).all().ok()? {
                    commit.ok()?;
                    n += 1;
                }
                Some(Ok(format!("{n}\n")))
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
}
