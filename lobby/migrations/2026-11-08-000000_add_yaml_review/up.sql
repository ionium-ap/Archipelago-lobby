-- Discord IDs of the people who can see a room's YAML review tab, besides its owner and admins.
-- Rooms made from a template start with the template's list.
ALTER TABLE rooms ADD COLUMN reviewer_list BIGINT[] NOT NULL DEFAULT '{}';
ALTER TABLE room_templates ADD COLUMN reviewer_list BIGINT[] NOT NULL DEFAULT '{}';

-- The builtin review filters a room's reviewers turned off. Builtins are on unless they're in
-- here, so the ones added to the lobby later are on in the rooms that exist.
ALTER TABLE rooms ADD COLUMN review_disabled_builtins TEXT[] NOT NULL DEFAULT '{}';

-- The review filters a room's reviewers defined
CREATE TABLE room_review_filters (
    id SERIAL PRIMARY KEY,
    room_id UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    filter JSONB NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT true,
    last_edited_by BIGINT NOT NULL,
    last_edited_by_name VARCHAR NOT NULL,
    last_edited_at TIMESTAMP NOT NULL DEFAULT now()
);

CREATE INDEX room_review_filters_room_id ON room_review_filters (room_id);
