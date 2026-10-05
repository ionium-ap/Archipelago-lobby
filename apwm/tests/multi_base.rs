//! What each Archipelago base sees of an index that describes several of them.
//!
//! Uses the same fixture as `legacy_contract.rs`, which covers the legacy base's view.

use std::path::{Path, PathBuf};

use apwm::changes::{compute_changes_between, Changes};
use apwm::{Index, IndexSet, Manifest, VersionReq, WorldOrigin};
use semver::Version;
use tempfile::TempDir;

const TWO_BASES_INDEX_TOML: &str = r#"
archipelago_repo = "https://github.com/ArchipelagoMW/Archipelago.git"
archipelago_version = "0.6.7"
index_homepage = "https://example.invalid/index"
index_dir = "index"

[bases."0.6.7"]
[bases."0.6.8"]
"#;

fn fixture_set() -> IndexSet {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multi_base");
    IndexSet::new(&root.join("index.toml")).expect("The fixture index should parse")
}

fn version(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn view<'a>(set: &'a IndexSet, base: &str) -> &'a Index {
    set.get(&version(base))
        .unwrap_or_else(|| panic!("The index should describe base {base}"))
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

fn parse(dir: &TempDir) -> anyhow::Result<IndexSet> {
    IndexSet::new(&dir.path().join("index.toml"))
}

#[test]
fn test_bases_are_listed() {
    let set = fixture_set();

    let bases: Vec<String> = set.bases().map(|b| b.to_string()).collect();
    assert_eq!(bases, vec!["0.6.7", "0.6.8"]);
    assert_eq!(set.legacy_base, version("0.6.7"));
    assert_eq!(set.legacy().archipelago_version, version("0.6.7"));
    assert_eq!(
        view(&set, "0.6.8").archipelago_version,
        version("0.6.8"),
        "Each view should report the base it was built for"
    );
}

#[test]
fn test_each_base_has_its_own_world_list() {
    let set = fixture_set();

    let worlds: Vec<&str> = view(&set, "0.6.8")
        .worlds
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        worlds,
        vec![
            "after_dark",
            "core",
            "custom",
            "graduated",
            "new_core",
            "new_custom",
            "origins"
        ],
        "0.6.8 gains the worlds that are new there and loses the one that left core"
    );
    assert!(!view(&set, "0.6.7").worlds.contains_key("new_core"));
    assert!(!view(&set, "0.6.7").worlds.contains_key("new_custom"));
    assert!(view(&set, "0.6.7").worlds.contains_key("left_core"));
}

#[test]
fn test_core_worlds_take_the_base_as_their_version() {
    let set = fixture_set();

    for apworld_name in ["core", "new_core"] {
        let versions: Vec<_> = view(&set, "0.6.8").worlds[apworld_name]
            .versions
            .iter()
            .collect();
        assert_eq!(
            versions,
            vec![(&version("0.6.8"), &WorldOrigin::Supported)],
            "{apworld_name} should be core at 0.6.8 and nothing else"
        );
    }
}

#[test]
fn test_releases_are_filtered_by_base() {
    let set = fixture_set();

    assert_eq!(
        version_keys(view(&set, "0.6.7"), "custom"),
        vec!["1.1.0", "1.1.1"]
    );
    assert_eq!(
        version_keys(view(&set, "0.6.8"), "custom"),
        vec!["1.1.1", "1.2.0"],
        "1.1.0 stops at 0.6.7, 1.2.0 starts at 0.6.8 and 1.1.1 has no constraint"
    );
    assert_eq!(
        version_keys(view(&set, "0.6.8"), "new_custom"),
        vec!["1.0.0"]
    );
}

#[test]
fn test_releases_carry_their_own_origin() {
    let set = fixture_set();

    let origins = &view(&set, "0.6.8").worlds["origins"];
    assert_eq!(
        version_keys(view(&set, "0.6.8"), "origins"),
        vec!["1.0.0", "1.0.1", "2.0.0", "2.0.1"]
    );
    assert_eq!(
        origins.versions[&version("2.0.0")],
        WorldOrigin::Url(
            "https://example.invalid/origins-2.0.0.apworld"
                .parse()
                .unwrap()
        )
    );
    assert_eq!(
        origins.versions[&version("2.0.1")],
        WorldOrigin::Local(PathBuf::from("../apworlds/origins-2.0.1.apworld"))
    );
    assert_eq!(
        view(&set, "0.6.8").worlds["custom"].versions[&version("1.2.0")],
        WorldOrigin::Default,
        "A release without an origin of its own uses the world's default_url"
    );
}

#[test]
fn test_world_entering_core_switches_to_the_core_copy() {
    let set = fixture_set();

    let graduated = &view(&set, "0.6.8").worlds["graduated"];
    assert!(graduated.supported);
    assert_eq!(graduated.default_version, VersionReq::LatestSupported);
    let versions: Vec<_> = graduated.versions.iter().collect();
    assert_eq!(
        versions,
        vec![(&version("0.6.8"), &WorldOrigin::Supported)],
        "The custom releases stop at 0.6.7"
    );
}

