-- The Archipelago version a room runs on. Every room that exists was made for 0.6.7, the only
-- version there was. The default stays: a lobby from before this column, still running while
-- this one is rolled out, creates rooms without naming a version, and those are 0.6.7 rooms too.
ALTER TABLE rooms ADD COLUMN ap_version TEXT NOT NULL DEFAULT '0.6.7';

-- NULL means "whatever the default version is when a room is made from the template".
ALTER TABLE room_templates ADD COLUMN ap_version TEXT;
