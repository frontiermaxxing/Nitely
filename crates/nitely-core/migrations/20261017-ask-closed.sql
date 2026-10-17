-- Why an ask closed without an answer: its issue closed (NIT-181). NULL while open or answered.
ALTER TABLE ask ADD COLUMN closed TEXT;
