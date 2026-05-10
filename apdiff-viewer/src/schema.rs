diesel::table! {
    fuzz_results (id) {
        id -> Int8,
        world_name -> Varchar,
        version -> Varchar,
        checksum -> Varchar,
        total -> Int4,
        success -> Int4,
        failure -> Int4,
        timeout -> Int4,
        ignored -> Int4,
        task_id -> Varchar,
        pr_number -> Nullable<Int4>,
        extra_args -> Nullable<Varchar>,
        recorded_at -> Timestamptz,
    }
}

diesel::table! {
    apworld_artifacts (id) {
        id -> Int8,
        world_name -> Text,
        version -> Text,
        sha256 -> Text,
        size_bytes -> Int8,
        first_seen_at -> Timestamptz,
    }
}

diesel::table! {
    submissions (id) {
        id -> Text,
        created_at -> Timestamptz,
        pr_number -> Nullable<Int4>,
        commit_sha -> Nullable<Text>,
        manifest -> Jsonb,
        changes_json -> Jsonb,
    }
}

diesel::table! {
    submission_artifacts (submission_id, world_name, version) {
        submission_id -> Text,
        world_name -> Text,
        version -> Text,
        sha256 -> Text,
    }
}

diesel::table! {
    submission_annotations (submission_id, world_name, version) {
        submission_id -> Text,
        world_name -> Text,
        version -> Text,
        content -> Jsonb,
    }
}
