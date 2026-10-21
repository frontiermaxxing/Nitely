//! What the land queue decides, as plain functions over plain data (NIT-524): when a run takes
//! its batch and which changes it takes, how a failed stacked gate finds the change that broke
//! it, and what a gate that passed lets land. land_queue.rs gathers the facts with git and the
//! database and does what these say; their tests need neither.

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

#[cfg(test)]
mod tests {
    use super::*;

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
    /// decision `land_queue::tests::a_bad_change_is_bisected_out_of_five` checks with real gates.
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
