//! What submit decides, as plain functions over plain data (NIT-525): where a submitted change
//! goes and what its agent is told, what still blocks it once the person's accepted findings are
//! set aside, what a fresh review after stalled rounds settles, whether an earlier review's pass
//! covers it, whether the agent's solution text describes what was gated, and whether a submit
//! job's process is gone. submit.rs and submit_job.rs gather the facts with git, the reviewer and
//! the database and do what these say; their tests need none of them.

use crate::project_modes::StalledReviews;
use crate::review::{Finding, Review};

/// What a submit came to, for where its issue goes next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The review rounds are not converging.
    pub stalled: bool,
    /// What the mode makes of that (NIT-568); either way the agent goes on.
    pub stalled_reviews: Option<StalledReviews>,
    /// The review asked for changes (P0 or P1 findings not accepted).
    pub changes: bool,
    /// It was to be pushed and the push failed.
    pub push_failed: bool,
    /// The human approved this exact patch before.
    pub approval_stands: bool,
    /// A human must approve it.
    pub human: bool,
    /// Nitely lands it itself once it may.
    pub by_nitely: bool,
    /// Its review waits for its land queue batch.
    pub batched: bool,
    pub yolo: bool,
    pub review_disabled: bool,
    pub gate_skipped: bool,
}

/// The issue's next state, and what its agent should do now.
pub fn next_state(o: &Outcome) -> (&'static str, &'static str) {
    if o.stalled {
        // Another round would cost as much again and likely end the same. The agent is never held
        // for it (NIT-568): an open question holds the land, not the work.
        match o.stalled_reviews.unwrap_or(StalledReviews::Ask) {
            StalledReviews::Fix => ("In Progress", SPLIT),
            StalledReviews::Ask => ("In Progress", ASKED_KEEP_FIXING),
        }
    } else if o.changes {
        ("In Progress", "The review found P0 or P1 problems. Fix them, commit, and submit again. If one is a trade-off the human decided or should decide, not a defect (they ruled out what it asks for), call request_acceptance with it and your reasoning instead of looping on it.")
    } else if o.push_failed {
        // Not handed in: nobody can see or merge the branch, so it must not look ready to land.
        ("In Progress", "The push failed, so the change is not handed in. Fix the remote or its access (ask the human if it is a permission error), then submit again.")
    } else if o.approval_stands {
        ("Approved", "The human approved this exact patch before and the review raised nothing new, so the approval stands. Call land.")
    } else if o.human && o.by_nitely {
        ("Needs You", "It waits for the human's approval in the Nitely inbox, and Nitely lands it as soon as they approve it; you do not call land. Nitely already asked them, so do not `ask` for this approval yourself. Pick up other work meanwhile.")
    } else if o.human {
        ("Needs You", "It waits for the human's approval in the Nitely inbox. Nitely already asked them, so do not `ask` for this approval yourself. Pick up other work meanwhile; once it is Approved, call land.")
    } else if o.batched {
        ("In Review", "The gate passed; its review waits for its land queue batch. Call land: the queue reviews it with whatever else is waiting, in one reviewer call, before their gate, and nothing lands unreviewed. If the review asks for changes, land says so and the issue is back with you.")
    } else if o.yolo {
        ("In Review", "YOLO mode: automatic gate and PR review skipped. This revision is unverified. Call land while YOLO remains enabled.")
    } else if o.review_disabled && o.gate_skipped {
        ("In Review", "Automatic tests and pre-merge review are disabled for this project. This revision is unverified. Call land while the current mode permits it.")
    } else if o.review_disabled {
        ("In Review", "Review disabled for this project. The gate passed and no human approval is needed. Call land to put it on the integration branch.")
    } else {
        ("In Review", "The review passed and no human approval is needed. Call land to put it on the integration branch.")
    }
}

/// What an agent is told when its review rounds stalled under `stalled_reviews = "fix"`.
pub const SPLIT: &str = "The review rounds are not converging, so split the change: keep what the reviews agree on, take out what the open P0/P1 findings are about, and file those findings as a follow-up issue in the same goal (file_issue, with the findings as its body). Then commit and submit the smaller change. The person is told, and nothing waits on them.";

