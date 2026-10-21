//! What land and the land queue decide, as plain functions over plain data (NIT-524): when a run
//! takes its batch and which changes it takes, how a failed stacked gate finds the change that
//! broke it, what a gate that passed lets land, whose lock a land may take over, which git
//! `index.lock` was left behind, and when a land waiting for others goes. land.rs, land_queue.rs
//! and land_job.rs gather the facts with git, the file system and the database and do what these
//! say; their tests need none of them.

use std::time::{Duration, SystemTime};

/// When the run may take its batch: None now, or the time the oldest change's window closes.
pub fn waits_until(oldest_queued_at: i64, window_secs: u64, now: i64) -> Option<i64> {
    let ready = oldest_queued_at + window_secs as i64;
    (now < ready).then_some(ready)
}

/// A waiting change as the batch sees it: the project mode it was handed in under, and whether
/// it must land alone.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub authority: Option<String>,
    pub alone: bool,
}

/// How many of the waiting changes, oldest first, one run takes under the mode `authority`: up
/// to the first that was handed in under another mode or must land alone, which gets its own run
/// next. The caller has already cut the list to the batch's maximum.
pub fn batch_len(candidates: &[Candidate], authority: &Option<String>) -> usize {
    candidates.iter().take_while(|c| &c.authority == authority && !c.alone).count()
}

/// A gate the bisection asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The first `n` changes of the stack.
    Prefix(usize),
    /// Every change but the one at this index, stacked again without it.
    Without(usize),
}

/// What broke the stacked gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The stack held one change, and its gate failed.
    Only,
    /// The suspect: the others passed without it, and land on that gate.
    Suspect(usize),
    /// The change at this index: the ones before it land on the last prefix that passed (none
    /// when it is the first), and the ones after it wait for the next run.
    Bad(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Gate(Probe),
    Done(Found),
}

/// The search for the one change that breaks a stack of `k` whose whole gate failed: first the
/// suspect alone (the only change touching a file the failing output names), then halving, where
/// the shortest failing prefix ends with the change that broke it. At most 1 + ⌈log2 k⌉ gates
/// after the whole one.
#[derive(Debug, Clone)]
pub struct Bisect {
    k: usize,
    suspect: Option<usize>,
    lo: usize,
    hi: usize,
    asked: Option<Probe>,
}

impl Bisect {
    pub fn new(k: usize, suspect: Option<usize>) -> Self {
        Bisect { k, suspect: suspect.filter(|i| *i < k), lo: 0, hi: k, asked: None }
    }

    /// The first step after the whole stack's gate failed.
    pub fn first(&mut self) -> Next {
        if self.k <= 1 {
            return Next::Done(Found::Only);
        }
        match self.suspect {
            Some(i) => self.ask(Probe::Without(i)),
            None => self.halve(),
        }
    }

    /// The step after the gate last asked for: `passed` is its outcome, None when it could not
    /// run (the stack without the suspect would not build).
    pub fn next(&mut self, passed: Option<bool>) -> Next {
        match self.asked {
            Some(Probe::Without(i)) if passed == Some(true) => Next::Done(Found::Suspect(i)),
            Some(Probe::Without(_)) => self.halve(),
            Some(Probe::Prefix(mid)) => {
                if passed == Some(true) {
                    self.lo = mid;
                } else {
                    self.hi = mid;
                }
                self.halve()
            }
            None => self.first(),
        }
    }

    fn halve(&mut self) -> Next {
        if self.hi - self.lo > 1 {
            self.ask(Probe::Prefix((self.lo + self.hi) / 2))
        } else {
            Next::Done(Found::Bad(self.hi - 1))
        }
    }

    fn ask(&mut self, probe: Probe) -> Next {
        self.asked = Some(probe);
        Next::Gate(probe)
    }
}

/// What a passed stacked gate lets happen, before each change's own authorization is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterGate {
    /// The integration branch moved during the gate: every change waits for the next run.
    RequeueAll,
    /// These changes' branches gained commits during the gate: they go back, the rest wait.
    SendBack(Vec<usize>),
    /// Nothing moved: check each change's authorization, then land.
    Authorize,
}

