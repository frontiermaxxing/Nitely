//! Replays past small changes through the reviewer, each alone and in batches, to measure how a
//! batched review agrees with a single one (NIT-217, docs/batching.md). It also sets each fresh
//! single review against the change's original one, for how much reviews of one change drift
//! between runs anyway.
//!
//!     cargo run -p nitely-core --example review_replay -- --repo C:\Code\my-app --db <nitely.db> \
//!         --project APP --count 15 --size 3 --out replay.json
//!
//! It reads the database read-only (a build with newer migrations never upgrades it), takes the
//! project's most recent first-round reviews of small changes whose commits are still in the repo,
//! reviews each in a scratch clone of the repo checked out at the change (the repo itself and its
//! worktrees are untouched), and prints both comparisons. Development only.
//!
//! `--blocking` takes only first rounds that asked for changes, and `--size 1` reviews each alone
//! with no batches: how a change to the severities moves findings that blocked (NIT-695).

use anyhow::{bail, Context, Result};
use nitely_core::playbook::{self, Playbook};
use nitely_core::review::{BatchChange, Finding, Review, ReviewRules};
use nitely_core::review_compare::{Pair, Report, Side};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Args {
    repo: PathBuf,
    db: PathBuf,
    project: String,
    count: usize,
    size: usize,
    parallel: usize,
    out: Option<PathBuf>,
    blocking: bool,
}

fn args() -> Result<Args> {
    let mut a = Args { repo: PathBuf::new(), db: PathBuf::new(), project: String::new(), count: 15, size: 3, parallel: 3, out: None, blocking: false };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--repo" => a.repo = value()?.into(),
            "--db" => a.db = value()?.into(),
            "--project" => a.project = value()?.to_uppercase(),
            "--count" => a.count = value()?.parse()?,
            "--size" => a.size = value()?.parse()?,
            "--parallel" => a.parallel = value()?.parse()?,
            "--out" => a.out = Some(value()?.into()),
            "--blocking" => a.blocking = true,
            other => bail!("unknown argument {other}"),
        }
    }
    if a.repo.as_os_str().is_empty() || a.db.as_os_str().is_empty() || a.project.is_empty() {
        bail!("usage: review_replay --repo <repo> --db <nitely.db> --project <KEY> [--count 15] [--size 3] [--parallel 3] [--out replay.json] [--blocking]");
    }
    Ok(a)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = playbook::run_git(Command::new("git").arg("-C").arg(dir).args(args)).ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A past change and its original review.
#[derive(Clone, serde::Serialize)]
struct Change {
    key: String,
    title: String,
    commit: String,
    base: String,
    lines: usize,
    files: Vec<String>,
    reviewer: String,
    original: Side,
}