/// What an agent is told when its review rounds stalled under `stalled_reviews = "ask"`.
pub const ASKED_KEEP_FIXING: &str = "The review rounds are not converging, so Nitely asked the human to decide each open P0/P1 finding in the Inbox: accept it as it is, or send it back. Keep fixing meanwhile: a finding a later review no longer raises closes its question by itself, and land waits only until each one is fixed or accepted. If you have reasoning for a finding (a trade-off they decided, say), add it in a `note` on the issue: they read it beside the finding.";

/// What the person reads when an issue's review rounds stalled under `stalled_reviews = "fix"`:
/// a notice, which nothing waits on.
/// The stalled-rounds notice once the open findings are filed as `follow_up` (NIT-634 review): it
/// names the follow-up and links it, so the person can open where the findings went.
pub fn stalled_notice_for(key: &str, why: &str, follow_up: &str) -> String {
    format!(
        "{key}{STALLED_NOTICE_START}{why}). The agreed part lands; the open findings moved to [{follow_up}]({}).{STALLED_NOTICE_END}",
        crate::links::issue_url(follow_up)
    )
}

/// What the follow-up for `key`'s open findings carries as its evidence: how a later stall of the
/// same issue finds it again instead of filing another.
pub fn follow_up_evidence(key: &str) -> String {
    format!("The open review findings of {key} when its review rounds stopped converging.")
}

/// The follow-up's body: each open finding (`[P1] text`), one per line, and where they came from.
pub fn follow_up_body(key: &str, open: &[String]) -> String {
    format!("{key}'s review rounds stopped converging. Its agent lands the part the reviews agree on; these findings are what is left:\n\n{}", open.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"))
}

/// An open follow-up's body with the findings of a later stall it does not list yet, one per line
/// as `follow_up_body` writes them, or None when it lists them all already (NIT-634 review).
pub fn follow_up_with(body: &str, open: &[String]) -> Option<String> {
    let new: Vec<String> = open.iter().map(|f| format!("- {f}")).filter(|line| !body.lines().any(|l| l.trim() == line)).collect();
    (!new.is_empty()).then(|| format!("{}\n{}", body.trim_end(), new.join("\n")))
}

/// What the follow-up for `key`'s P2 findings carries as its evidence (NIT-695): how every later
/// round of the same change finds it again, passing or not, instead of filing another.
pub fn minor_follow_up_evidence(key: &str) -> String {
    format!("The P2 review findings of {key}, which do not hold the change.")
}

/// The P2 follow-up's body: each finding (`[P2] text`), one per line as `follow_up_body` writes
/// them, so `follow_up_with` adds a later round's.
pub fn minor_follow_up_body(key: &str, minor: &[String]) -> String {
    format!(
        "The reviews of [{key}]({}) found these. They are real but minor (P2), so they did not hold the change; fix the ones that still apply:\n\n{}",
        crate::links::issue_url(key),
        minor.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n")
    )
}

/// A review's new P2 findings, as the follow-up lists them: not one carried from an earlier round,
/// which that round filed (its wording may have changed since), nor a P1 carried lower because the
/// code it cited was gone.
pub fn minor(findings: &[Finding]) -> Vec<String> {
    findings.iter().filter(|f| f.priority == 2 && !f.carried).map(|f| format!("[P2] {}", f.text)).collect()
}

/// What the agent is told to do under `fix` once Nitely filed the follow-up itself.
pub fn split_into(follow_up: &str) -> String {
    format!("The review rounds are not converging, so split the change: keep what the reviews agree on and take out what the open P0/P1 findings are about. Nitely filed those findings as {follow_up} in the same goal ({}); do not file them again. Then commit and submit the smaller change. The person is told, and nothing waits on them.", crate::links::issue_url(follow_up))
}

pub fn stalled_notice(key: &str, why: &str) -> String {
    format!("{key}{STALLED_NOTICE_START}{why}). The agreed part lands; the open findings move to a follow-up issue in the same goal.{STALLED_NOTICE_END}")
}

/// What stalled rounds do: what the mode says (`mode`, the person decides when unset), except that
/// an open P0 covers a part that must not land without the person, so they are asked about it
/// rather than told (NIT-633).
pub fn stalled_route(mode: Option<StalledReviews>, open_p0: bool) -> StalledReviews {
    if open_p0 {
        StalledReviews::Ask
    } else {
        mode.unwrap_or(StalledReviews::Ask)
    }
}

/// How a stalled-rounds notice starts after its issue's key and ends (`stalled_notice`): the Inbox
/// finds the notices by them (`Db::notices`, NIT-570).
pub const STALLED_NOTICE_START: &str = ": the review rounds are not converging (";
pub const STALLED_NOTICE_END: &str = " Nothing waits on you.";

/// Where an issue goes when the person answers a question about one of its findings (NIT-568):
/// `accepted`, with `blocking_left` other P0/P1 findings of the latest review not accepted; else
/// sent back. A send-back leaves an issue its agent is working on (`state`, In Progress) as it is,
/// since it was already fixing; one nobody holds goes to Todo for the next agent. An acceptance
/// with findings left keeps it with the agent; with none, it goes on as a passed review does:
/// `needs_approval` when a person must approve it (or only the app can check the acceptance).
pub fn after_answer(accepted: bool, state: &str, blocking_left: usize, needs_approval: bool) -> &'static str {
    match (accepted, blocking_left, needs_approval) {
        (false, _, _) if state == "In Progress" => "In Progress",
        (false, _, _) => "Todo",
        (true, 1.., _) => "In Progress",
        (true, 0, true) => "Needs You",
        (true, 0, false) => "In Review",
    }
}

