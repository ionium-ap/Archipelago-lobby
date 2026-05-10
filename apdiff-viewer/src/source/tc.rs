//! Taskcluster-backed `ArtifactSource`.
//!
//! Wraps the existing low-level `crate::tc` primitives (which still own the
//! HTTP/queue/index-list mechanics) and exposes them to `process_world`
//! through the `ArtifactSource` trait.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use apwm::changes::Changes;
use async_trait::async_trait;
use semver::Version;
use taskcluster::{Index, Queue};

use crate::apworld::{self, FileTree};
use crate::diff::Annotations;
use crate::source::ArtifactSource;
use crate::tc as tc_primitives;
use crate::TreeCache;

pub struct TcSource<'a> {
    queue: &'a Queue,
    index: &'a Index,
    namespace_prefix: &'a str,
    task_id: String,
    artifacts: Vec<String>,
    tree_cache: &'a TreeCache,
}

impl<'a> TcSource<'a> {
    /// Construct from a task ID. Eagerly fetches the task's artifact list
    /// because every downstream call disambiguates against it.
    pub async fn new(
        queue: &'a Queue,
        index: &'a Index,
        namespace_prefix: &'a str,
        task_id: String,
        tree_cache: &'a TreeCache,
    ) -> Result<Self> {
        let artifacts = tc_primitives::get_task_artifacts(queue, &task_id).await?;
        Ok(Self {
            queue,
            index,
            namespace_prefix,
            task_id,
            artifacts,
            tree_cache,
        })
    }
}

fn parse_json<T: for<'de> serde::Deserialize<'de>>(text: &str) -> Result<T> {
    let mut deser = serde_json::Deserializer::from_str(text);
    Ok(serde_path_to_error::deserialize(&mut deser)?)
}

#[async_trait]
impl ArtifactSource for TcSource<'_> {
    async fn changes_json(&self) -> Result<Changes> {
        let text =
            tc_primitives::fetch_artifact_text(self.queue, &self.task_id, "public/output/changes.json")
                .await?;
        parse_json(&text)
    }

    async fn fetch_tree(&self, world: &str, version: &str) -> Result<Arc<FileTree>> {
        // PR artifacts ride on the current task; everything else routes through
        // the TC Index to find the publishing task.
        let pr_artifact = format!("public/output/apworlds/{world}-{version}.apworld");
        let (source_task_id, artifact_name) = if self.artifacts.contains(&pr_artifact) {
            (self.task_id.clone(), pr_artifact)
        } else {
            let index_path = tc_primitives::index_path(self.namespace_prefix, world, version);
            let indexed_task_id = tc_primitives::find_indexed_task(self.index, &index_path)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("Version {version} of {world} not found in index")
                })?;
            let artifact_name = format!("public/{world}-{version}.apworld");
            (indexed_task_id, artifact_name)
        };

        let key = format!("{source_task_id}:{artifact_name}");

        if let Some(tree) = self
            .tree_cache
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(tree.clone());
        }

        let bytes =
            tc_primitives::fetch_artifact_bytes(self.queue, &source_task_id, &artifact_name)
                .await?;
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
        let aplint_name = format!("public/output/{world}-{version}.aplint");
        if !self.artifacts.iter().any(|a| a == &aplint_name) {
            return Ok(BTreeMap::new());
        }
        let text =
            tc_primitives::fetch_artifact_text(self.queue, &self.task_id, &aplint_name).await?;
        parse_json(&text)
    }

    async fn list_versions(&self, world: &str) -> Result<Vec<Version>> {
        Ok(
            tc_primitives::list_indexed_versions(self.index, self.namespace_prefix, world)
                .await?
                .into_iter()
                .map(|(v, _)| v)
                .collect(),
        )
    }
}
