-- The review level an issue declares (NIT-298, docs/verification.md): how much verification its
-- change is expected to need, from least to most: inert, contained, standard, sensitive (the app
-- shows Basic, Focused, Integration, Full). NULL is undeclared, which every issue filed before
-- this is: it gets today's gate, never the lightest. With it, who declared it, as what (author,
-- planner, coordinator, human, or implementer, whose declaration on its own issue is only a hint),
-- and when.
ALTER TABLE issue ADD COLUMN review_level TEXT
  CHECK (review_level IS NULL OR review_level IN ('inert', 'contained', 'standard', 'sensitive'));
ALTER TABLE issue ADD COLUMN review_level_by TEXT;
ALTER TABLE issue ADD COLUMN review_level_source TEXT
  CHECK (review_level_source IS NULL OR review_level_source IN ('author', 'planner', 'coordinator', 'human', 'implementer'));
ALTER TABLE issue ADD COLUMN review_level_at INTEGER;
