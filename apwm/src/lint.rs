//! Checks on an index that go beyond whether it parses.
//!
//! Most of them protect lobbies from before multi-base support. They read the same files with a
//! parser that can't be changed anymore, and that parser fails quietly: it offers every release
//! in `[versions]` whatever base it needs, and it drops the `url` of an entry that has a second
//! key. The rest catch what the current parser accepts without a word, like a misspelled key.

use std::{
    fmt::Display,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use semver::Version;

use crate::{IndexSet, VersionReq, WorldDef};

const INDEX_KEYS: &[&str] = &[
    "archipelago_repo",
    "archipelago_version",
    "index_homepage",
    "index_dir",
    "bases",
];
const WORLD_KEYS: &[&str] = &[
    "name",
    "display_name",
    "default_url",
    "default_version",
    "home",
    "versions",
    "releases",
    "disabled",
    "supported",
    "tags",
    "base",
];
const VERSION_KEYS: &[&str] = &["url", "local"];
const RELEASE_KEYS: &[&str] = &["min_ap_version", "max_ap_version", "url", "local"];
const BASE_OVERRIDE_KEYS: &[&str] = &["supported", "disabled", "default_version"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Problem {
    pub severity: Severity,
    /// Relative to the directory holding `index.toml`
    pub file: PathBuf,
    pub message: String,
}

impl Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };

        write!(f, "{severity}: {}: {}", self.file.display(), self.message)
    }
}

struct Report {
    file: PathBuf,
    problems: Vec<Problem>,
}

impl Report {
    fn error(&mut self, message: String) {
        self.push(Severity::Error, message);
    }

    fn warning(&mut self, message: String) {
        self.push(Severity::Warning, message);
    }

    fn push(&mut self, severity: Severity, message: String) {
        self.problems.push(Problem {
            severity,
            file: self.file.clone(),
            message,
        });
    }

    fn unknown_keys(&mut self, table: &toml::Table, known: &[&str], location: &str) {
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                self.error(format!(
                    "`{key}` isn't a known key {location}, it would be ignored"
                ));
            }
        }
    }
}

/// Lints the index at `index_path`. An index that doesn't parse is an `Err`, not a `Problem`.
pub fn lint(index_path: &Path) -> Result<Vec<Problem>> {
    let index_set = IndexSet::new(index_path)?;
    let root = index_path.parent().context("Invalid index path")?;

    let mut problems = lint_index_file(index_path, root, &index_set)?;
    for (apworld_name, world_def) in index_set.world_defs() {
        problems.extend(lint_world(apworld_name, world_def, root, &index_set)?);
    }

    Ok(problems)
}

fn read_table(path: &Path) -> Result<toml::Table> {
    std::fs::read_to_string(path)
        .with_context(|| format!("Reading {}", path.display()))?
        .parse::<toml::Table>()
        .with_context(|| format!("Parsing {}", path.display()))
}

fn lint_index_file(index_path: &Path, root: &Path, index_set: &IndexSet) -> Result<Vec<Problem>> {
    let raw = read_table(index_path)?;
    let mut report = Report {
        file: index_path.strip_prefix(root).unwrap_or(index_path).into(),
        problems: vec![],
    };

    report.unknown_keys(&raw, INDEX_KEYS, "of the index");

    let legacy_base = index_set.legacy_base.to_string();
    if let Some(bases) = raw.get("bases").and_then(|bases| bases.as_table()) {
        if !bases.is_empty() && !bases.contains_key(&legacy_base) {
            report.warning(format!(
                "`[bases]` doesn't list {legacy_base}. It's a base anyway, since it's the `archipelago_version`"
            ));
        }
    }

    Ok(report.problems)
}

