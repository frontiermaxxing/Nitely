//! Batched review measured against single review (NIT-217, docs/batching.md): for each change,
//! whether the two agree on its verdict, on its P0 and P1 findings (matched by file and symbol),
//! and on severities, and what each cost per change in reviewer time and tokens. The pairs come
//! from shadow batches (`[review] batch = "shadow"`) or from a replay of past changes.

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;
use crate::review::{citations, Finding, Review};

/// What one review said about one change, and what it cost.
#[derive(Debug, Clone, Serialize)]
pub struct Side {
    pub verdict: String,
    pub findings: Vec<Finding>,
    /// Reviewer seconds for this change; a batch's are shared evenly between its changes.
    pub seconds: f64,
    pub tokens: Option<u64>,
}

impl Side {
    /// A review's side. `share` is how many changes its call reviewed: one alone, the batch's size
    /// in a batch (the review's seconds are the call's; its tokens are already this change's share).
    pub fn of(review: &Review, share: usize) -> Side {
        Side { verdict: review.verdict.clone(), findings: review.findings.clone(), seconds: review.seconds as f64 / share.max(1) as f64, tokens: review.tokens }
    }
}

/// How two reviews of one change agree.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Agreement {
    pub same_verdict: bool,
    /// P0/P1 findings on each side.
    pub blocking: (usize, usize),
    /// Findings both raised (by file and symbol) at P0/P1 on both sides.
    pub blocking_matched: usize,
    /// Findings both raised, at any priority.
    pub matched: usize,
    /// Findings both raised at different priorities: (first side, second side).
    pub severity: Vec<(u8, u8)>,
}

impl Agreement {
    /// Findings both raised that one side calls P0/P1 and the other P2 or lower: the ones that
    /// change a verdict.
    pub fn crossed(&self) -> usize {
        self.severity.iter().filter(|(a, b)| (*a <= 1) != (*b <= 1)).count()
    }
}

/// Where a finding points: the files it cites and the symbol after the file, from the
/// `title: file, symbol: what` form the prompt asks for.
#[derive(Debug, PartialEq)]
struct Place {
    files: Vec<String>,
    symbol: Option<String>,
}

fn place(f: &Finding) -> Place {
    let path = |p: &str| p.replace('\\', "/").trim_start_matches("./").to_lowercase();
    let mut files: Vec<String> = citations(&f.text).into_iter().map(|(p, _)| path(&p)).collect();
    files.dedup();
    let symbol = f.text.split_once(": ").and_then(|(_, rest)| rest.split_once(": ")).and_then(|(at, _)| at.split_once(',')).map(|(_, s)| s.trim().trim_matches('`').trim().to_lowercase()).filter(|s| {
        !s.is_empty() && !s.contains(' ') && citations(s).is_empty()
    });
    Place { files, symbol }
}

fn same_place(a: &Place, b: &Place) -> bool {
    let symbols_agree = match (&a.symbol, &b.symbol) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    };
    if a.files.is_empty() || b.files.is_empty() {
        return a.symbol.is_some() && a.symbol == b.symbol;
    }
    // One path may be written shorter than the other (`land.rs`, `crates/x/src/land.rs`), but two
    // files of one name in different folders are different files.
    let same_file = |x: &String, y: &String| x == y || x.ends_with(&format!("/{y}")) || y.ends_with(&format!("/{x}"));
    a.files.iter().any(|f| b.files.iter().any(|g| same_file(f, g))) && symbols_agree
}

/// Compares two reviews of one change, pairing their findings one to one by place.
pub fn agree(a: &Side, b: &Side) -> Agreement {
    let blocking = |s: &Side| s.findings.iter().filter(|f| f.priority <= 1).count();
    let places_b: Vec<Place> = b.findings.iter().map(place).collect();
    let mut used = vec![false; b.findings.len()];
    let mut out = Agreement { same_verdict: a.verdict == b.verdict, blocking: (blocking(a), blocking(b)), ..Default::default() };
    for f in &a.findings {
        let here = place(f);
        // The closest priority first, so one defect is not paired with another of the same file.
        let mut candidates: Vec<usize> = (0..b.findings.len()).filter(|&j| !used[j] && same_place(&here, &places_b[j])).collect();
        candidates.sort_by_key(|&j| (b.findings[j].priority as i16 - f.priority as i16).abs());
        if let Some(&j) = candidates.first() {
            used[j] = true;
            out.matched += 1;
            let g = &b.findings[j];
            if f.priority <= 1 && g.priority <= 1 {
                out.blocking_matched += 1;
            }
            if f.priority != g.priority {
                out.severity.push((f.priority, g.priority));
            }
        }
    }
    out
}

/// One change reviewed both ways.
#[derive(Debug, Clone, Serialize)]
pub struct Pair {
    pub issue: String,
    pub batch: i64,
    pub batch_size: usize,
    pub single: Side,
    pub batched: Side,
}

/// The pairs' agreement and cost, summed.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Report {
    pub changes: usize,
    pub batches: usize,
    pub same_verdict: usize,
    pub blocking_single: usize,
    pub blocking_batched: usize,
    pub blocking_matched: usize,
    /// Findings both raised at different priorities, and of those, ones on different sides of P1.
    pub severity_shifts: usize,
    pub crossed: usize,
    pub seconds_single: f64,
    pub seconds_batched: f64,
    /// Over the changes whose reviews both said their tokens.
    pub tokens_single: Option<f64>,
    pub tokens_batched: Option<f64>,
}

