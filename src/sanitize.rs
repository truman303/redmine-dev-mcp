use serde_json::{Map, Value};

const SECRET_KEYS: &[&str] = &["reply_token", "tracking_uri", "api_key"];

/// Drop help-desk tokens and any API key nested in a payload.
pub fn sanitize(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize).collect()),
        Value::Object(map) => Value::Object(sanitize_object(map)),
        other => other,
    }
}

fn sanitize_object(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .filter(|(key, _)| !SECRET_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key, sanitize(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_secrets_at_every_level() {
        let value = sanitize(json!({
            "id": 1,
            "api_key": "secret",
            "author": { "name": "Ada", "reply_token": "t", "tracking_uri": "u" }
        }));
        let text = value.to_string();
        assert!(!text.contains("secret"));
        assert!(!text.contains("reply_token"));
        assert!(!text.contains("tracking_uri"));
        assert_eq!(value["author"]["name"], "Ada");
    }
}
