//! What `apwm lint` reports, one rule at a time.

use std::path::Path;

use apwm::lint::{lint, Problem, Severity};
use tempfile::TempDir;

const INDEX_TOML: &str = r#"
archipelago_repo = "https://github.com/ArchipelagoMW/Archipelago.git"
archipelago_version = "0.6.7"
index_homepage = "https://example.invalid/index"
index_dir = "index"

[bases."0.6.7"]
[bases."0.6.8"]
"#;

/// Lints an index holding a single world file, `index/world.toml`.
fn lint_world(world_toml: &str) -> Vec<Problem> {
    lint_index(INDEX_TOML, world_toml)
}

fn lint_index(index_toml: &str, world_toml: &str) -> Vec<Problem> {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("index.toml"), index_toml).unwrap();
    std::fs::create_dir(dir.path().join("index")).unwrap();
    std::fs::write(dir.path().join("index/world.toml"), world_toml).unwrap();

    lint(&dir.path().join("index.toml")).expect("The index should parse")
}

#[track_caller]
fn assert_one(problems: &[Problem], severity: Severity, file: &str, mentions: &[&str]) {
    assert_eq!(problems.len(), 1, "Expected one problem, got {problems:#?}");
    let problem = &problems[0];
    assert_eq!(problem.severity, severity, "{problem}");
    assert_eq!(problem.file, Path::new(file), "{problem}");
    for mention in mentions {
        assert!(
            problem.message.contains(mention),
            "`{problem}` should mention `{mention}`"
        );
    }
}

#[test]
fn test_fixture_is_clean() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multi_base");

    let problems = lint(&fixture.join("index.toml")).unwrap();

    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn test_plain_world_is_clean() {
    let problems = lint_world(
        r#"
name = "World"
home = "https://example.invalid"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = {}
"1.1.0" = { url = "https://example.invalid/world-1.1.0.apworld" }
"1.2.0" = { local = "../apworlds/world-1.2.0.apworld" }
"#,
    );

    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn test_second_key_in_versions_entry() {
    let problems = lint_world(
        r#"
name = "World"

[versions]
"1.0.0" = { url = "https://example.invalid/world.apworld", min_ap_version = "0.6.8" }
"#,
    );

    assert_one(
        &problems,
        Severity::Error,
        "index/world.toml",
        &["1.0.0", "2 keys", "`[releases]`"],
    );
}

#[test]
fn test_constraint_in_versions_entry() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = { min_ap_version = "0.6.8" }
"#,
    );

    assert_one(
        &problems,
        Severity::Error,
        "index/world.toml",
        &["`min_ap_version`", "1.0.0", "ignored"],
    );
}

#[test]
fn test_versions_entry_that_is_not_a_table() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = []
"#,
    );

    assert_one(
        &problems,
        Severity::Warning,
        "index/world.toml",
        &["1.0.0", "should be a table"],
    );
}

#[test]
fn test_release_visible_to_older_lobbies_that_they_cannot_run() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = {}
"2.0.0" = {}

[releases]
"2.0.0" = { min_ap_version = "0.6.8" }
"#,
    );

    assert_one(
        &problems,
        Severity::Error,
        "index/world.toml",
        &["2.0.0", "older lobbies offer it", "0.6.7"],
    );
}

#[test]
fn test_world_without_legacy_release_must_be_disabled() {
    let world = r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[releases]
"1.0.0" = { min_ap_version = "0.6.8" }
"#;

    assert_one(
        &lint_world(world),
        Severity::Error,
        "index/world.toml",
        &["nothing in `[versions]`", "`disabled = true`"],
    );

    let hidden_from_older_lobbies = r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"
disabled = true

[releases]
"1.0.0" = { min_ap_version = "0.6.8" }

[base.">=0.6.8"]
disabled = false
"#;
    let problems = lint_world(hidden_from_older_lobbies);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn test_release_older_lobbies_could_run_but_do_not_see() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = {}

[releases]
"1.1.0" = { max_ap_version = "0.6.8" }
"#,
    );

    assert_one(
        &problems,
        Severity::Warning,
        "index/world.toml",
        &["1.1.0", "only in `[releases]`"],
    );
}

