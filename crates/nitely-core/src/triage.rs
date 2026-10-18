//! Triage (NIT-245, docs/coordinator.md, decision 4): an agent's question about the board or the
//! code (who holds a file, whether an issue is already done, whether a carried finding still
//! applies, resubmit or wait) is answered by a triage agent, not the person. It reads the board's
//! facts and the repository, read-only, as a reviewer does, and its answer is a note on the issue
//! that the asking agent hears on its next nitely call. Only a question that turns out to be on the
//! closed list of the person's decisions becomes an ask in their Inbox, with what triage found.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{now, Db};
use crate::issues::Actor;

/// A question put to triage, and what came of it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Question {
    pub id: i64,
    pub issue: Option<String>,
    pub question: String,
    pub asked_by: String,
    /// Where the agent asked from (its worktree), to find the repository.
    pub place: Option<String>,
    /// `open`, `answered` (triage answered it), `escalated` (it is the person's: `ask` holds it),
    /// or `failed` (triage could not answer: `error` says why).
    pub state: String,
    pub answer: Option<String>,
    /// The Inbox ask it became, when it is the person's.
    pub ask_id: Option<i64>,
    pub error: Option<String>,
    pub asked_at: i64,
    pub answered_at: Option<i64>,
    pub pid: Option<i64>,
    pub delivered_at: Option<i64>,
}

impl Question {
    pub fn finished(&self) -> bool {
        self.state != "open"
    }

    /// One line for the asking agent.
    pub fn outcome(&self) -> String {
        let about = self.issue.as_deref().map(|k| format!(" ({k})")).unwrap_or_default();
        match self.state.as_str() {
            "answered" => format!("Triage answered your question{about}: {}", self.answer.as_deref().unwrap_or("")),
            "escalated" => format!(
                "Triage found your question{about} is the person's to decide, and put it in their Inbox as ask {}: check it with get_answer. What triage found: {}",
                self.ask_id.unwrap_or_default(),
                self.answer.as_deref().unwrap_or("")
            ),
            "failed" => format!("Triage could not answer your question{about}: {}", self.error.as_deref().unwrap_or("")),
            _ => format!("Triage is looking into your question{about}."),
        }
    }
}

const COLUMNS: &str = "id, issue_key, question, asked_by, place, state, answer, ask_id, error, asked_at, answered_at, pid, delivered_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Question> {
    Ok(Question {
        id: r.get(0)?,
        issue: r.get(1)?,
        question: r.get(2)?,
        asked_by: r.get(3)?,
        place: r.get(4)?,
        state: r.get(5)?,
        answer: r.get(6)?,
        ask_id: r.get(7)?,
        error: r.get(8)?,
        asked_at: r.get(9)?,
        answered_at: r.get(10)?,
        pid: r.get(11)?,
        delivered_at: r.get(12)?,
    })
}

impl Db {
    /// A triage question. One whose process died before it answered reads as failed.
    pub fn triage_question(&self, id: i64) -> Result<Option<Question>> {
        let q = self.conn.query_row(&format!("SELECT {COLUMNS} FROM triage WHERE id = ?1"), [id], row).optional()?;
        match q {
            Some(q) if q.state == "open" && q.pid.is_some_and(|p| p as u32 != std::process::id() && !crate::submit_job::process_runs(p as u32)) => {
                let why = "the process answering it stopped before it finished; ask again";
                self.conn.execute("UPDATE triage SET state = 'failed', error = ?2, answered_at = ?3 WHERE id = ?1 AND state = 'open'", params![q.id, why, now()])?;
                self.triage_question(q.id)
            }
            other => Ok(other),
        }
    }

    /// Answers `asked_by` has not heard yet, oldest first.
    pub fn undelivered_triage(&self, asked_by: &str) -> Result<Vec<Question>> {
        let ids: Vec<i64> = self
            .conn
            .prepare("SELECT id FROM triage WHERE asked_by = ?1 AND delivered_at IS NULL ORDER BY id")?
            .query_map([asked_by], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = vec![];
        for id in ids {
            if let Some(q) = self.triage_question(id)?.filter(Question::finished) {
                out.push(q);
            }
        }
        Ok(out)
    }

    pub fn mark_triage_delivered(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.conn.execute("UPDATE triage SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL", params![id, now()])?;
        }
        Ok(())
    }
}