impl Report {
    pub fn of(pairs: &[Pair]) -> Report {
        let mut r = Report { changes: pairs.len(), ..Default::default() };
        let mut batches: Vec<i64> = pairs.iter().map(|p| p.batch).collect();
        batches.sort();
        batches.dedup();
        r.batches = batches.len();
        let mut tokens = (0u64, 0u64, 0usize);
        for p in pairs {
            let a = agree(&p.single, &p.batched);
            r.same_verdict += a.same_verdict as usize;
            r.blocking_single += a.blocking.0;
            r.blocking_batched += a.blocking.1;
            r.blocking_matched += a.blocking_matched;
            r.severity_shifts += a.severity.len();
            r.crossed += a.crossed();
            r.seconds_single += p.single.seconds;
            r.seconds_batched += p.batched.seconds;
            if let (Some(x), Some(y)) = (p.single.tokens, p.batched.tokens) {
                tokens = (tokens.0 + x, tokens.1 + y, tokens.2 + 1);
            }
        }
        if r.changes > 0 {
            r.seconds_single /= r.changes as f64;
            r.seconds_batched /= r.changes as f64;
        }
        if tokens.2 > 0 {
            r.tokens_single = Some(tokens.0 as f64 / tokens.2 as f64);
            r.tokens_batched = Some(tokens.1 as f64 / tokens.2 as f64);
        }
        r
    }

    /// The report as a few lines for a person.
    pub fn lines(&self) -> Vec<String> {
        if self.changes == 0 {
            return vec!["No change has been reviewed both alone and in a shadow batch yet.".into()];
        }
        let pct = |n: usize, of: usize| if of == 0 { "-".to_string() } else { format!("{}%", n * 100 / of) };
        let mut lines = vec![
            format!("{} changes in {} batches.", self.changes, self.batches),
            format!("Same verdict: {} of {} ({}).", self.same_verdict, self.changes, pct(self.same_verdict, self.changes)),
            format!(
                "P0/P1 findings: {} alone, {} batched, {} the same (by file and symbol).",
                self.blocking_single, self.blocking_batched, self.blocking_matched
            ),
            format!("Findings both raised at a different priority: {}, of which {} on different sides of P1.", self.severity_shifts, self.crossed),
            format!("Reviewer seconds per change: {:.0} alone, {:.0} batched.", self.seconds_single, self.seconds_batched),
        ];
        if let (Some(a), Some(b)) = (self.tokens_single, self.tokens_batched) {
            lines.push(format!("Tokens per change: {:.0} alone, {:.0} batched ({:.0}% fewer).", a, b, (1.0 - b / a.max(1.0)) * 100.0));
        }
        lines
    }
}

/// The changes a shadow batch reviewed, each against the single review that decided it.
pub fn shadow_pairs(db: &Db) -> Result<Vec<Pair>> {
    let mut stmt = db.conn.prepare(
        "SELECT q.issue_key, q.batch_id, q.result, r.verdict, r.findings, COALESCE(r.seconds, 0), r.tokens,
                (SELECT COUNT(*) FROM review_queue o WHERE o.batch_id = q.batch_id)
         FROM review_queue q JOIN review r ON r.id = q.review_id
         WHERE q.shadow = 1 AND q.state = 'done' AND q.result IS NOT NULL ORDER BY q.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;
    let mut pairs = vec![];
    for row in rows {
        let (issue, batch, result, verdict, findings, seconds, tokens, size) = row?;
        let Ok(batched) = serde_json::from_str::<Review>(&result) else { continue };
        let single = Side { verdict, findings: serde_json::from_str(&findings).unwrap_or_default(), seconds: seconds as f64, tokens: tokens.map(|t| t as u64) };
        let size = size.max(1) as usize;
        pairs.push(Pair { issue, batch, batch_size: size, single, batched: Side::of(&batched, size) });
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(priority: u8, text: &str) -> Finding {
        Finding { priority, text: text.into(), carried: false, raised_as: None, now: None }
    }

    fn side(verdict: &str, findings: Vec<Finding>, seconds: f64, tokens: Option<u64>) -> Side {
        Side { verdict: verdict.into(), findings, seconds, tokens }
    }

    #[test]
    fn findings_pair_by_file_and_symbol_and_severity_shifts_are_counted() {
        let alone = side(
            "changes",
            vec![f(1, "Same-second approval: crates/nitely-core/src/land.rs, approved_at: two approvals in one second"), f(2, "Edge: src/x.rs, parse: an empty line")],
            60.0,
            Some(30_000),
        );
        let batched = side(
            "pass",
            vec![f(2, "Approval race: land.rs, approved_at: the same second"), f(2, "Other: src/y.rs, render: never drawn")],
            30.0,
            Some(18_000),
        );
        let a = agree(&alone, &batched);
        assert!(!a.same_verdict);
        assert_eq!((a.blocking, a.blocking_matched, a.matched), ((1, 0), 0, 1));
        assert_eq!((a.severity.clone(), a.crossed()), (vec![(1, 2)], 1));
        // Same file, another symbol: a different defect. Same name and symbol in another folder: too.
        let other = side("changes", vec![f(1, "Leak: crates/nitely-core/src/land.rs, push_token: logged")], 1.0, None);
        assert_eq!(agree(&alone, &other).matched, 0);
        let elsewhere = side("changes", vec![f(1, "Approval race: crates/other/src/land.rs, approved_at: the same second")], 1.0, None);
        assert_eq!(agree(&alone, &elsewhere).matched, 0);

        let report = Report::of(&[Pair { issue: "NIT-1".into(), batch: 1, batch_size: 2, single: alone.clone(), batched: batched.clone() }]);
        assert_eq!((report.changes, report.same_verdict, report.crossed, report.tokens_batched), (1, 0, 1, Some(18_000.0)));
        let said = report.lines().join("\n");
        assert!(said.contains("Same verdict: 0 of 1 (0%)") && said.contains("Tokens per change: 30000 alone, 18000 batched (40% fewer)"), "{said}");
    }
}
