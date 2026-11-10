use saphyr::{ScalarOwned, YamlOwned as Value};

use crate::yaml_value_as_string;

fn is_trueish_match(option_val: &Value, trigger_result: &str) -> bool {
    yaml_value_as_string(option_val).is_some_and(|s| s == trigger_result)
}

/// Whether the option a trigger checks could roll `trigger_result`. Archipelago's `get_choice`
/// takes a plain value as is, rolls a mapping by its weights, and picks a list's element at
/// random, so a weighted value with a nonzero weight or any element of a list could match.
fn value_can_match(val: &Value, trigger_result: &str) -> bool {
    if is_trueish_match(val, trigger_result) {
        return true;
    }

    if let Some(seq) = val.as_sequence() {
        return seq
            .iter()
            .any(|item| is_trueish_match(item, trigger_result));
    }

    if let Some(map) = val.as_mapping() {
        for (key, weight) in map.iter() {
            let w = weight.as_integer().unwrap_or(0);
            if w != 0 && is_trueish_match(key, trigger_result) {
                return true;
            }
        }
    }

    false
}

/// How a trigger's option changes the existing one, from the prefix on its name.
#[derive(Clone, Copy)]
enum Merge {
    /// No prefix: the trigger's value replaces the option.
    Replace,
    /// `+option`: the trigger's value is added to the option.
    Add,
    /// `-option`: the trigger's value is removed from the option.
    Remove,
}

/// One option a trigger or linked option changes.
struct TriggerOption {
    /// The section the option is in: a game's name, or `None` for the top level.
    category: Option<String>,
    key: String,
    value: Value,
    merge: Merge,
}

struct Trigger {
    /// The section the checked option is in: a game's name, or `None` for the top level.
    option_category: Option<String>,
    option_name: String,
    option_result: String,
    /// False when the trigger's `percentage` rules out firing.
    can_fire: bool,
    options: Vec<TriggerOption>,
}

/// Reads a section name the way Archipelago does, where `null` or an empty name means the top
/// level. `None` when the name is neither null nor a string.
fn parse_category(val: &Value) -> Option<Option<String>> {
    if val.is_null() {
        return Some(None);
    }
    let name = val.as_str()?;
    Some((!name.is_empty()).then(|| name.to_string()))
}

/// Reads the `options` of a trigger or linked option, the options it changes grouped by
/// section. `None` when a section name or an option name can't be read.
fn parse_options(options_yaml: &Value) -> Option<Vec<TriggerOption>> {
    let mut options = Vec::new();
    let Some(outer) = options_yaml.as_mapping() else {
        return Some(options);
    };

    for (category_key, category_options) in outer.iter() {
        let category = parse_category(category_key)?;
        let Some(inner) = category_options.as_mapping() else {
            continue;
        };
        for (opt_key, opt_val) in inner.iter() {
            let raw_key = opt_key.as_str()?;
            // Archipelago strips every leading `+` and `-`, and the first one decides.
            let merge = if raw_key.starts_with('+') {
                Merge::Add
            } else if raw_key.starts_with('-') {
                Merge::Remove
            } else {
                Merge::Replace
            };
            options.push(TriggerOption {
                category: category.clone(),
                key: raw_key.trim_start_matches(['+', '-']).to_string(),
                value: opt_val.clone(),
                merge,
            });
        }
    }

    Some(options)
}

/// Whether a `percentage` lets a trigger or linked option fire at all. Archipelago fires when a
/// random number is below `percentage / 100`, so any percentage above zero can. A weighted
/// percentage, or a list of them, can if any value it could roll is above zero. A percentage
/// that's missing (100 for a trigger) or isn't a number (which Archipelago rejects at upload)
/// counts as able to fire.
fn percentage_can_fire(percentage: Option<&Value>) -> bool {
    let Some(percentage) = percentage else {
        return true;
    };
    let above_zero = |val: &Value| percentage_value(val).is_none_or(|p| p > 0.0);
    if let Some(map) = percentage.as_mapping() {
        return map
            .iter()
            .any(|(value, weight)| weight.as_integer().unwrap_or(0) != 0 && above_zero(value));
    }
    if let Some(seq) = percentage.as_sequence() {
        return seq.iter().any(above_zero);
    }
    above_zero(percentage)
}

