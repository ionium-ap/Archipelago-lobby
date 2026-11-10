//! Filters that flag the YAMLs of a room for its reviewers.
//!
//! A filter is a rule from [`rules`] with a description. Some are builtin, either written in
//! code or as JSON files in `review-filters/builtins/`, and the others are defined by a room's
//! reviewers. Filters are exported and imported as [`FilterExport`].

use saphyr::{LoadableYamlNode, YamlOwned as Value};
use serde::{Deserialize, Serialize};

pub mod builtin;
pub mod rules;
pub mod triggers;

pub use builtin::{BuiltinRule, RoomYaml, builtin_rules};
pub use rules::{Outcome, Predicate, Rule, RuleCheck, Severity};

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_DESCRIPTION_LEN: usize = 2000;
/// How many conditions a filter can have, counting groups, in `when` and `then` together
pub const MAX_CONDITIONS: usize = 100;
/// How long a filter's regexes can be
pub const MAX_REGEX_LEN: usize = 1000;

pub fn yaml_value_as_string(val: &Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        return Some(s.to_string());
    }
    if let Some(b) = val.as_bool() {
        return Some(b.to_string());
    }
    if let Some(i) = val.as_integer() {
        return Some(i.to_string());
    }
    None
}

/// A rule with what reviewers need to know about it
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Filter {
    #[serde(flatten)]
    pub rule: Rule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Filter {
    /// Trims the texts of the filter, and leaves out the ones that are blank
    pub fn normalize(&mut self) {
        self.rule.name = self.rule.name.trim().to_string();
        let trimmed = |text: &Option<String>| {
            text.as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        self.description = trimmed(&self.description);
        self.rule.game = trimmed(&self.rule.game);
    }

    pub fn validate(&self) -> Result<(), String> {
        let name = self.rule.name.trim();
        if name.is_empty() {
            return Err("A filter needs a name".into());
        }
        // Counted in characters, as the editor's `maxlength` counts them, not in UTF-8 bytes
        if name.chars().count() > MAX_NAME_LEN {
            return Err(format!(
                "Filter names can't be longer than {MAX_NAME_LEN} characters"
            ));
        }
        if self
            .description
            .as_ref()
            .is_some_and(|d| d.chars().count() > MAX_DESCRIPTION_LEN)
        {
            return Err(format!(
                "Filter descriptions can't be longer than {MAX_DESCRIPTION_LEN} characters"
            ));
        }
        if self.rule.game.as_ref().is_some_and(|g| g.trim().is_empty()) {
            return Err("A filter's game can't be blank, leave it out to check every game".into());
        }

        // Checked before `Rule::validate`, which compiles the regexes with a larger limit
        // than evaluation does
        let mut conditions = 0;
        let predicates = self
            .rule
            .when
            .iter()
            .chain(std::iter::once(&self.rule.then));
        for predicate in predicates {
            validate_conditions(predicate, &mut conditions)
                .map_err(|e| format!("Filter \"{name}\": {e}"))?;
        }

        self.rule
            .validate()
            .map_err(|e| format!("Filter \"{name}\": {e}"))
    }
}

/// What `Rule::validate` lets through and shouldn't: groups with nothing in them, which are
/// always true for "all of", too many conditions, and regexes that evaluation can't compile
fn validate_conditions(predicate: &Predicate, conditions: &mut usize) -> Result<(), String> {
    *conditions += 1;
    if *conditions > MAX_CONDITIONS {
        return Err(format!(
            "A filter can't have more than {MAX_CONDITIONS} conditions"
        ));
    }

    match predicate {
        Predicate::Check { check, .. } => {
            if let RuleCheck::Regex { pattern } = check {
                if pattern.len() > MAX_REGEX_LEN {
                    return Err(format!(
                        "Regexes can't be longer than {MAX_REGEX_LEN} characters"
                    ));
                }
                // The same limit as evaluation, so that what's saved can be evaluated
                regex::RegexBuilder::new(pattern)
                    .size_limit(rules::REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|e| format!("Invalid regex: {e}"))?;
            }
            Ok(())
        }
        Predicate::And { predicates }
        | Predicate::Or { predicates }
        | Predicate::AtLeast { predicates, .. } => {
            if predicates.is_empty() {
                return Err("A group needs at least one condition in it".into());
            }
            for predicate in predicates {
                validate_conditions(predicate, conditions)?;
            }
            Ok(())
        }
        Predicate::Not { predicate } => validate_conditions(predicate, conditions),
    }
}

pub const EXPORT_VERSION: u32 = 1;

/// The JSON that filters are exported as
#[derive(Debug, Serialize, Deserialize)]
pub struct FilterExport {
    pub version: u32,
    pub filters: Vec<Filter>,
}

impl FilterExport {
    pub fn new(filters: Vec<Filter>) -> Self {
        Self {
            version: EXPORT_VERSION,
            filters,
        }
    }
}

/// Reads filters to import, see [`parse_import_value`]
pub fn parse_import(json: &str, max_filters: usize) -> Result<Vec<Filter>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("This isn't JSON: {e}"))?;
    parse_import_value(value, max_filters)
}

