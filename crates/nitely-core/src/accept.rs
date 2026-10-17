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
    /// person reads. It is an ask (answered like one, `get_answer`) that carries the finding, and
    /// the issue goes to Needs You.
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
        let ask = self.create_ask(why, &[], Some(&issue.key), cwd, actor)?;
        self.conn.execute("UPDATE ask SET finding = ?2 WHERE id = ?1", params![ask.id, serde_json::to_string(&asked)?])?;
        if issue.origin == "nitely" && issue.state != "Needs You" {
            self.move_issue(&issue.key, "Needs You", None, None, actor)?;
        }
        self.ask(ask.id)?.context("ask vanished")
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
    pub fn decline_finding(&mut self, ask_id: i64, why: &str, from: &str) -> Result<Decided> {
        let (_, key, finding, _) = self.acceptance_ask(ask_id)?;
        let why = one_line(why)?;
        let ask = self.answer_ask(ask_id, &format!("Not accepted: {why}. Fix it."), &Actor::Human)?;
        let issue = self.issue(&key)?.with_context(|| format!("no issue {key}"))?;
        if issue.owner == "human" {
            self.set_owner(&key, "agent", &Actor::Human)?;
        }
        let state = self.move_issue(&key, "Todo", None, None, &Actor::Human)?.state;
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
        let ask = self.ask(ask_id)?.with_context(|| format!("no ask {ask_id}"))?;
        let finding = ask.finding.clone().with_context(|| format!("ask {ask_id} does not ask to accept a finding"))?;
        if ask.answer.is_some() || ask.closed.is_some() {
            bail!("that request was already decided");
        }
        let key = ask.issue_key.clone().context("the request names no issue")?;
        let key = self.issue(&key)?.with_context(|| format!("no issue {key}"))?.key;
        let review = self.reviews(&key)?.into_iter().find(|r| !r.reviewer.is_empty()).with_context(|| format!("{key} has no review"))?;
        if !review.findings.iter().any(|f| f.text == finding.text) {
            bail!("{key} was reviewed again and that finding is not in the latest review; the request no longer applies");
        }
        Ok((ask, key, finding, review))
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
        let answered = self.answer_ask(_ask.id, &format!("Accepted: {reason}"), &Actor::Human)?;
        let review = self.reviews(key)?.into_iter().find(|r| !r.reviewer.is_empty()).context("no review")?;
        let blocking = self.unaccepted_blockers(key, &review)?;
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.owner == "human" {
            self.set_owner(key, "agent", &Actor::Human)?;
        }
        let (to, then) = if !blocking.is_empty() {
            ("In Progress", format!("{} other P0/P1 finding(s) still to fix", blocking.len()))
        } else if review.approver == "human" {
            ("Needs You", "the review now passes, and the change waits for your approval".to_string())
        } else {
            ("In Review", "the review now passes: land it".to_string())
        };
        let state = if issue.state != to { self.move_issue(key, to, None, None, &Actor::Human)?.state } else { issue.state };
        self.add_note(key, &format!("Accepted{from} [P{}] {}: {reason}. Then {then}.", finding.priority, finding.text), "note", &Actor::Human)?;
        Ok(Decided { ask: answered, state })
    }
}

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
