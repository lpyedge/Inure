//! 泄密检测：任务声明的秘密值不得出现在任何会进入模型或日志的文本里。

/// 返回在 `haystack` 中出现的秘密值（忽略空串）。非空即为泄露。
pub fn find_leaks<'a>(haystack: &str, secrets: &'a [String]) -> Vec<&'a str> {
    secrets
        .iter()
        .filter(|s| !s.is_empty() && haystack.contains(s.as_str()))
        .map(|s| s.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_present_secrets_only() {
        let secrets = vec!["hunter2".to_string(), "other".to_string(), String::new()];
        let leaks = find_leaks(r#"{"value":"hunter2"}"#, &secrets);
        assert_eq!(leaks, vec!["hunter2"]);
        assert!(find_leaks("clean", &secrets).is_empty());
    }

    #[test]
    fn works_on_a_real_redacted_observation() {
        use rig_proto::observation::*;
        let pw = Element::new("e1", "textbox", Origin::A11y)
            .with_value("hunter2")
            .with_states(&[ElementState::Password]);
        let mut o = Observation::new(
            1,
            ObservationSource::Cdp,
            Space {
                width: 100.0,
                height: 100.0,
                scale: 1.0,
            },
        );
        o.elements.push(pw);
        let secrets = vec!["hunter2".to_string()];

        assert!(!find_leaks(&serde_json::to_string(&o).unwrap(), &secrets).is_empty());
        o.redact_sensitive();
        assert!(find_leaks(&serde_json::to_string(&o).unwrap(), &secrets).is_empty());
    }
}