fn percentage_value(val: &Value) -> Option<f64> {
    val.as_integer()
        .map(|i| i as f64)
        .or_else(|| val.as_floating_point())
        .or_else(|| val.as_str().and_then(|s| s.trim().parse().ok()))
}

fn parse_trigger(trigger_yaml: &Value) -> Option<Trigger> {
    // Archipelago takes a missing category as the top level, as it does `null`.
    let category = match trigger_yaml.as_mapping_get("option_category") {
        Some(category) => parse_category(category)?,
        None => None,
    };
    let name = trigger_yaml.as_mapping_get("option_name")?.as_str()?;
    let result_str = yaml_value_as_string(trigger_yaml.as_mapping_get("option_result")?)?;
    let options = parse_options(trigger_yaml.as_mapping_get("options")?)?;

    Some(Trigger {
        option_category: category,
        option_name: name.to_string(),
        option_result: result_str,
        can_fire: percentage_can_fire(trigger_yaml.as_mapping_get("percentage")),
        options,
    })
}

/// The options of each top-level linked option that could fire, in order. Each is applied
/// like a trigger without a condition: Archipelago rolls only its `percentage`.
fn linked_options(yaml: &Value) -> Vec<Vec<TriggerOption>> {
    yaml.as_mapping_get("linked_options")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter(|linked| percentage_can_fire(linked.as_mapping_get("percentage")))
        .filter_map(|linked| parse_options(linked.as_mapping_get("options")?))
        .collect()
}

fn parse_triggers_from(val: &Value) -> impl Iterator<Item = Trigger> + '_ {
    val.as_sequence()
        .into_iter()
        .flatten()
        .filter_map(parse_trigger)
}

/// The triggers in each game section, in the order the sections appear. Archipelago runs only
/// the rolled game's section triggers; a review can't know which game a weighted `game` rolls,
/// so it runs them all.
fn game_section_triggers(yaml: &Value) -> Vec<Trigger> {
    let mut triggers = Vec::new();

    if let Some(map) = yaml.as_mapping() {
        for (key, game_section) in map.iter() {
            let Some(key_str) = key.as_str() else {
                continue;
            };
            if matches!(key_str, "triggers" | "game" | "name" | "description") {
                continue;
            }
            if let Some(game_triggers) = game_section.as_mapping_get("triggers") {
                triggers.extend(parse_triggers_from(game_triggers));
            }
        }
    }

    triggers
}

/// Applies the YAML's linked options and triggers the way `roll_settings` in Archipelago's
/// Generate.py does: the linked options first, then the top-level triggers, then the game
/// sections' triggers, each read as the YAML stands after the step before. Each one applies
/// once and in order, so it sees what earlier ones changed but not later ones. Archipelago
/// rolls the option a trigger checks and the percentages; here a trigger fires if any weighted
/// value of that option could match, and a percentage above zero always lets it.
pub fn resolve_triggers(yaml: &Value) -> Value {
    let mut current = yaml.clone();

    for options in &linked_options(yaml) {
        apply_options(&mut current, options);
    }

    let top_level: Vec<Trigger> = current
        .as_mapping_get("triggers")
        .map(|triggers| parse_triggers_from(triggers).collect())
        .unwrap_or_default();
    for trigger in &top_level {
        apply_trigger(&mut current, trigger);
    }

    for trigger in &game_section_triggers(&current) {
        apply_trigger(&mut current, trigger);
    }

    current
}

/// A section of the YAML: a game's, or the top level for `None`.
fn section<'a>(yaml: &'a Value, category: Option<&str>) -> Option<&'a Value> {
    match category {
        Some(name) => yaml.as_mapping_get(name),
        None => Some(yaml),
    }
}

fn section_mut<'a>(yaml: &'a mut Value, category: Option<&str>) -> Option<&'a mut Value> {
    match category {
        Some(name) => yaml.as_mapping_get_mut(name),
        None => Some(yaml),
    }
}

fn apply_trigger(yaml: &mut Value, trigger: &Trigger) {
    let fires = trigger.can_fire
        && section(yaml, trigger.option_category.as_deref())
            .and_then(|section| section.as_mapping_get(&trigger.option_name))
            .is_some_and(|option| value_can_match(option, &trigger.option_result));
    if fires {
        apply_options(yaml, &trigger.options);
    }
}

