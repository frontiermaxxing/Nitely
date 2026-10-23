//! A review finding the person accepts as it is (NIT-191, docs/approvals.md): a trade-off they
//! decided, not a defect left to fix, such as a migration they ruled out. An agent asks for it
//! with one P0/P1 finding of the latest review and its reasoning (`request_acceptance`); the person
//! answers in the Inbox or on the phone, Accept or Send back. An acceptance is signed like an
//! approval, by the app's key or a paired phone's, and bound to the finding and the patch it was
//! accepted on. `land` passes a review whose open P0/P1 findings are all accepted that way, and the
//! next review round is told about them instead of raising them again.

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::approval::{sign, unhex, verify, PHONE_SKEW};
use crate::db::{now, Db};
use crate::issues::{Actor, Ask};
use crate::review::Finding;
use crate::submit::ReviewRecord;

/// The finding an agent asks the person to accept, as the latest review recorded it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskedFinding {
    pub priority: u8,
    pub text: String,
}

/// A finding the person accepted.
#[derive(Debug, Clone, Serialize)]
pub struct Accepted {
    pub issue_key: String,
    pub patch_id: String,
    pub priority: u8,
    pub finding: String,
    pub reason: String,
    pub ts: i64,
}

/// What is signed for accepting `finding` of `key`'s review of `patch`, with `reason`, at `ts`.
pub fn message(key: &str, patch: &str, finding: &str, reason: &str, ts: i64) -> String {
    format!("nitely-accept-v1\n{}\n{patch}\n{finding}\n{reason}\n{ts}", key.to_uppercase())
}

/// What the issue does once the person decided: the next state and what the timeline says.
#[derive(Debug, Clone, Serialize)]
pub struct Decided {
    pub ask: Ask,
    pub state: String,
}

