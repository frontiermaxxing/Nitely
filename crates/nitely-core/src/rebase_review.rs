//! A land's rebase and the review it was handed in with (NIT-700). A rebase that stopped on
//! conflicts, resolved by taking lines from both sides and nothing else, keeps its review: the
//! change is the one that passed, placed among what landed meanwhile (`kept`). Anything more is
//! reviewed at the land on how it differs from the change that passed
//! (`review_batch::review_rebased`), not in a new submit round. Files the playbook names in
//! `[land] union`, append-only lists every change adds to, are merged with git's union driver in
//! the land's rebase, so they stop conflicting at all.
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::playbook::{git, Playbook};
use crate::submit::ReviewRecord;

/// The `git -c` options that merge the playbook's `[land] union` files with the union driver in
/// `dir`'s worktree, or none. The attributes are written to the worktree's own git dir, never the
/// repo's shared `info/attributes`, and keep the person's own global attributes file.
pub(crate) fn union_args(pb: &Playbook, dir: &Path) -> Result<Vec<String>> {
    let patterns: Vec<&str> = pb.land.union.iter().map(|p| p.trim()).filter(|p| !p.is_empty() && !p.contains(char::is_whitespace)).collect();
    if patterns.is_empty() {
        return Ok(vec![]);
    }
    let mut text = global_attributes(dir).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for p in patterns {
        text.push_str(&format!("{p} merge=union\n"));
    }
    let path = Path::new(git(dir, &["rev-parse", "--absolute-git-dir"])?.trim()).join("nitely-union.attributes");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&path, &text)?;
    }
    Ok(vec!["-c".into(), format!("core.attributesFile={}", path.to_string_lossy().replace('\\', "/"))])
}

/// The global attributes file `core.attributesFile` would replace: the configured one, else git's
/// default under the XDG config folder.
fn global_attributes(dir: &Path) -> Option<String> {
    let configured = git(dir, &["config", "--path", "--get", "core.attributesFile"]).ok().map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    let path = configured.map(std::path::PathBuf::from).or_else(|| {
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
        Some(xdg.join("git").join("attributes"))
    })?;
    std::fs::read_to_string(path).ok()
}