fn apply_options(yaml: &mut Value, options: &[TriggerOption]) {
    for option in options {
        let Some(section) = section_mut(yaml, option.category.as_deref()) else {
            continue;
        };
        let existing = section.as_mapping_get(&option.key);
        let new_value = match option.merge {
            Merge::Replace => option.value.clone(),
            Merge::Add => merge_additive(existing, &option.value),
            Merge::Remove => merge_removal(existing, &option.value),
        };
        set_option(section, &option.key, new_value);
    }
}

/// Applies a `+option` from a trigger or linked option, following `update_weights` in
/// Archipelago's Generate.py: an option that isn't set yet takes the addition as is, two mappings add up
/// their counts, and two lists are joined. Archipelago rejects any other pair as an invalid
/// trigger; here those fall back to the existing list, if any, followed by the addition.
fn merge_additive(existing: Option<&Value>, addition: &Value) -> Value {
    let Some(existing) = existing else {
        return addition.clone();
    };

    if let (Some(existing_map), Some(addition_map)) = (existing.as_mapping(), addition.as_mapping())
    {
        let mut merged = existing_map.clone();
        for (key, value) in addition_map {
            let sum = merged
                .get(key)
                .and_then(Value::as_integer)
                .zip(value.as_integer())
                .map(|(a, b)| Value::Value(ScalarOwned::Integer(a.saturating_add(b))));
            merged.replace(key.clone(), sum.unwrap_or_else(|| value.clone()));
        }
        return Value::Mapping(merged);
    }

    let mut items: Vec<Value> = existing.as_sequence().cloned().unwrap_or_default();
    match addition.as_sequence() {
        Some(seq) => items.extend(seq.iter().cloned()),
        None => items.push(addition.clone()),
    }
    Value::Sequence(items)
}

/// Applies a `-option` from a trigger or linked option, following `update_weights` in
/// Archipelago's Generate.py: each listed value is removed from a list once, at its first
/// match, and a mapping's counts are subtracted, keeping zero and negative results. An option
/// that isn't set yet takes the trigger's value as is, as Archipelago does. Archipelago rejects any other pair,
/// and removing a list value that isn't there, as an invalid trigger; here those leave the
/// option as it was.
fn merge_removal(existing: Option<&Value>, removal: &Value) -> Value {
    let Some(existing) = existing else {
        return removal.clone();
    };

    if let (Some(existing_map), Some(removal_map)) = (existing.as_mapping(), removal.as_mapping()) {
        let mut merged = existing_map.clone();
        for (key, value) in removal_map {
            let Some(amount) = value.as_integer() else {
                continue;
            };
            let count = match merged.get(key) {
                None => 0,
                Some(current) => match current.as_integer() {
                    Some(count) => count,
                    None => continue,
                },
            };
            merged.replace(
                key.clone(),
                Value::Value(ScalarOwned::Integer(count.saturating_sub(amount))),
            );
        }
        return Value::Mapping(merged);
    }

    if let (Some(existing_seq), Some(removal_seq)) = (existing.as_sequence(), removal.as_sequence())
    {
        let mut items = existing_seq.clone();
        for value in removal_seq {
            if let Some(index) = items.iter().position(|item| item == value) {
                items.remove(index);
            }
        }
        return Value::Sequence(items);
    }

    existing.clone()
}

