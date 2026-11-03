ALTER TABLE review ADD COLUMN scope_revision TEXT;
ALTER TABLE review ADD COLUMN scope_record TEXT;
CREATE INDEX review_scope_issue ON review(issue_key,id DESC) WHERE scope_record IS NOT NULL;