impl Db {
    /// An agent asks the person to accept one P0/P1 finding of `key`'s latest review: by its number
    /// in that review's findings (from 1) or its text. `why` is the agent's reasoning, which the
    /// person reads. It is an ask (answered like one, `get_answer`) that carries the finding. The
    /// question holds the land, never the agent (NIT-568): the issue stays where it is.
    pub fn request_acceptance(&mut self, key: &str, finding: &str, why: &str, cwd: Option<&str>, actor: &Actor) -> Result<Ask> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if why.trim().is_empty() {
            bail!("say why the person should accept it as it is: that is what they decide on");
        }
        let review = self.reviews(&issue.key)?.into_iter().find(|r| !r.reviewer.is_empty()).with_context(|| format!("{} has no review yet; submit it first", issue.key))?;
        let blocking: Vec<&Finding> = review.findings.iter().filter(|f| f.priority <= 1).collect();
        let wanted = finding.trim();
        let found = match wanted.parse::<usize>() {
            Ok(n) => review.findings.get(n.wrapping_sub(1)).filter(|f| f.priority <= 1),
            Err(_) => blocking.iter().copied().find(|f| f.text.trim() == wanted),
        };
        let Some(found) = found else {
            let listed: Vec<String> = review.findings.iter().enumerate().filter(|(_, f)| f.priority <= 1).map(|(i, f)| format!("{}. [P{}] {}", i + 1, f.priority, f.text)).collect();
            if listed.is_empty() {
                bail!("{}'s latest review has no P0 or P1 finding: nothing blocks it", issue.key);
            }
            bail!("{wanted:?} is not a P0 or P1 finding of {}'s latest review; give its number or exact text:\n{}", issue.key, listed.join("\n"));
        };
        if self.accepted_for(&issue.key, &review.patch_id)?.iter().any(|a| a.finding == found.text) {
            bail!("the person already accepted that finding for this patch");
        }
        let asked = AskedFinding { priority: found.priority, text: found.text.clone() };
        // One decision per finding: a request already open for it stands, with this reasoning (NIT-353).
        let ask = match self.open_request_for(&issue.key, &found.text)? {
            Some(open) => {
                self.conn.execute("UPDATE ask SET finding = ?2, question = ?3 WHERE id = ?1", params![open.id, serde_json::to_string(&asked)?, why])?;
                open
            }
            None => {
                let ask = self.create_ask(why, &[], Some(&issue.key), cwd, actor)?;
                self.conn.execute("UPDATE ask SET finding = ?2 WHERE id = ?1", params![ask.id, serde_json::to_string(&asked)?])?;
                ask
            }
        };
        self.ask(ask.id)?.context("ask vanished")
    }

    /// Review rounds on `key` stopped converging (`why`): the person decides each P0/P1 finding of
    /// its latest review not yet accepted, accept it as it is or send it back, as a request to
    /// accept it (the Inbox's Review finding). The issue stays with its agent, who goes on fixing
    /// (NIT-568). Returns its state.
    pub fn ask_to_decide_findings(&mut self, key: &str, why: &str, actor: &Actor) -> Result<String> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        let review = self.reviews(&issue.key)?.into_iter().find(|r| !r.reviewer.is_empty()).with_context(|| format!("{} has no review", issue.key))?;
        let open: Vec<Ask> = self.all_open_asks()?.into_iter().filter(|a| a.issue_key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&issue.key))).collect();
        let question = format!("The review rounds are not converging ({why}). Accept this finding as it is if it is a trade-off you decided; send it back and it stays a defect to fix.");
        for f in self.unaccepted_blockers(&issue.key, &review)? {
            // One request per finding; one already open for it stands.
            if open.iter().any(|a| a.finding.as_ref().is_some_and(|x| x.text == f.text)) {
                continue;
            }
            self.request_acceptance(&issue.key, &f.text, &question, None, actor)?;
        }
        Ok(self.issue(&issue.key)?.with_context(|| format!("no issue {key}"))?.state)
    }

    /// When the person last decided a request to accept a finding of `key` (0 when never): their
    /// decision to go on starts a new count of review rounds.
    /// The latest answer the person gave to one of the issue's findings, as its event; one answered
    /// without an event (older records) by its time alone.
    pub fn findings_decided_event(&self, key: &str) -> Result<Option<crate::submit::Decision>> {
        let at = self.findings_decided_at(key)?;
        if at == 0 {
            return Ok(None);
        }
        let id: Option<i64> = self.conn.query_row(
            "SELECT MAX(e.id) FROM event e JOIN ask a ON a.issue_key = e.issue_key COLLATE NOCASE AND e.type = 'answer' AND e.ts = a.answered_at AND e.summary = a.answer
             WHERE a.issue_key = ?1 COLLATE NOCASE AND a.finding IS NOT NULL AND a.answer IS NOT NULL AND a.answered_at = ?2",
            rusqlite::params![key, at],
            |r| r.get(0),
        )?;
        Ok(Some(crate::submit::Decision { id, ts: at }))
    }

    pub fn findings_decided_at(&self, key: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(answered_at), 0) FROM ask WHERE issue_key = ?1 COLLATE NOCASE AND finding IS NOT NULL AND answer IS NOT NULL",
            [key],
            |r| r.get(0),
        )?)
    }

    /// The person accepts the finding of open ask `ask_id`, signed with the app's key `secret`.
    pub fn accept_finding(&mut self, ask_id: i64, reason: &str, secret: &[u8; 32]) -> Result<Decided> {
        let (ask, key, finding, review) = self.acceptance_ask(ask_id)?;
        let reason = one_line(reason)?;
        let ts = now();
        let signature = sign(secret, &message(&key, &review.patch_id, &finding.text, &reason, ts));
        self.insert_acceptance(&key, &review.patch_id, &finding, &reason, ts, &signature, "", None)?;
        self.after_acceptance(ask, &key, &finding, &reason, "")
    }

    /// The person accepted the finding of open ask `ask_id` on a paired phone, which signed it at its
    /// own time `signed_at` for the patch `patch` it showed (docs/phone.md).
    pub fn accept_finding_from_phone(&mut self, ask_id: i64, patch: &str, reason: &str, device: &crate::phone::Device, signed_at: i64, signature: &str) -> Result<Decided> {
        let (ask, key, finding, review) = self.acceptance_ask(ask_id)?;
        if review.patch_id != patch {
            bail!("{key} was reviewed again since you looked; look at it once more");
        }
        let reason = one_line(reason)?;
        self.insert_acceptance(&key, patch, &finding, &reason, now(), signature, &device.id, Some(signed_at))?;
        self.after_acceptance(ask, &key, &finding, &reason, &format!(" from the phone “{}”", device.name))
    }

    /// The person sends the request back: the finding stays a defect to fix, for the reason given.
    /// An agent working on the issue goes on as it was; the issue's other questions stand, each
    /// its own decision (NIT-568).
    pub fn decline_finding(&mut self, ask_id: i64, why: &str, from: &str) -> Result<Decided> {
        let (_, key, finding, _) = self.acceptance_ask_as_asked(ask_id)?;
        let why = one_line(why)?;
        let ask = self.answer_ask(ask_id, &format!("Not accepted: {why}. Fix it."), &Actor::Human)?;
        self.close_decided_alike(&key, ask_id, &finding.text)?;
        let issue = self.issue(&key)?.with_context(|| format!("no issue {key}"))?;
        if issue.owner == "human" {
            self.set_owner(&key, "agent", &Actor::Human)?;
        }
        let issue = self.issue(&key)?.with_context(|| format!("no issue {key}"))?;
        let to = crate::submit_rules::after_answer(false, &issue.state, 0, false);
        let state = if issue.state != to { self.move_issue(&key, to, None, None, &Actor::Human)?.state } else { issue.state };
        self.add_note(&key, &format!("Did not accept{from} [P{}] {}: {why}", finding.priority, finding.text), "note", &Actor::Human)?;
        Ok(Decided { ask, state })
    }

    /// Every acceptance on `key` (newest first), signatures not checked: what was decided, for
    /// showing. What counts is `accepted_for`.
    pub fn acceptances(&self, key: &str) -> Result<Vec<Accepted>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, patch_id, priority, finding, reason, ts FROM accepted_finding WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts DESC, id DESC",
        )?;
        let rows = stmt.query_map([key], |r| Ok(Accepted { issue_key: r.get(0)?, patch_id: r.get(1)?, priority: r.get(2)?, finding: r.get(3)?, reason: r.get(4)?, ts: r.get(5)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The acceptances on `key` that count: signed by the app's key, or by a phone still paired that
    /// signed within PHONE_SKEW of it, for the patch `patch`.
    pub fn accepted_for(&self, key: &str, patch: &str) -> Result<Vec<Accepted>> {
        let Ok(public) = crate::approval::trusted_key() else { return Ok(vec![]) };
        Ok(self.signed_acceptances(key, &public)?.into_iter().filter(|a| a.patch_id == patch).collect())
    }

    /// Every acceptance on `key` that verifies with the app key `public`, on any patch: what the
    /// next review round is told.
    pub fn signed_acceptances(&self, key: &str, public: &[u8; 32]) -> Result<Vec<Accepted>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, patch_id, priority, finding, reason, ts, signature, device, signed_at FROM accepted_finding
             WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts DESC, id DESC",
        )?;
        type Row = (Accepted, String, String, Option<i64>);
        let rows: Vec<Row> = stmt
            .query_map([key], |r| {
                Ok((Accepted { issue_key: r.get(0)?, patch_id: r.get(1)?, priority: r.get(2)?, finding: r.get(3)?, reason: r.get(4)?, ts: r.get(5)? }, r.get(6)?, r.get(7)?, r.get(8)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut devices: Option<Vec<crate::phone::Device>> = None;
        let mut out = Vec::new();
        for (a, signature, device, signed_at) in rows {
            let ok = if device.is_empty() {
                verify(public, &message(&a.issue_key, &a.patch_id, &a.finding, &a.reason, a.ts), &signature)
            } else {
                let list = devices.get_or_insert_with(|| crate::phone::trusted_devices(public).unwrap_or_default());
                let key = list.iter().find(|d| d.id == device).and_then(|d| unhex::<32>(&d.public_key));
                match (key, signed_at.filter(|t| (t - a.ts).abs() <= PHONE_SKEW)) {
                    (Some(k), Some(t)) => verify(&k, &message(&a.issue_key, &a.patch_id, &a.finding, &a.reason, t), &signature),
                    _ => false,
                }
            };
            if ok {
                out.push(a);
            }
        }
        Ok(out)
    }

    /// Whether `review` lets the change go on: it did not ask for changes, or every P0/P1 finding it
    /// asked them for is accepted by the person, signed for its patch.
    /// Whether recorded acceptances for `review`'s patch, verified or not, cover each of its P0/P1
    /// findings: what a machine that cannot read the approval key can still tell (NIT-580 review).
    pub fn recorded_acceptances_cover(&self, key: &str, review: &ReviewRecord) -> Result<bool> {
        let mut stmt = self.conn.prepare("SELECT finding FROM accepted_finding WHERE issue_key = ?1 COLLATE NOCASE AND patch_id = ?2")?;
        let recorded: Vec<String> = stmt.query_map(rusqlite::params![key, review.patch_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(review.findings.iter().filter(|f| f.priority <= 1).all(|f| recorded.contains(&f.text)))
    }

    pub fn review_passes(&self, key: &str, review: &ReviewRecord) -> Result<bool> {
        Ok(review.verdict != "changes" || self.unaccepted_blockers(key, review)?.is_empty())
    }

    /// The P0/P1 findings of `review` the person has not accepted for its patch: what still blocks it.
    pub fn unaccepted_blockers(&self, key: &str, review: &ReviewRecord) -> Result<Vec<Finding>> {
        let accepted = self.accepted_for(key, &review.patch_id)?;
        Ok(review.findings.iter().filter(|f| f.priority <= 1 && !accepted.iter().any(|a| a.finding == f.text)).cloned().collect())
    }

    /// The open ask `ask_id` asking to accept a finding, its issue key, the finding, and the
    /// issue's latest review, which must still hold that finding.
    fn acceptance_ask(&self, ask_id: i64) -> Result<(Ask, String, AskedFinding, ReviewRecord)> {
        let (ask, key, finding, review) = self.acceptance_ask_as_asked(ask_id)?;
        let review = review.with_context(|| format!("{key} has no review"))?;
        if !review.findings.iter().any(|f| f.text == finding.text) {
            bail!("{key} was reviewed again and that finding is not in the latest review; the request no longer applies");
        }
        Ok((ask, key, finding, review))
    }

    /// As `acceptance_ask`, whether or not the latest review still holds the finding: sending a
    /// request back works even when a later review made it moot.
    fn acceptance_ask_as_asked(&self, ask_id: i64) -> Result<(Ask, String, AskedFinding, Option<ReviewRecord>)> {
        let ask = self.ask(ask_id)?.with_context(|| format!("no ask {ask_id}"))?;
        let finding = ask.finding.clone().with_context(|| format!("ask {ask_id} does not ask to accept a finding"))?;
        if ask.answer.is_some() || ask.closed.is_some() {
            bail!("that request was already decided");
        }
        let key = ask.issue_key.clone().context("the request names no issue")?;
        let key = self.issue(&key)?.with_context(|| format!("no issue {key}"))?.key;
        let review = self.reviews(&key)?.into_iter().find(|r| !r.reviewer.is_empty());
        Ok((ask, key, finding, review))
    }

    /// Brings `key`'s open requests to accept a finding up to its latest review (`findings`). Each
    /// round is told the findings the person is deciding, to report one that still holds in the
    /// same words (`submit::review_context`, NIT-353): whether a finding in other words is the same
    /// one is the reviewer's judgement, which no likeness of file or code can make safely (two
    /// blockers in one function are two decisions). So a request stands while its words do, and one
    /// the round no longer raises closes. Two open in the same words become one: the newest stands.
    /// Returns how many closed.
    pub fn close_superseded_requests(&self, key: &str, findings: &[Finding]) -> Result<usize> {
        let mut open = self.open_finding_requests(key)?;
        open.sort_by_key(|a| std::cmp::Reverse(a.id));
        let words = |a: &Ask| a.finding.as_ref().map(|f| f.text.clone()).unwrap_or_default();
        let mut closing: Vec<(i64, String)> = vec![];
        // Requests in the same words are one: the newest stands.
        let mut first: Vec<(String, i64)> = vec![];
        for a in &open {
            match first.iter().find(|(w, _)| *w == words(a)) {
                Some((_, newer)) => closing.push((a.id, format!("closed: ask #{newer} asks for the same finding"))),
                None if findings.iter().any(|x| x.text == words(a)) => first.push((words(a), a.id)),
                None => {
                    first.push((words(a), a.id));
                    closing.push((a.id, FIXED.to_string()));
                }
            }
        }
        for (id, why) in &closing {
            self.conn.execute("UPDATE ask SET closed = ?2 WHERE id = ?1", params![id, why])?;
        }
        Ok(closing.len())
    }

    /// `key`'s open requests to accept a finding.
    fn open_finding_requests(&self, key: &str) -> Result<Vec<Ask>> {
        Ok(self.all_open_asks()?.into_iter().filter(|a| a.issue_key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(key)) && a.finding.is_some()).collect())
    }

    /// The open request to accept `finding` of `key`, the newest when there are several.
    fn open_request_for(&self, key: &str, finding: &str) -> Result<Option<Ask>> {
        let mut open = self.open_finding_requests(key)?;
        open.sort_by_key(|a| std::cmp::Reverse(a.id));
        Ok(open.iter().position(|a| a.finding.as_ref().is_some_and(|f| f.text == finding)).map(|i| open.swap_remove(i)))
    }

    /// Closes the other open requests for the finding the person just decided in ask `decided`: one
    /// decision answers them all.
    fn close_decided_alike(&self, key: &str, decided: i64, finding: &str) -> Result<()> {
        for a in self.open_finding_requests(key)? {
            if a.id != decided && a.finding.as_ref().is_some_and(|f| f.text == finding) {
                self.conn.execute("UPDATE ask SET closed = ?2 WHERE id = ?1", params![a.id, format!("closed: decided in ask #{decided}")])?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_acceptance(&self, key: &str, patch: &str, f: &AskedFinding, reason: &str, ts: i64, signature: &str, device: &str, signed_at: Option<i64>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO accepted_finding (issue_key, patch_id, priority, finding, reason, ts, signature, device, signed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![key, patch, f.priority, f.text, reason, ts, signature, device, signed_at],
        )?;
        Ok(())
    }

    /// Answers the request and moves the issue: when nothing else blocks the latest review, it
    /// goes on as a passed review would (In Review, or Needs You when it still waits for an
    /// approval); otherwise back to the agent to fix the rest.
    fn after_acceptance(&mut self, _ask: Ask, key: &str, finding: &AskedFinding, reason: &str, from: &str) -> Result<Decided> {
        let answered = self.answer_ask(_ask.id, &format!("Accepted: {reason}. Stop working on this finding: land no longer waits on it."), &Actor::Human)?;
        self.close_decided_alike(key, _ask.id, &finding.text)?;
        let review = self.reviews(key)?.into_iter().find(|r| !r.reviewer.is_empty()).context("no review")?;
        // The finding the person just accepted is decided by this call, whatever key signed it: under
        // the test harness its throwaway key, which `accepted_for` never trusts. So a harness run
        // moves as a real one does, while land still checks every acceptance against the keychain's
        // key alone and nothing the harness signed ever lands (NIT-157).
        let mut blocking = self.unaccepted_blockers(key, &review)?;
        blocking.retain(|f| f.text != finding.text);
        // Handing a person's own issue back to the agents moves it, so its state is read after.
        if self.issue(key)?.with_context(|| format!("no issue {key}"))?.owner == "human" {
            self.set_owner(key, "agent", &Actor::Human)?;
        }
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        // Findings the person is still deciding stay with the agent, who goes on fixing (NIT-568).
        let asked: Vec<String> = self.all_open_asks()?.into_iter().filter(|a| a.issue_key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(key))).filter_map(|a| a.finding.map(|f| f.text)).collect();
        // Only the app reads the keys acceptances are checked with (macOS): an agent's land could
        // not see this one, so the change waits for an approval, and the app lands it.
        let needs_approval = review.approver == "human" || !crate::approval::agents_can_verify();
        let to = crate::submit_rules::after_answer(true, &issue.state, blocking.len(), needs_approval);
        // `approval`: the change now waits for the person's approval, a request of its own.
        let (then, approval) = if !blocking.is_empty() {
            let deciding = blocking.iter().filter(|f| asked.contains(&f.text)).count();
            (format!("{} other P0/P1 finding(s) still to fix or decide ({deciding} wait for your decision)", blocking.len()), false)
        } else if review.approver == "human" {
            (WAITS_FOR_APPROVAL.to_string(), true)
        } else if needs_approval {
            (APPROVE_IN_THE_APP.to_string(), true)
        } else {
            ("the review now passes: land it".to_string(), false)
        };
        let state = if issue.state != to { self.move_issue(key, to, None, None, &Actor::Human)?.state } else { issue.state };
        self.add_note(key, &format!("Accepted{from} [P{}] {}: {reason}. Then {then}.", finding.priority, finding.text), "note", &Actor::Human)?;
        // The issue was already in Needs You for the finding, so no move says it now waits for an
        // approval; this event does, for the goal report (NIT-354).
        if approval {
            self.add_note(key, &then, AWAITS_APPROVAL, &Actor::Human)?;
        }
        Ok(Decided { ask: answered, state })
    }
}

/// Why a question about a finding closed without an answer: a later review no longer raises it.
pub const FIXED: &str = "closed: fixed, no decision needed (the latest review no longer raises it)";

/// The event kind for a change that waits for the person's approval after they accepted a finding.
pub const AWAITS_APPROVAL: &str = "awaits_approval";

/// What the acceptance note says when the change then waits for the person's approval; the goal
/// report reads acceptances from before `AWAITS_APPROVAL` by these.
pub const WAITS_FOR_APPROVAL: &str = "the review now passes, and the change waits for your approval";
pub const APPROVE_IN_THE_APP: &str = "the review now passes; approve the change in the app, which lands it";

/// The person's reason, on one line and not empty.
fn one_line(text: &str) -> Result<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        bail!("give a one-line reason");
    }
    if line.chars().count() > 500 {
        bail!("keep the reason to one line (500 characters at most)");
    }
    Ok(line)
}
