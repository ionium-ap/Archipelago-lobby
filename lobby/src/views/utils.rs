use semver::Version;

use crate::db::Room;

pub fn partition_rooms_by_closed(rooms: &[Room]) -> (Vec<&Room>, Vec<&Room>) {
    rooms.iter().partition(|room| room.is_closed())
}

/// One entry of a selector of Archipelago versions
pub struct BaseOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// The entries for the versions on offer, newest first as they are given. `selected` is among
/// them even when it isn't on offer: a room or a template can be on a version that was
/// withdrawn, and the selector still has to say so.
pub fn base_options(offered: &[Version], selected: Option<&Version>) -> Vec<BaseOption> {
    let mut options: Vec<BaseOption> = offered
        .iter()
        .map(|base| BaseOption {
            value: base.to_string(),
            label: format!("Archipelago {base}"),
            selected: Some(base) == selected,
        })
        .collect();

    if let Some(selected) = selected {
        if !offered.contains(selected) {
            options.push(BaseOption {
                value: selected.to_string(),
                label: format!("Archipelago {selected} (no longer offered)"),
                selected: true,
            });
        }
    }

    options
}

/// The small label that says which Archipelago version a room is on, and how that compares
/// with the version a new room would get.
pub struct BaseChip {
    pub label: String,
    /// `current`, `behind` or `ahead`. Part of a class name.
    pub standing: &'static str,
    pub title: String,
}

pub fn base_chip(room_base: &Version, default_base: &Version) -> BaseChip {
    let (standing, title) = match room_base.cmp(default_base) {
        std::cmp::Ordering::Equal => (
            "current",
            format!("This room is on Archipelago {room_base}, as new rooms are."),
        ),
        std::cmp::Ordering::Less => (
            "behind",
            format!(
                "This room is on Archipelago {room_base}. New rooms are on {default_base}, which is newer."
            ),
        ),
        std::cmp::Ordering::Greater => (
            "ahead",
            format!(
                "This room is on Archipelago {room_base}. New rooms are on {default_base}, which is older."
            ),
        ),
    };

    BaseChip {
        label: format!("AP {room_base}"),
        standing,
        title,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(version: &str) -> Version {
        version.parse().unwrap()
    }

    #[test]
    fn test_chip_says_where_the_room_stands() {
        let default = version("0.6.8");

        let chip = base_chip(&version("0.6.8"), &default);
        assert_eq!(
            (chip.label.as_str(), chip.standing),
            ("AP 0.6.8", "current")
        );
        let chip = base_chip(&version("0.6.7"), &default);
        assert_eq!((chip.label.as_str(), chip.standing), ("AP 0.6.7", "behind"));
        let chip = base_chip(&version("0.7.0"), &default);
        assert_eq!((chip.label.as_str(), chip.standing), ("AP 0.7.0", "ahead"));
        // A release candidate comes before its release
        let chip = base_chip(&version("0.6.8-rc1"), &default);
        assert_eq!(chip.standing, "behind");
    }

    #[test]
    fn test_selector_lists_what_is_offered_and_what_is_selected() {
        let offered = [version("0.6.8"), version("0.6.7")];

        let options = base_options(&offered, Some(&version("0.6.7")));
        let selected: Vec<_> = options
            .iter()
            .map(|o| (o.value.as_str(), o.selected))
            .collect();
        assert_eq!(selected, [("0.6.8", false), ("0.6.7", true)]);

        let options = base_options(&offered, None);
        assert!(options.iter().all(|o| !o.selected));

        // A room on a version that isn't offered anymore
        let options = base_options(&offered, Some(&version("0.6.6")));
        let last = options.last().unwrap();
        assert_eq!((last.value.as_str(), last.selected), ("0.6.6", true));
        assert_eq!(options.len(), 3);
    }
}