/// Records a question for triage. Answer it with `answer` (a process of its own runs that).
pub fn ask(db: &Db, question: &str, issue: Option<&str>, place: Option<&str>, actor: &Actor) -> Result<Question> {
    if question.trim().is_empty() {
        bail!("say what you want to know");
    }
    let issue = match issue {
        Some(k) => Some(db.issue(k)?.with_context(|| format!("no issue {k}"))?.key),
        None => None,
    };
    db.conn.execute(
        "INSERT INTO triage (issue_key, question, asked_by, place, state, asked_at) VALUES (?1, ?2, ?3, ?4, 'open', ?5)",
        params![issue, question.trim(), actor.id(), place, now()],
    )?;
    db.triage_question(db.conn.last_insert_rowid())?.context("the question's record went missing")
}

/// The next step for an agent that just put a question to triage.
pub const ASKED_NEXT: &str = "Triage is looking into it, from the board and the code. Go on with what does not depend on the answer: \
it comes as a notice on your next nitely call, as a note on the issue, or from get_triage (wait_seconds to wait).";

/// What the person decides, and nobody else (docs/coordinator.md, decision 5).
const THE_PERSONS: &str = "a goal plan to approve; approving a change; a product question (scope, wording, what to build); something \
irreversible or outside this machine (credentials, accounts, licenses, signing, money, publishing); accepting a finding as a \
trade-off; a task only they can do (signing a CLI in, granting access, a check on their own desktop)";

/// Answers question `id`: the board's facts and the question to a read-only reviewer agent in the
/// repository, then a note on the issue, or an ask in the person's Inbox when the reply says the
/// question is theirs.
pub fn answer(db: &mut Db, config: &crate::Config, id: i64) -> Result<Question> {
    let q = db.triage_question(id)?.with_context(|| format!("no triage question {id}"))?;
    if q.state != "open" {
        return Ok(q);
    }
    db.conn.execute("UPDATE triage SET pid = ?2 WHERE id = ?1", params![id, std::process::id()])?;
    match look(db, config, &q) {
        Ok((answer, theirs)) => settle(db, &q, &answer, theirs.as_deref()),
        Err(e) => {
            let why = format!("{e:#}").lines().next().unwrap_or("").chars().take(400).collect::<String>();
            db.conn.execute("UPDATE triage SET state = 'failed', error = ?2, answered_at = ?3 WHERE id = ?1", params![id, why, now()])?;
            db.triage_question(id)?.context("the question's record went missing")
        }
    }
}

/// Records triage's answer: a note, or an ask when it is the person's.
fn settle(db: &mut Db, q: &Question, answer: &str, theirs: Option<&str>) -> Result<Question> {
    let actor = if q.asked_by == "human" { Actor::Human } else { Actor::Agent(q.asked_by.clone()) };
    match theirs {
        None => {
            if let Some(k) = &q.issue {
                db.add_note(k, &format!("Triage answered \"{}\": {answer}", q.question), "triage", &actor)?;
            }
            db.conn.execute("UPDATE triage SET state = 'answered', answer = ?2, answered_at = ?3 WHERE id = ?1", params![q.id, answer, now()])?;
        }
        Some(why) => {
            let question = format!("{}\n\nTriage looked first ({why}): {answer}", q.question);
            let ask = db.create_ask(&question, &[], q.issue.as_deref(), q.place.as_deref(), &actor)?;
            db.conn.execute(
                "UPDATE triage SET state = 'escalated', answer = ?2, ask_id = ?3, answered_at = ?4 WHERE id = ?1",
                params![q.id, answer, ask.id, now()],
            )?;
        }
    }
    db.triage_question(q.id)?.context("the question's record went missing")
}

