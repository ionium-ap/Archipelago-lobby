use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Component;
use std::sync::Arc;

use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::AsyncPgConnection;
use futures::StreamExt;
use rand::Rng;
use rocket::data::{Data, ToByteUnit};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{routes, Route, State};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::blob_store::BlobStore;
use crate::db::{
    self, insert_submission_with_artifacts, FuzzResult, NewApworldArtifact, NewFuzzResult,
    NewSubmission, NewSubmissionAnnotation, NewSubmissionArtifact, PreviousResult,
};
use crate::guards::{ApdiffApiKey, FuzzApiKey};
use crate::{Result, SubmissionConfig};

#[derive(Debug, Deserialize)]
pub struct RecordFuzzResultsRequest {
    pub task_id: String,
    pub pr_number: Option<i32>,
    pub extra_args: Option<String>,
    pub results: Vec<FuzzResultInput>,
}

#[derive(Debug, Deserialize)]
pub struct FuzzResultInput {
    pub world_name: String,
    pub version: String,
    pub checksum: String,
    pub total: i32,
    pub success: i32,
    pub failure: i32,
    pub timeout: i32,
    pub ignored: i32,
}

#[rocket::post("/fuzz-results", data = "<request>")]
async fn record_fuzz_results(
    _key: FuzzApiKey,
    pool: &State<Pool<AsyncPgConnection>>,
    request: Json<RecordFuzzResultsRequest>,
) -> Result<()> {
    let mut conn = pool.get().await?;

    let new_results: Vec<NewFuzzResult> = request
        .results
        .iter()
        .map(|r| NewFuzzResult {
            world_name: &r.world_name,
            version: &r.version,
            checksum: &r.checksum,
            total: r.total,
            success: r.success,
            failure: r.failure,
            timeout: r.timeout,
            ignored: r.ignored,
            task_id: &request.task_id,
            pr_number: request.pr_number,
            extra_args: request.extra_args.as_deref(),
        })
        .collect();

    db::insert_fuzz_results(&mut conn, new_results).await?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct FuzzResultsResponse {
    pub results: Vec<FuzzResult>,
}

#[rocket::get("/fuzz-results/<world_name>?<limit>&<offset>")]
async fn get_fuzz_results(
    pool: &State<Pool<AsyncPgConnection>>,
    world_name: &str,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Json<FuzzResultsResponse>> {
    let mut conn = pool.get().await?;

    let results = db::get_fuzz_results_for_world(
        &mut conn,
        world_name,
        limit.unwrap_or(50),
        offset.unwrap_or(0),
    )
    .await?;

    Ok(Json(FuzzResultsResponse { results }))
}

#[derive(Debug, serde::Serialize)]
pub struct PreviousResultsResponse {
    pub previous_results: Vec<PreviousResult>,
}

#[rocket::get("/fuzz-results/<world_name>/previous?<version>&<checksum>&<extra_args>")]
async fn get_previous_results(
    pool: &State<Pool<AsyncPgConnection>>,
    world_name: &str,
    version: &str,
    checksum: &str,
    extra_args: Option<&str>,
) -> Result<Json<PreviousResultsResponse>> {
    let mut conn = pool.get().await?;

    let previous_results =
        db::get_previous_results(&mut conn, world_name, version, checksum, extra_args).await?;

    Ok(Json(PreviousResultsResponse { previous_results }))
}

// ─── submissions ────────────────────────────────────────────────────────────

/// Hard cap on upload size. Tarballs are typically a few-MB to a few-tens-of-MB;
/// 100 MiB is generous headroom and protects the server from accidental floods.
const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Bytes of randomness per submission ID. Encoded as lowercase hex, so the
/// rendered ID length is `SUBMISSION_ID_BYTES * 2`.
const SUBMISSION_ID_BYTES: usize = 6;
const MAX_ID_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AddedEntry {
    world_name: String,
    version: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    #[serde(default)]
    pr_number: Option<i32>,
    #[serde(default)]
    commit_sha: Option<String>,
    added: Vec<AddedEntry>,
}

#[derive(Debug, Serialize)]
pub struct SubmissionResponse {
    pub id: String,
    pub url: String,
}

fn generate_submission_id() -> String {
    let bytes: [u8; SUBMISSION_ID_BYTES] = rand::thread_rng().gen();
    bytes
        .iter()
        .fold(String::with_capacity(SUBMISSION_ID_BYTES * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

type SubmissionError = (Status, String);

fn bad_request(msg: impl Into<String>) -> SubmissionError {
    (Status::BadRequest, msg.into())
}

fn internal(msg: impl Into<String>) -> SubmissionError {
    (Status::InternalServerError, msg.into())
}

/// Ingest a tarball of `.apworld` bytes + metadata, store the blobs, and
/// register a new submission row in postgres. Returns `201 Created` with
/// `{ id, url }` on success.
///
/// Tarball layout:
/// ```text
/// manifest.json
/// changes.json
/// apworlds/<world>-<version>.apworld          (one per manifest.added entry)
/// annotations/<world>-<version>.aplint        (optional, per added entry)
/// ```
#[rocket::post("/submissions", data = "<data>")]
async fn submit_apworlds(
    _key: ApdiffApiKey,
    pool: &State<Pool<AsyncPgConnection>>,
    blobs: &State<Arc<BlobStore>>,
    config: &State<SubmissionConfig>,
    data: Data<'_>,
) -> std::result::Result<Json<SubmissionResponse>, SubmissionError> {
    // 1. Drain the tar stream entry-by-entry into an in-memory map. Each
    //    entry's bytes are bounded by the total upload limit. The archive
    //    itself is streamed from the network; we don't buffer the whole
    //    request body before parsing.
    let stream = data.open(MAX_UPLOAD_BYTES.bytes());
    let mut archive = tokio_tar::Archive::new(stream);
    let mut entries = archive
        .entries()
        .map_err(|e| bad_request(format!("not a valid tar stream: {e}")))?;

    let mut entries_by_path: HashMap<String, Vec<u8>> = HashMap::new();
    while let Some(entry_result) = entries.next().await {
        let mut entry = entry_result
            .map_err(|e| bad_request(format!("tar entry error: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| bad_request(format!("tar entry path error: {e}")))?
            .into_owned();
        if path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            return Err(bad_request(format!(
                "rejecting unsafe path: {}",
                path.display()
            )));
        }
        let key = path.to_string_lossy().into_owned();
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .await
            .map_err(|e| bad_request(format!("read tar entry {key}: {e}")))?;
        entries_by_path.insert(key, buf);
    }

    // 2. Required structural files.
    let manifest_bytes = entries_by_path
        .remove("manifest.json")
        .ok_or_else(|| bad_request("tarball missing manifest.json"))?;
    let manifest_value: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| bad_request(format!("invalid manifest.json: {e}")))?;
    let manifest: Manifest = serde_json::from_value(manifest_value.clone())
        .map_err(|e| bad_request(format!("manifest.json schema mismatch: {e}")))?;

    let changes_bytes = entries_by_path
        .remove("changes.json")
        .ok_or_else(|| bad_request("tarball missing changes.json"))?;
    let changes_value: serde_json::Value = serde_json::from_slice(&changes_bytes)
        .map_err(|e| bad_request(format!("invalid changes.json: {e}")))?;
    // Validate it parses to the apwm schema, but store the JSON verbatim so
    // future apwm schema additions don't drop fields.
    let _: apwm::changes::Changes = serde_json::from_value(changes_value.clone())
        .map_err(|e| bad_request(format!("changes.json schema mismatch: {e}")))?;

    // 3. Pair every manifest.added[] with its apworld bytes.
    struct ArtifactUpload {
        added: AddedEntry,
        bytes: Vec<u8>,
        sha256_hex: String,
        size: i64,
    }
    let mut artifact_uploads: Vec<ArtifactUpload> = Vec::new();
    for added in &manifest.added {
        let key = format!("apworlds/{}-{}.apworld", added.world_name, added.version);
        let bytes = entries_by_path.remove(&key).ok_or_else(|| {
            bad_request(format!(
                "manifest references {key} but tarball has no such entry"
            ))
        })?;
        let sha256_hex = format!("{:x}", Sha256::digest(&bytes));
        let size = bytes.len() as i64;
        artifact_uploads.push(ArtifactUpload {
            added: added.clone(),
            bytes,
            sha256_hex,
            size,
        });
    }

    // 4. Optional annotations.
    let mut annotation_uploads: Vec<(AddedEntry, serde_json::Value)> = Vec::new();
    for added in &manifest.added {
        let key = format!("annotations/{}-{}.aplint", added.world_name, added.version);
        if let Some(bytes) = entries_by_path.remove(&key) {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| bad_request(format!("invalid {key}: {e}")))?;
            annotation_uploads.push((added.clone(), value));
        }
    }

    // 5. Write blobs to disk before the DB transaction. The store is
    //    idempotent on identical content, so a retried or duplicated submission
    //    is safe.
    for upload in &artifact_uploads {
        blobs
            .put(&upload.sha256_hex, &upload.bytes)
            .await
            .map_err(|e| internal(format!("blob write failed for {}: {e}", upload.sha256_hex)))?;
    }

    // 6. Insert with retry on submission-id collision.
    let mut conn = pool
        .get()
        .await
        .map_err(|e| internal(format!("db pool acquire: {e}")))?;

    let mut submission_id: Option<String> = None;
    for _ in 0..MAX_ID_ATTEMPTS {
        let candidate = generate_submission_id();

        let artifact_pairs: Vec<_> = artifact_uploads
            .iter()
            .map(|u| {
                (
                    NewApworldArtifact {
                        world_name: &u.added.world_name,
                        version: &u.added.version,
                        sha256: &u.sha256_hex,
                        size_bytes: u.size,
                    },
                    NewSubmissionArtifact {
                        submission_id: &candidate,
                        world_name: &u.added.world_name,
                        version: &u.added.version,
                        sha256: &u.sha256_hex,
                    },
                )
            })
            .collect();

        let annotations: Vec<NewSubmissionAnnotation<'_>> = annotation_uploads
            .iter()
            .map(|(a, v)| NewSubmissionAnnotation {
                submission_id: &candidate,
                world_name: &a.world_name,
                version: &a.version,
                content: v,
            })
            .collect();

        let new_sub = NewSubmission {
            id: &candidate,
            pr_number: manifest.pr_number,
            commit_sha: manifest.commit_sha.as_deref(),
            manifest: &manifest_value,
            changes_json: &changes_value,
        };

        match insert_submission_with_artifacts(&mut conn, &new_sub, &artifact_pairs, &annotations)
            .await
        {
            Ok(()) => {
                submission_id = Some(candidate);
                break;
            }
            Err(diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::UniqueViolation,
                _,
            )) => {
                // Submission ID collision — extremely rare for 48 random bits;
                // try a fresh ID a few times before giving up.
                continue;
            }
            Err(e) => return Err(internal(format!("submission insert failed: {e}"))),
        }
    }
    let submission_id =
        submission_id.ok_or_else(|| internal("could not generate a unique submission id"))?;

    let url = format!(
        "{}/submission/{}",
        config.public_base_url.trim_end_matches('/'),
        submission_id
    );
    Ok(Json(SubmissionResponse {
        id: submission_id,
        url,
    }))
}

pub fn routes() -> Vec<Route> {
    routes![
        record_fuzz_results,
        get_fuzz_results,
        get_previous_results,
        submit_apworlds,
    ]
}
