use std::sync::LazyLock;

use saphyr::YamlOwned as Value;

use crate::Filter;
use crate::rules::{self, Outcome, RuleResult, Severity};

pub struct RoomYaml {
    pub player_name: String,
}

pub trait BuiltinRule: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn evaluate(
        &self,
        yaml: &Value,
        game_name: &str,
        player_name: &str,
        room_yamls: &[RoomYaml],
    ) -> RuleResult;
}

/// The JSON files of `review-filters/builtins/`, as `(file stem, content)`
const BUILTIN_FILTER_FILES: &[(&str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/builtin_filters.rs"));

static BUILTIN_RULES: LazyLock<Vec<Box<dyn BuiltinRule>>> = LazyLock::new(|| {
    let mut rules: Vec<Box<dyn BuiltinRule>> = vec![Box::new(NoCrossPlando)];
    for (id, content) in BUILTIN_FILTER_FILES {
        // The tests check every file, so a broken one doesn't get this far
        let filter = parse_builtin_filter(content)
            .unwrap_or_else(|e| panic!("builtin filter {id} is invalid: {e}"));
        rules.push(Box::new(FilterBuiltin {
            id: id.to_string(),
            filter,
        }));
    }
    rules
});

/// Every builtin filter: the ones written in code, then the JSON ones by file name
pub fn builtin_rules() -> &'static [Box<dyn BuiltinRule>] {
    &BUILTIN_RULES
}

pub fn builtin_rule_info() -> Vec<BuiltinRuleInfo> {
    builtin_rules()
        .iter()
        .map(|r| BuiltinRuleInfo {
            id: r.id().to_string(),
            name: r.name().to_string(),
            description: r.description().to_string(),
        })
        .collect()
}

#[derive(serde::Serialize)]
pub struct BuiltinRuleInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// A builtin file holds one filter in the format of an export's entries
fn parse_builtin_filter(content: &str) -> Result<Filter, String> {
    let filter: Filter = serde_json::from_str(content).map_err(|e| e.to_string())?;
    filter.validate()?;
    Ok(filter)
}

/// A filter of `review-filters/builtins/`, which is evaluated like a reviewer's
struct FilterBuiltin {
    id: String,
    filter: Filter,
}

impl BuiltinRule for FilterBuiltin {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.filter.rule.name
    }

    fn description(&self) -> &str {
        self.filter.description.as_deref().unwrap_or("")
    }

    fn evaluate(
        &self,
        yaml: &Value,
        game_name: &str,
        _player_name: &str,
        _room_yamls: &[RoomYaml],
    ) -> RuleResult {
        rules::evaluate_rule(&self.filter.rule, yaml, game_name)
    }
}

struct NoCrossPlando;

impl BuiltinRule for NoCrossPlando {
    fn id(&self) -> &str {
        "no_cross_plando"
    }

    fn name(&self) -> &str {
        "No cross-player plando"
    }

    fn description(&self) -> &str {
        "Checks that plando sections don't reference other players' slot names"
    }

    fn evaluate(
        &self,
        yaml: &Value,
        game_name: &str,
        player_name: &str,
        room_yamls: &[RoomYaml],
    ) -> RuleResult {
        let other_names: Vec<&str> = room_yamls
            .iter()
            .filter(|y| y.player_name != player_name)
            .map(|y| y.player_name.as_str())
            .collect();

        let Some(game_section) = yaml.as_mapping_get(game_name) else {
            return RuleResult {
                rule_name: self.name().to_string(),
                outcome: Outcome::Skipped,
                severity: Severity::Error,
                detail: None,
            };
        };

        let Some(plando) = game_section.as_mapping_get("plando_items") else {
            // No plando_items = pass (nothing to check)
            return RuleResult {
                rule_name: self.name().to_string(),
                outcome: Outcome::Pass,
                severity: Severity::Error,
                detail: None,
            };
        };

        let mut violations = Vec::new();

        if let Some(seq) = plando.as_sequence() {
            for entry in seq {
                check_plando_entry_for_cross_refs(entry, &other_names, &mut violations);
            }
        }

        if violations.is_empty() {
            RuleResult {
                rule_name: self.name().to_string(),
                outcome: Outcome::Pass,
                severity: Severity::Error,
                detail: None,
            }
        } else {
            RuleResult {
                rule_name: self.name().to_string(),
                outcome: Outcome::Fail,
                severity: Severity::Error,
                detail: Some(format!(
                    "Plando references other players: {}",
                    violations.join(", ")
                )),
            }
        }
    }
}

