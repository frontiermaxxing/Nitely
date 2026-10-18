-- Crew works approved goals (NIT-243, docs/coordinator.md): the person ranks a project's active goals
-- (lower first; none: nearest target date, then oldest), and a goal may cap how many crew workers
-- work on it at once (none: the playbook's [crew] goal_workers).
ALTER TABLE goal ADD COLUMN rank INTEGER;
ALTER TABLE goal ADD COLUMN workers INTEGER;
