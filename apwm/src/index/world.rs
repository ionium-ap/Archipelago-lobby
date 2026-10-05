use crate::{
    utils::{de, git_clone_shallow},
    IndexLock, VersionReq,
};
use anyhow::{bail, Context, Result};
use http::Uri;
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tempfile::{tempdir, TempDir};

struct AllHosts;

impl PartialEq<&str> for AllHosts {
    fn eq(&self, _: &&str) -> bool {
        true
    }
}

fn retry_client() -> Client {
    Client::builder()
        .retry(
            reqwest::retry::for_host(AllHosts)
                .max_retries_per_request(3)
                .classify_fn(|req_rep| {
                    if req_rep.error().is_some() {
                        return req_rep.retryable();
                    }
                    if req_rep.status().is_some_and(|s| s.is_server_error()) {
                        return req_rep.retryable();
                    }
                    req_rep.success()
                }),
        )
        .build()
        .expect("Failed to build HTTP client")
}

#[derive(Deserialize, Debug, PartialEq, Default, Clone)]
pub enum WorldOrigin {
    #[serde(rename = "url")]
    Url(#[serde(with = "http_serde::uri")] Uri),
    #[serde(rename = "local")]
    Local(PathBuf),
    Supported,
    #[default]
    Default,
}

impl WorldOrigin {
    pub fn is_local(&self) -> bool {
        matches!(self, WorldOrigin::Local(_))
    }

    pub fn is_supported(&self) -> bool {
        matches!(self, WorldOrigin::Supported)
    }

    // TODO: Add support for patching
    pub fn has_patches(&self) -> bool {
        false
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WorldTag {
    #[serde(rename = "ad")]
    AfterDark,
}

static AP_CACHE: OnceLock<TempDir> = OnceLock::new();

/// A release declared in `[releases]`: which Archipelago bases can run it and, optionally, where
/// to get it. Lobbies from before multi-base support don't read that table.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct Release {
    #[serde(default)]
    pub min_ap_version: Option<Version>,
    #[serde(default)]
    pub max_ap_version: Option<Version>,
    #[serde(with = "http_serde::option::uri", default)]
    pub url: Option<Uri>,
    #[serde(default)]
    pub local: Option<PathBuf>,
}

impl Release {
    /// Both bounds are inclusive, and a missing bound doesn't constrain anything.
    pub fn runs_on(&self, base: &Version) -> bool {
        self.min_ap_version.as_ref().is_none_or(|min| min <= base)
            && self.max_ap_version.as_ref().is_none_or(|max| base <= max)
    }

    fn origin(&self) -> Option<WorldOrigin> {
        if let Some(url) = &self.url {
            return Some(WorldOrigin::Url(url.clone()));
        }

        self.local.clone().map(WorldOrigin::Local)
    }
}

/// What a `[base."<requirement>"]` table changes for the bases matching its requirement.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct BaseOverride {
    #[serde(default)]
    pub supported: Option<bool>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(deserialize_with = "de::option_version_req_external", default)]
    pub default_version: Option<VersionReq>,
}

/// A world file as it's written in the index. It describes the world for every Archipelago base
/// at once, `for_base` turns it into the `World` that a given base sees.
#[derive(Deserialize, Debug, Clone)]
pub struct WorldDef {
    #[serde(skip)]
    pub path: PathBuf,
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(with = "http_serde::option::uri", default)]
    pub default_url: Option<Uri>,
    #[serde(deserialize_with = "de::version_req_external", default)]
    pub default_version: VersionReq,
    #[serde(deserialize_with = "de::empty_string_as_none", default)]
    pub home: Option<String>,
    #[serde(deserialize_with = "de::map_with_default_value", default)]
    pub versions: BTreeMap<Version, WorldOrigin>,
    #[serde(default)]
    pub releases: BTreeMap<Version, Release>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub supported: bool,
    #[serde(default)]
    pub tags: Vec<WorldTag>,
    #[serde(default, rename = "base")]
    raw_base_overrides: BTreeMap<String, BaseOverride>,
    #[serde(skip)]
    pub base_overrides: Vec<(semver::VersionReq, BaseOverride)>,
}