/// Asks the triage agent: the playbook's reviewer for the asker's vendor, read-only, in the repo.
fn look(db: &Db, config: &crate::Config, q: &Question) -> Result<(String, Option<String>)> {
    let issue = match &q.issue {
        Some(k) => db.issue(k)?,
        None => None,
    };
    let place = q.place.clone().unwrap_or_default();
    let root = match &issue {
        Some(i) => crate::playbook::repo_for_issue(config, i.project.as_deref(), &place)?,
        None => crate::playbook::repo_root(config, &place).context("triage needs the issue the question is about, or a place in a repo to look")?,
    };
    // Read where the agent works when that is a worktree of the repo, so its branch is there.
    let dir = Some(PathBuf::from(&place)).filter(|p| p.is_dir()).unwrap_or_else(|| root.clone());
    let rules = crate::playbook::Playbook::load(&root)?.review;
    let vendor = q.asked_by.split(':').next().unwrap_or("");
    let choice = rules.choose(vendor, &|r| rules.is_installed(r, &dir))?;
    let project = issue.as_ref().and_then(|i| i.project.clone());
    let reply = rules.run_prompt(&choice.reviewer, &dir, &prompt(db, config, q, issue.as_ref(), project.as_deref())?)?;
    parse(&reply.text)
}

/// The triage agent's instructions: the question, what the board knows, and the closed list.
fn prompt(db: &Db, config: &crate::Config, q: &Question, issue: Option<&crate::issues::IssueRecord>, project: Option<&str>) -> Result<String> {
    let mut facts = String::new();
    if let Some(i) = issue {
        let body: String = i.body.chars().take(1500).collect();
        facts.push_str(&format!(
            "The issue: {} \"{}\", {} (owner {}{}{}).\n{body}\n",
            i.key,
            i.title,
            i.state,
            i.owner,
            i.claimed_by.as_deref().map(|c| format!(", claimed by {c}")).unwrap_or_default(),
            if i.blocked_by.is_empty() { String::new() } else { format!(", blocked by {}", i.blocked_by.join(", ")) }
        ));
        if let Some(r) = db.reviews(&i.key)?.into_iter().find(|r| !r.reviewer.is_empty()) {
            facts.push_str(&format!("\nIts latest review, by {} at {}, says {}:\n", r.reviewer, r.commit_sha, r.verdict));
            for f in r.findings.iter().take(12) {
                let now = f.now.as_deref().map(|n| format!(" (last said still open: {n})")).unwrap_or_default();
                facts.push_str(&format!("- [P{}]{} {}{now}\n", f.priority, if f.carried { " carried" } else { "" }, f.text));
            }
        }
    }
    if let Some(p) = project {
        let held = crate::overlap::held(config, db, p).unwrap_or_default();
        if !held.is_empty() {
            facts.push_str("\nWork in flight, and the files each holds:\n");
            for h in held.iter().take(40) {
                let files: Vec<&str> = h.files.iter().take(12).map(String::as_str).collect();
                facts.push_str(&format!("- {}{}: {}\n", h.key, h.branch.as_deref().map(|b| format!(" ({b})")).unwrap_or_default(), files.join(", ")));
            }
        }
    }
    let about = q.issue.as_deref().map(|k| format!(", about {k}")).unwrap_or_default();
    Ok(format!(
        "You answer an agent's question about the work in this repository, so the person who owns it is not interrupted for \
it. Answer from the facts below and the repository: read files and run `git log`, `git diff` and `git show`. Do not edit \
anything. Be short and concrete: name issues, files and lines, and say what you checked.\n\n\
The question, from {asker}{about}:\n{question}\n\n\
What Nitely knows:\n{facts}\n\
Only the person decides these: {persons}. Everything else you answer yourself, even when unsure: then say what you \
checked and what you would do.\n\n\
End your reply with exactly these two lines:\n\
ANSWER: <your answer, in one paragraph>\n\
PERSON: <no, or yes and which of the person's decisions it is>",
        asker = q.asked_by,
        question = q.question,
        facts = if facts.is_empty() { "(nothing beyond the question)\n".to_string() } else { facts },
        persons = THE_PERSONS,
    ))
}