/// Where submit moves the issue at its end, from where it is now (`now_in`): nowhere when it is
/// there already, or when the person accepted every blocking finding while the submit ran
/// (`decided_meanwhile`): that acceptance moved it from the review as it now stands, and submit's
/// own verdict, read before it, must not undo it (NIT-575).
pub fn final_move<'a>(computed: &'a str, now_in: &str, meanwhile: Meanwhile) -> Option<&'a str> {
    (meanwhile == Meanwhile::No && computed != now_in).then_some(computed)
}

/// Whether the person accepted a review's blocking findings while its submit ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meanwhile {
    No,
    /// Every blocker is accepted, verified with the approval key.
    Accepted,
    /// Acceptances cover every blocker, but this machine cannot read the approval key to verify
    /// them (macOS: only the app can): the person's decision stands, for the app to check.
    Unverified,
}

/// What the person decided meanwhile, from the review's verdict (`changes`), whether the approval
/// key can be read here, the blockers left unaccepted by verified acceptances, and whether the
/// recorded acceptances, verified or not, cover every blocker. An unreadable key is never a
/// revert: it cannot say the acceptance is false.
pub fn meanwhile(changes: bool, key_readable: bool, unaccepted_verified: usize, recorded_cover_all: bool) -> Meanwhile {
    match (changes, key_readable) {
        (false, _) => Meanwhile::No,
        (true, true) if unaccepted_verified == 0 => Meanwhile::Accepted,
        (true, false) if recorded_cover_all => Meanwhile::Unverified,
        _ => Meanwhile::No,
    }
}

/// The state submit's own land goes by: its computed state, or, when the person's acceptance kept
/// the issue where it is, that state (`now_in`). An acceptance this machine cannot verify lands
/// nowhere here: the land would refuse it, so the app, which can check it, lands it.
pub fn land_state<'a>(computed: &'a str, now_in: &'a str, meanwhile: Meanwhile) -> Option<&'a str> {
    match meanwhile {
        Meanwhile::No => Some(computed),
        Meanwhile::Accepted => Some(now_in),
        Meanwhile::Unverified => None,
    }
}

/// The P0 and P1 findings the person has not accepted as they are, as `[P1] text`: what still
/// blocks the change, whichever review says it.
pub fn blocking(findings: &[Finding], accepted: &[String]) -> Vec<String> {
    findings
        .iter()
        .filter(|f| f.priority <= 1 && !accepted.contains(&f.text))
        .map(|f| format!("[P{}] {}", f.priority, f.text))
        .collect()
}

