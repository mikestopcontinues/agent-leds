//! Settings by name, as the command line shows and changes them: a dotted
//! key such as `snake.step_ms`, and a value written the way it is typed.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::config::Config;

/// Every setting, with what it means.
const SETTINGS: [(&str, &str); 14] = [
    ("brightness", "Overall brightness, 0-255"),
    (
        "front_led",
        "The light that shows the front of the queue: 0 or 7; light 7 is the far end in the slot",
    ),
    (
        "balance",
        "How bright the red, green and blue dies look relative to each other: three numbers above 0",
    ),
    (
        "keep_read_minutes",
        "How long a stopped session keeps its light after you look at it, 0-1440",
    ),
    (
        "abandon_days",
        "How long a session that reports nothing keeps its light, 1-365",
    ),
    ("colors.error", "The hue of an error, RRGGBB"),
    ("colors.question", "The hue of a question, RRGGBB"),
    ("colors.stopped", "The hue of a stopped session, RRGGBB"),
    ("colors.working", "The hue of a working session, RRGGBB"),
    (
        "snake.head",
        "Brightness of the snake's front, middle and back lights, 0-1 each; 0 leaves an end out",
    ),
    (
        "snake.gap",
        "Whether a dark light separates the head from the tail: true or false",
    ),
    (
        "snake.tail",
        "Brightness of the queue while the head pulls it in, 0-1",
    ),
    (
        "snake.fade_out_ms",
        "How long the old queue takes to go dark, 1-5000",
    ),
    (
        "snake.step_ms",
        "How long the head, and then the wave, takes to move on one light, 1-5000",
    ),
];

/// The help that lists the settings, with their defaults.
pub fn help() -> String {
    let defaults = settings(&Config::default());
    let width = SETTINGS.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    let lines: Vec<String> = SETTINGS
        .iter()
        .map(|(key, about)| {
            let default = defaults
                .iter()
                .find(|(name, _)| name == key)
                .map_or(String::new(), |(_, value)| format!(" [default: {value}]"));
            format!("  {key:<width$}  {about}{default}")
        })
        .collect();
    format!(
        "Settings:\n{}\n\nExamples:\n  agent-leds config snake.step_ms 150\n  agent-leds config colors.question ff7000\n  agent-leds config balance 0.32 0.54 0.14",
        lines.join("\n")
    )
}

/// Every setting of `config` with its value as it would be typed.
pub fn settings(config: &Config) -> Vec<(String, String)> {
    let mut settings = Vec::new();
    flatten("", &serde_json::to_value(config).unwrap(), &mut settings);
    settings
}

fn flatten(prefix: &str, value: &Value, settings: &mut Vec<(String, String)>) {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                flatten(&format!("{prefix}{name}."), value, settings);
            }
        }
        value => settings.push((prefix.trim_end_matches('.').to_owned(), typed(value))),
    }
}

fn typed(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(typed).collect::<Vec<_>>().join(" "),
        value => value.to_string(),
    }
}

/// The settings `key` names: one, or every setting in a group such as
/// `snake`, or every setting at all without a key.
pub fn shown(config: &Config, key: Option<&str>) -> Result<Vec<(String, String)>> {
    let shown: Vec<_> = settings(config)
        .into_iter()
        .filter(|(name, _)| {
            key.is_none_or(|key| name == key || name.starts_with(&format!("{key}.")))
        })
        .collect();
    if shown.is_empty() {
        bail!(
            "no setting named {:?}; see `agent-leds config -h`",
            key.unwrap_or_default()
        );
    }
    Ok(shown)
}

/// `config` with the setting `key` changed to `values`, which hold one
/// value, or one per item of a list.
pub fn set(config: &Config, key: &str, values: &[String]) -> Result<Config> {
    let mut changed = serde_json::to_value(config)?;
    let mut setting = &mut changed;
    for name in key.split('.') {
        setting = match setting {
            Value::Object(fields) => fields.get_mut(name),
            _ => None,
        }
        .with_context(|| format!("no setting named {key:?}; see `agent-leds config -h`"))?;
    }
    *setting = match &*setting {
        Value::Object(fields) => bail!("name one of {key}'s settings: {}", names(key, fields)),
        Value::Array(items) => Value::Array(
            values
                .iter()
                .map(|text| parsed(items.first(), text))
                .collect(),
        ),
        single => match values {
            [text] => parsed(Some(single), text),
            _ => bail!("{key} takes one value"),
        },
    };
    let config: Config =
        serde_json::from_value(changed).with_context(|| format!("setting {key}"))?;
    config.check()?;
    Ok(config)
}

fn names(key: &str, fields: &Map<String, Value>) -> String {
    let names: Vec<String> = fields.keys().map(|name| format!("{key}.{name}")).collect();
    names.join(", ")
}

/// A typed value read as the kind of value it replaces: text stays text,
/// so that a color such as 123456 is not taken for a number.
fn parsed(like: Option<&Value>, text: &str) -> Value {
    match like {
        Some(Value::String(_)) => Value::String(text.to_owned()),
        _ => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Color;

    fn set(key: &str, values: &[&str]) -> Result<Config> {
        let values: Vec<String> = values.iter().map(|&text| text.to_owned()).collect();
        super::set(&Config::default(), key, &values)
    }

    #[test]
    fn the_help_names_every_setting() {
        let keys: Vec<String> = settings(&Config::default())
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let named: Vec<&str> = SETTINGS.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, named);
    }

    #[test]
    fn settings_read_as_they_are_typed() {
        let config = Config::default();
        let shown = settings(&config);
        assert!(shown.contains(&("balance".into(), "0.32 0.54 0.14".into())));
        assert!(shown.contains(&("colors.error".into(), "#FF0000".into())));
        assert!(shown.contains(&("snake.gap".into(), "true".into())));
    }

    #[test]
    fn a_key_shows_one_setting_or_a_group() {
        let config = Config::default();
        assert_eq!(shown(&config, None).unwrap().len(), SETTINGS.len());
        assert_eq!(shown(&config, Some("snake")).unwrap().len(), 5);
        assert_eq!(
            shown(&config, Some("brightness")).unwrap(),
            [("brightness".to_owned(), "255".to_owned())]
        );
        assert!(shown(&config, Some("snak")).is_err());
    }

    #[test]
    fn a_setting_changes_by_name() {
        assert_eq!(set("snake.step_ms", &["150"]).unwrap().snake.step_ms, 150);
        assert!(!set("snake.gap", &["false"]).unwrap().snake.gap);
        assert_eq!(
            set("balance", &["1", "2", "3"]).unwrap().balance,
            [1.0, 2.0, 3.0]
        );
        assert_eq!(
            set("colors.question", &["123456"]).unwrap().colors.question,
            Color(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn a_value_that_cannot_be_used_is_refused() {
        let problem = |key, values| format!("{:#}", set(key, values).unwrap_err());
        assert!(problem("snake.speed", &["1"]).contains("no setting named"));
        assert!(problem("snake", &["1"]).contains("snake.step_ms"));
        assert!(problem("brightness", &["1", "2"]).contains("one value"));
        assert!(problem("brightness", &["300"]).contains("brightness"));
        assert!(problem("balance", &["1", "2"]).contains("balance"));
        assert!(problem("front_led", &["3"]).contains("front"));
        assert!(problem("colors.error", &["red"]).contains("RRGGBB"));
    }
}
