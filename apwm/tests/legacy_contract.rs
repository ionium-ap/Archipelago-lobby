//! What a lobby from before multi-base support sees when it reads an index written in the
//! multi-base format.
//!
//! The fixture in `fixtures/multi_base` uses every addition the format makes: `[bases]` in
//! `index.toml`, `[releases]` and `[base.<req>]` in world files, and lock entries for releases
//! that only `[releases]` declares. Lobbies running the older code keep reading the same index,
//! so all of that has to leave them with a plain, correct 0.6.7 view.
//!
//! The tests at the end pin the parser behaviors the format works around. They are the reason a
//! constraint cannot simply be added to a `[versions]` entry.
//!
//! All of this first passed against the parser from before multi-base support, unchanged. That
//! parser is gone from this crate: `Index::new` now returns the legacy base's view of an
//! `IndexSet`, so these tests check that this view is what the older parser produced. The
//! older parser itself is run against the real index by the index's CI.

use std::path::{Path, PathBuf};

use apwm::{Index, IndexLock, Manifest, VersionReq, WorldOrigin};
use semver::Version;
use tempfile::TempDir;

const LEGACY_INDEX_TOML: &str = r#"
archipelago_repo = "https://github.com/ArchipelagoMW/Archipelago.git"
archipelago_version = "0.6.7"
index_homepage = "https://example.invalid/index"
index_dir = "index"
"#;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multi_base")
}

fn fixture_index() -> Index {
    Index::new(&fixture_root().join("index.toml")).expect("The fixture index should parse")
}

fn version(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn version_keys(index: &Index, apworld_name: &str) -> Vec<String> {
    index.worlds[apworld_name]
        .versions
        .keys()
        .map(|v| v.to_string())
        .collect()
}

/// Writes an index with the given `(apworld name, world toml)` pairs to a temporary directory.
fn write_index(index_toml: &str, worlds: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("index.toml"), index_toml).unwrap();
    std::fs::create_dir(dir.path().join("index")).unwrap();
    for (apworld_name, content) in worlds {
        std::fs::write(
            dir.path()
                .join("index")
                .join(format!("{apworld_name}.toml")),
            content,
        )
        .unwrap();
    }

    dir
}

#[test]
fn test_base_is_the_legacy_version() {
    let index = fixture_index();

    assert_eq!(index.archipelago_version, version("0.6.7"));
}

#[test]
fn test_only_legacy_worlds_are_listed() {
    let index = fixture_index();

    let worlds: Vec<&str> = index.worlds.keys().map(String::as_str).collect();
    assert_eq!(
        worlds,
        vec![
            "after_dark",
            "core",
            "custom",
            "graduated",
            "left_core",
            "origins"
        ],
        "Worlds that only exist from 0.6.8 on, and disabled worlds, should not be listed"
    );
}

#[test]
fn test_core_worlds_have_only_the_legacy_version() {
    let index = fixture_index();

    for apworld_name in ["core", "left_core"] {
        let versions: Vec<_> = index.worlds[apworld_name].versions.iter().collect();
        assert_eq!(
            versions,
            vec![(&version("0.6.7"), &WorldOrigin::Supported)],
            "{apworld_name} should be core at 0.6.7 and nothing else"
        );
    }
}

#[test]
fn test_releases_table_is_not_read() {
    let index = fixture_index();

    assert_eq!(version_keys(&index, "custom"), vec!["1.1.0", "1.1.1"]);
    assert_eq!(version_keys(&index, "origins"), vec!["1.0.0", "1.0.1"]);
    assert_eq!(version_keys(&index, "graduated"), vec!["2.1.5", "2.1.7"]);
}

#[test]
fn test_base_overrides_are_not_read() {
    let index = fixture_index();

    let graduated = &index.worlds["graduated"];
    assert!(
        !graduated.supported,
        "A world that enters core in 0.6.8 should stay a custom world at 0.6.7"
    );
    assert_eq!(graduated.default_version, VersionReq::Latest);

    assert!(index.worlds["left_core"].supported);
}

#[test]
fn test_version_origins_are_preserved() {
    let index = fixture_index();

    let origins = &index.worlds["origins"];
    assert_eq!(
        origins.versions[&version("1.0.0")],
        WorldOrigin::Url(
            "https://example.invalid/origins-1.0.0.apworld"
                .parse()
                .unwrap()
        )
    );
    assert_eq!(
        origins.versions[&version("1.0.1")],
        WorldOrigin::Local(PathBuf::from("../apworlds/origins-1.0.1.apworld"))
    );

    let custom = &index.worlds["custom"];
    assert_eq!(custom.versions[&version("1.1.0")], WorldOrigin::Default);
    assert_eq!(
        custom.get_url_for_version(&version("1.1.1")).unwrap(),
        "https://example.invalid/custom-1.1.1/custom.apworld"
    );
}