/// Whether the change at `head` on top of `base` is the change `review` passed with its conflicts
/// resolved and nothing else: replaying the reviewed change onto `base` (`git merge-tree`, with
/// the union files as the land merges them) leaves `head` differing only in conflicted files, each
/// resolved with lines from its two sides, and the lines it adds and removes, file by file, are
/// the ones the review saw.
pub(crate) fn kept(pb: &Playbook, dir: &Path, base: &str, head: &str, review: &ReviewRecord) -> Result<bool> {
    if review.commit_sha.is_empty() || git(dir, &["merge-base", "--is-ancestor", base, head]).is_err() {
        return Ok(false);
    }
    let old_base = git(dir, &["merge-base", base, &review.commit_sha])?.trim().to_string();
    // The review's patch id is recomputed before its commit is trusted to be what it saw.
    if crate::submit::patch_id_range(dir, &old_base, &review.commit_sha)? != review.patch_id {
        return Ok(false);
    }
    if lines(dir, &old_base, &review.commit_sha)? != lines(dir, base, head)? {
        return Ok(false);
    }
    let Some((merged, conflicted)) = replay(pb, dir, &old_base, base, &review.commit_sha)? else { return Ok(false) };
    let head_tree = format!("{head}^{{tree}}");
    for path in git(dir, &["diff", "--name-only", "--no-renames", &merged, &head_tree])?.lines().filter(|l| !l.is_empty()) {
        if !conflicted.contains(path) {
            return Ok(false);
        }
        let (Ok(with_markers), Ok(resolved)) = (git(dir, &["show", &format!("{merged}:{path}")]), git(dir, &["show", &format!("{head}:{path}")])) else {
            return Ok(false);
        };
        if !resolves(&with_markers, &resolved) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The reviewed change replayed onto `base` from `old_base`: the merged tree, conflict markers in
/// the files that conflicted, and those files. None when git cannot merge them at all.
fn replay(pb: &Playbook, dir: &Path, old_base: &str, base: &str, reviewed: &str) -> Result<Option<(String, HashSet<String>)>> {
    let mut cmd = crate::playbook::shell_free_command("git");
    cmd.arg("-C").arg(dir).args(union_args(pb, dir)?).args(["-c", "merge.conflictStyle=merge", "merge-tree", "--write-tree", "--name-only", "--no-messages"]);
    cmd.arg(format!("--merge-base={old_base}")).arg(base).arg(reviewed);
    let out = crate::playbook::run_git(&mut cmd)?;
    // 0: merged cleanly; 1: merged with conflicts; anything else: no merge.
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let Some(tree) = lines.next().map(str::trim).filter(|t| !t.is_empty()) else { return Ok(None) };
    let conflicted = lines.take_while(|l| !l.is_empty()).map(str::to_string).collect();
    Ok(Some((tree.to_string(), conflicted)))
}

/// The lines `from..to` adds and removes, counted, file by file: what a review of the change saw,
/// whatever surrounds them.
fn lines(dir: &Path, from: &str, to: &str) -> Result<BTreeMap<String, HashMap<String, i64>>> {
    let diff = git(dir, &["diff", "-U0", "--text", "--no-color", "--no-ext-diff", "--no-renames", from, to])?;
    let mut out: BTreeMap<String, HashMap<String, i64>> = BTreeMap::new();
    let mut file = String::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            file = line.to_string();
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && (line.starts_with('+') || line.starts_with('-')) {
            *out.entry(file.clone()).or_default().entry(line.to_string()).or_default() += 1;
        } else if !in_hunk && !file.is_empty() {
            // A header line (mode, binary, index): it is part of what changed too.
            if !line.starts_with("index ") && !line.starts_with("--- ") && !line.starts_with("+++ ") {
                *out.entry(file.clone()).or_default().entry(line.to_string()).or_default() += 1;
            }
        }
    }
    Ok(out)
}

/// Whether `resolved` is `with_markers` with each conflict replaced by its two sides whole, one
/// after the other in either order: everything outside the conflicts as it is, and each side's
/// lines in their own order, so the reviewed code runs as it was reviewed (NIT-711 review).
fn resolves(with_markers: &str, resolved: &str) -> bool {
    enum Part<'a> {
        Fixed(&'a str),
        Conflict(Vec<&'a str>, Vec<&'a str>),
    }
    let mut parts: Vec<Part> = vec![];
    let mut side: Option<(Vec<&str>, Vec<&str>)> = None;
    let mut ours = true;
    for line in with_markers.split('\n') {
        match &mut side {
            None if line.starts_with("<<<<<<<") => {
                side = Some((vec![], vec![]));
                ours = true;
            }
            None => parts.push(Part::Fixed(line)),
            Some(_) if ours && line.trim_end_matches('\r') == "=======" => ours = false,
            Some(_) if !ours && line.starts_with(">>>>>>>") => {
                let (a, b) = side.take().unwrap_or_default();
                parts.push(Part::Conflict(a, b));
            }
            Some((a, _)) if ours => a.push(line),
            Some((_, b)) => b.push(line),
        }
    }
    if side.is_some() || !parts.iter().any(|p| matches!(p, Part::Conflict(..))) {
        return false;
    }
    let have: Vec<&str> = resolved.split('\n').collect();
    // Whether parts from `i` on make up the resolution from line `at`; failures remembered.
    fn fits(parts: &[Part], have: &[&str], i: usize, at: usize, failed: &mut HashSet<(usize, usize)>) -> bool {
        if i == parts.len() {
            return at == have.len();
        }
        if failed.contains(&(i, at)) {
            return false;
        }
        let ok = match &parts[i] {
            Part::Fixed(line) => have.get(at) == Some(line) && fits(parts, have, i + 1, at + 1, failed),
            Part::Conflict(a, b) => {
                let ab: Vec<&str> = a.iter().chain(b.iter()).copied().collect();
                let ba: Vec<&str> = b.iter().chain(a.iter()).copied().collect();
                [ab, ba].iter().any(|whole| have.get(at..at + whole.len()) == Some(whole.as_slice()) && fits(parts, have, i + 1, at + whole.len(), failed))
            }
        };
        if !ok {
            failed.insert((i, at));
        }
        ok
    }
    fits(&parts, &have, 0, 0, &mut HashSet::new())
}

/// The review `check_approval` would find passed but covers neither the change at HEAD in
/// `worktree` nor, by `kept`, its conflict resolutions, and the branch was rebased since: the
/// land reviews the rebased change now (`review_batch::review_rebased`). A commit added on top of
/// the reviewed one, a review that did not pass, or none, is left to `check_approval`, which says
/// to submit again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn review_at_land(db: &mut crate::db::Db, root: &Path, pb: &Playbook, key: &str, worktree: &Path, base: &str, branch: &str, actor: &crate::issues::Actor) -> Result<()> {
    let Some(last) = db.reviews(key)?.into_iter().next() else { return Ok(()) };
    if !rebased_past(pb, worktree, base, "HEAD", &last)? {
        return Ok(());
    }
    if let Err(why) = crate::review_batch::review_rebased(db, root, pb, key, worktree, base, branch, &last, actor)? {
        bail!("{why}");
    }
    Ok(())
}

