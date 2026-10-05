//! What moving a room to another Archipelago version does to the YAMLs that are in it.
//!
//! The room keeps its manifest, and the manifest is read against the worlds and releases of
//! the version the room is on. So the same manifest can give another release of a world after
//! the move, or none.

use std::collections::BTreeMap;

use apwm::{Index, Manifest, VersionReq};
use semver::Version;

/// A world the room has YAMLs for, and what the move does to it
#[derive(Debug, PartialEq)]
pub struct WorldChange {
    pub game: String,
    /// How many of the room's YAMLs can roll this world
    pub yamls: usize,
    pub kind: ChangeKind,
}

#[derive(Debug, PartialEq)]
pub enum ChangeKind {
    /// The target version doesn't have the world: its YAMLs become unsupported
    Lost { from: Version },
    /// The room pins a release that the target version doesn't have. A pin that can't be
    /// honored falls back on the newest release, which nobody chose.
    PinNotAvailable { pinned: Version, to: Version },
    /// Another release, by the room's own rules: an unpinned world follows the version
    Moved { from: Version, to: Version },
    /// Unsupported where the room is, available on the target version
    Gained { to: Version },
}

#[derive(Debug, Default, PartialEq)]
pub struct SwitchReport {
    /// The ones an owner has to read before going on
    pub warnings: Vec<WorldChange>,
    /// The ones that are what moving to another version means
    pub changes: Vec<WorldChange>,
    /// Worlds with YAMLs that keep the release they have
    pub unchanged: usize,
}

/// `games` has the name of each world of each YAML in the room, so a world comes up once per
/// YAML that can roll it.
pub fn switch_report<'a>(
    manifest: &Manifest,
    current: &Index,
    target: &Index,
    games: impl IntoIterator<Item = &'a str>,
) -> SwitchReport {
    let mut yamls_per_game: BTreeMap<&str, usize> = BTreeMap::new();
    for game in games {
        *yamls_per_game.entry(game).or_default() += 1;
    }

    let mut report = SwitchReport::default();
    for (game, yamls) in yamls_per_game {
        let from = manifest
            .resolve_from_game_name(game, current)
            .ok()
            .map(|(_, version)| version);
        let to = manifest
            .resolve_from_game_name(game, target)
            .ok()
            .map(|(_, version)| version);

        let kind = match (from, to) {
            (Some(from), Some(to)) if from == to => {
                report.unchanged += 1;
                continue;
            }
            // Unsupported before and after
            (None, None) => continue,
            (Some(from), None) => ChangeKind::Lost { from },
            (None, Some(to)) => ChangeKind::Gained { to },
            (Some(from), Some(to)) => match unavailable_pin(manifest, target, game) {
                Some(pinned) => ChangeKind::PinNotAvailable { pinned, to },
                None => ChangeKind::Moved { from, to },
            },
        };

        let change = WorldChange {
            game: game.to_string(),
            yamls,
            kind,
        };
        match change.kind {
            ChangeKind::Lost { .. } | ChangeKind::PinNotAvailable { .. } => {
                report.warnings.push(change)
            }
            ChangeKind::Moved { .. } | ChangeKind::Gained { .. } => report.changes.push(change),
        }
    }

    report
}

/// The release the manifest pins for `game`, if `index` doesn't have that release
fn unavailable_pin(manifest: &Manifest, index: &Index, game: &str) -> Option<Version> {
    let (apworld_name, world) = index.worlds.iter().find(|(_, world)| world.name == game)?;
    match manifest.get_version_req(apworld_name, index) {
        VersionReq::Specific(pinned) if !world.versions.contains_key(&pinned) => Some(pinned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use apwm::IndexSet;

    use super::*;

    /// The index the `apwm` crate tests its own reading of several versions with
    fn index() -> IndexSet {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../apwm/tests/fixtures/multi_base/index.toml");

        IndexSet::new(&fixture).unwrap()
    }

    fn version(version: &str) -> Version {
        version.parse().unwrap()
    }

    fn report(manifest: &Manifest, from: &str, to: &str, games: &[&str]) -> SwitchReport {
        let index = index();

        switch_report(
            manifest,
            index.get(&version(from)).unwrap(),
            index.get(&version(to)).unwrap(),
            games.iter().copied(),
        )
    }

    fn change(game: &str, yamls: usize, kind: ChangeKind) -> WorldChange {
        WorldChange {
            game: game.to_string(),
            yamls,
            kind,
        }
    }

    #[test]
    fn test_unpinned_worlds_follow_the_version() {
        let report = report(
            &Manifest::new(),
            "0.6.7",
            "0.6.8",
            &[
                "Core World",
                "Custom World",
                "Custom World",
                "After Dark World",
            ],
        );

        assert_eq!(report.warnings, []);
        assert_eq!(
            report.changes,
            [
                change(
                    "Core World",
                    1,
                    ChangeKind::Moved {
                        from: version("0.6.7"),
                        to: version("0.6.8"),
                    }
                ),
                change(
                    "Custom World",
                    2,
                    ChangeKind::Moved {
                        from: version("1.1.1"),
                        to: version("1.2.0"),
                    }
                ),
            ]
        );
        // The same release on both
        assert_eq!(report.unchanged, 1);
    }

    #[test]
    fn test_pin_the_target_does_not_have_is_a_warning() {
        let mut manifest = Manifest::new();
        manifest.add_version_req("custom", VersionReq::Specific(version("1.1.0")));

        let report = report(&manifest, "0.6.7", "0.6.8", &["Custom World"]);

        assert_eq!(
            report.warnings,
            [change(
                "Custom World",
                1,
                ChangeKind::PinNotAvailable {
                    pinned: version("1.1.0"),
                    to: version("1.2.0"),
                }
            )]
        );
        assert_eq!(report.changes, []);
    }

    #[test]
    fn test_pin_the_target_has_changes_nothing() {
        let mut manifest = Manifest::new();
        manifest.add_version_req("custom", VersionReq::Specific(version("1.1.1")));

        let report = report(&manifest, "0.6.7", "0.6.8", &["Custom World"]);

        assert_eq!(report.warnings, []);
        assert_eq!(report.changes, []);
        assert_eq!(report.unchanged, 1);
    }

    #[test]
    fn test_world_the_target_does_not_have_is_a_warning() {
        let report = report(
            &Manifest::new(),
            "0.6.7",
            "0.6.8",
            &["Left Core World", "New Core World", "Nobody Knows This One"],
        );

        assert_eq!(
            report.warnings,
            [change(
                "Left Core World",
                1,
                ChangeKind::Lost {
                    from: version("0.6.7")
                }
            )]
        );
        assert_eq!(
            report.changes,
            [change(
                "New Core World",
                1,
                ChangeKind::Gained {
                    to: version("0.6.8")
                }
            )]
        );
        assert_eq!(report.unchanged, 0);
    }

    #[test]
    fn test_going_back_is_reported_the_same_way() {
        let report = report(
            &Manifest::new(),
            "0.6.8",
            "0.6.7",
            &["New Core World", "Graduated World"],
        );

        assert_eq!(
            report.warnings,
            [change(
                "New Core World",
                1,
                ChangeKind::Lost {
                    from: version("0.6.8")
                }
            )]
        );
        assert_eq!(
            report.changes,
            [change(
                "Graduated World",
                1,
                ChangeKind::Moved {
                    from: version("0.6.8"),
                    to: version("2.1.7"),
                }
            )]
        );
    }
}