/// Whether a goal item lands into its goal branch after its item review (NIT-563): an advisory
/// review never holds it, and a blocking one does while a P0/P1 finding is not accepted.
pub fn item_lands(mode: crate::project_modes::ItemReview, findings: &[Finding], accepted: &[String]) -> bool {
    mode != crate::project_modes::ItemReview::Blocking || blocking(findings, accepted).is_empty()
}

/// What the agent of a goal item its blocking item review sent back is told (NIT-563): fix and
/// submit again, or, once the rounds stopped converging, what `stalled_reviews` says (NIT-568).
pub fn item_sent_back(stalled: bool, stalled_reviews: Option<StalledReviews>) -> &'static str {
    next_state(&Outcome { stalled, stalled_reviews, changes: true, ..Default::default() }).1
}

/// Whether a goal item may land into its goal branch, given its mode's item review and its latest
/// review, when there is one: whether it reviewed this very patch, and whether it passes (its P0/P1
/// findings fixed or accepted). A blocking item review holds a direct `land` as it holds a submit
/// (NIT-563); an advisory one never holds it.
pub fn item_land_check(mode: crate::project_modes::ItemReview, latest: Option<(bool, bool)>) -> Result<(), &'static str> {
    if mode != crate::project_modes::ItemReview::Blocking {
        return Ok(());
    }
    match latest {
        None => Err("its blocking item review has not run: submit it, and it lands when the review passes"),
        Some((false, _)) => Err("its item review was of another version of the change: submit it again"),
        Some((true, false)) => Err("its blocking item review asked for changes that are neither fixed nor accepted"),
        Some((true, true)) => Ok(()),
    }
}

/// A review that asked for changes only in findings the person accepted passes.
pub fn accepted_only(findings: &[Finding], accepted: &[String]) -> bool {
    blocking(findings, accepted).is_empty()
}

/// After the rounds stalled (`why`), a fresh review that has not seen them looked: it keeps the
/// earlier review's interactions with other changes (no reviewer's opinion: only a landing ends
/// one), asks for changes when a P0 or P1 remains, and settles what goes to the person. None: it
/// passes, so nothing stalls; else why it still stalls, with how many findings the fresh review still finds open.
pub fn settle_fresh(earlier: &Review, fresh: &mut Review, accepted: &[String], why: &str) -> Option<String> {
    for f in earlier.findings.iter().filter(|f| crate::review_batch::is_interaction(&f.text)) {
        if !fresh.findings.iter().any(|x| x.text == f.text) {
            fresh.findings.push(f.clone());
        }
    }
    if fresh.findings.iter().any(|f| f.priority <= 1) {
        fresh.verdict = "changes".into();
    }
    let still = blocking(&fresh.findings, accepted);
    if still.is_empty() {
        return None;
    }
    // The rounds' reason without its count (or, from an older round, its pasted findings), then
    // how many the fresh review still finds open: the findings are on the review (NIT-633).
    let reason = why.split(". Open:").next().unwrap_or(why).split("; ").next().unwrap_or(why);
    let open: Vec<crate::review::Finding> = fresh.findings.iter().filter(|f| f.priority <= 1 && !accepted.contains(&f.text)).cloned().collect();
    Some(format!("{reason}; a fresh review without the earlier rounds still finds {}", crate::submit::open_count(&open)))
}

/// The issue's last review by a reviewer, as reuse sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastReview<'a> {
    pub reviewer: &'a str,
    pub verdict: &'a str,
    /// What it reviewed: the diff's id, which tells apart two edits with the same patch id.
    pub diff_id: &'a str,
    /// The review rules it ran under.
    pub rules: Option<&'a str>,
}

/// Whether that review's pass covers the change now: the same diff, under the same rules, by the
/// reviewer the rules would choose now. Anything else is reviewed again.
pub fn reusable(last: &LastReview, diff_id: &str, rules: &str, chosen: Option<&str>) -> bool {
    !diff_id.is_empty() && last.verdict == "pass" && last.diff_id == diff_id && last.rules == Some(rules) && chosen == Some(last.reviewer)
}