impl WorldDef {
    pub fn new(world_path: &Path) -> Result<Self> {
        let world_content = std::fs::read_to_string(world_path)?;
        let deser = toml::Deserializer::parse(&world_content)?;
        let mut world: Self = serde_path_to_error::deserialize(deser)?;
        world.path = world_path.into();
        if world.display_name.is_empty() {
            world.display_name = world.name.clone();
        }

        for (requirement, base_override) in std::mem::take(&mut world.raw_base_overrides) {
            let parsed = semver::VersionReq::parse(&requirement).with_context(|| {
                format!(
                    "World {}: `{requirement}` in [base] isn't a version requirement",
                    world.name
                )
            })?;
            world.base_overrides.push((parsed, base_override));
        }

        for (version, release) in &world.releases {
            if release.url.is_some() && release.local.is_some() {
                bail!(
                    "World {}: release {version} has both a `url` and a `local` path",
                    world.name
                );
            }
        }

        Ok(world)
    }

    fn override_for(&self, base: &Version) -> Result<Option<&BaseOverride>> {
        let mut matching = self
            .base_overrides
            .iter()
            .filter(|(requirement, _)| requirement.matches(base));

        let Some((first, base_override)) = matching.next() else {
            return Ok(None);
        };
        if let Some((second, _)) = matching.next() {
            bail!(
                "World {}: both [base.\"{first}\"] and [base.\"{second}\"] apply to Archipelago {base}",
                self.name
            );
        }

        Ok(Some(base_override))
    }

    /// The world as lobbies see it for rooms on `base`, or `None` if that base doesn't have it:
    /// either because it's disabled there, or because none of its releases can run on it.
    pub fn for_base(&self, base: &Version) -> Result<Option<World>> {
        let base_override = self.override_for(base)?;

        let disabled = base_override
            .and_then(|o| o.disabled)
            .unwrap_or(self.disabled);
        if disabled {
            return Ok(None);
        }

        let supported = base_override
            .and_then(|o| o.supported)
            .unwrap_or(self.supported);
        let default_version = base_override
            .and_then(|o| o.default_version.clone())
            .unwrap_or_else(|| self.default_version.clone());

        let mut versions = BTreeMap::new();
        for (version, origin) in &self.versions {
            let runs_on_base = self.releases.get(version).is_none_or(|r| r.runs_on(base));
            if runs_on_base {
                versions.insert(version.clone(), origin.clone());
            }
        }
        for (version, release) in &self.releases {
            if !release.runs_on(base) {
                continue;
            }

            // A release that's also in `[versions]` keeps the origin it has there unless
            // `[releases]` gives it one
            match release.origin() {
                Some(origin) => {
                    versions.insert(version.clone(), origin);
                }
                None => {
                    versions.entry(version.clone()).or_default();
                }
            }
        }

        if supported {
            versions.insert(base.clone(), WorldOrigin::Supported);
        }
        if versions.is_empty() {
            return Ok(None);
        }

        Ok(Some(World {
            path: self.path.clone(),
            name: self.name.clone(),
            display_name: self.display_name.clone(),
            default_url: self.default_url.clone(),
            default_version,
            home: self.home.clone(),
            versions,
            disabled,
            supported,
            tags: self.tags.clone(),
        }))
    }
}

/// A world as one Archipelago base sees it. Built by `WorldDef::for_base`.
#[derive(Debug, Clone)]
pub struct World {
    pub path: PathBuf,
    pub name: String,
    pub display_name: String,
    pub default_url: Option<Uri>,
    pub default_version: VersionReq,
    pub home: Option<String>,
    pub versions: BTreeMap<Version, WorldOrigin>,
    pub disabled: bool,
    pub supported: bool,
    pub tags: Vec<WorldTag>,
}

impl World {
    pub fn get_latest_release(&self) -> Option<(&Version, &WorldOrigin)> {
        self.versions.iter().max_by_key(|p| p.0)
    }

    pub fn get_latest_supported_release(&self) -> Option<(&Version, &WorldOrigin)> {
        self.versions
            .iter()
            .find(|(_, origin)| origin.is_supported())
    }

    pub fn get_version(&self, version: &Version) -> Option<&WorldOrigin> {
        self.versions.get(version)
    }

