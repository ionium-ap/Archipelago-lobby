//! Submission-storage-backed `ArtifactSource`.
//!
//! Each instance is bound to a specific submission ID (the "current task"
//! equivalent in the TC backend). Apworld bytes live on disk in a content-
//! addressed `BlobStore` keyed by sha256; metadata about which submission
//! contributed which artifact lives in postgres.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use apwm::changes::Changes;
use async_trait::async_trait;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::AsyncPgConnection;
use semver::Version;

use crate::apworld::{self, FileTree};
use crate::blob_store::BlobStore;
use crate::db;
use crate::diff::Annotations;
use crate::source::ArtifactSource;
use crate::TreeCache;

pub struct SubmissionSource<'a> {
    pool: &'a Pool<AsyncPgConnection>,
    blobs: &'a Arc<BlobStore>,
    submission_id: String,
    changes: Changes,
    tree_cache: &'a TreeCache,
}

impl<'a> SubmissionSource<'a> {
    /// Load a submission's metadata up front. Returns `Ok(None)` if no
    /// submission with this ID exists.
    pub async fn load(
        pool: &'a Pool<AsyncPgConnection>,
        blobs: &'a Arc<BlobStore>,
        tree_cache: &'a TreeCache,
        submission_id: &str,
    ) -> Result<Option<Self>> {
        let mut conn = pool.get().await?;
        let submission = match db::get_submission(&mut conn, submission_id).await? {
            Some(s) => s,
            None => return Ok(None),
        };
        let changes: Changes = serde_json::from_value(submission.changes_json)
            .context("stored changes_json failed to parse")?;
        Ok(Some(Self {
            pool,
            blobs,
            submission_id: submission.id,
            changes,
            tree_cache,
        }))
    }

}

#[async_trait]
impl ArtifactSource for SubmissionSource<'_> {
    async fn changes_json(&self) -> Result<Changes> {
        Ok(self.changes.clone())
    }

    async fn fetch_tree(&self, world: &str, version: &str) -> Result<Arc<FileTree>> {
        // The `apworld_artifacts` row is the source of truth for which sha256
        // backs a given (world, version). It may have been uploaded by this
        // submission, a prior submission, or never — the trait method maps the
        // "never" case to an Err since callers (process_world) treat that as a
        // missing prior version.
        let mut conn = self.pool.get().await?;
        let sha256 = db::lookup_artifact_sha(&mut conn, world, version)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no stored blob for {world} v{version}"))?;

        // Cache key is the sha256 — content-addressed, so different submissions
        // pointing at identical bytes share a parsed FileTree across requests.
        let key = format!("sha:{sha256}");
        if let Some(tree) = self
            .tree_cache
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(tree.clone());
        }

        let bytes = self.blobs.read(&sha256).await?;
        let tree = Arc::new(apworld::extract_apworld(&bytes)?);
        self.tree_cache
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(key, tree.clone());
        Ok(tree)
    }

    async fn annotations(
        &self,
        world: &str,
        version: &str,
    ) -> Result<BTreeMap<String, Vec<Annotations>>> {
        let mut conn = self.pool.get().await?;
        let rows = db::get_submission_annotations(&mut conn, &self.submission_id).await?;
        for row in rows {
            if row.world_name == world && row.version == version {
                let map: BTreeMap<String, Vec<Annotations>> =
                    serde_json::from_value(row.content)
                        .context("stored annotation content failed to parse")?;
                return Ok(map);
            }
        }
        Ok(BTreeMap::new())
    }

    async fn list_versions(&self, world: &str) -> Result<Vec<Version>> {
        let mut conn = self.pool.get().await?;
        let rows = db::list_prior_versions(&mut conn, world).await?;
        Ok(rows.into_iter().map(|(v, _sha)| v).collect())
    }
}