/// Whether the agent's solution text describes what was gated: it was written for the commit the
/// gate ran on (`gate_ran_on_it`) and for the same patch.
pub fn solution_applies(gate_ran_on_it: bool, explained_patch: Option<&str>, submitted_patch: &str) -> bool {
    gate_ran_on_it && explained_patch == Some(submitted_patch)
}

/// Whether a submit job's process is gone: it no longer runs, or (past `pid_check_after` seconds
/// since the job began, when the OS may have given its number to another program) the process
/// with that number is not this job's. `runs_this_job` reads its command line, only when needed:
/// None when it cannot be read.
pub fn job_abandoned(process_runs: bool, age_secs: i64, pid_check_after: i64, runs_this_job: impl FnOnce() -> Option<bool>) -> bool {
    !process_runs || (age_secs > pid_check_after && runs_this_job() == Some(false))
}

/// What a failed gate's result says of the review that ran alongside it (NIT-699).
pub fn beside_failed_gate(changes: bool, reviewer: &str) -> String {
    if changes {
        format!("Its review by {reviewer}, which ran with the gate, asked for changes too: fix its findings in the same round.")
    } else {
        format!("Its review by {reviewer}, which ran with the gate, found nothing to change.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The routing `a_failed_push_hands_nothing_in_and_a_repo_without_origin_is_local`,
    /// `submits_with_a_review_and_routes_flags_to_a_human`, `a_p1_that_two_rounds_leave_open...`
    /// and the review-off and YOLO submit tests reach with real submits: each outcome, and which
    /// one wins when several hold.
    #[test]
    fn a_submit_goes_where_its_outcome_says_and_the_first_that_holds_wins() {
        let state = |o: Outcome| next_state(&o).0;
        let base = Outcome::default();
        assert_eq!(state(base), "In Review");
        let stalled = Outcome { stalled: true, changes: true, push_failed: true, ..base };
        assert_eq!(state(stalled), "In Progress", "stalled rounds never hold the agent");
        assert_eq!(next_state(&Outcome { stalled_reviews: Some(StalledReviews::Fix), ..stalled }).1, SPLIT);
        assert_eq!(next_state(&Outcome { stalled_reviews: Some(StalledReviews::Ask), ..stalled }).1, ASKED_KEEP_FIXING);
        assert_eq!(state(Outcome { changes: true, push_failed: true, human: true, ..base }), "In Progress");
        assert!(next_state(&Outcome { changes: true, ..base }).1.contains("request_acceptance"));
        let failed_push = next_state(&Outcome { push_failed: true, approval_stands: true, human: true, ..base });
        assert_eq!(failed_push.0, "In Progress", "not handed in, so never ready to land");
        assert!(failed_push.1.contains("push failed"));
        assert_eq!(state(Outcome { approval_stands: true, human: true, ..base }), "Approved");
        let waits = next_state(&Outcome { human: true, by_nitely: true, batched: true, ..base });
        assert_eq!(waits.0, "Needs You");
        assert!(waits.1.contains("you do not call land"));
        assert!(!next_state(&Outcome { human: true, ..base }).1.contains("you do not call land"));
        assert!(next_state(&Outcome { batched: true, yolo: true, ..base }).1.contains("land queue batch"));
        assert!(next_state(&Outcome { yolo: true, review_disabled: true, ..base }).1.starts_with("YOLO"));
        assert!(next_state(&Outcome { review_disabled: true, gate_skipped: true, ..base }).1.contains("unverified"));
        assert!(next_state(&Outcome { review_disabled: true, ..base }).1.starts_with("Review disabled"));
        assert!(next_state(&base).1.starts_with("The review passed"));
    }

    /// NIT-568: an answer about a finding never takes the issue from an agent fixing it.
    #[test]
    fn an_answer_keeps_a_working_agent_on_its_issue_and_an_acceptance_goes_on_when_nothing_is_left() {
        assert_eq!(after_answer(false, "In Progress", 2, false), "In Progress", "sent back: it was already fixing");
        assert_eq!(after_answer(false, "In Review", 0, false), "Todo", "sent back, nobody on it: for the next agent");
        assert_eq!(after_answer(true, "In Progress", 1, false), "In Progress", "another finding still to fix or decide");
        assert_eq!(after_answer(true, "In Progress", 0, true), "Needs You", "it waits for the person's approval");
        assert_eq!(after_answer(true, "In Progress", 0, false), "In Review");
        assert!(stalled_notice("APP-7", "6 review rounds in a row asked for changes").contains("Nothing waits on you"));
    }

    /// NIT-633: stalled rounds go as the mode says, except that an open P0 always asks the person;
    /// the notice is one line with no finding pasted in.
    #[test]
    fn the_notice_names_and_links_the_follow_up_the_findings_moved_to() {
        let notice = stalled_notice_for("APP-7", "7 review rounds in a row asked for changes; 2 open findings (1 P0, 1 P1)", "APP-9");
        assert!(notice.contains("moved to [APP-9](http://127.0.0.1:7423/i/APP-9)"), "{notice}");
        assert!(notice.starts_with(&format!("APP-7{STALLED_NOTICE_START}")) && notice.ends_with(STALLED_NOTICE_END), "the Inbox still finds it: {notice}");
        assert_eq!(notice.lines().count(), 1);
        let body = follow_up_body("APP-7", &["[P0] Leaks the token: b.rs".into(), "[P1] Drops the last row: a.rs".into()]);
        assert!(body.ends_with("- [P0] Leaks the token: b.rs\n- [P1] Drops the last row: a.rs"), "{body}");
        assert!(split_into("APP-9").contains("filed those findings as APP-9") && split_into("APP-9").contains("do not file them again"));
        assert_ne!(follow_up_evidence("APP-7"), follow_up_evidence("APP-8"), "one follow-up per issue");
        // A later stall adds only the findings the open follow-up does not list yet.
        let later = ["[P0] Leaks the token: b.rs".to_string(), "[P1] Off by one: c.rs".to_string()];
        assert_eq!(follow_up_with(&body, &later).unwrap(), format!("{body}\n- [P1] Off by one: c.rs"));
        assert_eq!(follow_up_with(&body, &later[..1]), None, "nothing new: left as it is");
    }

    #[test]
    fn an_open_p0_asks_the_person_and_the_notice_is_one_line() {
        assert_eq!(stalled_route(Some(StalledReviews::Fix), false), StalledReviews::Fix);
        assert_eq!(stalled_route(Some(StalledReviews::Fix), true), StalledReviews::Ask, "a P0 does not land without the person");
        assert_eq!(stalled_route(None, false), StalledReviews::Ask);
        let notice = stalled_notice("APP-7", "7 review rounds in a row asked for changes; 4 open findings (3 P0, 1 P1)");
        assert_eq!(notice.lines().count(), 1);
        assert_eq!(notice, "APP-7: the review rounds are not converging (7 review rounds in a row asked for changes; 4 open findings (3 P0, 1 P1)). The agreed part lands; the open findings move to a follow-up issue in the same goal. Nothing waits on you.");
    }

    /// NIT-575: an acceptance made while the submit ran stands.
    #[test]
    fn submit_never_undoes_an_acceptance_made_while_it_ran() {
        assert_eq!(final_move("In Progress", "In Review", Meanwhile::Accepted), None, "accepted meanwhile: it stays where the acceptance put it");
        assert_eq!(final_move("In Progress", "Needs You", Meanwhile::Unverified), None, "an acceptance it cannot verify stays too");
        assert_eq!(final_move("In Progress", "In Review", Meanwhile::No), Some("In Progress"));
        assert_eq!(final_move("In Review", "In Review", Meanwhile::No), None, "already there");
    }

    /// The person's acceptance during a submit is kept whether or not this machine can read the
    /// approval key, and the submit's own land follows the state it kept (NIT-580 review).
    #[test]
    fn an_acceptance_made_during_a_submit_is_kept_and_lands_where_it_can_be_checked() {
        assert_eq!(meanwhile(false, true, 0, true), Meanwhile::No, "a passing review decides nothing");
        assert_eq!(meanwhile(true, true, 0, true), Meanwhile::Accepted);
        assert_eq!(meanwhile(true, true, 1, true), Meanwhile::No, "one blocker left");
        assert_eq!(meanwhile(true, false, 2, true), Meanwhile::Unverified, "unreadable key: kept, not reverted");
        assert_eq!(meanwhile(true, false, 2, false), Meanwhile::No, "nothing recorded covers a blocker");
        assert_eq!(land_state("In Progress", "In Review", Meanwhile::Accepted), Some("In Review"), "the kept state lands");
        assert_eq!(land_state("In Progress", "Needs You", Meanwhile::Unverified), None, "the app lands what only it can check");
        assert_eq!(land_state("In Review", "In Progress", Meanwhile::No), Some("In Review"));
    }

    /// NIT-563: what an item review does to a goal item's land into its goal branch.
    #[test]
    fn an_advisory_item_review_never_holds_an_item_and_a_blocking_one_holds_it_for_its_p1s() {
        use crate::project_modes::ItemReview::*;
        let found = [finding(1, "Drops a row: rows.rs"), finding(2, "Wordy: rows.rs")];
        assert!(item_lands(Advisory, &found, &[]), "advisory: noted, and it lands");
        assert!(!item_lands(Blocking, &found, &[]), "blocking: the P1 sends it back");
        assert!(item_lands(Blocking, &found, &["Drops a row: rows.rs".to_string()]), "accepted as it is");
        assert!(item_lands(Blocking, &found[1..], &[]), "a P2 never holds it");
        assert!(item_lands(Off, &found, &[]));
    }

    /// NIT-563's rework: a direct land meets the blocking item review a submit does, and a sent-back
    /// item whose rounds stalled is told what the mode says, as a change is.
    #[test]
    fn a_blocking_item_review_holds_a_direct_land_and_stalled_item_rounds_go_as_a_changes_do() {
        use crate::project_modes::ItemReview::*;
        assert!(item_land_check(Advisory, None).is_ok() && item_land_check(Off, Some((false, false))).is_ok());
        assert!(item_land_check(Blocking, None).unwrap_err().contains("has not run"));
        assert!(item_land_check(Blocking, Some((false, true))).unwrap_err().contains("another version"));
        assert!(item_land_check(Blocking, Some((true, false))).is_err());
        assert!(item_land_check(Blocking, Some((true, true))).is_ok());
        assert_eq!(item_sent_back(true, Some(StalledReviews::Fix)), SPLIT);
        assert_eq!(item_sent_back(true, Some(StalledReviews::Ask)), ASKED_KEEP_FIXING);
        assert!(item_sent_back(false, Some(StalledReviews::Fix)).contains("Fix them, commit, and submit again"));
    }

    fn finding(priority: u8, text: &str) -> Finding {
        serde_json::from_value(serde_json::json!({ "priority": priority, "text": text })).unwrap()
    }

    fn review(findings: Vec<Finding>) -> Review {
        serde_json::from_value(serde_json::json!({
            "reviewer": "codex", "verdict": "pass", "findings": findings, "flagged": [], "summary": "", "seconds": 0
        }))
        .unwrap()
    }

    /// What `the_fresh_review_is_told_what_the_person_accepted_so_a_reworded_repeat_does_not_reach_them`,
    /// `a_finding_only_the_rounds_carry_is_checked_fresh_before_it_goes_to_a_person` and
    /// `a_finding_the_person_accepted_does_not_block_the_land` settle with real reviewer runs.
    #[test]
    fn accepted_findings_block_nothing_and_a_fresh_review_settles_stalled_rounds() {
        let accepted = vec!["Drops the last row".to_string()];
        assert!(accepted_only(&[finding(1, "Drops the last row"), finding(2, "Naming")], &accepted));
        assert!(!accepted_only(&[finding(1, "Drops the last row"), finding(0, "Leaks the token")], &accepted));
        assert_eq!(blocking(&[finding(0, "Leaks the token"), finding(3, "Style")], &[]), ["[P0] Leaks the token"]);

        let why = "Three rounds without converging. Open: [P1] Drops the last row";
        // The fresh review finds nothing: it passes.
        let mut fresh = review(vec![]);
        assert_eq!(settle_fresh(&review(vec![finding(1, "Drops the last row")]), &mut fresh, &[], why), None);
        assert_eq!(fresh.verdict, "pass");
        // It finds only what the person accepted: it asks for changes, and nothing stalls.
        let mut fresh = review(vec![finding(1, "Drops the last row")]);
        assert_eq!(settle_fresh(&review(vec![]), &mut fresh, &accepted, why), None);
        assert_eq!(fresh.verdict, "changes");
        // It still finds something: that goes to the person, after the rounds' reason.
        let mut fresh = review(vec![finding(1, "Leaks the token")]);
        assert_eq!(
            settle_fresh(&review(vec![]), &mut fresh, &accepted, why).as_deref(),
            Some("Three rounds without converging; a fresh review without the earlier rounds still finds 1 open finding (1 P1)")
        );
        // An interaction with another change is kept, whatever the fresh review says.
        let interaction = review(vec![finding(1, "Interacts with APP-2, reviewed in the same batch: both edit the same table")]);
        let mut fresh = review(vec![]);
        assert!(settle_fresh(&interaction, &mut fresh, &[], why).is_some());
        assert_eq!(fresh.findings.len(), 1);
    }

    /// The reuse rule `a_patch_that_passed_is_not_reviewed_again_and_a_rewritten_one_starts_from_the_pass`
    /// and `the_same_edit_somewhere_else_is_reviewed_though_its_patch_id_is_the_same` check with
    /// four submits and three reviewer runs.
    #[test]
    fn a_pass_is_reused_only_for_the_same_diff_rules_and_reviewer() {
        let last = LastReview { reviewer: "codex", verdict: "pass", diff_id: "d1", rules: Some("r1") };
        assert!(reusable(&last, "d1", "r1", Some("codex")));
        assert!(!reusable(&last, "d2", "r1", Some("codex")), "another diff, even with the same patch id");
        assert!(!reusable(&last, "d1", "r2", Some("codex")), "rules changed since");
        assert!(!reusable(&last, "d1", "r1", Some("claude")), "the rules would choose another reviewer now");
        assert!(!reusable(&last, "d1", "r1", None), "no reviewer can run");
        assert!(!reusable(&LastReview { verdict: "changes", ..last.clone() }, "d1", "r1", Some("codex")));
        assert!(!reusable(&LastReview { rules: None, ..last.clone() }, "d1", "r1", Some("codex")), "a review from before rules were recorded");
        assert!(!reusable(&LastReview { diff_id: "", ..last }, "", "r1", Some("codex")), "an empty diff is never a match");
    }

    /// What `solution_summary_is_bound_to_the_revision_handed_in`,
    /// `solution_summary_does_not_follow_a_changed_patch_at_the_same_head` and
    /// `a_job_whose_process_is_gone_is_failed...` check through real submit jobs.
    #[test]
    fn a_solution_follows_only_its_revision_and_a_job_whose_process_is_gone_fails() {
        assert!(solution_applies(true, Some("p1"), "p1"));
        assert!(!solution_applies(false, Some("p1"), "p1"), "the gate ran on a later commit");
        assert!(!solution_applies(true, Some("p0"), "p1"), "same head, another patch");
        assert!(!solution_applies(true, None, "p1"));

        let never = || -> Option<bool> { panic!("the command line is read only past the check age") };
        assert!(!job_abandoned(true, 10, 1800, never));
        assert!(job_abandoned(false, 10, 1800, never));
        assert!(job_abandoned(true, 1801, 1800, || Some(false)), "its number now names another program");
        assert!(!job_abandoned(true, 1801, 1800, || Some(true)));
        assert!(!job_abandoned(true, 1801, 1800, || None), "a command line that cannot be read is given the benefit");
    }
}
