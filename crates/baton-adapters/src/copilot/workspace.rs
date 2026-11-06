use serde_json::Value;

// The `name` of a workspace.yaml, which Copilot writes as a plain scalar and baton as a double-quoted one
pub(super) fn name(yaml: &str) -> Option<String> {
    let value = yaml
        .lines()
        .find_map(|line| line.strip_prefix("name:"))?
        .trim();
    let name = if value.starts_with('"') {
        serde_json::from_str(value).unwrap_or_else(|_| value.trim_matches('"').to_owned())
    } else if let Some(quoted) = value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        quoted.replace("''", "'")
    } else {
        value.to_owned()
    };
    (!name.is_empty()).then_some(name)
}

// Strings become JSON strings, which are valid double-quoted YAML once the characters YAML 1.1 reads as line breaks are escaped
pub(super) fn render(fields: &[(&str, Value)]) -> String {
    let mut yaml = String::new();
    for (key, value) in fields {
        let value = match value {
            Value::String(_) => value
                .to_string()
                .replace('\u{85}', "\\u0085")
                .replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029"),
            other => other.to_string(),
        };
        yaml.push_str(&format!("{key}: {value}\n"));
    }
    yaml
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn names_read_back_from_every_scalar_style() {
        let written = render(&[
            ("id", json!("x")),
            ("name", json!("Fix \"it\"\u{2028}now")),
            ("user_named", json!(true)),
        ]);
        assert!(written.contains("\\u2028") && written.contains("user_named: true\n"));
        assert_eq!(name(&written).as_deref(), Some("Fix \"it\"\u{2028}now"));
        assert_eq!(
            name("cwd: /w\nname: Plain title\n").as_deref(),
            Some("Plain title")
        );
        assert_eq!(
            name("name: 'It''s quoted'\n").as_deref(),
            Some("It's quoted")
        );
        assert_eq!(name("cwd: /w\n"), None);
    }
}