/// Whether `review` passed and no longer covers the change at `head` on top of `base`, which was
/// rebased since: the reviewed commit is no longer in its history.
pub(crate) fn rebased_past(pb: &Playbook, dir: &Path, base: &str, head: &str, review: &ReviewRecord) -> Result<bool> {
    if review.reviewer.is_empty() || review.verdict != "pass" || git(dir, &["merge-base", "--is-ancestor", &review.commit_sha, head]).is_ok() {
        return Ok(false);
    }
    Ok(!covers(pb, dir, base, head, review)?)
}

/// Whether `review` covers the change at `head` on top of `base`: the same patch, the same change
/// with only its test list merged (test_command_merge.rs), or its conflicts resolved (`kept`).
pub(crate) fn covers(pb: &Playbook, dir: &Path, base: &str, head: &str, review: &ReviewRecord) -> Result<bool> {
    Ok(review.patch_id == crate::submit::patch_id_range(dir, base, head)?
        || crate::test_command_merge::equivalent(dir, base, head, review).unwrap_or(false)
        || kept(pb, dir, base, head, review).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolution_takes_lines_from_the_two_sides_and_nothing_else() {
        let conflicted = "a\n<<<<<<< main\nx\n=======\ny\n>>>>>>> theirs\nb\n";
        for ok in ["a\nx\ny\nb\n", "a\ny\nx\nb\n"] {
            assert!(resolves(conflicted, ok), "{ok:?}");
        }
        for not in ["a\nx\ny\nz\nb\n", "a\nx\nx\nb\n", "A\nx\ny\nb\n", "a\nx\ny\n", "a\nx\nb\n"] {
            assert!(!resolves(conflicted, not), "{not:?}");
        }
        // Each side keeps its order: the reviewed lines are not reordered around the other side's.
        let ordered = "<<<<<<< main\nlog\n=======\nauthorize\ndelete\n>>>>>>> theirs\n";
        assert!(resolves(ordered, "log\nauthorize\ndelete\n") && resolves(ordered, "authorize\ndelete\nlog\n"));
        assert!(!resolves(ordered, "log\ndelete\nauthorize\n") && !resolves(ordered, "authorize\nlog\ndelete\n"));
    }
}
