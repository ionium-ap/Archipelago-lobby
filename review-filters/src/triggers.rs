use std::collections::HashMap;

use saphyr::{ScalarOwned, YamlOwned as Value};

use crate::yaml_value_as_string;

fn is_trueish_match(option_val: &Value, trigger_result: &str) -> bool {
    yaml_value_as_string(option_val).is_some_and(|s| s == trigger_result)
}

fn value_can_match(val: &Value, trigger_result: &str) -> bool {
    if is_trueish_match(val, trigger_result) {
        return true;
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

struct Trigger {
    option_category: String,
    option_name: String,
    option_result: String,
    options: Vec<(String, String, Value, bool)>, // (game, key, value, is_additive)
}

fn parse_trigger(trigger_yaml: &Value) -> Option<Trigger> {
    let category = trigger_yaml.as_mapping_get("option_category")?.as_str()?;
    let name = trigger_yaml.as_mapping_get("option_name")?.as_str()?;
    let result_str = yaml_value_as_string(trigger_yaml.as_mapping_get("option_result")?)?;

    let options_map = trigger_yaml.as_mapping_get("options")?;
    let mut options = Vec::new();

    if let Some(outer) = options_map.as_mapping() {
        for (game_key, game_opts) in outer.iter() {
            let game_name = game_key.as_str()?;
            if let Some(inner) = game_opts.as_mapping() {
                for (opt_key, opt_val) in inner.iter() {
                    let raw_key = opt_key.as_str()?;
                    let is_additive = raw_key.starts_with('+');
                    let key = if is_additive { &raw_key[1..] } else { raw_key };
                    options.push((
                        game_name.to_string(),
                        key.to_string(),
                        opt_val.clone(),
                        is_additive,
                    ));
                }
            }
        }
    }

    Some(Trigger {
        option_category: category.to_string(),
        option_name: name.to_string(),
        option_result: result_str,
        options,
    })
}

fn parse_triggers_from(val: &Value) -> impl Iterator<Item = Trigger> + '_ {
    val.as_sequence()
        .into_iter()
        .flatten()
        .filter_map(parse_trigger)
}

fn collect_triggers(yaml: &Value) -> Vec<Trigger> {
    let mut triggers = Vec::new();

    if let Some(top_triggers) = yaml.as_mapping_get("triggers") {
        triggers.extend(parse_triggers_from(top_triggers));
    }

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

pub fn resolve_triggers(yaml: &Value) -> Value {
    let triggers = collect_triggers(yaml);
    if triggers.is_empty() {
        return yaml.clone();
    }

    let mut current = yaml.clone();
    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 50;

    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            break;
        }

        let triggers = collect_triggers(&current);
        // game -> key -> value
        let mut changes: HashMap<(&str, &str), Value> = HashMap::new();

        for trigger in &triggers {
            let option_val = current
                .as_mapping_get(&trigger.option_category)
                .and_then(|game| game.as_mapping_get(&trigger.option_name));

            let Some(option_val) = option_val else {
                continue;
            };

            if !value_can_match(option_val, &trigger.option_result) {
                continue;
            }

            for (game, key, value, is_additive) in &trigger.options {
                let Some(game_section) = current.as_mapping_get(game) else {
                    continue;
                };

                if *is_additive {
                    let existing = game_section.as_mapping_get(key);
                    let merged = merge_additive(existing, value);
                    if existing.is_some_and(|e| e == &merged) {
                        continue;
                    }
                    changes.insert((game, key), merged);
                } else {
                    let existing = game_section.as_mapping_get(key);
                    if existing.is_some_and(|e| yaml_values_equal(e, value)) {
                        continue;
                    }
                    changes.insert((game, key), value.clone());
                }
            }
        }

        if changes.is_empty() {
            break;
        }

        set_yaml_values(&mut current, changes);
    }

    current
}

fn yaml_values_equal(a: &Value, b: &Value) -> bool {
    if let (Some(a_str), Some(b_str)) = (a.as_str(), b.as_str()) {
        return a_str == b_str;
    }
    if let (Some(a_int), Some(b_int)) = (a.as_integer(), b.as_integer()) {
        return a_int == b_int;
    }
    if let (Some(a_bool), Some(b_bool)) = (a.as_bool(), b.as_bool()) {
        return a_bool == b_bool;
    }
    false
}

/// Applies a `+option` from a trigger, following `update_weights` in Archipelago's
/// Generate.py: an option that isn't set yet takes the addition as is, two mappings add up
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
            merged.insert(key.clone(), sum.unwrap_or_else(|| value.clone()));
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

/// Writes each change into its game's section of the parsed YAML, leaving every other node
/// as it was parsed. A game whose section is missing or isn't a mapping is skipped.
fn set_yaml_values(yaml: &mut Value, changes: HashMap<(&str, &str), Value>) {
    for ((game, key), value) in changes {
        let Some(game_section) = yaml.as_mapping_get_mut(game) else {
            continue;
        };
        if let Some(existing) = game_section.as_mapping_get_mut(key) {
            *existing = value;
        } else if let Some(game_map) = game_section.as_mapping_mut() {
            game_map.insert(Value::Value(ScalarOwned::String(key.to_string())), value);
        }
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
    fn test_max_iteration_cap() {
        // Two triggers that fire each other forever
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
        let yaml = parse_yaml(raw);
        // Should not infinite loop — just terminates after MAX_ITERATIONS
        let _resolved = resolve_triggers(&yaml);
    }
}
