-- A review's rules (NIT-185): the playbook's [review] table it ran under, as JSON, so a pass is
-- carried over to the same patch only while the rules are the same.
ALTER TABLE review ADD COLUMN rules TEXT;
