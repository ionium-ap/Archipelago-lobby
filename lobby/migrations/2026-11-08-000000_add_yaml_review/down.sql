DROP TABLE room_review_filters;
ALTER TABLE rooms DROP COLUMN review_disabled_builtins;
ALTER TABLE room_templates DROP COLUMN reviewer_list;
ALTER TABLE rooms DROP COLUMN reviewer_list;