fn candidates(a: &Args, rules: &ReviewRules) -> Result<Vec<Change>> {
    let db = Connection::open_with_flags(&a.db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let mut stmt = db.prepare(
        "SELECT r.issue_key, COALESCE(i.title, ''), r.commit_sha, r.reviewer, r.verdict, r.findings FROM review r
         LEFT JOIN issue i ON i.key = r.issue_key
         WHERE r.reviewer != '' AND r.issue_key LIKE ?1 || '-%'
           AND NOT EXISTS (SELECT 1 FROM review e WHERE e.issue_key = r.issue_key AND e.reviewer != '' AND (e.ts < r.ts OR (e.ts = r.ts AND e.id < r.id)))
         ORDER BY r.ts DESC",
    )?;
    let rows: Vec<(String, String, String, String, String, String)> =
        stmt.query_map([&a.project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = vec![];
    for (key, title, commit, reviewer, verdict, findings) in rows {
        if out.len() == a.count {
            break;
        }
        if a.blocking && verdict != "changes" {
            continue;
        }
        if git(&a.repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_none() {
            continue;
        }
        // The change as it was reviewed: what it adds over where it left the integration branch.
        // One fast-forwarded onto the branch has no such point any more, and is left out.
        let Some(base) = git(&a.repo, &["merge-base", &commit, "origin/main"]).or_else(|| git(&a.repo, &["merge-base", &commit, "main"])) else { continue };
        if base == commit {
            continue;
        }
        let numstat = git(&a.repo, &["diff", "--numstat", "--no-renames", &format!("{base}...{commit}")]).unwrap_or_default();
        let files: Vec<String> = numstat.lines().filter_map(|l| l.split('\t').nth(2).map(str::to_string)).collect();
        let lines: usize = numstat.lines().map(|l| l.split_whitespace().take(2).map(|n| n.parse::<usize>().unwrap_or(0)).sum::<usize>()).sum();
        if files.is_empty() || lines > rules.small_lines || files.len() > rules.small_files {
            continue;
        }
        let findings: Vec<Finding> = serde_json::from_str(&findings).unwrap_or_default();
        out.push(Change { key, title, commit, base, lines, files, reviewer, original: Side { verdict, findings, seconds: 0.0, tokens: None } });
    }
    // Oldest first: a batch holds changes handed in close together.
    out.reverse();
    Ok(out)
}

/// A scratch clone of the repo at the change's commit, sharing the repo's objects.
fn checkout(repo: &Path, scratch: &Path, c: &Change) -> Result<PathBuf> {
    let dir = scratch.join(&c.key);
    if !dir.exists() {
        let ok = playbook::run_git(Command::new("git").args(["clone", "--quiet", "--shared", "--no-checkout"]).arg(repo).arg(&dir))?.status.success();
        if !ok || git(&dir, &["checkout", "--quiet", "--detach", &c.commit]).is_none() {
            bail!("could not check out {} at {}", c.key, c.commit);
        }
    }
    Ok(dir)
}

fn main() -> Result<()> {
    let a = args()?;
    let rules = Playbook::load(&a.repo)?.review;
    let changes = candidates(&a, &rules)?;
    eprintln!("{} small first-round changes to replay", changes.len());
    let scratch = nitely_core::paths::unique_temp("nitely-review-replay", "");
    std::fs::create_dir_all(&scratch)?;
    let dirs: Vec<PathBuf> = changes.iter().map(|c| checkout(&a.repo, &scratch, c)).collect::<Result<_>>()?;

    // Each change alone, a few at a time.
    let mut singles: Vec<Option<Review>> = vec![None; changes.len()];
    for chunk in (0..changes.len()).collect::<Vec<_>>().chunks(a.parallel.max(1)) {
        let done: Vec<(usize, Result<Review>)> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&i| {
                    let (rules, c, dir) = (&rules, &changes[i], &dirs[i]);
                    s.spawn(move || (i, rules.run(&c.reviewer, dir, &c.base, None)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (i, review) in done {
            match review {
                Ok(r) => {
                    eprintln!("{} alone: {} ({} s)", changes[i].key, r.verdict, r.seconds);
                    singles[i] = Some(r);
                }
                Err(e) => eprintln!("{} alone: did not finish: {e:#}", changes[i].key),
            }
        }
    }

    // The same changes in batches of `size`, by reviewer, in the order they were handed in.
    let mut batched: Vec<Option<(Review, usize, usize)>> = vec![None; changes.len()];
    let mut groups: Vec<Vec<usize>> = vec![];
    for reviewer in changes.iter().map(|c| c.reviewer.clone()).collect::<std::collections::BTreeSet<_>>() {
        let of: Vec<usize> = (0..changes.len()).filter(|&i| changes[i].reviewer == reviewer).collect();
        if a.size > 1 {
            groups.extend(of.chunks(a.size).filter(|g| g.len() > 1).map(|g| g.to_vec()));
        }
    }
    for (n, group) in groups.iter().enumerate() {
        let parts: Vec<BatchChange> = group
            .iter()
            .map(|&i| BatchChange {
                key: changes[i].key.clone(),
                title: changes[i].title.clone(),
                branch: format!("{}-replay", changes[i].key.to_lowercase()),
                base: changes[i].base.clone(),
                head: changes[i].commit.clone(),
                worktree: dirs[i].clone(),
                files: changes[i].files.clone(),
                previous: None,
            })
            .collect();
        let reviewer = &changes[group[0]].reviewer;
        let reply = match rules.run_review_prompt(reviewer, &dirs[group[0]], &rules.batch_prompt(&parts)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("batch {}: did not finish: {e:#}", n + 1);
                continue;
            }
        };
        let split = rules.parse_batch(reviewer, &reply.text, reply.seconds, &parts);
        let share = reply.tokens.map(|t| t / group.len() as u64);
        for (&i, review) in group.iter().zip(split.reviews) {
            match review {
                Ok(mut r) => {
                    r.tokens = share;
                    eprintln!("{} batched: {} (batch {}, {} s)", changes[i].key, r.verdict, n + 1, reply.seconds);
                    batched[i] = Some((r, n + 1, group.len()));
                }
                Err(why) => eprintln!("{} batched: goes alone: {why}", changes[i].key),
            }
        }
        for (j, k, what) in split.interactions {
            eprintln!("batch {}: INTERACTION {} and {}: {what}", n + 1, changes[group[j]].key, changes[group[k]].key);
        }
    }

    let mut pairs = vec![];
    let mut drift = vec![];
    let mut kept = vec![];
    for (i, c) in changes.iter().enumerate() {
        let Some(single) = &singles[i] else { continue };
        let alone = Side::of(single, 1);
        drift.push(Pair { issue: c.key.clone(), batch: 0, batch_size: 1, single: c.original.clone(), batched: alone.clone() });
        if let Some((review, batch, size)) = &batched[i] {
            let together = Side::of(review, *size);
            pairs.push(Pair { issue: c.key.clone(), batch: *batch as i64, batch_size: *size, single: alone.clone(), batched: together.clone() });
            kept.push(serde_json::json!({ "change": c, "alone": alone, "batched": together, "batch": batch }));
        }
    }
    println!("Batched against alone (both fresh):");
    for line in Report::of(&pairs).lines() {
        println!("  {line}");
    }
    println!("A fresh review alone against the original one (how much reviews drift anyway; the first side is the original):");
    for line in Report::of(&drift).lines().into_iter().filter(|l| !l.contains("seconds") && !l.contains("Tokens")) {
        println!("  {line}");
    }
    // By priority, the original reviews against the fresh ones, of the changes both cover.
    let count = |f: &[Finding], p: u8| f.iter().filter(|x| x.priority == p).count();
    let reviewed: Vec<(&Change, &Review)> = changes.iter().zip(&singles).filter_map(|(c, s)| s.as_ref().map(|s| (c, s))).collect();
    let orig: Vec<usize> = (0..4).map(|p| reviewed.iter().map(|(c, _)| count(&c.original.findings, p)).sum()).collect();
    let fresh: Vec<usize> = (0..4).map(|p| reviewed.iter().map(|(_, r)| count(&r.findings, p)).sum()).collect();
    let blocked = |v: &dyn Fn(&(&Change, &Review)) -> bool| reviewed.iter().filter(|x| v(x)).count();
    println!("Severities over {} changes (P0/P1/P2/P3): original {:?}, fresh {:?}", reviewed.len(), orig, fresh);
    println!(
        "Asked for changes: original {}, fresh {}",
        blocked(&|(c, _)| c.original.verdict == "changes"),
        blocked(&|(_, r)| r.verdict == "changes")
    );
    for (c, r) in &reviewed {
        println!("  {} {} -> {}", c.key, c.original.verdict, r.verdict);
        for f in &c.original.findings {
            println!("    was P{} {}", f.priority, f.text);
        }
        for f in &r.findings {
            println!("    now P{} {}", f.priority, f.text);
        }
    }
    if let Some(out) = &a.out {
        std::fs::write(out, serde_json::to_string_pretty(&kept)?)?;
    }
    std::fs::remove_dir_all(&scratch).ok();
    Ok(())
}