fn check_plando_entry_for_cross_refs(
    entry: &Value,
    other_names: &[&str],
    violations: &mut Vec<String>,
) {
    for field in &["from_player", "to_player", "world"] {
        if let Some(val) = entry.as_mapping_get(field) {
            check_value_for_player_names(val, field, other_names, violations);
        }
    }
}

fn check_value_for_player_names(
    val: &Value,
    field_name: &str,
    other_names: &[&str],
    violations: &mut Vec<String>,
) {
    if let Some(name) = val.as_str()
        && other_names.contains(&name)
    {
        violations.push(format!("{}: {}", field_name, name));
    }
    if let Some(seq) = val.as_sequence() {
        for item in seq {
            if let Some(name) = item.as_str()
                && other_names.contains(&name)
            {
                violations.push(format!("{}: {}", field_name, name));
            }
        }
    }
    if let Some(map) = val.as_mapping() {
        for (key, _) in map.iter() {
            if let Some(name) = key.as_str()
                && other_names.contains(&name)
            {
                violations.push(format!("{}: {}", field_name, name));
            }
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
    fn test_no_plando_passes() {
        let yaml = parse_yaml("Test:\n  death_link: true\n");
        let room = vec![
            RoomYaml {
                player_name: "Player1".into(),
            },
            RoomYaml {
                player_name: "Player2".into(),
            },
        ];
        let result = NoCrossPlando.evaluate(&yaml, "Test", "Player1", &room);
        assert_eq!(result.outcome, Outcome::Pass);
    }

    #[test]
    fn test_plando_no_cross_ref_passes() {
        let raw = r#"
Test:
  plando_items:
    - items:
        Sword: 1
      locations:
        - "Starting Chest"
"#;
        let yaml = parse_yaml(raw);
        let room = vec![
            RoomYaml {
                player_name: "Player1".into(),
            },
            RoomYaml {
                player_name: "Player2".into(),
            },
        ];
        let result = NoCrossPlando.evaluate(&yaml, "Test", "Player1", &room);
        assert_eq!(result.outcome, Outcome::Pass);
    }

    #[test]
    fn test_plando_with_cross_ref_fails() {
        let raw = r#"
Test:
  plando_items:
    - items:
        Sword: 1
      world: Player2
"#;
        let yaml = parse_yaml(raw);
        let room = vec![
            RoomYaml {
                player_name: "Player1".into(),
            },
            RoomYaml {
                player_name: "Player2".into(),
            },
        ];
        let result = NoCrossPlando.evaluate(&yaml, "Test", "Player1", &room);
        assert_eq!(result.outcome, Outcome::Fail);
        assert!(result.detail.unwrap().contains("Player2"));
    }

    #[test]
    fn test_plando_from_player_cross_ref() {
        let raw = r#"
Test:
  plando_items:
    - items:
        Sword: 1
      from_player: Player2
"#;
        let yaml = parse_yaml(raw);
        let room = vec![
            RoomYaml {
                player_name: "Player1".into(),
            },
            RoomYaml {
                player_name: "Player2".into(),
            },
        ];
        let result = NoCrossPlando.evaluate(&yaml, "Test", "Player1", &room);
        assert_eq!(result.outcome, Outcome::Fail);
    }

    #[test]
    fn test_self_plando_does_not_false_positive() {
        let raw = r#"
Test:
  plando_items:
    - items:
        Sword: 1
      world: Player1
"#;
        let yaml = parse_yaml(raw);
        let room = vec![
            RoomYaml {
                player_name: "Player1".into(),
            },
            RoomYaml {
                player_name: "Player2".into(),
            },
        ];
        let result = NoCrossPlando.evaluate(&yaml, "Test", "Player1", &room);
        assert_eq!(result.outcome, Outcome::Pass);
    }
}
