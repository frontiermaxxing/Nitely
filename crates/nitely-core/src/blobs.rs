//! Reading many git blobs in two git processes, however many there are: their sizes first
//! (`cat-file --batch-check`), then the contents of those within the limit (`cat-file --batch`).
//! A git process costs 50 to 400 ms on Windows (NIT-458), and the affected-tests selector reads
//! every source file of two snapshots (NIT-508).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// What one object named to `read` turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Blob {
    Text(Vec<u8>),
    /// A blob over the limit, not read.
    TooLarge(usize),
    /// Not a blob, or nothing by that name.
    Missing,
}

/// The objects in `names` (an object id or `revision:path`, as `git cat-file` takes them), each
/// read only when it is a blob of at most `max` bytes. None, with nothing read, when the blobs to
/// read come to more than `total` bytes.
pub(crate) fn read(repo: &Path, names: &[String], max: usize, total: usize) -> Result<Option<HashMap<String, Blob>>> {
    let mut found = HashMap::new();
    let mut names: Vec<&String> = names.iter().collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(Some(found));
    }
    let checked = batch(repo, "--batch-check", &names)?;
    let mut sizes = checked.as_slice();
    let mut wanted = Vec::new();
    let mut bytes = 0usize;
    for name in &names {
        let (header, rest) = line(sizes).with_context(|| format!("git cat-file --batch-check stopped before {name}"))?;
        sizes = rest;
        let fields: Vec<&str> = header.split(' ').collect();
        let blob = match fields.as_slice() {
            [_, "blob", size] => {
                let size: usize = size.parse().with_context(|| format!("git cat-file gave {header:?}"))?;
                if size > max {
                    Blob::TooLarge(size)
                } else {
                    wanted.push(*name);
                    bytes += size;
                    continue;
                }
            }
            _ => Blob::Missing,
        };
        found.insert((*name).clone(), blob);
    }
    if bytes > total {
        return Ok(None);
    }
    if wanted.is_empty() {
        return Ok(Some(found));
    }
    let contents = batch(repo, "--batch", &wanted)?;
    let mut rest = contents.as_slice();
    for name in wanted {
        let (header, after) = line(rest).with_context(|| format!("git cat-file --batch stopped before {name}"))?;
        let size: usize = match header.split(' ').collect::<Vec<_>>().as_slice() {
            [_, "blob", size] => size.parse().with_context(|| format!("git cat-file gave {header:?}"))?,
            // Gone between the two calls: as if it had never been there.
            _ => {
                found.insert(name.clone(), Blob::Missing);
                rest = after;
                continue;
            }
        };
        if after.len() < size + 1 {
            bail!("git cat-file --batch ended inside {name}");
        }
        found.insert(name.clone(), Blob::Text(after[..size].to_vec()));
        rest = &after[size + 1..];
    }
    Ok(Some(found))
}

/// The first line of `bytes` as text, and what follows it.
fn line(bytes: &[u8]) -> Option<(&str, &[u8])> {
    let end = bytes.iter().position(|b| *b == b'\n')?;
    Some((std::str::from_utf8(&bytes[..end]).ok()?.trim_end_matches('\r'), &bytes[end + 1..]))
}

/// One `git cat-file <mode>` over `names`, one per line on its input, from a file: `run_git` keeps
/// its retries for a git that never started.
fn batch(repo: &Path, mode: &str, names: &[&String]) -> Result<Vec<u8>> {
    let input = crate::paths::unique_temp("nitely-blobs", ".txt");
    let text: String = names.iter().map(|n| format!("{n}\n")).collect();
    std::fs::write(&input, text).with_context(|| format!("writing {}", input.display()))?;
    let out = std::fs::File::open(&input).and_then(|stdin| {
        crate::playbook::run_git(crate::playbook::shell_free_command("git").arg("-C").arg(repo).args(["cat-file", mode]).stdin(stdin))
    });
    std::fs::remove_file(&input).ok();
    let out = out.with_context(|| format!("starting git cat-file {mode}"))?;
    if !out.status.success() {
        bail!("git cat-file {mode} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text, a blob over the limit, a tree and a name that is nothing, in one call each way.
    #[test]
    fn reads_what_fits_and_says_what_does_not() {
        let repo = crate::paths::unique_temp("nitely-blobs-test", "");
        crate::test_repo::init(&repo, &[("a.txt", "one\ntwo\n"), ("big.txt", "0123456789"), ("dir/b.txt", "")]);
        let names: Vec<String> = ["HEAD:a.txt", "HEAD:big.txt", "HEAD:dir", "HEAD:nothing", "HEAD:dir/b.txt", "HEAD:a.txt"].iter().map(|s| s.to_string()).collect();
        let got = read(&repo, &names, 9, 100).unwrap().unwrap();
        assert_eq!(got["HEAD:a.txt"], Blob::Text(b"one\ntwo\n".to_vec()));
        assert_eq!(got["HEAD:big.txt"], Blob::TooLarge(10));
        assert_eq!(got["HEAD:dir"], Blob::Missing);
        assert_eq!(got["HEAD:nothing"], Blob::Missing);
        assert_eq!(got["HEAD:dir/b.txt"], Blob::Text(vec![]));
        assert_eq!(read(&repo, &names, 9, 7).unwrap(), None, "a.txt alone is 8 bytes");
        std::fs::remove_dir_all(&repo).ok();
    }
}