#[test]
fn test_default_manifest_resolves_on_every_base() {
    let set = fixture_set();
    let index = view(&set, "0.6.8");

    let manifest = Manifest::from_index_with_default_versions(index)
        .expect("Every listed world should have a release");
    let (resolved, errors) = manifest.resolve_with(index);
    assert!(errors.is_empty(), "Unexpected resolve errors: {errors:?}");

    let resolved: Vec<(&str, String)> = resolved
        .iter()
        .map(|(name, (_, version))| (name.as_str(), version.to_string()))
        .collect();
    assert_eq!(
        resolved,
        vec![
            ("after_dark", "1.0.0".to_string()),
            ("core", "0.6.8".to_string()),
            ("custom", "1.2.0".to_string()),
            ("graduated", "0.6.8".to_string()),
            ("new_core", "0.6.8".to_string()),
            ("new_custom", "1.0.0".to_string()),
            ("origins", "2.0.1".to_string()),
        ]
    );
}

#[test]
fn test_index_without_bases_table_has_one_base() {
    let index_toml = TWO_BASES_INDEX_TOML.split("[bases").next().unwrap();
    let dir = write_index(
        index_toml,
        &[(
            "world",
            r#"
name = "World"
supported = true
"#,
        )],
    );

    let set = parse(&dir).unwrap();

    let bases: Vec<String> = set.bases().map(|b| b.to_string()).collect();
    assert_eq!(bases, vec!["0.6.7"]);
}

#[test]
fn test_legacy_base_is_always_a_base() {
    let index_toml = TWO_BASES_INDEX_TOML.replace("[bases.\"0.6.7\"]\n", "");
    let dir = write_index(&index_toml, &[]);

    let set = parse(&dir).unwrap();

    let bases: Vec<String> = set.bases().map(|b| b.to_string()).collect();
    assert_eq!(bases, vec!["0.6.7", "0.6.8"]);
}

#[test]
fn test_release_in_both_tables_keeps_its_versions_origin() {
    let dir = write_index(
        TWO_BASES_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"

[versions]
"1.0.0" = { url = "https://example.invalid/world-1.0.0.apworld" }
"1.1.0" = { url = "https://example.invalid/world-1.1.0.apworld" }

[releases]
"1.0.0" = { max_ap_version = "0.6.7" }
"1.1.0" = { url = "https://example.invalid/moved/world-1.1.0.apworld" }
"#,
        )],
    );

    let set = parse(&dir).unwrap();

    assert_eq!(
        view(&set, "0.6.7").worlds["world"].versions[&version("1.0.0")],
        WorldOrigin::Url(
            "https://example.invalid/world-1.0.0.apworld"
                .parse()
                .unwrap()
        )
    );
    assert_eq!(version_keys(view(&set, "0.6.8"), "world"), vec!["1.1.0"]);
    assert_eq!(
        view(&set, "0.6.8").worlds["world"].versions[&version("1.1.0")],
        WorldOrigin::Url(
            "https://example.invalid/moved/world-1.1.0.apworld"
                .parse()
                .unwrap()
        ),
        "An origin given in [releases] should win over the one in [versions]"
    );
}

#[test]
fn test_overlapping_base_overrides_are_an_error() {
    let dir = write_index(
        TWO_BASES_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"
supported = true

[base.">=0.6.8"]
disabled = true

[base."<0.7.0"]
supported = false
"#,
        )],
    );

    let error = parse(&dir).unwrap_err().to_string();
    assert!(
        error.contains("World") && error.contains("0.6.8"),
        "The error should name the world and the base: {error}"
    );
}

#[test]
fn test_invalid_base_requirement_is_an_error() {
    let dir = write_index(
        TWO_BASES_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"
supported = true

[base."newer"]
disabled = true
"#,
        )],
    );

    assert!(parse(&dir).is_err());
}

#[test]
fn test_release_with_url_and_local_is_an_error() {
    let dir = write_index(
        TWO_BASES_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"

[releases]
"1.0.0" = { url = "https://example.invalid/world.apworld", local = "../apworlds/world.apworld" }
"#,
        )],
    );

    assert!(parse(&dir).is_err());
}

#[test]
fn test_malformed_release_is_an_error() {
    let dir = write_index(
        TWO_BASES_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[releases]
"1.0.0" = { min_ap_version = "the next one" }
"#,
        )],
    );

    assert!(
        parse(&dir).is_err(),
        "Unlike [versions], a [releases] entry that doesn't parse shouldn't be accepted silently"
    );
}

const CUSTOM_WORLD: &str = r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = {}
"#;

const CORE_WORLD: &str = r#"
name = "Core World"
supported = true
"#;

fn changes_between(old: &TempDir, new: &TempDir) -> Changes {
    compute_changes_between(&parse(old).unwrap(), &parse(new).unwrap()).unwrap()
}

fn strings(versions: &[Version]) -> Vec<String> {
    versions.iter().map(|v| v.to_string()).collect()
}