/// The answer and, when the question is the person's, which of their decisions it is.
fn parse(text: &str) -> Result<(String, Option<String>)> {
    let lines: Vec<&str> = text.lines().collect();
    let field = |name: &str| lines.iter().rposition(|l| l.trim().trim_matches('*').to_uppercase().starts_with(&format!("{name}:")));
    let (Some(a), Some(p)) = (field("ANSWER"), field("PERSON")) else {
        bail!("the triage agent did not end with ANSWER and PERSON lines");
    };
    let after = |l: &str| l.trim().trim_matches('*').split_once(':').map(|(_, v)| v.trim().trim_matches('*').trim().to_string()).unwrap_or_default();
    // The answer may run over several lines, up to PERSON.
    let mut answer = after(lines[a]);
    if p > a {
        for l in &lines[a + 1..p] {
            if !l.trim().is_empty() {
                answer.push(' ');
                answer.push_str(l.trim());
            }
        }
    }
    if answer.is_empty() {
        bail!("the triage agent gave an empty answer");
    }
    let person = after(lines[p]);
    let theirs = person.to_lowercase().starts_with("yes").then(|| person.trim_start_matches(|c: char| c.is_alphabetic() && "yesYES".contains(c)).trim_start_matches([',', ':', ' ', '-']).to_string());
    Ok((answer, theirs.map(|w| if w.is_empty() { "the person's decision".to_string() } else { w })))
}

