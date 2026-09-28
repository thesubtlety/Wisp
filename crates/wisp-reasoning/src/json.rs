/// Pull the first complete JSON object out of model text. Handles code
/// fences and leading/trailing prose.
pub fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(trimmed) {
        return Some(v);
    }
    let bytes = trimmed.as_bytes();
    let mut start = 0;
    while let Some(off) = trimmed[start..].find('{') {
        let open = start + off;
        if let Some(end) = matching_brace(bytes, open) {
            if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(&trimmed[open..=end])
            {
                return Some(v);
            }
        }
        start = open + 1;
    }
    None
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if in_str {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_object() {
        assert_eq!(extract_json_object(r#"{"a":1}"#), Some(json!({"a":1})));
    }

    #[test]
    fn fenced_with_prose() {
        let t = "Here you go:\n```json\n{\"a\": {\"b\": \"}\"}}\n```\nDone.";
        assert_eq!(extract_json_object(t), Some(json!({"a": {"b": "}"}})));
    }

    #[test]
    fn skips_non_json_braces() {
        let t = "set {x} then {\"ok\": true}";
        assert_eq!(extract_json_object(t), Some(json!({"ok": true})));
    }

    #[test]
    fn none_when_absent() {
        assert_eq!(extract_json_object("no json here"), None);
        assert_eq!(extract_json_object("[1,2]"), None);
    }
}