#[test]
fn test_lock_with_release_only_entries_parses() {
    let lock =
        IndexLock::new(&fixture_root().join("index.lock")).expect("The fixture lock should parse");

    assert!(lock.get_checksum("custom", &version("1.1.1")).is_some());
    assert!(
        lock.get_checksum("custom", &version("1.2.0")).is_some(),
        "An entry for a release that is only in [releases] should be carried along"
    );
    assert!(lock.get_checksum("new_custom", &version("1.0.0")).is_some());
}

#[test]
fn test_default_manifest_resolves_every_world() {
    let index = fixture_index();

    let manifest = Manifest::from_index_with_default_versions(&index)
        .expect("Every listed world should have a release");
    let (resolved, errors) = manifest.resolve_with(&index);
    assert!(errors.is_empty(), "Unexpected resolve errors: {errors:?}");

    let resolved: Vec<(&str, String)> = resolved
        .iter()
        .map(|(name, (_, version))| (name.as_str(), version.to_string()))
        .collect();
    assert_eq!(
        resolved,
        vec![
            ("after_dark", "1.0.0".to_string()),
            ("core", "0.6.7".to_string()),
            ("custom", "1.1.1".to_string()),
            ("graduated", "2.1.7".to_string()),
            ("left_core", "0.6.7".to_string()),
            ("origins", "1.0.1".to_string()),
        ]
    );
}

#[test]
fn test_freeze_pins_legacy_versions() {
    let index = fixture_index();

    let mut manifest = Manifest::from_index_with_default_versions(&index).unwrap();
    manifest.freeze(&index).unwrap();

    assert_eq!(
        manifest.get_version_req("custom", &index),
        VersionReq::Specific(version("1.1.1"))
    );
    assert_eq!(
        manifest.get_version_req("core", &index),
        VersionReq::Specific(version("0.6.7"))
    );
}

#[test]
fn test_second_key_in_versions_entry_drops_the_url() {
    let dir = write_index(
        LEGACY_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"

[versions]
"1.0.0" = { url = "https://example.invalid/world-1.0.0.apworld", min_ap_version = "0.6.8" }
"#,
        )],
    );

    let index = Index::new(&dir.path().join("index.toml")).unwrap();

    assert_eq!(
        index.worlds["world"].versions[&version("1.0.0")],
        WorldOrigin::Default,
        "The url is silently lost, which is why [versions] entries stay single-key"
    );
}

#[test]
fn test_versions_entry_cannot_be_hidden() {
    let dir = write_index(
        LEGACY_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.1.0" = {}
"1.2.0" = { min_ap_version = "0.6.8" }
"#,
        )],
    );

    let index = Index::new(&dir.path().join("index.toml")).unwrap();

    let (latest, _) = index.worlds["world"].get_latest_release().unwrap();
    assert_eq!(
        latest,
        &version("1.2.0"),
        "A release that needs a newer base is still the latest one, which is why it goes in [releases]"
    );
}

/// The older parser listed this world with no release at all, and then failed to build a
/// default manifest. That is why such a world has to be disabled at the top level.
#[test]
fn test_world_without_a_release_on_the_base_is_not_listed() {
    let dir = write_index(
        LEGACY_INDEX_TOML,
        &[
            (
                "good",
                r#"
name = "Good World"
supported = true
"#,
            ),
            (
                "world",
                r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[releases]
"1.0.0" = { min_ap_version = "0.6.8" }
"#,
            ),
        ],
    );

    let index = Index::new(&dir.path().join("index.toml")).unwrap();

    assert!(!index.worlds.contains_key("world"));
    assert!(Manifest::from_index_with_default_versions(&index).is_ok());
}

#[test]
fn test_unknown_tag_fails_the_whole_index() {
    let dir = write_index(
        LEGACY_INDEX_TOML,
        &[
            (
                "good",
                r#"
name = "Good World"
supported = true
"#,
            ),
            (
                "tagged",
                r#"
name = "Tagged World"
supported = true
tags = ["ad", "something_new"]
"#,
            ),
        ],
    );

    assert!(Index::new(&dir.path().join("index.toml")).is_err());
}

#[test]
fn test_directory_in_index_dir_fails_the_whole_index() {
    let dir = write_index(
        LEGACY_INDEX_TOML,
        &[(
            "good",
            r#"
name = "Good World"
supported = true
"#,
        )],
    );
    std::fs::create_dir(dir.path().join("index").join("0.6.8")).unwrap();

    assert!(Index::new(&dir.path().join("index.toml")).is_err());
}

#[test]
fn test_archipelago_version_must_be_a_single_version() {
    let index_toml = LEGACY_INDEX_TOML.replace(
        r#"archipelago_version = "0.6.7""#,
        r#"archipelago_version = ["0.6.7", "0.6.8"]"#,
    );
    let dir = write_index(&index_toml, &[]);

    assert!(Index::new(&dir.path().join("index.toml")).is_err());
}

#[test]
fn test_lock_accepts_only_checksum_tables() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("index.lock");
    std::fs::write(
        &lock_path,
        r#"
format = 2

[world]
"1.0.0" = "1111111111111111111111111111111111111111111111111111111111111111"
"#,
    )
    .unwrap();

    assert!(IndexLock::new(&lock_path).is_err());
}
