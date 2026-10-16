//! Where an issue is on the lifecycle track (docs/visualization.md, "The lifecycle track"):
//! Todo, Started, Working, Gate, Review, Needs you, Landing, Done, with the "changes asked" loop
//! and stuck states. Every view draws from this, so a row, the issue panel and Pulse agree.
//! `src/ui/lifecycle.ts` mirrors it for the browser preview's sample data.

use serde::Serialize;
use std::collections::BTreeMap;

use crate::issues::{IssueRecord, TimelineEntry};

pub const STATIONS: [&str; 8] = ["todo", "started", "working", "gate", "review", "needs", "landing", "done"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Track {
    pub station: &'static str,
    /// When each station was first reached. Stations behind `station` that are missing were skipped.
    pub reached: BTreeMap<&'static str, i64>,
    /// Review rounds that asked for changes.
    pub rounds: i64,
    /// Why the issue cannot move on by itself, if it cannot.
    pub stuck: Option<String>,
    /// An agent asked something that is not answered yet.
    pub asking: bool,
    /// `canceled` or `duplicate`: off the track.
    pub closed: Option<&'static str>,
    /// Still in Backlog, before Todo.
    pub backlog: bool,
}

impl Default for Track {
    fn default() -> Track {
        Track { station: "todo", reached: BTreeMap::new(), rounds: 0, stuck: None, asking: false, closed: None, backlog: false }
    }
}

fn index(station: &str) -> usize {
    STATIONS.iter().position(|s| *s == station).unwrap_or(0)
}

/// The station a state alone implies (mirrored Linear issues have no timeline to refine it).
pub fn station_of_state(state: &str, state_type: &str) -> &'static str {
    match state {
        "Backlog" | "Todo" => "todo",
        "In Progress" => "working",
        "In Review" => "review",
        "Needs You" | "Needs Andrew" => "needs",
        "Approved" => "landing",
        "Done" | "Canceled" | "Duplicate" => "done",
        _ => match state_type {
            "completed" | "canceled" => "done",
            "started" => "working",
            _ => "todo",
        },
    }
}

/// `Todo → In Progress` → `In Progress`; `In Progress → Done: tests passed` → `Done`.
fn moved_to(summary: Option<&str>) -> Option<&str> {
    let (_, rest) = summary?.split_once('→')?;
    let to = rest.split(':').next().unwrap_or("").trim();
    (!to.is_empty()).then_some(to)
}

fn reach(t: &mut Track, current: &mut &'static str, s: &'static str, ts: i64) {
    t.reached.entry(s).or_insert(ts);
    *current = s;
}