fn lint_world(
    apworld_name: &str,
    world_def: &WorldDef,
    root: &Path,
    index_set: &IndexSet,
) -> Result<Vec<Problem>> {
    let raw = read_table(&world_def.path)?;
    let legacy_base = &index_set.legacy_base;
    let mut report = Report {
        file: world_def
            .path
            .strip_prefix(root)
            .unwrap_or(&world_def.path)
            .into(),
        problems: vec![],
    };

    report.unknown_keys(&raw, WORLD_KEYS, "of a world");
    lint_versions_table(&raw, &mut report);
    lint_releases_table(&raw, world_def, legacy_base, &mut report);
    lint_base_tables(&raw, world_def, index_set, &mut report);

    for version in world_def.versions.keys() {
        let Some(release) = world_def.releases.get(version) else {
            continue;
        };
        if !release.runs_on(legacy_base) {
            report.error(format!(
                "{version} is in `[versions]`, so older lobbies offer it, but `[releases]` says it can't run on {legacy_base}. Keep it in `[releases]` only"
            ));
        }
    }

    if world_def.versions.is_empty() && !world_def.supported && !world_def.disabled {
        report.error(
            "this world has nothing in `[versions]` and isn't `supported`, older lobbies fail on a world without a release. Set `disabled = true` at the top level and enable it in a `[base.\"...\"]` table".to_string(),
        );
    }

    for base in index_set.bases() {
        let Some(world) = index_set.get(base).and_then(|i| i.worlds.get(apworld_name)) else {
            continue;
        };

        match &world.default_version {
            VersionReq::Specific(version) if !world.versions.contains_key(version) => {
                report.error(format!(
                    "the default version {version} isn't available on Archipelago {base}"
                ));
            }
            VersionReq::LatestSupported if !world.supported => {
                report.error(format!(
                    "the default version is `latest_supported` but the world isn't supported on Archipelago {base}"
                ));
            }
            _ => {}
        }
    }

    Ok(report.problems)
}

fn lint_versions_table(raw: &toml::Table, report: &mut Report) {
    let Some(versions) = raw.get("versions").and_then(|versions| versions.as_table()) else {
        return;
    };

    for (version, value) in versions {
        let Some(entry) = value.as_table() else {
            report.warning(format!(
                "{version} in `[versions]` should be a table, like `{{}}`"
            ));
            continue;
        };

        if entry.len() > 1 {
            report.error(format!(
                "{version} in `[versions]` has {} keys. Older lobbies silently drop the `url` or `local` of an entry with more than one, constraints go in `[releases]`",
                entry.len()
            ));
            continue;
        }

        for key in entry.keys() {
            if !VERSION_KEYS.contains(&key.as_str()) {
                report.error(format!(
                    "`{key}` on {version} in `[versions]` is ignored there. Constraints go in `[releases]`"
                ));
            }
        }
    }
}

fn lint_releases_table(
    raw: &toml::Table,
    world_def: &WorldDef,
    legacy_base: &Version,
    report: &mut Report,
) {
    if let Some(releases) = raw.get("releases").and_then(|releases| releases.as_table()) {
        for (version, value) in releases {
            if let Some(entry) = value.as_table() {
                report.unknown_keys(
                    entry,
                    RELEASE_KEYS,
                    &format!("of {version} in `[releases]`"),
                );
            }
        }
    }

    for (version, release) in &world_def.releases {
        if let (Some(min), Some(max)) = (&release.min_ap_version, &release.max_ap_version) {
            if min > max {
                report.error(format!(
                    "{version} in `[releases]` can't run anywhere: its minimum {min} is above its maximum {max}"
                ));
            }
        }

        let in_versions = world_def.versions.contains_key(version);
        let constrained = release.min_ap_version.is_some() || release.max_ap_version.is_some();
        let has_origin = release.url.is_some() || release.local.is_some();
        if in_versions && !constrained && !has_origin {
            report.warning(format!(
                "{version} in `[releases]` changes nothing, it's already in `[versions]`"
            ));
        }

        // A world that's disabled at the top level is hidden from older lobbies on purpose
        if !in_versions && !world_def.disabled && release.runs_on(legacy_base) {
            report.warning(format!(
                "{version} can run on {legacy_base} but is only in `[releases]`, so older lobbies won't offer it. Add it to `[versions]` too"
            ));
        }
    }
}

fn lint_base_tables(
    raw: &toml::Table,
    world_def: &WorldDef,
    index_set: &IndexSet,
    report: &mut Report,
) {
    if let Some(overrides) = raw.get("base").and_then(|overrides| overrides.as_table()) {
        for (requirement, value) in overrides {
            if let Some(entry) = value.as_table() {
                report.unknown_keys(
                    entry,
                    BASE_OVERRIDE_KEYS,
                    &format!("of `[base.\"{requirement}\"]`"),
                );
            }
        }
    }

    for (requirement, _) in &world_def.base_overrides {
        if !index_set.bases().any(|base| requirement.matches(base)) {
            report.warning(format!(
                "`[base.\"{requirement}\"]` doesn't match any base the index declares"
            ));
        }
    }
}