/// Reads filters to import: an export of the lobby's, a single filter (the format of the
/// builtin files), a list of filters, or an export of a preset of community-ap-tools
/// (`[{"rule": ..., "position": ...}]`). Every filter is validated, unless there are more than
/// `max_filters` of them: validating compiles their regexes, which takes time.
pub fn parse_import_value(
    value: serde_json::Value,
    max_filters: usize,
) -> Result<Vec<Filter>, String> {
    #[derive(Deserialize)]
    struct PresetRule {
        rule: Filter,
        #[serde(default)]
        position: i32,
    }

    // Untagged enums only say that nothing matched, so each format is told apart beforehand,
    // and the error comes from the one the file looks like it is in
    let filters = match &value {
        serde_json::Value::Object(o) if o.contains_key("filters") => {
            if let Some(version) = o.get("version").and_then(|v| v.as_u64())
                && version > EXPORT_VERSION as u64
            {
                return Err(format!(
                    "This export is of version {version}, which is newer than this lobby knows"
                ));
            }
            serde_json::from_value::<FilterExport>(value)
                .map_err(|e| format!("Invalid export: {e}"))?
                .filters
        }
        serde_json::Value::Object(_) => {
            vec![
                serde_json::from_value::<Filter>(value)
                    .map_err(|e| format!("Invalid filter: {e}"))?,
            ]
        }
        serde_json::Value::Array(entries)
            if entries.first().is_some_and(|e| e.get("rule").is_some()) =>
        {
            let mut entries = serde_json::from_value::<Vec<PresetRule>>(value)
                .map_err(|e| format!("Invalid preset export: {e}"))?;
            entries.sort_by_key(|e| e.position);
            entries.into_iter().map(|e| e.rule).collect()
        }
        serde_json::Value::Array(_) => serde_json::from_value::<Vec<Filter>>(value)
            .map_err(|e| format!("Invalid filter list: {e}"))?,
        _ => return Err("Expected an export of filters".into()),
    };

    if filters.len() > max_filters {
        return Err(format!(
            "This file has {} filters, only {max_filters} more fit in the room",
            filters.len()
        ));
    }

    let mut filters = filters;
    for filter in &mut filters {
        filter.normalize();
        filter.validate()?;
    }

    Ok(filters)
}

/// A filter to evaluate, with how results should point back at it
pub struct ActiveFilter<'a> {
    pub id: String,
    pub builtin: bool,
    pub filter: FilterRef<'a>,
}

