-- What a room's reviewers made of a YAML. A YAML without a row hasn't been reviewed.
-- `content_sha256` is the hash of the YAML's content when it was marked, which tells whether
-- it changed since: the owner of the room can edit YAMLs.
CREATE TABLE yaml_review_marks (
    yaml_id UUID PRIMARY KEY REFERENCES yamls(id) ON DELETE CASCADE,
    room_id UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('ok', 'needs_changes')),
    content_sha256 TEXT NOT NULL,
    marked_by BIGINT NOT NULL,
    marked_by_name VARCHAR NOT NULL,
    marked_at TIMESTAMP NOT NULL DEFAULT now()
);

CREATE INDEX yaml_review_marks_room_id ON yaml_review_marks (room_id);

-- What reviewers say about a YAML, to each other. Players never see them.
CREATE TABLE yaml_review_comments (
    id SERIAL PRIMARY KEY,
    yaml_id UUID NOT NULL REFERENCES yamls(id) ON DELETE CASCADE,
    room_id UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL,
    author_name VARCHAR NOT NULL,
    content TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT now(),
    edited_at TIMESTAMP
);

CREATE INDEX yaml_review_comments_room_id ON yaml_review_comments (room_id);
CREATE INDEX yaml_review_comments_yaml_id ON yaml_review_comments (yaml_id);
