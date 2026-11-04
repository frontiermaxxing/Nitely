//! Once an issue's review rounds stop converging, one question to an agent that has not worked on
//! it (NIT-735): do the blocking findings of every round share one cause the fixes so far have not
//! addressed? Each round's reviewer sees one round, and the worker, who patched each path the
//! reviewer found, set the premise and is the weakest judge of it; side by side, the findings of
//! several rounds can show one defect reached by several paths. "No shared cause" is an answer: the
//! findings may be separate bugs. A cause must be testable: it predicts two cases no round
//! reported, and its fix and test cover the class, not the cases reported.

use crate::review::Finding;
use std::collections::BTreeMap;

/// The event an answer is recorded as, once per run of rounds that do not converge.
pub const EVENT: &str = "shared_cause";

/// One round's blocking findings, oldest round first: what it raised that no round before it had.
pub struct Round {
    pub number: usize,
    pub findings: Vec<Finding>,
}

/// The rounds' P0/P1 findings, each once, in the round that first raised it, oldest first.
/// `newest_first` is the run of rounds, the current one first, as the database lists them.
pub fn rounds(newest_first: &[Vec<Finding>]) -> Vec<Round> {
    let mut seen: Vec<String> = vec![];
    let mut out = vec![];
    for (i, findings) in newest_first.iter().rev().enumerate() {
        let new: Vec<Finding> = findings.iter().filter(|f| f.raised_as.unwrap_or(f.priority) <= 1 && !seen.contains(&f.text)).cloned().collect();
        seen.extend(new.iter().map(|f| f.text.clone()));
        if !new.is_empty() {
            out.push(Round { number: i + 1, findings: new });
        }
    }
    out
}

/// Where a finding says it is: `title: file:line, symbol: what goes wrong` gives `file, symbol`,
/// without the line, so paths through one function show together.
fn place(text: &str) -> Option<String> {
    let (_, rest) = text.split_once(": ")?;
    let (place, _) = rest.split_once(": ")?;
    let place = place.trim();
    let (file, symbol) = place.split_once(',').map_or((place, ""), |(f, s)| (f, s));
    let file = match file.rsplit_once(':') {
        Some((path, line)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit() || c == '-') => path,
        _ => file,
    };
    let place = if symbol.is_empty() { file.trim().to_string() } else { format!("{}, {}", file.trim(), symbol.trim()) };
    Some(place).filter(|p| !p.is_empty() && p.len() <= 160)
}

/// The prompt: what the change is for, every round's blocking findings grouped by where they are
/// (with the round that raised each and whether the newest round still has it open), the `total`
/// rounds there were, the diff to
/// read, and the answer's format. Nothing of the worker's notes or sessions.
pub fn prompt(title: &str, base: &str, total: usize, rounds: &[Round], open_now: &[String]) -> String {
    let mut by_place: BTreeMap<String, Vec<(usize, &Finding)>> = BTreeMap::new();
    for r in rounds {
        for f in &r.findings {
            let at = place(&f.text).unwrap_or_else(|| "(no file named)".into());
            by_place.entry(at).or_default().push((r.number, f));
        }
    }
    let mut p = format!(
        "A change on this branch (\"{title}\") went through {n} review rounds that did not converge: each round found \
blocking problems, and the author fixed each one the reviewer found. You have not worked on it. Run `git diff {base}...HEAD` \
for the change as it is now and read the code the findings below cite.\n\nThe blocking findings of every round, grouped by \
where they are:\n",
        n = total
    );
    let mut i = 0;
    for (at, findings) in &by_place {
        p.push_str(&format!("\n{at}\n"));
        for (round, f) in findings {
            i += 1;
            let state = if open_now.contains(&f.text) { "open now" } else { "fixed since" };
            p.push_str(&format!("{i}. round {round}, [P{}], {state}: {}\n", f.raised_as.unwrap_or(f.priority), f.text));
        }
    }
    p.push_str(
        "\nDo these findings share one cause that the fixes so far have not addressed: one defect, reached by several paths, \
that the author patched path by path? Answer from the code, not from the findings' wording. They may be separate bugs, \
and then say so: that is a good answer.\n\nEnd your reply with exactly one of these. Either:\nSHARED CAUSE: none\n\nor:\n\
SHARED CAUSE: <the cause, in one sentence: the file and symbol where it is, and why it produces these findings>\n\
EXPLAINS: <the numbers of the findings above it explains>\n\
PREDICTION 1: <a case it produces that no round reported: the input or state, the file:line it goes through, and what goes wrong>\n\
PREDICTION 2: <a second such case>\n\
FIX: <the change that removes the cause, so no path to it is left, rather than one more guard per reported case>\n\
TEST: <a test that covers the class, such as generated inputs or every path to the cause, rather than the reported cases>\n\
A cause without two predictions no round reported is not one: answer none instead.\n",
    );
    p
}

/// What the agent answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The findings are separate bugs.
    None,
    Cause { cause: String, explains: String, predictions: [String; 2], fix: String, test: String },
}

