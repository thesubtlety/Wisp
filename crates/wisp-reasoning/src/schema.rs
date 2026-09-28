use serde_json::Value;

/// Validate `value` against a practical JSON Schema subset: `type`,
/// `properties`, `required`, `additionalProperties: false`, `items`, `enum`,
/// `minimum`, `maximum`.
/// Returns the first violation as a JSON-pointer-ish path and message.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "$")
}

fn check(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };
    if let Some(t) = obj.get("type") {
        let ok = match t {
            Value::String(s) => type_matches(s, value),
            Value::Array(ts) => ts
                .iter()
                .filter_map(Value::as_str)
                .any(|s| type_matches(s, value)),
            _ => true,
        };
        if !ok {
            return Err(format!(
                "{path}: expected type {t}, got {}",
                type_name(value)
            ));
        }
    }
    if let Some(Value::Array(options)) = obj.get("enum") {
        if !options.contains(value) {
            return Err(format!("{path}: {value} not in enum"));
        }
    }
    if let (Some(n), Some(min)) = (value.as_f64(), obj.get("minimum").and_then(Value::as_f64)) {
        if n < min {
            return Err(format!("{path}: {n} < minimum {min}"));
        }
    }
    if let (Some(n), Some(max)) = (value.as_f64(), obj.get("maximum").and_then(Value::as_f64)) {
        if n > max {
            return Err(format!("{path}: {n} > maximum {max}"));
        }
    }
    if let Value::Object(map) = value {
        if let Some(Value::Array(req)) = obj.get("required") {
            for key in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    return Err(format!("{path}: missing required '{key}'"));
                }
            }
        }
        if let Some(Value::Object(props)) = obj.get("properties") {
            for (k, sub) in props {
                if let Some(v) = map.get(k) {
                    check(sub, v, &format!("{path}.{k}"))?;
                }
            }
            if obj.get("additionalProperties") == Some(&Value::Bool(false)) {
                if let Some(extra) = map.keys().find(|k| !props.contains_key(*k)) {
                    return Err(format!("{path}: unexpected property '{extra}'"));
                }
            }
        }
    }
    if let (Value::Array(items), Some(item_schema)) = (value, obj.get("items")) {
        for (i, v) in items.iter().enumerate() {
            check(item_schema, v, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {"type": "array", "items": {
                    "type": "object",
                    "required": ["kind", "confidence"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["a", "b"]},
                        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                        "note": {"type": ["string", "null"]}
                    }
                }}
            }
        })
    }

    #[test]
    fn accepts_valid() {
        let v = json!({"items": [{"kind": "a", "confidence": 0.5, "note": null}]});
        assert!(validate(&schema(), &v).is_ok());
    }

    #[test]
    fn rejects_missing_required() {
        let err = validate(&schema(), &json!({})).unwrap_err();
        assert!(err.contains("missing required 'items'"), "{err}");
    }

    #[test]
    fn rejects_extra_properties_only_when_closed() {
        let closed =
            json!({"type": "object", "properties": {"a": {}}, "additionalProperties": false});
        let err = validate(&closed, &json!({"a": 1, "b": 2})).unwrap_err();
        assert!(err.contains("unexpected property 'b'"), "{err}");
        let open = json!({"type": "object", "properties": {"a": {}}});
        assert!(validate(&open, &json!({"a": 1, "b": 2})).is_ok());
    }

    #[test]
    fn rejects_bad_enum_and_range_with_path() {
        let err = validate(
            &schema(),
            &json!({"items": [{"kind": "z", "confidence": 0.1}]}),
        )
        .unwrap_err();
        assert!(err.starts_with("$.items[0].kind"), "{err}");
        let err = validate(
            &schema(),
            &json!({"items": [{"kind": "a", "confidence": 3}]}),
        )
        .unwrap_err();
        assert!(err.contains("maximum"), "{err}");
    }
}
