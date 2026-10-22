//! Host-side secrets (`[secrets]` in hostd.toml). A plan names a secret as `{{secret:NAME}}`; the
//! host puts the value in only where it is needed (text typed into a page element, and environment
//! values for a spawned process) and nowhere else. Every value is redacted back to its reference
//! from whatever leaves the host towards the model (each LLM request), from the observations, from
//! the audit log and from log lines, so a secret never reaches the model.

use std::fmt;

use anyhow::{Result, bail};
use serde_json::Value;

/// The configured secrets: their names and values.
#[derive(Clone, Default)]
pub struct Secrets {
    values: Vec<(String, String)>,
}

/// Never prints a value.
impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.values.iter().map(|(name, _)| name))
            .finish()
    }
}

const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";

impl Secrets {
    /// `values`, as (name, value) pairs. Empty values are dropped (they cannot be redacted).
    pub fn new(values: Vec<(String, String)>) -> Self {
        let mut values: Vec<_> = values.into_iter().filter(|(_, v)| !v.is_empty()).collect();
        // Longest first, so a value containing another is redacted whole.
        values.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        Self { values }
    }

    /// `text` with every `{{secret:NAME}}` replaced by its value; an unknown name is an error.
    pub fn resolve(&self, text: &str) -> Result<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(OPEN) {
            out.push_str(&rest[..start]);
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find(CLOSE) else {
                bail!("an unclosed secret reference in the plan");
            };
            let name = &after[..end];
            let Some((_, value)) = self.values.iter().find(|(n, _)| n == name) else {
                bail!("the plan names an unknown secret `{name}`");
            };
            out.push_str(value);
            rest = &after[end + CLOSE.len()..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// `text` with every secret value replaced by its reference.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (name, value) in &self.values {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("{OPEN}{name}{CLOSE}"));
            }
        }
        out
    }

    /// `value` with every string in it redacted ([`Self::redact`]), keys included.
    pub fn redact_value(&self, value: &Value) -> Value {
        if self.values.is_empty() {
            return value.clone();
        }
        match value {
            Value::String(s) => Value::String(self.redact(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.redact_value(v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (self.redact(k), self.redact_value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// Bytes with every secret value replaced by its reference (where they are UTF-8; other bytes
    /// pass through).
    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        if self.values.is_empty() {
            return bytes.to_vec();
        }
        match std::str::from_utf8(bytes) {
            Ok(text) => self.redact(text).into_bytes(),
            Err(_) => {
                let mut out = bytes.to_vec();
                for (name, value) in &self.values {
                    out = replace_bytes(
                        &out,
                        value.as_bytes(),
                        format!("{OPEN}{name}{CLOSE}").as_bytes(),
                    );
                }
                out
            }
        }
    }
}

fn replace_bytes(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> Secrets {
        Secrets::new(vec![
            ("pw".into(), "hunter2".into()),
            ("token".into(), "hunter2-long".into()),
            ("empty".into(), String::new()),
        ])
    }

    #[test]
    fn references_resolve_to_their_values() {
        let s = secrets();
        assert_eq!(s.resolve("x {{secret:pw}} y").unwrap(), "x hunter2 y");
        assert_eq!(
            s.resolve("{{secret:token}}{{secret:pw}}").unwrap(),
            "hunter2-longhunter2"
        );
        assert_eq!(s.resolve("no references").unwrap(), "no references");
        assert!(s.resolve("{{secret:nope}}").is_err());
        assert!(s.resolve("{{secret:pw").is_err());
    }

    #[test]
    fn values_are_redacted_to_their_references_longest_first() {
        let s = secrets();
        assert_eq!(s.redact("a hunter2-long b"), "a {{secret:token}} b");
        assert_eq!(s.redact("hunter2"), "{{secret:pw}}");
        let v = serde_json::json!({ "out": ["TOKEN=hunter2"], "hunter2": 1 });
        assert_eq!(
            s.redact_value(&v),
            serde_json::json!({ "out": ["TOKEN={{secret:pw}}"], "{{secret:pw}}": 1 })
        );
        assert_eq!(
            s.redact_bytes(b"\xffhunter2"),
            b"\xff{{secret:pw}}".to_vec()
        );
    }

    #[test]
    fn debug_never_shows_a_value() {
        let shown = format!("{:?}", secrets());
        assert!(
            shown.contains("pw") && !shown.contains("hunter2"),
            "{shown}"
        );
    }
}