    pub async fn copy_to(
        &self,
        version: &Version,
        mut destination: &File,
        expected_checksum: Option<String>,
    ) -> Result<String> {
        let url = self.get_url_for_version(version)?;

        let origin = self.versions.get(version).with_context(|| {
            format!("Unable to find version {} for world {}", self.name, version)
        })?;
        match origin {
            WorldOrigin::Default | WorldOrigin::Url(_) => {
                self.download_to(&url, destination, expected_checksum).await
            }
            WorldOrigin::Local(_) => {
                let full_path = self.get_path_for_origin(origin)?;
                let mut src = std::fs::File::open(&full_path)?;
                let mut buf = Vec::new();
                src.read_to_end(&mut buf)?;
                let checksum = format!("{:x}", Sha256::digest(&buf));
                if expected_checksum.is_some() && Some(&checksum) != expected_checksum.as_ref() {
                    bail!("Error while copying apworld {:?}. Checksum didn't match what was expected.", full_path);
                }

                destination.write_all(&buf[..])?;

                Ok(checksum)
            }
            WorldOrigin::Supported => Ok("none".into()),
        }
    }

    pub async fn extract_to(
        &self,
        version: &Version,
        destination: &Path,
        ap_index_url: &str,
        ap_index_ref: &str,
        lock_file: &IndexLock,
    ) -> Result<String> {
        let origin = self.versions.get(version).with_context(|| {
            format!("Unable to find version {} for world {}", version, self.name)
        })?;

        if origin.is_supported() {
            let ap_cache = AP_CACHE.get_or_init(|| {
                let cache = tempdir().unwrap();
                git_clone_shallow(ap_index_url, ap_index_ref, cache.path()).unwrap();
                cache
            });

            crate::utils::copy_dir_all(
                &ap_cache.path().join("worlds").join(self.get_ap_name()?),
                &destination.join(self.get_ap_name()?),
            )?;

            return Ok("supported".to_string());
        }

        let download_dir = tempdir()?;
        let apworld_path = download_dir.path().join("apworld");
        let apworld_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&apworld_path)?;

        let apworld_name = self.get_ap_name()?;
        let expected_checksum = lock_file.get_checksum(&apworld_name, version);
        let checksum = self
            .copy_to(version, &apworld_file, expected_checksum)
            .await?;

        let mut archive = zip::ZipArchive::new(File::open(apworld_path)?)?;
        archive.extract(destination)?;

        Ok(checksum)
    }

    pub fn get_ap_name(&self) -> Result<String> {
        Ok(self
            .path
            .file_stem()
            .context("Invalid path for world")?
            .to_string_lossy()
            .to_string())
    }

    pub fn get_url_for_version(&self, version: &Version) -> Result<String> {
        let origin = self.versions.get(version).with_context(|| {
            format!("Unable to find version {} for world {}", self.name, version)
        })?;

        match origin {
            WorldOrigin::Default => {
                let url = self.default_url.as_ref().with_context(|| {
                    format!(
                        "World {} has no default URL but contains a release ({}) without a set URL",
                        self.name, version
                    )
                })?;
                let url = url.to_string().replace("{{version}}", &version.to_string());
                Ok(url)
            }
            WorldOrigin::Url(url) => {
                let url = url.to_string().replace("{{version}}", &version.to_string());
                Ok(url)
            }
            WorldOrigin::Local(_) => Ok("".into()),
            WorldOrigin::Supported => Ok("https://archipelago.gg/games".into()),
        }
    }

    pub fn get_path_for_origin(&self, origin: &WorldOrigin) -> Result<PathBuf> {
        match origin {
            WorldOrigin::Local(path) => {
                let full_path = self.path.parent().context("Invalid world path")?.join(path);
                Ok(full_path)
            }
            _ => bail!("This isn't a local world origin, no path"),
        }
    }

    async fn download_to(
        &self,
        uri: &str,
        mut destination: &File,
        expected_checksum: Option<String>,
    ) -> Result<String> {
        let client = retry_client();
        let req = client.get(uri).send().await?.error_for_status()?;
        let body = req.bytes().await?;
        let checksum = format!("{:x}", Sha256::digest(&body));
        if expected_checksum.is_some() && Some(&checksum) != expected_checksum.as_ref() {
            bail!(
                "Error while downloading apworld from {}. Checksum didn't match what was expected.",
                uri
            );
        }

        destination.write_all(&body)?;
        Ok(checksum)
    }
}
