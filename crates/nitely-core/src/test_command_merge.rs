//! A deliberately narrow reconciliation: appended explicit JavaScript test files in
//! package.json's `scripts.test`. Other commands, flags, globs and JSON changes stay conflicts.
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

use crate::playbook::git;

fn script(value: &Value) -> Option<&str> {
    value.get("scripts")?.get("test")?.as_str()
}

fn rest(value: &Value) -> Option<Value> {
    let mut value = value.clone();
    value.get_mut("scripts")?.as_object_mut()?.remove("test")?;
    Some(value)
}

fn command(text: &str) -> Option<(&str, Vec<&str>)> {
    let (prefix, files) = text.rsplit_once("node --test ")?;
    if !prefix.is_empty() && !prefix.trim_end().ends_with("&&") {
        return None;
    }
    let files: Vec<_> = files.split_whitespace().collect();
    if files.is_empty() || files.iter().any(|file| {
        file.starts_with('-')
            || !file.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
            || ![".js", ".mjs", ".cjs"].iter().any(|ext| file.ends_with(ext))
    }) {
        return None;
    }
    let unique: std::collections::HashSet<_> = files.iter().collect();
    (unique.len() == files.len()).then_some((prefix, files))
}

fn additions(before: &Value, after: &Value) -> Option<Vec<String>> {
    if rest(before)? != rest(after)? {
        return None;
    }
    let (prefix, before) = command(script(before)?)?;
    let (next_prefix, after) = command(script(after)?)?;
    if prefix != next_prefix || after.len() <= before.len() || !after.starts_with(&before) {
        return None;
    }
    Some(after[before.len()..].iter().map(|s| (*s).to_string()).collect())
}

fn merge(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let (base, mut ours, theirs): (Value, Value, Value) = (
        serde_json::from_str(base).ok()?, serde_json::from_str(ours).ok()?, serde_json::from_str(theirs).ok()?,
    );
    let ours_added = additions(&base, &ours)?;
    let theirs_added = additions(&base, &theirs)?;
    if theirs_added.iter().any(|s| ours_added.contains(s)) {
        return None;
    }
    let merged = format!("{} {}", script(&ours)?, theirs_added.join(" "));
    *ours.get_mut("scripts")?.get_mut("test")? = Value::String(merged);
    Some(format!("{}\n", serde_json::to_string_pretty(&ours).ok()?))
}

/// Continue only conflicts entirely confined to this one structured field. A later unsupported
/// conflict remains in progress for the author. The caller still checks review and approval.
pub(crate) fn finish_rebase(worktree: &Path) -> Result<bool> {
    for _ in 0..32 {
        let conflicts = git(worktree, &["diff", "--name-only", "--diff-filter=U"])?;
        if conflicts.trim() != "package.json" {
            return Ok(false);
        }
        let stages = git(worktree, &["ls-files", "-u", "--", "package.json"])?;
        if stages.lines().count() != 3 || stages.lines().any(|line| !line.starts_with("100644 ")) {
            return Ok(false);
        }
        let base = git(worktree, &["show", ":1:package.json"])?;
        let ours = git(worktree, &["show", ":2:package.json"])?;
        let theirs = git(worktree, &["show", ":3:package.json"])?;
        let Some(merged) = merge(&base, &ours, &theirs) else { return Ok(false) };
        std::fs::write(worktree.join("package.json"), merged)?;
        git(worktree, &["add", "--", "package.json"])?;
        if git(worktree, &["-c", "core.editor=true", "rebase", "--continue"]).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Prove that only the surrounding test list changed. The review's original patch id is
/// recomputed before trusting its source commit; signatures remain bound to that original id.
pub(crate) fn equivalent(worktree: &Path, base: &str, review: &crate::submit::ReviewRecord) -> Result<bool> {
    let old_base = git(worktree, &["merge-base", base, &review.commit_sha])?;
    let old_base = old_base.trim();
    if crate::submit::patch_id_range(worktree, old_base, &review.commit_sha)? != review.patch_id {
        return Ok(false);
    }
    let read = |at: &str| -> Result<Option<Value>> {
        let mode = git(worktree, &["ls-tree", at, "--", "package.json"])?;
        if !mode.starts_with("100644 ") {
            return Ok(None);
        }
        Ok(serde_json::from_str(&git(worktree, &["show", &format!("{at}:package.json")])?).ok())
    };
    let (Some(old), Some(reviewed), Some(current), Some(candidate)) =
        (read(old_base)?, read(&review.commit_sha)?, read(base)?, read("HEAD")?) else { return Ok(false) };
    let Some(expected) = additions(&old, &reviewed) else { return Ok(false) };
    if additions(&current, &candidate).as_ref() != Some(&expected) {
        return Ok(false);
    }
    let other_patch = |from: &str, to: &str| -> Result<String> {
        let diff = git(worktree, &["diff", "-U0", "--no-ext-diff", "--no-renames", from, to, "--", ".", ":(exclude)package.json"])?;
        crate::submit::patch_id_of(worktree, diff)
    };
    Ok(other_patch(old_base, &review.commit_sha)? == other_patch(base, "HEAD")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn package(test: &str) -> String {
        serde_json::json!({"name": "example", "scripts": {"test": test}}).to_string()
    }
    #[test]
    fn combines_only_disjoint_explicit_test_file_appends() {
        let base = package("tsc -p tests.json && node --test out/a.js");
        let ours = package("tsc -p tests.json && node --test out/a.js out/b.js");
        let theirs = package("tsc -p tests.json && node --test out/a.js out/c.js");
        let result: Value = serde_json::from_str(&merge(&base, &ours, &theirs).unwrap()).unwrap();
        assert_eq!(script(&result), Some("tsc -p tests.json && node --test out/a.js out/b.js out/c.js"));
        for test in [
            "node --test out/a.js out/c.js", // different setup
            "tsc -p tests.json && node --test out/c.js", // removal
            "tsc -p tests.json && node --test out/a.js out/*.js",
            "tsc -p tests.json && node --test out/a.js --test-force-exit",
            "tsc -p tests.json && node --test out/a.js out/c.js && echo done",
            "tsc -p tests.json && node --test out/a.js out/b.js", // overlapping append
        ] {
            assert!(merge(&base, &ours, &package(test)).is_none(), "{test}");
        }
        assert!(merge(&base, &ours, &theirs.replace("example", "changed")).is_none());
    }
}
