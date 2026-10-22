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
pub fn stalled_notice(key: &str, why: &str) -> String {
    format!("{key}: the review rounds are not converging ({why}). Its agent splits the change: what the reviews agree on lands, and the open findings become a follow-up issue in the same goal. Nothing waits on you.")
}

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
pub fn final_move<'a>(computed: &'a str, now_in: &str, decided_meanwhile: bool) -> Option<&'a str> {
    (!decided_meanwhile && computed != now_in).then_some(computed)
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

/// A review that asked for changes only in findings the person accepted passes.
pub fn accepted_only(findings: &[Finding], accepted: &[String]) -> bool {
    blocking(findings, accepted).is_empty()
}

/// After the rounds stalled (`why`), a fresh review that has not seen them looked: it keeps the
/// earlier review's interactions with other changes (no reviewer's opinion: only a landing ends
/// one), asks for changes when a P0 or P1 remains, and settles what goes to the person. None: it
/// passes, so nothing stalls; else why it still stalls, with what the fresh review still finds.
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
    Some(format!("{}; a fresh review without the earlier rounds still finds: {}", why.split(". Open:").next().unwrap_or(why), still.join("; ")))
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

    /// NIT-575: an acceptance made while the submit ran stands.
    #[test]
    fn submit_never_undoes_an_acceptance_made_while_it_ran() {
        assert_eq!(final_move("In Progress", "In Review", true), None, "accepted meanwhile: it stays where the acceptance put it");
        assert_eq!(final_move("In Progress", "In Review", false), Some("In Progress"));
        assert_eq!(final_move("In Review", "In Review", false), None, "already there");
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
            Some("Three rounds without converging; a fresh review without the earlier rounds still finds: [P1] Leaks the token")
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