/// Sets one option in a section, in place, leaving every other node as it was parsed.
/// A section that isn't a mapping is left alone.
fn set_option(section: &mut Value, key: &str, value: Value) {
    if let Some(existing) = section.as_mapping_get_mut(key) {
        *existing = value;
    } else if let Some(map) = section.as_mapping_mut() {
        map.insert(Value::Value(ScalarOwned::String(key.to_string())), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saphyr::LoadableYamlNode;

    fn parse_yaml(raw: &str) -> Value {
        Value::load_from_str(raw)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn test_no_triggers() {
        let yaml = parse_yaml("Test:\n  death_link: true\n");
        let resolved = resolve_triggers(&yaml);
        let val = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap();
        assert!(val.as_bool().unwrap());
    }

    #[test]
    fn test_top_level_trigger_fires() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        death_link: true
Test:
  mode: hard
  death_link: false
"#;
        let yaml = parse_yaml(raw);
        let resolved = resolve_triggers(&yaml);
        let dl = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap();
        assert!(dl.as_bool().unwrap());
    }

    #[test]
    fn test_trigger_does_not_fire() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        death_link: true
Test:
  mode: easy
  death_link: false
"#;
        let yaml = parse_yaml(raw);
        let resolved = resolve_triggers(&yaml);
        let dl = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap();
        assert!(!dl.as_bool().unwrap());
    }

    #[test]
    fn test_trigger_within_game_block() {
        let raw = r#"
Test:
  mode: hard
  death_link: false
  triggers:
    - option_category: Test
      option_name: mode
      option_result: hard
      options:
        Test:
          death_link: true
"#;
        let yaml = parse_yaml(raw);
        let resolved = resolve_triggers(&yaml);
        let dl = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap();
        assert!(dl.as_bool().unwrap());
    }

    #[test]
    fn test_trigger_with_weighted_map() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        death_link: true
Test:
  mode:
    hard: 50
    easy: 50
  death_link: false
"#;
        let yaml = parse_yaml(raw);
        let resolved = resolve_triggers(&yaml);
        let dl = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap();
        assert!(dl.as_bool().unwrap());
    }

    #[test]
    fn test_trigger_weighted_map_zero_weight() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        death_link: true
Test:
  mode:
    hard: 0
    easy: 50
  death_link: false
"#;
        let yaml = parse_yaml(raw);
        let resolved = resolve_triggers(&yaml);
        let dl = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap();
        assert!(!dl.as_bool().unwrap());
    }

    #[test]
    fn test_trigger_sets_new_option() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        extra_option: enabled
Test:
  mode: hard
"#;
        let yaml = parse_yaml(raw);
        let resolved = resolve_triggers(&yaml);
        let extra = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("extra_option")
            .unwrap();
        assert_eq!(extra.as_str().unwrap(), "enabled");
    }

    const FIRING_TRIGGER: &str = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        extra_option: enabled
"#;

    #[test]
    fn test_fired_trigger_keeps_non_string_keys() {
        let raw = format!(
            "{FIRING_TRIGGER}Test:\n  mode: hard\n  death_link:\n    false: 0\n    true: 50\n"
        );
        let resolved = resolve_triggers(&parse_yaml(&raw));
        let death_link = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap()
            .as_mapping()
            .unwrap();
        let weight_of = |b: bool| {
            death_link
                .get(&Value::Value(ScalarOwned::Boolean(b)))
                .and_then(Value::as_integer)
        };
        assert_eq!(weight_of(false), Some(0));
        assert_eq!(weight_of(true), Some(50));
    }

    #[test]
    fn test_fired_trigger_keeps_mappings_inside_lists() {
        let raw = format!(
            "{FIRING_TRIGGER}Test:\n  mode: hard\n  plando_items:\n    - items:\n        Sword: 1\n      world: P2\n"
        );
        let resolved = resolve_triggers(&parse_yaml(&raw));
        let entry = &resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("plando_items")
            .unwrap()
            .as_sequence()
            .unwrap()[0];
        assert_eq!(entry.as_mapping_get("world").unwrap().as_str(), Some("P2"));
        let sword = entry
            .as_mapping_get("items")
            .unwrap()
            .as_mapping_get("Sword");
        assert_eq!(sword.and_then(Value::as_integer), Some(1));
    }

    #[test]
    fn test_trigger_sets_weighted_mapping() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        death_link:
          true: 1