/// `base_moved`: the integration branch is not where the stack was built; `gained[i]`: change i's
/// branch is not at the head that was reviewed and stacked.
pub fn after_gate(base_moved: bool, gained: &[bool]) -> AfterGate {
    if base_moved {
        return AfterGate::RequeueAll;
    }
    let gained: Vec<usize> = gained.iter().enumerate().filter(|(_, g)| **g).map(|(i, _)| i).collect();
    if gained.is_empty() {
        AfterGate::Authorize
    } else {
        AfterGate::SendBack(gained)
    }
}

/// The land that holds a land lock with this text (`<holder> pid <n> since <time>`), when that
/// process is gone and is not `me`: a lock a dead land left, for the next land to take over.
pub fn dead_holder(text: &str, me: u32, alive: impl Fn(u32) -> bool) -> Option<String> {
    let mut words = text.split_whitespace();
    let holder = words.next()?.to_string();
    let pid: u32 = words.skip_while(|w| *w != "pid").nth(1)?.parse().ok()?;
    (pid != me && !alive(pid)).then_some(holder)
}

/// A git `index.lock` made more than `grace` before the land began: no running git holds it, so
/// one that ended without removing it left it (NIT-280).
pub fn index_lock_left(made: SystemTime, land_began: SystemTime, grace: Duration) -> bool {
    made + grace < land_began
}

/// A killed land's own `index.lock`: taken during its fast-forward, from `slack` before the mark
/// it wrote to `takes` after it (NIT-341). One taken outside that is someone else's.
pub fn killed_fast_forwards_lock(mark: SystemTime, lock: SystemTime, slack: Duration, takes: Duration) -> bool {
    lock + slack >= mark && lock <= mark + takes
}

/// What a land waiting for other issues does, from their states in the order it named them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blockers {
    Wait,
    Go,
    /// One was closed without landing: what it waited for will not come.
    Over(String),
}

pub fn blockers(states: &[(&str, &str)]) -> Blockers {
    if let Some((key, state)) = states.iter().find(|(_, s)| matches!(*s, "Canceled" | "Duplicate")) {
        return Blockers::Over(format!("{key} was closed as {state} without landing; land again without waiting for it"));
    }
    if states.iter().all(|(_, s)| *s == "Done") {
        Blockers::Go
    } else {
        Blockers::Wait
    }
}

/// What a review's verdict lets land: Ok with what the approval note adds (findings the person
/// accepted), or why not. `blockers` reads (open P0/P1 findings not accepted, P0/P1 findings in
/// all) only for a review that asked for changes.
pub fn verdict_lets_land(verdict: &str, reviewer: &str, blockers: impl FnOnce() -> anyhow::Result<(usize, usize)>) -> anyhow::Result<String> {
    match verdict {
        // The Basic review exemption is retired (NIT-510): a change recorded under it gets a review.
        "light" => anyhow::bail!("the Basic review exemption is retired; submit again for a review"),
        "pass" | "none" | "disabled" | "yolo" => Ok(String::new()),
        // Never reviewed: its land batch reviews it first (review_batch::review_pending), so nothing
        // lands unreviewed.
        crate::review_batch::PENDING => anyhow::bail!("its review waits for its land batch, and has not run; land it again"),
        _ => {
            // Findings the person accepted as they are, signed for this patch, block nothing.
            let (open, all) = blockers()?;
            if open > 0 {
                anyhow::bail!("the {reviewer} review asked for changes; fix them, commit, and submit again");
            }
            Ok(format!("; its P0/P1 findings were accepted by a human, signed for this patch ({all})"))
        }
    }
}

/// Why a change may land without a person, when nothing (`reasons`) asks for one: None when one
/// must approve it.
pub fn lands_without_a_person(reasons: &[String], verdict: &str, reviewer: &str, accepted: &str) -> Option<String> {
    if !reasons.is_empty() {
        return None;
    }
    Some(match verdict {
        "yolo" => format!("{}; no human approval is required", crate::project_yolo::NOTE),
        "disabled" => format!("{}; no human approval is required", crate::review::DISABLED_NOTE),
        _ => format!("the {reviewer} review passed and no human approval is needed{accepted}"),
    })
}

