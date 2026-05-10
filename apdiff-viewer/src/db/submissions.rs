use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use semver::Version;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::schema::{apworld_artifacts, submission_annotations, submission_artifacts, submissions};

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = apworld_artifacts)]
pub struct ApworldArtifact {
    pub id: i64,
    pub world_name: String,
    pub version: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub first_seen_at: DateTime<Utc>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = apworld_artifacts)]
pub struct NewApworldArtifact<'a> {
    pub world_name: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
    pub size_bytes: i64,
}

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = submissions)]
pub struct Submission {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub pr_number: Option<i32>,
    pub commit_sha: Option<String>,
    pub manifest: JsonValue,
    pub changes_json: JsonValue,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = submissions)]
pub struct NewSubmission<'a> {
    pub id: &'a str,
    pub pr_number: Option<i32>,
    pub commit_sha: Option<&'a str>,
    pub manifest: &'a JsonValue,
    pub changes_json: &'a JsonValue,
}

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = submission_artifacts)]
pub struct SubmissionArtifact {
    pub submission_id: String,
    pub world_name: String,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = submission_artifacts)]
pub struct NewSubmissionArtifact<'a> {
    pub submission_id: &'a str,
    pub world_name: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
}

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = submission_annotations)]
pub struct SubmissionAnnotation {
    pub submission_id: String,
    pub world_name: String,
    pub version: String,
    pub content: JsonValue,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = submission_annotations)]
pub struct NewSubmissionAnnotation<'a> {
    pub submission_id: &'a str,
    pub world_name: &'a str,
    pub version: &'a str,
    pub content: &'a JsonValue,
}

/// Insert a submission together with its `apworld_artifacts` upserts, the
/// `submission_artifacts` linking rows, and any `submission_annotations`,
/// in a single transaction.
///
/// Concurrent uploads of the same `(world_name, version, sha256)` across
/// different submissions are safe via `ON CONFLICT DO NOTHING` on the
/// global dedup table.
pub async fn insert_submission_with_artifacts<'a>(
    conn: &mut AsyncPgConnection,
    submission: &NewSubmission<'a>,
    artifact_pairs: &[(NewApworldArtifact<'a>, NewSubmissionArtifact<'a>)],
    annotations: &[NewSubmissionAnnotation<'a>],
) -> QueryResult<()> {
    conn.transaction(|conn| {
        async move {
            for (artifact, _) in artifact_pairs {
                diesel::insert_into(apworld_artifacts::table)
                    .values(artifact)
                    .on_conflict((
                        apworld_artifacts::world_name,
                        apworld_artifacts::version,
                        apworld_artifacts::sha256,
                    ))
                    .do_nothing()
                    .execute(conn)
                    .await?;
            }

            diesel::insert_into(submissions::table)
                .values(submission)
                .execute(conn)
                .await?;

            for (_, sub_artifact) in artifact_pairs {
                diesel::insert_into(submission_artifacts::table)
                    .values(sub_artifact)
                    .execute(conn)
                    .await?;
            }

            if !annotations.is_empty() {
                diesel::insert_into(submission_annotations::table)
                    .values(annotations)
                    .execute(conn)
                    .await?;
            }

            Ok(())
        }
        .scope_boxed()
    })
    .await
}

/// Upsert into the global apworld dedup table. Returns 1 if a new row was
/// written, 0 if `(world_name, version, sha256)` was already present.
/// Used by the bootstrap import path (`POST /api/import`) to seed historical
/// versions without going through the submission flow.
pub async fn insert_apworld_artifact<'a>(
    conn: &mut AsyncPgConnection,
    artifact: &NewApworldArtifact<'a>,
) -> QueryResult<usize> {
    diesel::insert_into(apworld_artifacts::table)
        .values(artifact)
        .on_conflict((
            apworld_artifacts::world_name,
            apworld_artifacts::version,
            apworld_artifacts::sha256,
        ))
        .do_nothing()
        .execute(conn)
        .await
}

pub async fn get_submission(
    conn: &mut AsyncPgConnection,
    id: &str,
) -> QueryResult<Option<Submission>> {
    submissions::table
        .filter(submissions::id.eq(id))
        .select(Submission::as_select())
        .first(conn)
        .await
        .optional()
}

pub async fn get_submission_artifacts(
    conn: &mut AsyncPgConnection,
    submission_id: &str,
) -> QueryResult<Vec<SubmissionArtifact>> {
    submission_artifacts::table
        .filter(submission_artifacts::submission_id.eq(submission_id))
        .select(SubmissionArtifact::as_select())
        .load(conn)
        .await
}

pub async fn get_submission_annotations(
    conn: &mut AsyncPgConnection,
    submission_id: &str,
) -> QueryResult<Vec<SubmissionAnnotation>> {
    submission_annotations::table
        .filter(submission_annotations::submission_id.eq(submission_id))
        .select(SubmissionAnnotation::as_select())
        .load(conn)
        .await
}

/// All prior versions of an apworld across every submission, deduplicated by
/// `(version, sha256)` and ordered ascending by semver. Used to populate the
/// from-version dropdown in the diff page.
///
/// Rows whose `version` column does not parse as semver are silently dropped,
/// matching the existing `tc::list_indexed_versions` behavior.
pub async fn list_prior_versions(
    conn: &mut AsyncPgConnection,
    world_name: &str,
) -> QueryResult<Vec<(Version, String)>> {
    let rows: Vec<(String, String)> = apworld_artifacts::table
        .filter(apworld_artifacts::world_name.eq(world_name))
        .select((apworld_artifacts::version, apworld_artifacts::sha256))
        .load(conn)
        .await?;

    let mut parsed: Vec<(Version, String)> = rows
        .into_iter()
        .filter_map(|(v, sha)| Version::parse(&v).ok().map(|ver| (ver, sha)))
        .collect();
    parsed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    parsed.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    Ok(parsed)
}

/// Most-recent sha256 for a (world, version) tuple across any submission.
/// Returns None when no submission has ever uploaded this tuple.
pub async fn lookup_artifact_sha(
    conn: &mut AsyncPgConnection,
    world_name: &str,
    version: &str,
) -> QueryResult<Option<String>> {
    apworld_artifacts::table
        .filter(apworld_artifacts::world_name.eq(world_name))
        .filter(apworld_artifacts::version.eq(version))
        .select(apworld_artifacts::sha256)
        .order(apworld_artifacts::first_seen_at.desc())
        .first::<String>(conn)
        .await
        .optional()
}