Test:
  mode: hard
  death_link: false
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        let death_link = resolved
            .as_mapping_get("Test")
            .unwrap()
            .as_mapping_get("death_link")
            .unwrap()
            .as_mapping()
            .unwrap();
        let weight = death_link
            .get(&Value::Value(ScalarOwned::Boolean(true)))
            .and_then(Value::as_integer);
        assert_eq!(weight, Some(1));
    }

    #[test]
    fn test_merge_additive_unset_option_takes_addition_as_is() {
        let addition = parse_yaml("Sword: 1\n");
        assert_eq!(merge_additive(None, &addition), addition);
    }

    #[test]
    fn test_merge_additive_mappings_add_counts() {
        let existing = parse_yaml("Sword: 1\nShield: 2\n");
        let addition = parse_yaml("Sword: 2\nBow: 1\n");
        let merged = merge_additive(Some(&existing), &addition);
        let count = |item| merged.as_mapping_get(item).and_then(Value::as_integer);
        assert_eq!(count("Sword"), Some(3));
        assert_eq!(count("Shield"), Some(2));
        assert_eq!(count("Bow"), Some(1));
    }

    #[test]
    fn test_merge_additive_lists_join() {
        let existing = parse_yaml("- Sword\n");
        let addition = parse_yaml("- Bow\n- Shield\n");
        let merged = merge_additive(Some(&existing), &addition);
        assert_eq!(merged, parse_yaml("- Sword\n- Bow\n- Shield\n"));
    }

    #[test]
    fn test_triggers_fire_each_other_without_looping() {
        let raw = r#"
triggers:
  - option_category: Test
    option_name: a
    option_result: "1"
    options:
      Test:
        b: "1"
  - option_category: Test
    option_name: b
    option_result: "1"
    options:
      Test:
        a: "1"
Test:
  a: "1"
  b: "0"
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        assert_eq!(game_option(&resolved, "Test", "a").as_str(), Some("1"));
        assert_eq!(game_option(&resolved, "Test", "b").as_str(), Some("1"));
    }

    fn game_option<'a>(yaml: &'a Value, game: &str, key: &str) -> &'a Value {
        yaml.as_mapping_get(game)
            .unwrap()
            .as_mapping_get(key)
            .unwrap_or_else(|| panic!("{game} has no {key}"))
    }

    #[test]
    fn test_triggers_apply_in_order() {
        // Top-level triggers run before the game section's, each once: the first trigger
        // checks `b` before the second sets it, and the game section's trigger sees `x`, which
        // a top-level trigger set.
        let raw = r#"
triggers:
  - option_category: Test
    option_name: b
    option_result: "1"
    options:
      Test:
        c: "1"
  - option_category: Test
    option_name: a
    option_result: "1"
    options:
      Test:
        b: "1"
        x: "1"
Test:
  a: "1"
  b: "0"
  triggers:
    - option_category: Test
      option_name: x
      option_result: "1"
      options:
        Test:
          y: "1"
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        let test = resolved.as_mapping_get("Test").unwrap();
        assert_eq!(game_option(&resolved, "Test", "b").as_str(), Some("1"));
        assert!(test.as_mapping_get("c").is_none());
        assert_eq!(game_option(&resolved, "Test", "y").as_str(), Some("1"));
    }

    #[test]
    fn test_additive_list_trigger_applies_once() {
        // The example in Archipelago's worlds/generic/docs/triggers_en.md.
        let raw = r#"
Super Metroid:
  start_location:
    landing_site: 50
    aqueduct: 50
  start_hints:
    - Morph Ball
  start_inventory:
    Power Bombs: 1
  triggers:
    - option_category: Super Metroid
      option_name: start_location
      option_result: aqueduct
      options:
        Super Metroid:
          +start_hints:
            - Gravity Suit
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        assert_eq!(
            game_option(&resolved, "Super Metroid", "start_hints"),
            &parse_yaml("- Morph Ball\n- Gravity Suit\n")
        );
    }

    #[test]
    fn test_additive_mapping_trigger_applies_once() {
        let raw = r#"
Test:
  mode: hard
  start_inventory:
    Power Bombs: 1
  triggers:
    - option_category: Test
      option_name: mode
      option_result: hard
      options:
        Test:
          +start_inventory:
            Power Bombs: 1
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        let inventory = game_option(&resolved, "Test", "start_inventory");
        let power_bombs = inventory.as_mapping_get("Power Bombs");
        assert_eq!(power_bombs.and_then(Value::as_integer), Some(2));
    }

    #[test]
    fn test_removal_trigger_on_list() {
        let raw = r#"
Test:
  mode: hard
  start_hints:
    - Morph Ball
    - Gravity Suit
    - Morph Ball
  triggers:
    - option_category: Test
      option_name: mode
      option_result: hard
      options:
        Test:
          -start_hints:
            - Morph Ball
            - Screw Attack
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        let test = resolved.as_mapping_get("Test").unwrap();
        assert_eq!(
            game_option(&resolved, "Test", "start_hints"),
            &parse_yaml("- Gravity Suit\n- Morph Ball\n")
        );
        assert!(test.as_mapping_get("-start_hints").is_none());
    }

    #[test]
    fn test_removal_trigger_on_mapping() {
        let raw = r#"
Test:
  mode: hard
  start_inventory:
    Power Bombs: 1
    Missile: 5
  triggers:
    - option_category: Test
      option_name: mode
      option_result: hard
      options:
        Test:
          -start_inventory:
            Power Bombs: 1
            Super Missile: 2
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        let inventory = game_option(&resolved, "Test", "start_inventory");
        let count = |item| inventory.as_mapping_get(item).and_then(Value::as_integer);
        assert_eq!(count("Power Bombs"), Some(0));
        assert_eq!(count("Missile"), Some(5));
        assert_eq!(count("Super Missile"), Some(-2));
    }

    #[test]
    fn test_trigger_without_category_checks_the_top_level() {
        let raw = r#"
game:
  Test: 1
  Other: 0
triggers:
  - option_name: game
    option_result: Test
    options:
      Test:
        death_link: true
Test:
  death_link: false
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        assert_eq!(
            game_option(&resolved, "Test", "death_link").as_bool(),
            Some(true)
        );
    }

    #[test]
    fn test_null_category_means_the_top_level() {
        let raw = r#"
game: Test
description: before
triggers:
  - option_category: null
    option_name: game
    option_result: Test
    options:
      null:
        description: after
      "":
        name: Renamed
Test: {}
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        let top = |key| resolved.as_mapping_get(key).and_then(Value::as_str);
        assert_eq!(top("description"), Some("after"));
        assert_eq!(top("name"), Some("Renamed"));
    }

    #[test]
    fn test_trigger_on_a_list_fires_if_any_element_matches() {
        let raw = r#"
Test:
  mode: [hard, easy]
  difficulty: [normal, easy]
  death_link: false
  goal: beat_the_game
  triggers:
    - option_category: Test
      option_name: mode
      option_result: hard
      options:
        Test:
          death_link: true
    - option_category: Test
      option_name: difficulty
      option_result: hard
      options:
        Test:
          goal: never
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        assert_eq!(
            game_option(&resolved, "Test", "death_link").as_bool(),
            Some(true)
        );
        assert_eq!(
            game_option(&resolved, "Test", "goal").as_str(),
            Some("beat_the_game")
        );
    }

    #[test]
    fn test_trigger_with_zero_percentage_never_fires() {
        let raw = r#"
Test:
  mode: hard
  death_link: false
  triggers:
    - option_category: Test
      option_name: mode
      option_result: hard
      percentage: 0
      options:
        Test:
          death_link: true
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        assert_eq!(
            game_option(&resolved, "Test", "death_link").as_bool(),
            Some(false)
        );
    }

    #[test]
    fn test_linked_options_apply_before_triggers() {
        let raw = r#"
linked_options:
  - name: Hard mode
    percentage: 25
    options:
      Test:
        mode: hard
        +start_inventory:
          Sword: 1
  - name: Never
    percentage: 0
    options:
      Test:
        goal: never
triggers:
  - option_category: Test
    option_name: mode
    option_result: hard
    options:
      Test:
        death_link: true
Test:
  mode: easy
  goal: beat_the_game
  death_link: false
  start_inventory:
    Sword: 1
"#;
        let resolved = resolve_triggers(&parse_yaml(raw));
        assert_eq!(
            game_option(&resolved, "Test", "mode").as_str(),
            Some("hard")
        );
        assert_eq!(
            game_option(&resolved, "Test", "death_link").as_bool(),
            Some(true)
        );
        let inventory = game_option(&resolved, "Test", "start_inventory");
        let swords = inventory.as_mapping_get("Sword");
        assert_eq!(swords.and_then(Value::as_integer), Some(2));
        assert_eq!(
            game_option(&resolved, "Test", "goal").as_str(),
            Some("beat_the_game")
        );
    }

    #[test]
    fn test_merge_removal_unset_option_takes_value_as_is() {
        let removal = parse_yaml("- Morph Ball\n");
        assert_eq!(merge_removal(None, &removal), removal);
    }
}
