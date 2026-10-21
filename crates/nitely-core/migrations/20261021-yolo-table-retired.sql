-- The legacy YOLO switch is gone (NIT-499): the review mode alone decides, and Custom with tests and
-- review off is what YOLO was. Its rows were already migrated into project_review_mode.
DROP TABLE project_yolo;
