pub mod lock;
pub mod world;

use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt};
use http::Uri;
use lock::IndexLock;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use std::{
    fs::{self, OpenOptions},
    io::Read,
};
use tempfile::NamedTempFile;

use world::{World, WorldDef};

/// `index.toml` as it's written
#[derive(Deserialize, Debug, Clone)]
struct IndexFile {
    #[serde(with = "http_serde::uri")]
    archipelago_repo: Uri,
    archipelago_version: Version,
    index_homepage: String,
    index_dir: PathBuf,
    #[serde(default)]
    bases: BTreeMap<Version, BaseDef>,
}

/// A `[bases."<version>"]` table. Declaring the base is all it does for now.
#[derive(Deserialize, Debug, Clone, Default)]
struct BaseDef {}

/// Every Archipelago base an index describes, each with the `Index` that rooms on it see.
#[derive(Debug, Clone)]
pub struct IndexSet {
    pub path: PathBuf,
    /// `archipelago_version` in `index.toml`. It's the only base that lobbies from before
    /// multi-base support know about, and it always has a view here.
    pub legacy_base: Version,
    bases: BTreeMap<Version, Index>,
    index_file: IndexFile,
    world_defs: BTreeMap<String, WorldDef>,
}

impl IndexSet {
    pub fn new(index_path: &Path) -> Result<Self> {
        let index_content = std::fs::read_to_string(index_path).context("Reading index.toml")?;
        let deser = toml::Deserializer::parse(&index_content)?;
        let index_file: IndexFile = serde_path_to_error::deserialize(deser)?;
        let index_dir_resolved = index_path
            .parent()
            .context("index_path doesn't have a parent")?
            .join(&index_file.index_dir);

        if !index_dir_resolved.is_dir() {
            bail!("The specified index directory isn't a directory or doesn't exist");
        }

        let mut world_defs = BTreeMap::new();
        let world_tomls = fs::read_dir(index_dir_resolved)?;
        for world_toml in world_tomls {
            let world_toml = world_toml?;
            let world_path = world_toml.path();
            let apworld_name = world_path
                .file_stem()
                .with_context(|| format!("World path {world_path:?} is invalid"))?
                .to_string_lossy();
            let world_def = WorldDef::new(&world_toml.path())
                .with_context(|| format!("Reading {}", world_path.display()))?;

            world_defs.insert(apworld_name.to_string(), world_def);
        }

        // An index without a `[bases]` table describes a single base
        let mut base_versions: Vec<Version> = index_file.bases.keys().cloned().collect();
        if !base_versions.contains(&index_file.archipelago_version) {
            base_versions.push(index_file.archipelago_version.clone());
        }

        let mut index_set = Self {
            path: index_path.into(),
            legacy_base: index_file.archipelago_version.clone(),
            bases: BTreeMap::new(),
            index_file,
            world_defs,
        };
        for base in base_versions {
            let index = index_set.view_for_base(&base)?;
            index_set.bases.insert(base, index);
        }

        Ok(index_set)
    }

    pub fn bases(&self) -> impl Iterator<Item = &Version> {
        self.bases.keys()
    }

    pub fn get(&self, base: &Version) -> Option<&Index> {
        self.bases.get(base)
    }

    pub fn legacy(&self) -> &Index {
        &self.bases[&self.legacy_base]
    }

    /// The world files as they're written, before any base is applied to them
    pub fn world_defs(&self) -> &BTreeMap<String, WorldDef> {
        &self.world_defs
    }

    /// What `base` sees of this index, whether or not the index declares that base. This is how
    /// an index gets compared with one that has a base it doesn't know about yet.
    pub fn view_for_base(&self, base: &Version) -> Result<Index> {
        let mut worlds = BTreeMap::new();
        for (apworld_name, world_def) in &self.world_defs {
            if let Some(world) = world_def.for_base(base)? {
                worlds.insert(apworld_name.clone(), world);
            }
        }

        Ok(Index {
            path: self.path.clone(),
            archipelago_repo: self.index_file.archipelago_repo.clone(),
            archipelago_version: base.clone(),
            index_homepage: self.index_file.index_homepage.clone(),
            index_dir: self.index_file.index_dir.clone(),
            worlds,
        })
    }

    /// Downloads the releases of every base. A release is the same file whichever base runs it,
    /// so they all share `destination` and the lock file.
    pub async fn refresh_into(
        &self,
        destination: &Path,
        only_new: bool,
        precise: Option<(String, Version)>,
    ) -> Result<IndexLock> {
        self.all_releases()
            .refresh_into(destination, only_new, precise)
            .await
    }

    /// An index holding the releases of every base at once, to look one up without knowing which
    /// bases have it. It isn't what any base sees.
    pub fn all_releases(&self) -> Index {
        let mut all = self.legacy().clone();
        for index in self.bases.values() {
            for (apworld_name, world) in &index.worlds {
                let merged = all
                    .worlds
                    .entry(apworld_name.clone())
                    .or_insert_with(|| world.clone());
                for (version, origin) in &world.versions {
                    merged
                        .versions
                        .entry(version.clone())
                        .or_insert_with(|| origin.clone());
                }
            }
        }

        all
    }
}