#[test]
fn test_declaring_a_base_adds_nothing_by_itself() {
    let single_base_index_toml = TWO_BASES_INDEX_TOML.split("[bases").next().unwrap();
    let worlds = [("world", CUSTOM_WORLD), ("core", CORE_WORLD)];
    let old = write_index(single_base_index_toml, &worlds);
    let new = write_index(TWO_BASES_INDEX_TOML, &worlds);

    let changes = changes_between(&old, &new);

    assert!(
        changes.worlds.is_empty(),
        "The old index is compared as if it had always had the new base: {:#?}",
        changes.worlds
    );
    assert_eq!(strings(&changes.bases), vec!["0.6.7", "0.6.8"]);
    assert_eq!(strings(&changes.added_bases), vec!["0.6.8"]);
}

#[test]
fn test_changes_name_the_bases_a_release_was_added_on() {
    let old = write_index(TWO_BASES_INDEX_TOML, &[("world", CUSTOM_WORLD)]);
    let new_world = format!(
        "{CUSTOM_WORLD}\"1.1.0\" = {{}}\n\n[releases]\n\"2.0.0\" = {{ min_ap_version = \"0.6.8\" }}\n"
    );
    let new = write_index(TWO_BASES_INDEX_TOML, &[("world", &new_world)]);

    let changes = changes_between(&old, &new);

    let world = &changes.worlds["world"];
    assert_eq!(strings(&world.added_versions), vec!["1.1.0", "2.0.0"]);
    assert_eq!(
        strings(&world.added_on[&version("1.1.0")]),
        vec!["0.6.7", "0.6.8"]
    );
    assert_eq!(strings(&world.added_on[&version("2.0.0")]), vec!["0.6.8"]);
    assert!(world.removed_versions.is_empty());
    assert!(changes.added_bases.is_empty());
}

#[test]
fn test_capping_a_release_removes_it_from_the_newer_base() {
    let old = write_index(TWO_BASES_INDEX_TOML, &[("world", CUSTOM_WORLD)]);
    let new_world = format!(
        "{CUSTOM_WORLD}\"1.1.0\" = {{}}\n\n[releases]\n\"1.0.0\" = {{ max_ap_version = \"0.6.7\" }}\n"
    );
    let new = write_index(TWO_BASES_INDEX_TOML, &[("world", &new_world)]);

    let changes = changes_between(&old, &new);

    let world = &changes.worlds["world"];
    assert_eq!(strings(&world.removed_versions), vec!["1.0.0"]);
    assert_eq!(
        strings(&world.removed_from[&version("1.0.0")]),
        vec!["0.6.8"]
    );
    assert_eq!(strings(&world.added_versions), vec!["1.1.0"]);
}

#[test]
fn test_world_entering_core_shows_up_on_that_base_only() {
    let old = write_index(TWO_BASES_INDEX_TOML, &[("world", CUSTOM_WORLD)]);
    let new_world = format!(
        "{CUSTOM_WORLD}\n[releases]\n\"1.0.0\" = {{ max_ap_version = \"0.6.7\" }}\n\n[base.\">=0.6.8\"]\nsupported = true\ndefault_version = \"latest_supported\"\n"
    );
    let new = write_index(TWO_BASES_INDEX_TOML, &[("world", &new_world)]);

    let changes = changes_between(&old, &new);

    let world = &changes.worlds["world"];
    assert_eq!(strings(&world.added_versions), vec!["0.6.8"]);
    assert_eq!(strings(&world.added_on[&version("0.6.8")]), vec!["0.6.8"]);
    assert_eq!(
        strings(&world.removed_from[&version("1.0.0")]),
        vec!["0.6.8"]
    );
}

#[test]
fn test_changes_from_before_bases_still_parse() {
    let json = r#"{"worlds":{"some":{"world_name":"Some","added_versions":["0.1.0"],"removed_versions":[],"checksums":{}}}}"#;

    let changes: Changes = serde_json::from_str(json).unwrap();

    assert!(changes.bases.is_empty());
    assert!(changes.worlds["some"].added_on.is_empty());
}

#[tokio::test]
async fn test_refresh_downloads_the_releases_of_every_base() {
    let dir = write_index(
        TWO_BASES_INDEX_TOML,
        &[(
            "world",
            r#"
name = "World"

[versions]
"1.0.0" = { local = "../apworlds/world-1.0.0.apworld" }

[releases]
"2.0.0" = { min_ap_version = "0.6.8", local = "../apworlds/world-2.0.0.apworld" }
"#,
        )],
    );
    std::fs::create_dir(dir.path().join("apworlds")).unwrap();
    std::fs::write(dir.path().join("apworlds/world-1.0.0.apworld"), "one").unwrap();
    std::fs::write(dir.path().join("apworlds/world-2.0.0.apworld"), "two").unwrap();
    let destination = TempDir::new().unwrap();

    let set = parse(&dir).unwrap();
    let lock = set
        .refresh_into(destination.path(), false, None)
        .await
        .unwrap();

    assert!(destination.path().join("world-1.0.0.apworld").is_file());
    assert!(
        destination.path().join("world-2.0.0.apworld").is_file(),
        "A release that only 0.6.8 can run should be downloaded too"
    );
    assert!(lock.get_checksum("world", &version("1.0.0")).is_some());
    assert!(lock.get_checksum("world", &version("2.0.0")).is_some());
}