/// Waits up to `limit` for question `id` to be answered, and returns it as it stands then.
pub fn wait(db: &Db, id: i64, limit: Duration) -> Result<Question> {
    let deadline = Instant::now() + limit;
    loop {
        let q = db.triage_question(id)?.with_context(|| format!("no triage question {id}"))?;
        if q.finished() || Instant::now() >= deadline {
            return Ok(q);
        }
        std::thread::sleep(Duration::from_millis(500).min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Starts `nitely triage-job <id>` as a process of its own; when none can be started, answers here.
pub fn start(db: &mut Db, config: &crate::Config, q: &Question, nitely: &Path) -> Result<Question> {
    let cwd = q.place.clone().filter(|p| Path::new(p).is_dir()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let log = crate::paths::data_dir().join("jobs").join(format!("triage-{}.log", q.id));
    match crate::submit_job::spawn_nitely(nitely, &["triage-job".into(), q.id.to_string()], &cwd, &log) {
        Ok(pid) => {
            db.conn.execute("UPDATE triage SET pid = ?2 WHERE id = ?1 AND state = 'open'", params![q.id, pid])?;
            db.triage_question(q.id)?.context("the question's record went missing")
        }
        Err(_) => answer(db, config, q.id),
    }
}

/// How often the person's Inbox was needed, per landed issue, since `since` (NIT-245): asks to the
/// person (an acceptance request counts), issues landed, and what triage answered and escalated.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InboxRate {
    pub since: i64,
    pub asks: i64,
    pub landed: i64,
    pub per_landed: f64,
    pub triage_answered: i64,
    pub triage_escalated: i64,
}

pub fn inbox_rate(db: &Db, project: &str, since: i64) -> Result<InboxRate> {
    let like = format!("{}-%", project.to_uppercase());
    let count = |sql: &str| -> Result<i64> { Ok(db.conn.query_row(sql, params![like, since], |r| r.get(0))?) };
    let asks = count("SELECT COUNT(*) FROM ask WHERE issue_key LIKE ?1 AND asked_at >= ?2")?;
    let landed = count("SELECT COUNT(DISTINCT issue_key) FROM event WHERE type = 'landed' AND issue_key LIKE ?1 AND ts >= ?2")?;
    let triage_answered = count("SELECT COUNT(*) FROM triage WHERE issue_key LIKE ?1 AND asked_at >= ?2 AND state = 'answered'")?;
    let triage_escalated = count("SELECT COUNT(*) FROM triage WHERE issue_key LIKE ?1 AND asked_at >= ?2 AND state = 'escalated'")?;
    let per_landed = if landed == 0 { 0.0 } else { asks as f64 / landed as f64 };
    Ok(InboxRate { since, asks, landed, per_landed, triage_answered, triage_escalated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_gives_an_answer_and_whether_it_is_the_persons() {
        let (a, theirs) = parse("I read land.rs.\nANSWER: NIT-12 holds crates/nitely-core/src/land.rs\non its branch; wait for it.\nPERSON: no").unwrap();
        assert_eq!((a.as_str(), theirs), ("NIT-12 holds crates/nitely-core/src/land.rs on its branch; wait for it.", None));
        let (_, theirs) = parse("ANSWER: Which license is a product decision.\n**PERSON:** yes, a product question").unwrap();
        assert_eq!(theirs.as_deref(), Some("a product question"));
        assert!(parse("No closing lines here.").is_err());
    }

    /// A repo whose triage agent (the playbook's claude reviewer) answers from a script: a question
    /// about the SDK's license is the person's, anything else it answers itself.
    fn repo_with_triage() -> (PathBuf, Db, PathBuf, Actor) {
        let base = crate::test_repo::copy("triage", |base| {
            let script = "prompt=$(cat)\ncase \"$prompt\" in\n  *SDK*) echo 'Which license is a product decision.'; echo 'ANSWER: Nothing in the repo decides it.'; echo 'PERSON: yes, a product question';;\n  *) echo 'ANSWER: APP-2 holds crates/a.rs on its branch: wait for it to land, then rebase.'; echo 'PERSON: no';;\nesac\n";
            let playbook = format!("[review]\nreviewer = {{ \"*\" = \"claude\" }}\ncommands = {{ claude = [\"{}\", \"triage.sh\"] }}\n", crate::playbook::posix_sh());
            crate::test_repo::init(&base.join("repo"), &[("triage.sh", script), (crate::playbook::PLAYBOOK_PATH, &playbook)]);
        });
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("APP", "App").unwrap();
        (base.clone(), db, base.join("repo"), Actor::Agent("codex:triage-test".into()))
    }

    #[test]
    fn triage_answers_the_routine_and_puts_only_the_persons_in_their_inbox() {
        let (base, mut db, repo, actor) = repo_with_triage();
        let config = crate::Config::default();
        let issue = db
            .create_issue(
                &crate::issues::NewIssue { project: "APP".into(), title: "Footer".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() },
                &actor,
            )
            .unwrap();
        let place = repo.to_string_lossy().into_owned();

        // Routine: answered, as a note on the issue, and told to the agent once.
        let q = ask(&db, "Who holds crates/a.rs? Should I wait?", Some(&issue.key), Some(&place), &actor).unwrap();
        let done = answer(&mut db, &config, q.id).unwrap();
        assert_eq!((done.state.as_str(), done.answer.as_deref()), ("answered", Some("APP-2 holds crates/a.rs on its branch: wait for it to land, then rebase.")));
        let said: Vec<String> = db.timeline(&issue.key).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(said.iter().any(|s| s.starts_with("Triage answered \"Who holds crates/a.rs?")), "{said:?}");
        assert!(db.open_asks().unwrap().is_empty(), "the person is not asked");
        assert_eq!(db.undelivered_triage(actor.id()).unwrap().len(), 1);
        db.mark_triage_delivered(&[q.id]).unwrap();
        assert!(db.undelivered_triage(actor.id()).unwrap().is_empty());

        // The person's: an ask in their Inbox, with what triage found.
        let q = ask(&db, "Which license should the SDK use?", Some(&issue.key), Some(&place), &actor).unwrap();
        let done = answer(&mut db, &config, q.id).unwrap();
        assert_eq!(done.state, "escalated");
        let asks = db.open_asks().unwrap();
        assert_eq!(asks.len(), 1);
        assert_eq!(Some(asks[0].id), done.ask_id);
        assert!(asks[0].question.contains("Triage looked first (a product question): Nothing in the repo decides it."), "{}", asks[0].question);
        assert!(done.outcome().contains("put it in their Inbox"), "{}", done.outcome());

        let rate = inbox_rate(&db, "APP", 0).unwrap();
        assert_eq!((rate.asks, rate.triage_answered, rate.triage_escalated), (1, 1, 1));
        std::fs::remove_dir_all(base).ok();
    }
}
