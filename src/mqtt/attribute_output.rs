/// Flattens a device-state JSON object into `(topic_suffix, raw_value)` pairs
/// for `advanced.output: attribute`/`attribute_and_json` mode -- mirrors real
/// zigbee2mqtt's `Controller.iteratePayloadAttributeOutput` exactly:
///
/// - top-level keys become the topic suffix directly; nested object keys
///   join with `-` (e.g. `{"color": {"x": 1}}` -> topic `color-x`)
/// - `null` -> empty string payload
/// - arrays -> comma-joined string of each element's plain (unquoted) form
/// - a `color` object containing `r`/`g`/`b` is a special case: it publishes
///   as a single `color` topic with value `"r,g,b"`, instead of recursing
/// - strings publish raw/unquoted; other scalars (numbers, bools) publish
///   their plain string form
use serde_json::{Map, Value};

pub fn flatten_attribute_output(payload: &Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    flatten_into(payload, "", &mut out);
    out
}

fn flatten_into(payload: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, String)>) {
    for (key, value) in payload {
        let topic = format!("{prefix}{key}");

        if key == "color" {
            if let Some(obj) = value.as_object() {
                if let (Some(r), Some(g), Some(b)) = (obj.get("r"), obj.get("g"), obj.get("b")) {
                    let joined = [r, g, b]
                        .iter()
                        .map(|v| scalar_to_string(v))
                        .collect::<Vec<_>>()
                        .join(",");
                    out.push((topic, joined));
                    continue;
                }
            }
        }

        match value {
            Value::Null => out.push((topic, String::new())),
            Value::Array(arr) => {
                let joined = arr
                    .iter()
                    .map(scalar_to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                out.push((topic, joined));
            }
            Value::Object(obj) => flatten_into(obj, &format!("{topic}-"), out),
            Value::String(s) => out.push((topic, s.clone())),
            other => out.push((topic, scalar_to_string(other))),
        }
    }
}

/// Plain (unquoted) string form of a JSON scalar, matching JS's `${x}`
/// template-literal coercion / `String(x)`: strings pass through as-is,
/// `null` becomes the literal text `"null"` (only relevant inside arrays --
/// a *top-level* `null` is handled separately as an empty string), and
/// numbers/bools use their plain decimal/boolean text form.
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flatten(v: Value) -> Vec<(String, String)> {
        flatten_attribute_output(v.as_object().unwrap())
    }

    #[test]
    fn flat_string_value_is_unquoted() {
        let pairs = flatten(json!({"state": "ON"}));
        assert_eq!(pairs, vec![("state".to_string(), "ON".to_string())]);
    }

    #[test]
    fn flat_number_value() {
        let pairs = flatten(json!({"battery": 80}));
        assert_eq!(pairs, vec![("battery".to_string(), "80".to_string())]);
    }

    #[test]
    fn flat_float_value() {
        let pairs = flatten(json!({"temperature": 22.5}));
        assert_eq!(pairs, vec![("temperature".to_string(), "22.5".to_string())]);
    }

    #[test]
    fn flat_bool_value() {
        let pairs = flatten(json!({"occupancy": true}));
        assert_eq!(pairs, vec![("occupancy".to_string(), "true".to_string())]);
    }

    #[test]
    fn null_value_is_empty_string() {
        let pairs = flatten(json!({"battery": null}));
        assert_eq!(pairs, vec![("battery".to_string(), String::new())]);
    }

    #[test]
    fn nested_object_joins_with_dash_and_does_not_publish_itself() {
        let pairs = flatten(json!({"color": {"x": 0.5, "y": 0.3}}));
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("color-x".to_string(), "0.5".to_string())));
        assert!(pairs.contains(&("color-y".to_string(), "0.3".to_string())));
        assert!(!pairs.iter().any(|(k, _)| k == "color"));
    }

    #[test]
    fn array_of_numbers_is_comma_joined() {
        let pairs = flatten(json!({"list": [1, 2, 3]}));
        assert_eq!(pairs, vec![("list".to_string(), "1,2,3".to_string())]);
    }

    #[test]
    fn array_with_null_element_stringifies_to_literal_null() {
        // Only a *top-level* null becomes "" -- inside an array it follows
        // JS `${x}` coercion, which stringifies null as the text "null".
        let pairs = flatten(json!({"list": [1, null, 3]}));
        assert_eq!(pairs, vec![("list".to_string(), "1,null,3".to_string())]);
    }

    #[test]
    fn color_rgb_special_case_is_single_comma_joined_topic() {
        let pairs = flatten(json!({"color": {"r": 10, "g": 20, "b": 30}}));
        assert_eq!(pairs, vec![("color".to_string(), "10,20,30".to_string())]);
    }

    #[test]
    fn color_with_rgb_and_extra_keys_still_hits_rgb_special_case() {
        // Real z2m's `objectHasProperties(subPayload, ["r","g","b"])` only
        // requires r/g/b to be present -- extra keys don't disqualify it.
        let pairs = flatten(json!({"color": {"r": 1, "g": 2, "b": 3, "x": 0.4}}));
        assert_eq!(pairs, vec![("color".to_string(), "1,2,3".to_string())]);
    }

    #[test]
    fn color_without_full_rgb_recurses_normally() {
        // Missing "b" -> not the rgb special case, falls through to normal
        // object recursion.
        let pairs = flatten(json!({"color": {"r": 1, "g": 2}}));
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("color-r".to_string(), "1".to_string())));
        assert!(pairs.contains(&("color-g".to_string(), "2".to_string())));
    }

    #[test]
    fn multi_level_nesting_recurses_fully() {
        let pairs = flatten(json!({"a": {"b": {"c": 5}}}));
        assert_eq!(pairs, vec![("a-b-c".to_string(), "5".to_string())]);
    }

    #[test]
    fn realistic_full_device_state() {
        let pairs = flatten(json!({
            "state": "ON",
            "brightness": 200,
            "linkquality": 55,
            "last_seen": "2026-08-19T08:45:46+00:00",
            "battery": 80,
            "action": "toggle",
        }));

        let expected: std::collections::HashSet<(String, String)> = [
            ("state", "ON"),
            ("brightness", "200"),
            ("linkquality", "55"),
            ("last_seen", "2026-08-19T08:45:46+00:00"),
            ("battery", "80"),
            ("action", "toggle"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let actual: std::collections::HashSet<(String, String)> = pairs.into_iter().collect();
        assert_eq!(actual, expected);
    }
}
