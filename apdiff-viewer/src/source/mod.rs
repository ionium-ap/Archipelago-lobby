//! Abstraction over where apworld artifacts come from.
//!
//! `process_world` in `main.rs` was originally hardwired to fetch bytes from
//! Taskcluster. The takeover requires it to also serve diffs sourced from
//! POST-uploaded submissions stored in postgres + a local blob store. This
//! module defines the trait both backends implement.
//!
//! All caching of parsed file trees lives inside each implementation; callers
//! treat the trait as a transparent byte source.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use apwm::changes::Changes;
use async_trait::async_trait;
use semver::Version;

use crate::apworld::FileTree;
use crate::diff::Annotations;

pub mod tc;

/// Apworld name conventionally used as the parent of `manual_*` worlds.
/// Manual worlds inherit a base tree from this apworld; the diff page lets
/// reviewers pick a `base manual` version as the from-version.
pub const BASE_MANUAL: &str = "manual_ultimatemarvelvscapcom3_manualteam";

#[async_trait]
pub trait ArtifactSource: Send + Sync {
    /// The `changes.json` describing what this source represents
    /// (added / removed versions per apworld).
    async fn changes_json(&self) -> Result<Changes>;

    /// Extracted file tree of `(world, version)`. Each implementation is
    /// responsible for caching parsed trees so callers can request the same
    /// pair multiple times cheaply.
    async fn fetch_tree(&self, world: &str, version: &str) -> Result<Arc<FileTree>>;

    /// Lint annotations bundled with `(world, version)`. Empty map when the
    /// source has no annotations for that pair.
    async fn annotations(
        &self,
        world: &str,
        version: &str,
    ) -> Result<BTreeMap<String, Vec<Annotations>>>;

    /// All versions of `world` known to this source, ascending by semver.
    /// Used to populate the from-version dropdown — caller is responsible
    /// for filtering out versions that are themselves the diff's "to" side.
    async fn list_versions(&self, world: &str) -> Result<Vec<Version>>;
}
