-- Bind :project and :since (UTC Unix seconds) against a read-only Nitely database.
WITH landed AS (
 SELECT e.issue_key,min(e.ts) landed FROM event e JOIN issue i ON i.key=e.issue_key
 WHERE i.project=:project AND e.type='landed' GROUP BY e.issue_key HAVING min(e.ts)>=:since
), rounds AS (
 SELECT r.*, lag(r.commit_sha) OVER w prev_head, lag(r.ts) OVER w prev_at,
 lag(r.findings) OVER w prev_findings FROM review r JOIN landed l ON l.issue_key=r.issue_key
 WHERE r.reviewer!='' AND r.ts<=l.landed WINDOW w AS(PARTITION BY r.issue_key ORDER BY r.ts,r.id)
)
SELECT l.issue_key,l.landed,
 l.landed-(SELECT min(e.ts) FROM event e WHERE e.issue_key=l.issue_key AND e.type='worktree') seconds_start_to_land,
 count(r.id) reviews,
 sum(CASE WHEN r.commit_sha=r.prev_head AND EXISTS(SELECT 1 FROM event e WHERE e.issue_key=l.issue_key AND e.type IN ('note','handoff','reviewer_note','answer') AND e.ts BETWEEN r.prev_at AND r.ts) THEN 1 ELSE 0 END) clarification_candidates,
 sum((SELECT count(*) FROM json_each(r.findings) f WHERE json_extract(f.value,'$.carried')=1 OR EXISTS (SELECT 1 FROM json_each(r.prev_findings) p WHERE json_extract(p.value,'$.text')=json_extract(f.value,'$.text')))) repeated_findings
FROM landed l LEFT JOIN rounds r ON r.issue_key=l.issue_key GROUP BY l.issue_key,l.landed;