/// The track from the issue's state and timeline (oldest first). `change_reviews` is how many
/// review records asked for changes. Without a timeline only the station is known.
pub fn track_of(issue: &IssueRecord, timeline: &[TimelineEntry], change_reviews: i64) -> Track {
    let mut t = Track {
        station: station_of_state(&issue.state, &issue.state_type),
        closed: match issue.state.as_str() {
            "Duplicate" => Some("duplicate"),
            "Canceled" => Some("canceled"),
            _ if issue.state_type == "canceled" => Some("canceled"),
            _ => None,
        },
        backlog: issue.state == "Backlog",
        ..Track::default()
    };
    if timeline.is_empty() {
        t.rounds = change_reviews;
        return t;
    }

    let mut current = "todo";
    let (mut gate_failed, mut review_failed, mut land_failed, mut push_failed) = (false, false, false, false);
    let (mut open_asks, mut rounds) = (0i64, 0i64);

    for e in timeline {
        let summary = e.summary.as_deref().unwrap_or("");
        match e.kind.as_str() {
            "issue_created" => reach(&mut t, &mut current, "todo", e.ts),
            "claimed" | "worktree" => reach(&mut t, &mut current, "started", e.ts),
            "status" | "note" | "handoff" => {
                if t.reached.contains_key("started") && index(current) <= index("working") {
                    reach(&mut t, &mut current, "working", e.ts);
                }
            }
            "gate_passed" => {
                gate_failed = false;
                land_failed = false;
                reach(&mut t, &mut current, "gate", e.ts);
            }
            "gate_failed" => {
                gate_failed = true;
                reach(&mut t, &mut current, "gate", e.ts);
            }
            "review_started" => {
                review_failed = false;
                land_failed = false;
                reach(&mut t, &mut current, "review", e.ts);
            }
            "review_failed" => review_failed = true,
            "submit" => {
                // A submit that got this far finished its review, and a failed push leaves the
                // branch unseen until the next submit.
                review_failed = false;
                push_failed = summary.contains("push failed");
                // Submit leaves a rejected issue In Progress without a move, so the note is the
                // only sign that the work went back from Review to Working.
                if summary.contains("requested changes") {
                    rounds += 1;
                    current = "working";
                }
            }
            "ask" => open_asks += 1,
            "answer" => open_asks = (open_asks - 1).max(0),
            "land_failed" => {
                land_failed = true;
                reach(&mut t, &mut current, "landing", e.ts);
            }
            "landed" => {
                land_failed = false;
                push_failed = false;
                reach(&mut t, &mut current, "landing", e.ts);
                reach(&mut t, &mut current, "done", e.ts);
            }
            "issue_moved" => {
                let Some(to) = moved_to(e.summary.as_deref()) else { continue };
                // A person moving the issue has taken it from wherever it stopped.
                land_failed = false;
                review_failed = false;
                push_failed = false;
                let s = station_of_state(to, "");
                if to == "Todo" || to == "Backlog" {
                    current = "todo";
                } else if s == "working" {
                    if index(current) >= index("review") {
                        current = "working";
                    } else if !t.reached.contains_key("started") {
                        reach(&mut t, &mut current, "started", e.ts);
                    }
                } else {
                    reach(&mut t, &mut current, s, e.ts);
                }
            }
            _ => {}
        }
    }

    // The state is authoritative once the issue leaves the agent's hands; while it is In
    // Progress, the events say how far the work got.
    if t.station == "working" {
        t.station = if index(current) >= index("started") { current } else { "working" };
    }
    if matches!(t.station, "landing" | "needs" | "review") {
        let last = timeline.last().map_or(0, |e| e.ts);
        t.reached.entry(t.station).or_insert(last);
    }
    t.rounds = rounds.max(change_reviews);
    t.asking = open_asks > 0;
    if t.station != "done" {
        if land_failed {
            // A stopped land keeps the issue's state (often In Review), but the work is at Landing.
            t.station = "landing";
            t.stuck = Some("Land stopped".into());
        } else if gate_failed {
            t.stuck = Some("Gate failed".into());
        } else if push_failed {
            t.stuck = Some("Push failed".into());
        } else if review_failed {
            t.stuck = Some("Review did not finish".into());
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(state: &str) -> IssueRecord {
        let state_type = match state {
            "Done" => "completed",
            "Canceled" | "Duplicate" => "canceled",
            "Todo" => "unstarted",
            "Backlog" => "backlog",
            _ => "started",
        };
        IssueRecord { key: "X-1".into(), state: state.into(), state_type: state_type.into(), ..Default::default() }
    }

    /// Builds a timeline from (kind, summary) pairs, one second apart.
    fn tl(events: &[(&str, &str)]) -> Vec<TimelineEntry> {
        events
            .iter()
            .enumerate()
            .map(|(n, (kind, summary))| TimelineEntry {
                ts: n as i64 + 1,
                actor: "claude:x".into(),
                kind: (*kind).into(),
                summary: (!summary.is_empty()).then(|| (*summary).into()),
            })
            .collect()
    }

    const BASE: &[(&str, &str)] = &[
        ("issue_created", "Filed as Todo"),
        ("claimed", "Took the issue"),
        ("issue_moved", "Todo → In Progress"),
        ("worktree", "Started x from main"),
        ("status", "Working"),
        ("gate_passed", "Gate passed"),
        ("review_started", "Review by codex started"),
    ];

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<TimelineEntry> {
        let mut all = BASE.to_vec();
        all.extend_from_slice(extra);
        tl(&all)
    }

    #[test]
    fn state_alone_gives_the_station() {
        assert_eq!(track_of(&issue("In Review"), &[], 0).station, "review");
        assert_eq!(track_of(&issue("Approved"), &[], 2).rounds, 2);
        let backlog = track_of(&issue("Backlog"), &[], 0);
        assert!(backlog.backlog && backlog.station == "todo");
        assert_eq!(track_of(&issue("Duplicate"), &[], 0).closed, Some("duplicate"));
    }

    #[test]
    fn changes_asked_goes_back_to_working_and_counts_rounds() {
        let t = track_of(&issue("In Progress"), &with(&[("submit", "Review by codex requested changes. Pull request: x"), ("status", "Fixing")]), 0);
        assert_eq!((t.station, t.rounds), ("working", 1));
        let t = track_of(
            &issue("In Progress"),
            &with(&[("submit", "Review by codex requested changes."), ("status", ""), ("gate_passed", ""), ("review_started", "")]),
            0,
        );
        assert_eq!(t.station, "review");
    }

    #[test]
    fn failed_gate_is_stuck_until_it_passes() {
        let t = track_of(&issue("In Progress"), &tl(&BASE[..5].iter().copied().chain([("gate_failed", "Gate failed")]).collect::<Vec<_>>()), 0);
        assert_eq!((t.station, t.stuck.as_deref()), ("gate", Some("Gate failed")));
        // A resubmit can fail without moving an issue that is In Review.
        let t = track_of(&issue("In Review"), &with(&[("submit", "passed; ready to land"), ("issue_moved", "In Progress → In Review"), ("status", ""), ("gate_failed", "")]), 0);
        assert_eq!((t.station, t.stuck.as_deref()), ("review", Some("Gate failed")));
    }

    #[test]
    fn stopped_land_is_at_landing_until_the_work_moves_again() {
        let t = track_of(&issue("Approved"), &with(&[("submit", "passed; waits"), ("issue_moved", "In Progress → Needs You"), ("issue_moved", "Needs You → Approved"), ("land_failed", "Land stopped")]), 0);
        assert_eq!((t.station, t.stuck.as_deref()), ("landing", Some("Land stopped")));
        assert!(t.reached.contains_key("needs"));
        let t = track_of(&issue("In Review"), &with(&[("submit", "passed; ready to land"), ("issue_moved", "In Progress → In Review"), ("land_failed", "Land stopped")]), 0);
        assert_eq!((t.station, t.stuck.as_deref()), ("landing", Some("Land stopped")));
        let t = track_of(&issue("In Review"), &with(&[("land_failed", ""), ("status", ""), ("gate_passed", ""), ("review_started", ""), ("submit", "passed"), ("issue_moved", "In Progress → In Review")]), 0);
        assert_eq!(t.stuck, None);
    }

    #[test]
    fn moves_with_evidence_parse_to_their_state() {
        let t = track_of(&issue("Done"), &with(&[("issue_moved", "In Progress → Done: tests passed → all green")]), 0);
        assert_eq!(t.station, "done");
        assert!(t.reached.contains_key("done"));
        assert_eq!(moved_to(Some("Todo → In Progress")), Some("In Progress"));
        assert_eq!(moved_to(Some("no arrow")), None);
    }

    #[test]
    fn review_failure_clears_on_a_later_submit_or_move() {
        let t = track_of(&issue("In Progress"), &with(&[("review_failed", "")]), 0);
        assert_eq!(t.stuck.as_deref(), Some("Review did not finish"));
        let t = track_of(&issue("Needs You"), &with(&[("review_failed", ""), ("submit", "Submitted without a review; waits for a human"), ("issue_moved", "In Progress → Needs You")]), 0);
        assert_eq!((t.station, t.stuck.as_deref()), ("needs", None));
        let t = track_of(&issue("Approved"), &with(&[("review_failed", ""), ("issue_moved", "In Progress → Approved")]), 0);
        assert_eq!(t.stuck, None);
    }

    #[test]
    fn failed_push_is_stuck_until_the_next_submit() {
        let t = track_of(&issue("In Progress"), &with(&[("submit", "Review by codex passed; ready to land. No pull request: push failed: rejected")]), 0);
        assert_eq!(t.stuck.as_deref(), Some("Push failed"));
        let t = track_of(&issue("In Review"), &with(&[("submit", "No pull request: push failed: x"), ("submit", "Review by codex passed; ready to land. Pull request: y"), ("issue_moved", "In Progress → In Review")]), 0);
        assert_eq!(t.stuck, None);
    }

    #[test]
    fn open_asks_and_answers() {
        let t = track_of(&issue("In Progress"), &with(&[("ask", "Which?")]), 0);
        assert!(t.asking);
        let t = track_of(&issue("In Progress"), &with(&[("ask", "Which?"), ("answer", "That one")]), 0);
        assert!(!t.asking);
    }

    #[test]
    fn skipped_stations_are_left_out_and_done_is_not_stuck() {
        let t = track_of(&issue("Done"), &with(&[("submit", "passed"), ("issue_moved", "In Progress → In Review"), ("gate_failed", ""), ("landed", "Landed"), ("issue_moved", "In Review → Done")]), 0);
        assert_eq!((t.station, t.stuck.as_deref()), ("done", None));
        assert!(!t.reached.contains_key("needs"));
    }
}