/// The approval note of a change a person approved, signed by the app.
pub fn approved_by_a_person(reasons: &[String]) -> String {
    format!("approved by a human in Nitely after the review, signed by the app ({})", reasons.join("; "))
}

/// Why a change that needs a person and has no approval waits.
pub fn waits_for_a_person(reasons: &[String]) -> String {
    format!("this change waits for a human ({}); it lands once they approve it in Nitely, after the review", reasons.join("; "))
}

/// What a `[[land.after]]` step does for what landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterStep {
    /// It does not run, and why.
    Skip(String),
    /// It runs once the land lock is released (`after_land_outside_lock`).
    Outside,
    Run,
}

/// The step for the files that landed, or None when they cannot be told: then every step runs,
/// since skipping a needed one silently is worse than running one more.
pub fn after_step(step: &crate::playbook::GateStep, landed: Option<&[String]>) -> AfterStep {
    let skip = match landed.map(|f| step.skip_reason(f)) {
        None => None,
        Some(Ok(skip)) => skip,
        Some(Err(e)) => Some(format!("its patterns do not parse: {}", format!("{e:#}").lines().next().unwrap_or(""))),
    };
    match skip {
        Some(why) => AfterStep::Skip(why),
        None if step.outside_lock => AfterStep::Outside,
        None => AfterStep::Run,
    }
}

/// The dependency files that landed and that no after step which ran and passed (`covered`) names
/// by pattern: the main checkout's setup is stale for them.
pub fn stale_setup(changed_setup_files: Vec<String>, covered: &[&crate::playbook::GateStep]) -> Vec<String> {
    let names = |step: &crate::playbook::GateStep, file: &str| step.when_changed.iter().any(|p| regex::Regex::new(p).is_ok_and(|r| r.is_match(file)));
    changed_setup_files.into_iter().filter(|f| !covered.iter().any(|s| names(s, f))).collect()
}

/// What a change's project allows its land, for `alone_reason`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Batching {
    pub batch_off: bool,
    /// It is a goal's checkpoint.
    pub checkpoint: bool,
    pub lanes: bool,
    pub large_repo: bool,
}

/// A change's review, for `alone_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reviewed {
    /// Reduced verification is bound to this patch alone.
    pub individual: bool,
    /// A person must approve it (or did).
    pub needs_signature: bool,
}

