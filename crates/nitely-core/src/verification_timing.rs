//! Observed command time, never a cache certificate or an estimate from unlike revisions.
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::path::Path;

use crate::{playbook::{git, CommandResult, GateResult, PLAYBOOK_PATH}, Db};

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub head: String,
    pub depth: String,
    pub reduced_lane: i64,
    pub full_lane: i64,
    pub full_run: i64,
    pub reduced_seconds: u64,
    pub full_seconds: u64,
    /// Null for sub-second Full runs, rather than an infinite or fabricated speedup.
    pub fraction_saved: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub comparisons: Vec<Comparison>,
    pub note: &'static str,
}

fn seconds(results: &[CommandResult], commands: &[String]) -> Option<u64> {
    if results.is_empty() || results.len() != commands.len()
        || results.iter().zip(commands).any(|(r,c)| !r.ok || r.skipped || r.command != *c) {
        return None;
    }
    results.iter().try_fold(0u64, |total,r| total.checked_add(r.seconds))
}

fn local(db:&Db, id:i64)->Result<bool> {
    Ok(db.conn.query_row("SELECT state='done' AND exit_code=0 AND executor='local' FROM lane_run WHERE id=?1",
        [id],|r|r.get(0)).optional()?.unwrap_or(false))
}

/// Compare only actual successful local executions on the same immutable candidate and
/// committed policy. A later failed attempt hides an older pass. No tests are launched.
pub fn report(db:&Db, repo:&Path, key:&str)->Result<Report> {
    let id=crate::verification_audit::repository_id(repo)?;
    let json=db.conn.prepare("SELECT gate FROM review WHERE issue_key=?1 COLLATE NOCASE AND gate IS NOT NULL ORDER BY id DESC LIMIT 20")?
        .query_map([key],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut comparisons=Vec::new();
    for value in json {
        let gate:GateResult=serde_json::from_str(&value)?;
        let Some(plan)=&gate.plan else {continue};
        if plan.shadow || !gate.passed || gate.skipped.is_some() || !gate.flaked.is_empty()
            || comparisons.iter().any(|c:&Comparison|c.head==plan.head) {continue;}
        let Some(reduced_lane)=gate.lane_run else {continue};
        if !local(db,reduced_lane)? {continue;}
        let Some(reduced_seconds)=seconds(&gate.results,&plan.commands) else {continue};
        let policy=git(repo,&["rev-parse","--verify",&format!("{}:{PLAYBOOK_PATH}",plan.policy_commit)]);
        let Ok(policy)=policy else {continue};
        // Select the latest attempt first: filtering to passed rows would hide a later failure.
        type FullRun=(i64,String,String,Option<i64>,String,String);
        let full:Option<FullRun>=db.conn.query_row(
            "SELECT id,state,policy,lane_run,commands,evidence FROM verification_backstop_run WHERE repo=?1 AND head=?2 ORDER BY id DESC LIMIT 1",
            rusqlite::params![id,plan.head],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let Some((full_run,state,full_policy,Some(full_lane),commands,evidence))=full else {continue};
        if state!="passed" || full_policy!=policy.trim() || !local(db,full_lane)? {continue;}
        let commands:Vec<String>=serde_json::from_str(&commands)?;
        let evidence:Vec<CommandResult>=serde_json::from_str(&evidence)?;
        let Some(full_seconds)=seconds(&evidence,&commands) else {continue};
        comparisons.push(Comparison{head:plan.head.clone(),depth:plan.depth.clone(),reduced_lane,full_lane,full_run,
            reduced_seconds,full_seconds,
            fraction_saved:(full_seconds>0).then(||1.0-reduced_seconds as f64/full_seconds as f64)});
    }
    Ok(Report{comparisons,note:"Observed command seconds on the same commit and policy; excludes queue/setup. Cache warmth and machine load may differ. No pair means not measured, not zero cost. Remote and reused checks are excluded; this report never authorizes a gate."})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{issues::Actor, test_repo::git as g, NewIssue};

    #[test]
    fn comparisons_require_the_same_candidate_policy_and_latest_successful_execution() {
        let repo=crate::test_repo::copy("verification-timing",|p|crate::test_repo::init(p,&[
            (PLAYBOOK_PATH,"[verification]\nversion=1\nmode='active'\ncomments=['math[.]rs']\nbasic=['echo basic']\n"),
            ("math.rs","// old\nfn value()->i32 { 1 }")]));
        let mut db=Db::open(&crate::paths::unique_temp("nitely-timing",".sqlite")).unwrap();
        db.create_project("APP","Example").unwrap();
        let issue=db.create_issue(&NewIssue{project:"APP".into(),title:"Clarify source comment".into(),body:"Explain calculation".into(),
            kind:"feature".into(),source:"codex".into(),evidence:"test".into(),review_level:Some("inert".into()),..Default::default()},&Actor::Human).unwrap();
        let base=g(&repo,&["rev-parse","HEAD"]).trim().to_string();
        std::fs::write(repo.join("math.rs"),"// clearer\nfn value()->i32 { 1 }").unwrap();
        g(&repo,&["commit","-qam","comment"]);
        let plan=crate::verification::assess(&repo,&base,"HEAD",&issue).unwrap();
        let result=|command:&str,seconds|CommandResult{command:command.into(),ok:true,seconds,output:"executed".into(),skipped:false,timed_out:false};
        let gate=GateResult{mode_authority:None,passed:true,skipped:None,commit:plan.head.clone(),
            results:plan.commands.iter().map(|c|result(c,1)).collect(),plan:Some(plan.clone()),
            warnings:vec![],flaked:vec![],lane_run:Some(1)};
        db.conn.execute("INSERT INTO review(issue_key,patch_id,commit_sha,reviewer,verdict,approver,ts,gate) VALUES(?1,'patch',?2,'','light','agent',1,?3)",
            rusqlite::params![issue.key,plan.head,serde_json::to_string(&gate).unwrap()]).unwrap();
        for id in [1,2] {
            db.conn.execute("INSERT INTO lane_run(id,project,repo,requested_by,kind,snapshot,tree,base,command,state,queued_at,exit_code) VALUES(?1,'APP',?2,'test','gate',?3,?4,?5,'echo','done',1,0)",
                rusqlite::params![id,repo.to_string_lossy(),plan.head,plan.tree,base]).unwrap();
        }
        assert!(report(&db,&repo,&issue.key).unwrap().comparisons.is_empty());
        let policy=g(&repo,&["rev-parse",&format!("{base}:{PLAYBOOK_PATH}")]).trim().to_string();
        db.conn.execute("INSERT INTO verification_backstop_run(repo,head,policy,commands,started_at,state,lane_run,evidence) VALUES(?1,?2,'wrong','[\"echo full\"]',1,'passed',2,?3)",
            rusqlite::params![crate::verification_audit::repository_id(&repo).unwrap(),base,serde_json::to_string(&vec![result("echo full",20)]).unwrap()]).unwrap();
        assert!(report(&db,&repo,&issue.key).unwrap().comparisons.is_empty(),"unlike revisions are not a speedup");
        db.conn.execute("UPDATE verification_backstop_run SET head=?1",[&plan.head]).unwrap();
        assert!(report(&db,&repo,&issue.key).unwrap().comparisons.is_empty(),"policy changes invalidate comparisons");
        db.conn.execute("UPDATE verification_backstop_run SET policy=?1",[policy]).unwrap();
        let paired=report(&db,&repo,&issue.key).unwrap();
        assert_eq!(paired.comparisons.len(),1);
        assert_eq!(paired.comparisons[0].reduced_seconds,2);
        assert_eq!(paired.comparisons[0].full_seconds,20);
        assert_eq!(paired.comparisons[0].fraction_saved,Some(0.9));
        db.conn.execute("UPDATE lane_run SET executor='remote' WHERE id=2",[]).unwrap();
        assert!(report(&db,&repo,&issue.key).unwrap().comparisons.is_empty(),"unmatched execution environments are excluded");
        db.conn.execute("UPDATE lane_run SET executor='local' WHERE id=2",[]).unwrap();
        db.conn.execute("INSERT INTO verification_backstop_run(repo,head,policy,commands,started_at,state) SELECT repo,head,policy,commands,2,'failed' FROM verification_backstop_run LIMIT 1",[]).unwrap();
        assert!(report(&db,&repo,&issue.key).unwrap().comparisons.is_empty(),"latest failure must not be hidden behind old green evidence");
    }
}