/// The answer in `text`, from its last `SHARED CAUSE:` line on. None when there is none, or when a
/// cause lacks what makes it testable: two predictions, a fix and a test.
pub fn parse(text: &str) -> Option<Answer> {
    let lines: Vec<&str> = text.lines().map(|l| l.trim().trim_start_matches(['-', '*', ' ']).trim()).collect();
    let start = lines.iter().rposition(|l| label(l, "SHARED CAUSE").is_some())?;
    let cause = label(lines[start], "SHARED CAUSE")?;
    if cause.trim_end_matches('.').eq_ignore_ascii_case("none") {
        return Some(Answer::None);
    }
    let field = |name: &str| lines[start + 1..].iter().find_map(|l| label(l, name)).filter(|v| !v.is_empty()).map(str::to_string);
    Some(Answer::Cause {
        cause: cause.to_string(),
        explains: field("EXPLAINS").unwrap_or_default(),
        predictions: [field("PREDICTION 1")?, field("PREDICTION 2")?],
        fix: field("FIX")?,
        test: field("TEST")?,
    })
    .filter(|_| !cause.is_empty())
}

/// What follows `NAME:` on a line, whatever its case and any bold around the name.
fn label<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let l = line.trim_start_matches('*');
    let head = l.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    let rest = l[name.len()..].trim_start_matches('*');
    rest.strip_prefix(':').map(|v| v.trim().trim_matches('*').trim())
}

/// What the worker reads after what the stalled rounds tell it, and the note on the issue.
pub fn told(answer: &Answer, by: &str) -> String {
    match answer {
        Answer::None => format!(
            "A {by} agent that had not worked on the change read every round's blocking findings side by side and found no shared cause: treat them as separate bugs."
        ),
        Answer::Cause { cause, explains, predictions, fix, test } => {
            let explains = if explains.is_empty() { String::new() } else { format!(" (it explains findings {explains})") };
            format!(
                "A {by} agent that had not worked on the change read every round's blocking findings side by side and names one shared cause{explains}: {cause}\n\
It predicts two cases no round reported:\n1. {}\n2. {}\nFix the cause, not the paths to it: {fix}\nTest the class: {test}\n\
Check both predictions against the code first: if they do not hold, the cause is wrong, so say so in reviewer_note when you submit. \
If they hold, make the fix and its test cover the class, and say in reviewer_note what the cause was and how the change removes it.",
                predictions[0], predictions[1]
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(priority: u8, text: &str, carried: bool) -> Finding {
        Finding { priority, text: text.into(), carried, raised_as: None, now: None }
    }

    /// The case behind NIT-735: several rounds, each finding a new path to one collision. Each
    /// finding shows once, in the round that raised it, grouped by where it is, with whether it is
    /// still open; P2s and repeats are left out.
    #[test]
    fn every_round_s_blocking_findings_show_once_side_by_side() {
        let a = "Two threads merge: intake.ts:40, conversationKey: same subject collides on a held reply";
        let b = "Lost reply: intake.ts:73, conversationKey: an unlisted sender with the same subject";
        let c = "Seeded scan: mail.ts, scan: the seed reuses the subject key";
        let newest_first = vec![
            vec![f(1, c, false), f(1, b, true), f(2, "Typo: x.ts, y: z", false)],
            vec![f(1, b, false)],
            vec![f(1, a, false)],
        ];
        let rounds = rounds(&newest_first);
        assert_eq!(rounds.iter().map(|r| (r.number, r.findings.len())).collect::<Vec<_>>(), vec![(1, 1), (2, 1), (3, 1)]);
        let p = prompt("Read mail threads", "main", newest_first.len(), &rounds, &[b.to_string(), c.to_string()]);
        let intake = p.find("\nintake.ts, conversationKey\n").expect(&p);
        let mail = p.find("\nmail.ts, scan\n").expect(&p);
        assert!(intake < p.find(a).unwrap() && p.find(b).unwrap() < mail, "grouped by place:\n{p}");
        assert!(p.contains(&format!("round 1, [P1], fixed since: {a}")) && p.contains(&format!("round 3, [P1], open now: {c}")), "{p}");
        assert!(!p.contains("Typo") && p.contains("3 review rounds"), "{p}");
    }

    /// A cause counts only with what makes it testable; "none" is an answer of its own.
    #[test]
    fn a_cause_needs_two_predictions_a_fix_and_a_test() {
        assert_eq!(parse("They are unrelated.\nSHARED CAUSE: none"), Some(Answer::None));
        let full = "Reasoning.\n**SHARED CAUSE:** the subject is the conversation key: intake.ts, conversationKey\nEXPLAINS: 1, 2\nPREDICTION 1: a forwarded mail\nPREDICTION 2: a reply with Re: stripped\nFIX: key by thread id\nTEST: generated pairs of conversations with one subject\n";
        match parse(full) {
            Some(Answer::Cause { cause, explains, predictions, .. }) => {
                assert!(cause.starts_with("the subject is") && explains == "1, 2" && predictions[1].contains("Re:"), "{cause}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse(&full.replace("PREDICTION 2: a reply with Re: stripped\n", "")), None, "one prediction is not enough");
        assert_eq!(parse(&full.replace("TEST: generated pairs of conversations with one subject\n", "")), None);
        assert_eq!(parse("No answer line at all."), None);
        let told = told(&parse(full).unwrap(), "codex");
        assert!(told.contains("Check both predictions") && told.contains("reviewer_note"), "{told}");
    }
}