/// Why a change lands alone rather than in a batch, or None. The later facts are read only when
/// the earlier ones leave it open, in this order: whether a managed submodule's baseline is
/// unchanged (None: the repo manages none), the review (None: none yet), and whether a signed
/// approval stands.
pub fn alone_reason(
    b: Batching,
    submodules_unchanged: impl FnOnce() -> anyhow::Result<Option<bool>>,
    review: impl FnOnce() -> anyhow::Result<Option<Reviewed>>,
    signed: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<Option<&'static str>> {
    if b.batch_off {
        return Ok(Some("the review mode lands each change alone"));
    }
    // A goal's slice is already one change with one review: it is gated alone, so a failure
    // bisects over its own items (goal_branch.rs), not over unrelated changes stacked with it.
    if b.checkpoint {
        return Ok(Some("a goal's checkpoint lands alone: its slice is gated as one change and bisected over its own items"));
    }
    if !b.lanes {
        return Ok(Some("a batch is gated in a build lane, and this repo has no [lanes]"));
    }
    if b.large_repo {
        return Ok(Some("large-repo mode lands each change alone"));
    }
    if submodules_unchanged()? == Some(false) {
        return Ok(Some("a change with a different managed submodule baseline lands alone"));
    }
    let Some(r) = review()? else { return Ok(Some("it has no review yet")) };
    if r.individual {
        return Ok(Some("reduced verification is bound to this individual patch"));
    }
    if r.needs_signature && !signed()? {
        return Ok(Some("it still needs a signed approval, checked separately for this change"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `lands_reviewed_work_and_waits_for_human_approval`,
    /// `lands_only_what_a_human_approved_when_the_playbook_says_human...`,
    /// `a_finding_the_person_accepted_does_not_block_the_land` and
    /// `tests_and_review_off_skip_automatic_checks...` decide with real submits and lands.
    #[test]
    fn a_verdict_and_the_reasons_for_a_person_decide_whether_a_change_lands() {
        let never = || -> anyhow::Result<(usize, usize)> { panic!("read only for a review that asked for changes") };
        assert_eq!(verdict_lets_land("pass", "codex", never).unwrap(), "");
        for v in ["none", "disabled", "yolo"] {
            assert!(verdict_lets_land(v, "codex", never).is_ok(), "{v}");
        }
        assert!(verdict_lets_land("light", "codex", never).unwrap_err().to_string().contains("retired"));
        assert!(verdict_lets_land(crate::review_batch::PENDING, "codex", never).unwrap_err().to_string().contains("land batch"));
        assert_eq!(verdict_lets_land("changes", "codex", || Ok((1, 2))).unwrap_err().to_string(), "the codex review asked for changes; fix them, commit, and submit again");
        assert_eq!(verdict_lets_land("changes", "codex", || Ok((0, 2))).unwrap(), "; its P0/P1 findings were accepted by a human, signed for this patch (2)");

        assert_eq!(lands_without_a_person(&[], "pass", "codex", "").as_deref(), Some("the codex review passed and no human approval is needed"));
        assert!(lands_without_a_person(&[], "yolo", "", "").unwrap().ends_with("no human approval is required"));
        assert!(lands_without_a_person(&[], "disabled", "", "").unwrap().ends_with("no human approval is required"));
        let reasons = vec!["touches billing/".to_string(), "the reviewer flagged MONEY".to_string()];
        assert_eq!(lands_without_a_person(&reasons, "pass", "codex", ""), None);
        assert_eq!(waits_for_a_person(&reasons), "this change waits for a human (touches billing/; the reviewer flagged MONEY); it lands once they approve it in Nitely, after the review");
        assert!(approved_by_a_person(&reasons).starts_with("approved by a human in Nitely after the review, signed by the app ("));
    }

    /// What `steps_outside_the_lock_build_the_newest_commit...` and
    /// `warns_when_a_dependency_landed_that_no_after_step_set_up` check by landing.
    #[test]
    fn after_steps_run_for_what_landed_and_setup_is_stale_for_what_none_covered() {
        let step = |when: &[&str], outside: bool| crate::playbook::GateStep {
            run: "x".into(),
            when_changed: when.iter().map(|s| s.to_string()).collect(),
            unless_only: vec![],
            outside_lock: outside,
        };
        let npm = step(&["^package(-lock)?\\.json$"], false);
        let install = step(&["^crates/"], true);
        let landed = ["src/App.tsx".to_string()];
        assert!(matches!(after_step(&npm, Some(&landed)), AfterStep::Skip(why) if why.contains("nothing matching")));
        assert_eq!(after_step(&npm, Some(&["package.json".to_string()])), AfterStep::Run);
        assert_eq!(after_step(&install, Some(&["crates/a.rs".to_string()])), AfterStep::Outside);
        assert_eq!(after_step(&npm, None), AfterStep::Run, "what landed cannot be told: every step runs");
        assert!(matches!(after_step(&step(&["("], false), Some(&landed)), AfterStep::Skip(why) if why.contains("do not parse")));

        let changed = vec!["package-lock.json".to_string(), "Cargo.lock".to_string()];
        assert_eq!(stale_setup(changed.clone(), &[&npm]), ["Cargo.lock"]);
        assert_eq!(stale_setup(changed, &[]), ["package-lock.json", "Cargo.lock"]);
    }

    /// The reasons `approvals_large_repos_and_repos_without_lanes_land_alone` and
    /// `approved_changes_with_an_unchanged_managed_submodule_share_one_gate` reach with real
    /// repos, and the order they are read in.
    #[test]
    fn a_change_lands_alone_for_the_first_reason_that_holds() {
        let open = Batching { lanes: true, ..Default::default() };
        let none = || -> anyhow::Result<Option<bool>> { Ok(None) };
        let reviewed = |individual, needs_signature| move || -> anyhow::Result<Option<Reviewed>> { Ok(Some(Reviewed { individual, needs_signature })) };
        let unread = || -> anyhow::Result<bool> { panic!("not read") };
        let reason = |b, s: Option<bool>, r: Option<Reviewed>, signed: bool| alone_reason(b, || Ok(s), || Ok(r), || Ok(signed)).unwrap();
        assert_eq!(alone_reason(open, none, reviewed(false, false), unread).unwrap(), None);
        assert!(reason(Batching { batch_off: true, ..open }, None, None, false).unwrap().contains("each change alone"));
        assert!(reason(Batching { checkpoint: true, ..open }, None, None, false).unwrap().contains("checkpoint"));
        assert!(reason(Batching { lanes: false, ..open }, None, None, false).unwrap().contains("no [lanes]"));
        assert!(reason(Batching { large_repo: true, ..open }, None, None, false).unwrap().contains("large-repo"));
        assert!(reason(open, Some(false), None, false).unwrap().contains("submodule baseline"));
        assert_eq!(reason(open, None, None, false), Some("it has no review yet"));
        assert!(reason(open, Some(true), Some(Reviewed { individual: true, needs_signature: true }), true).unwrap().contains("individual patch"));
        assert!(reason(open, None, Some(Reviewed { individual: false, needs_signature: true }), false).unwrap().contains("signed approval"));
        assert_eq!(reason(open, None, Some(Reviewed { individual: false, needs_signature: true }), true), None);
        // The earlier reasons are read before any later fact.
        let gone = || -> anyhow::Result<Option<bool>> { panic!("not read") };
        let no_review = || -> anyhow::Result<Option<Reviewed>> { panic!("not read") };
        assert!(alone_reason(Batching { batch_off: true, ..open }, gone, no_review, unread).unwrap().is_some());
    }

    /// The rules `a_dead_lands_lock_is_taken_over_and_a_left_index_lock_is_named`,
    /// `a_killed_lands_index_lock_is_cleared_and_anyone_elses_named` and
    /// `a_land_after_another_waits_for_it_then_lands_or_stops_cleanly` check through real locks,
    /// processes and a waiting thread.
    #[test]
    fn a_dead_lands_lock_a_left_or_killed_index_lock_and_a_wait_for_blockers() {
        let alive = |pid: u32| pid == 7;
        assert_eq!(dead_holder("claude:a pid 9 since 2026-10-21T10:00:00Z", 1, alive).as_deref(), Some("claude:a"));
        assert_eq!(dead_holder("claude:a pid 7 since x", 1, alive), None, "its process runs");
        assert_eq!(dead_holder("claude:a pid 1 since x", 1, alive), None, "it is this process");
        assert_eq!(dead_holder("claude:a since x", 1, alive), None, "it names no process");
        assert_eq!(dead_holder("", 1, alive), None);

        let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + s);
        let minute = Duration::from_secs(60);
        assert!(index_lock_left(t(0), t(61), minute), "made before the land's minute: left");
        assert!(!index_lock_left(t(0), t(60), minute), "within the minute: maybe a git still running");
        assert!(!index_lock_left(t(100), t(60), minute), "made after the land began");

        let (slack, takes) = (Duration::from_secs(2), Duration::from_secs(120));
        assert!(killed_fast_forwards_lock(t(10), t(9), slack, takes), "a coarse clock may read it as a little older");
        assert!(killed_fast_forwards_lock(t(10), t(130), slack, takes));
        assert!(!killed_fast_forwards_lock(t(10), t(7), slack, takes), "before the fast-forward: someone else's");
        assert!(!killed_fast_forwards_lock(t(10), t(131), slack, takes), "long after: someone else's");

        assert_eq!(blockers(&[("APP-1", "Done"), ("APP-2", "In Review")]), Blockers::Wait);
        assert_eq!(blockers(&[("APP-1", "Done"), ("APP-2", "Done")]), Blockers::Go);
        assert_eq!(blockers(&[]), Blockers::Go);
        assert_eq!(
            blockers(&[("APP-1", "In Progress"), ("APP-2", "Canceled")]),
            Blockers::Over("APP-2 was closed as Canceled without landing; land again without waiting for it".into())
        );
    }

    /// Runs a search against a stack whose change `bad` breaks the gate: a prefix passes while it
    /// stops before `bad`, and the stack without one change passes only without `bad`.
    fn search(k: usize, bad: usize, suspect: Option<usize>, without_builds: bool) -> (Found, usize) {
        let mut b = Bisect::new(k, suspect);
        let mut next = b.first();
        let mut gates = 0;
        loop {
            match next {
                Next::Done(found) => return (found, gates),
                Next::Gate(Probe::Prefix(n)) => {
                    assert!(n >= 1 && n < k, "a prefix of {n} of {k}");
                    gates += 1;
                    next = b.next(Some(n <= bad));
                }
                Next::Gate(Probe::Without(i)) => {
                    if without_builds {
                        gates += 1;
                        next = b.next(Some(i == bad));
                    } else {
                        next = b.next(None);
                    }
                }
            }
        }
    }

    /// For every stack up to 9 and every change in it that breaks the gate, with no suspect, the
    /// right suspect, a wrong one, and a suspect whose stack would not build: the search names
    /// the change that broke it, within 1 + ⌈log2 k⌉ gates after the whole one. This is the
    /// decision `land_queue::tests::a_bad_change_is_bisected_out_of_three` checks with real gates.
    #[test]
    fn bisection_finds_the_change_that_broke_the_stack_within_its_gate_bound() {
        assert_eq!(search(1, 0, None, true), (Found::Only, 0));
        for k in 2..=9 {
            let bound = 1 + (k as f64).log2().ceil() as usize;
            for bad in 0..k {
                let (found, gates) = search(k, bad, None, true);
                assert_eq!(found, Found::Bad(bad), "k={k} bad={bad}");
                assert!(gates < bound, "k={k} bad={bad}: {gates} gates");
                assert_eq!(search(k, bad, Some(bad), true), (Found::Suspect(bad), 1), "k={k} bad={bad}");
                for wrong in (0..k).filter(|i| *i != bad) {
                    let (found, gates) = search(k, bad, Some(wrong), true);
                    assert_eq!(found, Found::Bad(bad), "k={k} bad={bad} suspect={wrong}");
                    assert!(gates <= bound, "k={k} bad={bad} suspect={wrong}: {gates} gates");
                }
                assert_eq!(search(k, bad, Some(bad), false).0, Found::Bad(bad), "k={k} bad={bad}, no stack without it");
            }
        }
    }

    /// The decision `land_queue::tests::small_changes_handed_in_together_are_reviewed_in_one_batch`
    /// once waited on the clock for (NIT-468, NIT-479): the run waits for its window from the
    /// oldest change, and then takes what came, up to the first change of another mode or one
    /// that must land alone.
    #[test]
    fn a_run_waits_for_its_window_then_takes_the_changes_of_its_mode() {
        assert_eq!(waits_until(100, 60, 130), Some(160));
        assert_eq!(waits_until(100, 60, 160), None);
        assert_eq!(waits_until(100, 0, 100), None, "no window: at once");
        let fast = Some("fast".to_string());
        let c = |authority: &Option<String>, alone: bool| Candidate { authority: authority.clone(), alone };
        assert_eq!(batch_len(&[c(&fast, false), c(&fast, false), c(&fast, false)], &fast), 3);
        assert_eq!(batch_len(&[c(&fast, false), c(&None, false), c(&fast, false)], &fast), 1, "another mode stops the batch");
        assert_eq!(batch_len(&[c(&fast, false), c(&fast, true), c(&fast, false)], &fast), 1, "one that lands alone stops it");
        assert_eq!(batch_len(&[c(&None, false)], &None), 1);
        assert_eq!(batch_len(&[], &fast), 0);
    }

    /// The checks `changes_wait_again_when_the_branch_moves_during_their_gate` and
    /// `a_branch_that_gains_commits_during_the_gate_keeps_them_and_goes_back` make with real
    /// gates: main moving sends everything back to the queue first; otherwise only the changes
    /// whose branches moved go back.
    #[test]
    fn after_a_passed_gate_a_moved_base_requeues_all_and_a_moved_branch_goes_back() {
        assert_eq!(after_gate(true, &[true, false]), AfterGate::RequeueAll);
        assert_eq!(after_gate(false, &[false, true, false, true]), AfterGate::SendBack(vec![1, 3]));
        assert_eq!(after_gate(false, &[false, false]), AfterGate::Authorize);
    }
}
