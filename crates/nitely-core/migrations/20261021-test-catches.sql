-- A failed gate's tests, kept until the issue's next passing gate says what fixed them (NIT-557).
-- `tests` holds each failed test with the step that ran it; `passed_steps` the steps that ran and
-- passed, so a later failure shows which earlier failures it saw pass.
CREATE TABLE gate_failure (
 id INTEGER PRIMARY KEY,
 issue_key TEXT NOT NULL,
 commit_sha TEXT NOT NULL,
 tests TEXT NOT NULL,
 passed_steps TEXT NOT NULL DEFAULT '[]',
 failed_at INTEGER NOT NULL,
 resolved_at INTEGER
);
CREATE INDEX gate_failure_open ON gate_failure(issue_key, resolved_at);
-- Each failed test, once fixed: a catch when the fix changed product code, a false alarm when it
-- changed only tests.
CREATE TABLE test_catch (
 id INTEGER PRIMARY KEY,
 issue_key TEXT NOT NULL,
 test TEXT NOT NULL,
 module TEXT NOT NULL,
 verdict TEXT NOT NULL,
 failed_commit TEXT NOT NULL,
 fixed_commit TEXT NOT NULL,
 at INTEGER NOT NULL
);
CREATE INDEX test_catch_module ON test_catch(module);