pub enum FilterRef<'a> {
    Custom(&'a Filter),
    Builtin(&'a dyn BuiltinRule),
}

/// A filter that matched a YAML, or couldn't be evaluated on it
#[derive(Debug, Serialize)]
pub struct Flag {
    pub filter_id: String,
    pub builtin: bool,
    pub name: String,
    pub severity: Severity,
    /// `Fail` when the filter matched, `Error` when it couldn't be evaluated
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The game the filter matched in, when the YAML can roll more than one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct YamlEvaluation {
    /// The games the YAML can roll
    pub games: Vec<String>,
    pub flags: Vec<Flag>,
    /// Why the YAML couldn't be read, in which case no filter was evaluated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

/// The games a YAML can roll: the `game` key, or the keys with a weight in it
pub fn extract_game_names(yaml: &Value) -> Vec<String> {
    let Some(game_val) = yaml.as_mapping_get("game") else {
        return vec![];
    };

    if let Some(name) = game_val.as_str() {
        return vec![name.to_string()];
    }

    if let Some(map) = game_val.as_mapping() {
        return map
            .iter()
            .filter(|(_, weight)| weight.as_integer().unwrap_or(0) != 0)
            .filter_map(|(key, _)| key.as_str().map(|s| s.to_string()))
            .collect();
    }

    vec![]
}

/// Evaluates filters on a YAML, after applying its triggers. `fallback_game` is used when the
/// YAML doesn't say which games it can roll. `room_yamls` are the players of the room, for the
/// builtins that look at the whole room.
pub fn evaluate_yaml(
    content: &str,
    fallback_game: &str,
    player_name: &str,
    filters: &[ActiveFilter<'_>],
    room_yamls: &[RoomYaml],
) -> YamlEvaluation {
    let parsed = match Value::load_from_str(content) {
        Ok(docs) => docs.into_iter().find(|doc| doc.is_mapping()),
        Err(e) => {
            return YamlEvaluation {
                games: vec![fallback_game.to_string()],
                flags: vec![],
                parse_error: Some(e.to_string()),
            };
        }
    };
    let Some(yaml) = parsed else {
        return YamlEvaluation {
            games: vec![fallback_game.to_string()],
            flags: vec![],
            parse_error: Some("The YAML has no options in it".into()),
        };
    };

    let resolved = triggers::resolve_triggers(&yaml);

    let mut games = extract_game_names(&resolved);
    if games.is_empty() {
        games.push(fallback_game.to_string());
    }
    let multi_game = games.len() > 1;

    let mut flags = Vec::new();
    for game in &games {
        for active in filters {
            let result = match active.filter {
                FilterRef::Custom(filter) => rules::evaluate_rule(&filter.rule, &resolved, game),
                FilterRef::Builtin(rule) => rule.evaluate(&resolved, game, player_name, room_yamls),
            };
            if !matches!(result.outcome, Outcome::Fail | Outcome::Error) {
                continue;
            }
            flags.push(Flag {
                filter_id: active.id.clone(),
                builtin: active.builtin,
                name: result.rule_name,
                severity: result.severity,
                outcome: result.outcome,
                detail: result.detail,
                game: multi_game.then(|| game.clone()),
            });
        }
    }

    YamlEvaluation {
        games,
        flags,
        parse_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(json: &str) -> Filter {
        serde_json::from_str(json).unwrap()
    }

    fn custom<'a>(id: &str, filter: &'a Filter) -> ActiveFilter<'a> {
        ActiveFilter {
            id: id.into(),
            builtin: false,
            filter: FilterRef::Custom(filter),
        }
    }

    const DEATH_LINK: &str = r#"{
        "name": "Death link",
        "description": "Death link is on",
        "game": null,
        "when": null,
        "then": {"type": "check", "path": "death_link", "check": {"type": "truthy"}},
        "severity": "warning"
    }"#;

    #[test]
    fn test_every_builtin_filter_is_valid() {
        // Loading them panics on an invalid one
        let ids = builtin_rules().iter().map(|r| r.id()).collect::<Vec<_>>();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            ids.len(),
            unique.len(),
            "builtin ids should be unique: {ids:?}"
        );
    }

    #[test]
    fn test_filter_round_trips_with_description() {
        let f = filter(DEATH_LINK);
        assert_eq!(f.description.as_deref(), Some("Death link is on"));
        let json = serde_json::to_string(&f).unwrap();
        let again: Filter = serde_json::from_str(&json).unwrap();
        assert_eq!(again.rule.name, "Death link");
        assert_eq!(again.description.as_deref(), Some("Death link is on"));
    }

    #[test]
    fn test_filter_without_description_has_none() {
        let f = filter(
            r#"{"name": "x", "then": {"type": "check", "path": "a", "check": {"type": "exists"}}, "severity": "info"}"#,
        );
        assert!(f.description.is_none());
        assert!(f.rule.game.is_none());
        assert!(!serde_json::to_string(&f).unwrap().contains("description"));
    }

    #[test]
    fn test_normalize_trims_and_drops_blanks() {
        let mut f = filter(DEATH_LINK);
        f.rule.name = "  Death link ".into();
        f.description = Some("   ".into());
        f.rule.game = Some(" Celeste ".into());
        f.normalize();
        assert_eq!(f.rule.name, "Death link");
        assert!(f.description.is_none());
        assert_eq!(f.rule.game.as_deref(), Some("Celeste"));

        f.rule.game = Some("  ".into());
        f.normalize();
        assert!(f.rule.game.is_none());
    }

    #[test]
    fn test_validate_rejects_blank_name() {
        let mut f = filter(DEATH_LINK);
        f.rule.name = "  ".into();
        assert!(f.validate().is_err());
    }

    #[test]
    fn test_validate_counts_characters_not_bytes() {
        // Three UTF-8 bytes each, so these fit in characters but not in bytes
        let mut f = filter(DEATH_LINK);
        f.rule.name = "名".repeat(MAX_NAME_LEN);
        f.description = Some("説".repeat(MAX_DESCRIPTION_LEN));
        assert_eq!(f.validate(), Ok(()));

        f.rule.name.push('名');
        assert!(f.validate().is_err());

        f.rule.name = "Death link".into();
        f.description.as_mut().unwrap().push('説');
        assert!(f.validate().is_err());
    }

    #[test]
    fn test_evaluate_flags_matching_yaml() {
        let f = filter(DEATH_LINK);
        let filters = [custom("1", &f)];
        let yaml = "name: P1\ngame: Test\nTest:\n  death_link: true\n";
        let eval = evaluate_yaml(yaml, "Test", "P1", &filters, &[]);
        assert_eq!(eval.games, vec!["Test"]);
        assert_eq!(eval.flags.len(), 1);
        assert_eq!(eval.flags[0].filter_id, "1");
        assert_eq!(eval.flags[0].outcome, Outcome::Fail);
        assert!(eval.flags[0].game.is_none());

        let yaml = "name: P1\ngame: Test\nTest:\n  death_link: false\n";
        let eval = evaluate_yaml(yaml, "Test", "P1", &filters, &[]);
        assert!(eval.flags.is_empty());
    }

    #[test]
    fn test_evaluate_applies_triggers() {
        let f = filter(DEATH_LINK);
        let filters = [custom("1", &f)];
        let yaml = r#"
name: P1
game: Test
Test:
  death_link: false
  goal: hard
triggers:
  - option_category: Test
    option_name: goal
    option_result: hard
    options:
      Test:
        death_link: true
"#;
        let eval = evaluate_yaml(yaml, "Test", "P1", &filters, &[]);
        assert_eq!(eval.flags.len(), 1);
    }

    #[test]
    fn test_evaluate_multi_game_names_the_game() {
        let f = filter(DEATH_LINK);
        let filters = [custom("1", &f)];
        let yaml = r#"
name: P1
game:
  A: 1
  B: 1
  C: 0
A:
  death_link: true
B:
  death_link: false
"#;
        let eval = evaluate_yaml(yaml, "Random", "P1", &filters, &[]);
        assert_eq!(eval.games, vec!["A", "B"]);
        assert_eq!(eval.flags.len(), 1);
        assert_eq!(eval.flags[0].game.as_deref(), Some("A"));
    }

    #[test]
    fn test_evaluate_runs_builtins_with_room() {
        let rule = builtin_rules()
            .iter()
            .find(|r| r.id() == "no_cross_plando")
            .unwrap();
        let filters = [ActiveFilter {
            id: rule.id().into(),
            builtin: true,
            filter: FilterRef::Builtin(rule.as_ref()),
        }];
        let room = [
            RoomYaml {
                player_name: "P1".into(),
            },
            RoomYaml {
                player_name: "P2".into(),
            },
        ];
        let yaml = "name: P1\ngame: Test\nTest:\n  plando_items:\n    - items:\n        Sword: 1\n      world: P2\n";
        let eval = evaluate_yaml(yaml, "Test", "P1", &filters, &room);
        assert_eq!(eval.flags.len(), 1);
        assert!(eval.flags[0].builtin);
        assert_eq!(eval.flags[0].severity, Severity::Error);
    }

    #[test]
    fn test_evaluate_reports_unparseable_yaml() {
        let eval = evaluate_yaml("a: [", "Test", "P1", &[], &[]);
        assert!(eval.parse_error.is_some());
        assert!(eval.flags.is_empty());
    }

    #[test]
    fn test_import_formats() {
        let export = format!(r#"{{"version": 1, "filters": [{DEATH_LINK}]}}"#);
        assert_eq!(parse_import(&export, usize::MAX).unwrap().len(), 1);

        let list = format!("[{DEATH_LINK}, {DEATH_LINK}]");
        assert_eq!(parse_import(&list, usize::MAX).unwrap().len(), 2);

        let preset = r#"[
            {"rule": {"name": "second", "game": null, "when": null, "then": {"type": "check", "path": "a", "check": {"type": "exists"}}, "severity": "info"}, "position": 1},
            {"rule": {"name": "first", "game": null, "when": null, "then": {"type": "check", "path": "a", "check": {"type": "exists"}}, "severity": "info"}, "position": 0}
        ]"#;
        let filters = parse_import(preset, usize::MAX).unwrap();
        assert_eq!(filters[0].rule.name, "first");
        assert_eq!(filters[1].rule.name, "second");

        assert!(parse_import("[]", usize::MAX).unwrap().is_empty());

        // A single filter, like the builtin files
        assert_eq!(parse_import(DEATH_LINK, usize::MAX).unwrap().len(), 1);
    }

    fn filter_with_then(then: &str) -> Filter {
        filter(&format!(
            r#"{{"name": "x", "then": {then}, "severity": "info"}}"#
        ))
    }

    #[test]
    fn test_validate_rejects_empty_groups() {
        for group in [
            r#"{"type": "and", "predicates": []}"#,
            r#"{"type": "or", "predicates": []}"#,
            r#"{"type": "not", "predicate": {"type": "or", "predicates": []}}"#,
            r#"{"type": "and", "predicates": [{"type": "or", "predicates": []}]}"#,
        ] {
            let error = filter_with_then(group).validate().unwrap_err();
            assert!(error.contains("at least one condition"), "{group}: {error}");
        }
        assert!(
            filter_with_then(r#"{"type": "and", "predicates": [{"type": "check", "path": "a", "check": {"type": "exists"}}]}"#)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn test_validate_regexes_with_the_evaluation_limit() {
        // Compiles under the regex crate's default limit, not under evaluation's
        let big = filter_with_then(
            r#"{"type": "check", "path": "a", "check": {"type": "regex", "pattern": "\\w{100}"}}"#,
        );
        assert!(big.validate().unwrap_err().contains("Invalid regex"));

        let long = "a".repeat(MAX_REGEX_LEN + 1);
        let long = filter_with_then(&format!(
            r#"{{"type": "check", "path": "a", "check": {{"type": "regex", "pattern": "{long}"}}}}"#
        ));
        assert!(long.validate().unwrap_err().contains("longer than"));

        let fine = filter_with_then(
            r#"{"type": "check", "path": "a", "check": {"type": "regex", "pattern": "^foo\\d+$"}}"#,
        );
        assert!(fine.validate().is_ok());
    }

    #[test]
    fn test_validate_caps_conditions() {
        let check = r#"{"type": "check", "path": "a", "check": {"type": "exists"}}"#;
        let group = |n: usize| {
            format!(
                r#"{{"type": "or", "predicates": [{}]}}"#,
                vec![check; n].join(",")
            )
        };
        // The group counts as one
        assert!(
            filter_with_then(&group(MAX_CONDITIONS - 1))
                .validate()
                .is_ok()
        );
        let error = filter_with_then(&group(MAX_CONDITIONS))
            .validate()
            .unwrap_err();
        assert!(error.contains("more than"), "{error}");
    }

    #[test]
    fn test_import_counts_before_validating() {
        // Invalid filters, but too many of them is what's reported
        let bad = r#"{"name": "", "then": {"type": "check", "path": "a", "check": {"type": "exists"}}, "severity": "info"}"#;
        let list = format!("[{bad}, {bad}, {bad}]");
        let error = parse_import(&list, 2).unwrap_err();
        assert!(error.contains("only 2 more"), "{error}");
        assert!(parse_import(&list, 3).unwrap_err().contains("needs a name"));
    }

    #[test]
    fn test_import_rejects_bad_input() {
        assert!(parse_import("not json", usize::MAX).is_err());
        assert!(parse_import("42", usize::MAX).is_err());
        assert!(parse_import(r#"{"version": 2, "filters": []}"#, usize::MAX).is_err());
        assert!(parse_import(r#"[{"name": "x"}]"#, usize::MAX).is_err());
        // Valid JSON, invalid filter
        let bad_regex = r#"[{"name": "x", "then": {"type": "check", "path": "a", "check": {"type": "regex", "pattern": "("}}, "severity": "info"}]"#;
        assert!(parse_import(bad_regex, usize::MAX).is_err());
    }
}