/// The worlds and releases available to rooms on one Archipelago base, `archipelago_version`
#[derive(Debug, Clone)]
pub struct Index {
    pub path: PathBuf,
    pub archipelago_repo: Uri,
    pub archipelago_version: Version,
    pub index_homepage: String,
    pub index_dir: PathBuf,
    pub worlds: BTreeMap<String, World>,
}

impl Index {
    /// The index as its legacy base sees it. See `IndexSet` for the other bases.
    pub fn new(index_path: &Path) -> Result<Self> {
        let index_set = IndexSet::new(index_path)?;

        Ok(index_set.legacy().clone())
    }

    pub async fn refresh_into(
        &self,
        destination: &Path,
        only_new: bool,
        precise: Option<(String, Version)>,
    ) -> Result<IndexLock> {
        log::info!("Refreshing index into {destination:?}");

        let parent = self.path.parent().context("Invalid index path")?;
        let lock_toml = parent.join("index.lock");
        let old_lock = IndexLock::new(&lock_toml)?;
        let mut new_lock = IndexLock::default();
        new_lock.path = old_lock.path.clone();
        if destination.is_file() {
            bail!("Error downloading, destination exists and is a file");
        }

        std::fs::create_dir_all(destination)?;

        struct DownloadTask {
            apworld_name: String,
            version: Version,
            world: World,
            destination_path: PathBuf,
            expected_checksum: Option<String>,
        }

        let mut download_tasks = Vec::new();

        for (apworld_name, world) in &self.worlds {
            for (version, origin) in &world.versions {
                log::debug!("Refreshing world: {apworld_name}, version: {version}");
                if let Some((ref target_world, ref target_version)) = precise {
                    if apworld_name != target_world {
                        log::debug!("Ignoring world because of precise requirement");
                        continue;
                    }
                    if version != target_version {
                        log::debug!("Ignoring version because of precise requirement");
                        continue;
                    }
                }

                if world.disabled {
                    log::debug!("World is disabled, ignoring");
                    continue;
                }

                if origin.is_supported() {
                    log::debug!("World is supported, skipping");
                    continue;
                }

                let apworld_destination_path =
                    self.get_world_local_path(destination, apworld_name, version);
                let expected_checksum = old_lock.get_checksum(apworld_name, version);

                if let Some(ref expected_checksum) = expected_checksum {
                    if only_new {
                        log::debug!(
                            "World exists in lockfile and we only want to refresh new ones, ignoring."
                        );
                        new_lock.set_checksum(apworld_name, version, expected_checksum);
                        continue;
                    }
                }

                if apworld_destination_path.is_file() {
                    let mut apworld_destination = OpenOptions::new()
                        .read(true)
                        .open(&apworld_destination_path)?;
                    let mut buf = Vec::new();
                    apworld_destination.read_to_end(&mut buf)?;
                    let current_checksum = format!("{:x}", Sha256::digest(&buf));
                    if expected_checksum == Some(current_checksum.clone()) {
                        log::debug!("World exists in lockfile and on disk, checksums are matching, ignoring");
                        new_lock.set_checksum(apworld_name, version, &current_checksum);
                        continue;
                    }

                    if expected_checksum.is_none() {
                        log::debug!("World exists in index but not in lockfile, continuing.");
                    }
                }

                download_tasks.push(DownloadTask {
                    apworld_name: apworld_name.clone(),
                    version: version.clone(),
                    world: world.clone(),
                    destination_path: apworld_destination_path,
                    expected_checksum,
                });
            }
        }

        let mut stream = stream::iter(download_tasks)
            .map(|task| async move {
                let apworld_name = task.apworld_name.clone();
                let version = task.version.clone();
                log::debug!("Copying world {apworld_name} into worlds folder.");
                let result: Result<String> = async move {
                    let parent_dir = task
                        .destination_path
                        .parent()
                        .context("destination has no parent directory")?;
                    let tempfile = NamedTempFile::new_in(parent_dir)?;
                    let checksum = task
                        .world
                        .copy_to(&task.version, tempfile.as_file(), task.expected_checksum)
                        .await?;
                    tempfile
                        .persist(&task.destination_path)
                        .map_err(|e| e.error)?;
                    Ok(checksum)
                }
                .await;
                (apworld_name, version, result)
            })
            .buffer_unordered(10);

        while let Some((apworld_name, version, result)) = stream.next().await {
            match result {
                Ok(checksum) => {
                    new_lock.set_checksum(&apworld_name, &version, &checksum);
                }
                Err(e) => {
                    log::error!(
                        "\x1b[1;31m!!! SKIPPING APWORLD {apworld_name} v{version}: {e:#} !!!\x1b[0m"
                    );
                    log::error!(
                        "\x1b[1;31m!!! This world will not be available until the index entry is fixed. !!!\x1b[0m"
                    );
                }
            }
        }

        Ok(new_lock)
    }

    pub fn get_world_local_path(
        &self,
        apworld_root: &Path,
        apworld_name: &str,
        version: &Version,
    ) -> PathBuf {
        apworld_root.join(format!("{apworld_name}-{version}.apworld"))
    }

    pub fn get_world_by_name(&self, game_name: &str) -> Option<&World> {
        self.worlds.values().find(|game| game.name == game_name)
    }
}