#[test]
fn test_release_that_can_run_nowhere() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"
disabled = true

[releases]
"1.0.0" = { min_ap_version = "0.6.8", max_ap_version = "0.6.7" }
"#,
    );

    assert_one(
        &problems,
        Severity::Error,
        "index/world.toml",
        &["1.0.0", "can't run anywhere"],
    );
}

#[test]
fn test_release_that_changes_nothing() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = {}

[releases]
"1.0.0" = {}
"#,
    );

    assert_one(
        &problems,
        Severity::Warning,
        "index/world.toml",
        &["1.0.0", "changes nothing"],
    );
}

#[test]
fn test_misspelled_keys() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"
suported = true

[versions]
"1.0.0" = {}

[releases]
"1.0.0" = { max_ap_verison = "0.6.7" }

[base.">=0.6.8"]
default = "1.0.0"
"#,
    );

    let errors: Vec<&str> = problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(errors.len(), 3, "{problems:#?}");
    assert!(errors.iter().any(|m| m.contains("`suported`")));
    assert!(errors.iter().any(|m| m.contains("`max_ap_verison`")));
    assert!(errors.iter().any(|m| m.contains("`default`")));
}

#[test]
fn test_base_table_matching_no_declared_base() {
    let problems = lint_world(
        r#"
name = "World"
supported = true

[base.">=0.7.0"]
disabled = true
"#,
    );

    assert_one(
        &problems,
        Severity::Warning,
        "index/world.toml",
        &[">=0.7.0", "doesn't match any base"],
    );
}

#[test]
fn test_default_version_missing_on_a_base() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"
default_version = "1.0.0"

[versions]
"1.0.0" = {}
"1.1.0" = {}

[releases]
"1.0.0" = { max_ap_version = "0.6.7" }
"#,
    );

    assert_one(
        &problems,
        Severity::Error,
        "index/world.toml",
        &["default version 1.0.0", "0.6.8"],
    );
}

#[test]
fn test_latest_supported_on_a_world_that_is_not_core() {
    let problems = lint_world(
        r#"
name = "World"
default_url = "https://example.invalid/world-{{version}}/world.apworld"

[versions]
"1.0.0" = {}

[base.">=0.6.8"]
default_version = "latest_supported"
"#,
    );

    assert_one(
        &problems,
        Severity::Error,
        "index/world.toml",
        &["`latest_supported`", "0.6.8"],
    );
}

#[test]
fn test_unknown_key_in_index_toml() {
    let index_toml = INDEX_TOML.replace(
        "index_dir = \"index\"",
        "index_dir = \"index\"\narchipelago_versions = [\"0.6.7\", \"0.6.8\"]",
    );

    let problems = lint_index(&index_toml, "name = \"World\"\nsupported = true\n");

    assert_one(
        &problems,
        Severity::Error,
        "index.toml",
        &["`archipelago_versions`"],
    );
}

#[test]
fn test_bases_table_without_the_legacy_base() {
    let index_toml = INDEX_TOML.replace("[bases.\"0.6.7\"]\n", "");

    let problems = lint_index(&index_toml, "name = \"World\"\nsupported = true\n");

    assert_one(
        &problems,
        Severity::Warning,
        "index.toml",
        &["doesn't list 0.6.7"],
    );
}

#[test]
fn test_unparsable_index_is_an_error_not_a_problem() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("index.toml"), INDEX_TOML).unwrap();
    std::fs::create_dir(dir.path().join("index")).unwrap();
    std::fs::write(
        dir.path().join("index/broken.toml"),
        "name = \"World\"\ntags = [\"unheard_of\"]\n",
    )
    .unwrap();

    let error = lint(&dir.path().join("index.toml")).unwrap_err();

    assert!(
        format!("{error:#}").contains("broken.toml"),
        "The error should name the file: {error:#}"
    );
}
