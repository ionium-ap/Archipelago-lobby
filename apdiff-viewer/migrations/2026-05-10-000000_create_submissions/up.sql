-- Every (world_name, version) ever uploaded, deduplicated by content sha256.
CREATE TABLE apworld_artifacts (
    id BIGSERIAL PRIMARY KEY,
    world_name TEXT NOT NULL,
    version TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (world_name, version, sha256)
);

CREATE INDEX idx_apworld_artifacts_world_version ON apworld_artifacts(world_name, version);

-- One row per CI submission. Holds the changes.json blob + manifest verbatim.
CREATE TABLE submissions (
    id TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    pr_number INTEGER,
    commit_sha TEXT,
    manifest JSONB NOT NULL,
    changes_json JSONB NOT NULL
);

CREATE INDEX idx_submissions_created_at ON submissions(created_at DESC);

-- Which apworlds this submission contributed (the "added" set in changes.json).
-- Composite FK on (world_name, version, sha256) into apworld_artifacts ensures
-- that no submission references a blob we don't know about.
CREATE TABLE submission_artifacts (
    submission_id TEXT NOT NULL REFERENCES submissions(id) ON DELETE CASCADE,
    world_name TEXT NOT NULL,
    version TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (submission_id, world_name, version),
    FOREIGN KEY (world_name, version, sha256)
        REFERENCES apworld_artifacts(world_name, version, sha256)
);

CREATE INDEX idx_submission_artifacts_world_version ON submission_artifacts(world_name, version);

-- Optional aplint annotations. Stored as JSONB rather than blobs since they're small.
CREATE TABLE submission_annotations (
    submission_id TEXT NOT NULL REFERENCES submissions(id) ON DELETE CASCADE,
    world_name TEXT NOT NULL,
    version TEXT NOT NULL,
    content JSONB NOT NULL,
    PRIMARY KEY (submission_id, world_name, version)
);
