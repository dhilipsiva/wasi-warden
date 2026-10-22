//! Telemetry (`[telemetry] path`; docs/design/telemetry.md): one JSON line per LLM request
//! attempt, with its provider, outcome, tokens, cost and latency. Every line passes the secrets'
//! redaction on its way out, like the audit log.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use camino::Utf8Path;
use serde::{Deserialize, Serialize};

use crate::secrets::Secrets;

/// The version of [`TelemetryEvent`]'s schema. A change that removes or retypes a field bumps it.
pub const SCHEMA_VERSION: u32 = 1;

/// The most characters of an error kept in an event.
const MAX_ERROR_CHARS: usize = 512;

/// One attempt at an LLM request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEvent {
    /// [`SCHEMA_VERSION`] when written.
    pub version: u32,
    /// What happened: `llm_request` (the only kind so far).
    pub kind: String,
    /// The run (the audit log's run id) and its planner round.
    pub run: String,
    pub step: u32,
    pub provider: String,
    pub tier: String,
    pub model: String,
    /// 1 for the first provider asked for this call, 2 for the next, and so on.
    pub attempt: u32,
    pub ok: bool,
    pub error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// The endpoint reported no usage; the token counts are estimates.
    pub tokens_estimated: bool,
    /// What the attempt cost, in millionths of the price unit.
    pub cost_micros: u64,
    pub latency_ms: u64,
    /// Why this provider was asked.
    pub route_reason: String,
}

impl TelemetryEvent {
    /// An event with `error` cut to [`MAX_ERROR_CHARS`].
    pub fn with_error(mut self, error: &str) -> Self {
        let cut: String = error.chars().take(MAX_ERROR_CHARS).collect();
        self.error = Some(cut);
        self.ok = false;
        self
    }
}

/// The run's telemetry sink.
pub struct Telemetry {
    file: Option<Mutex<File>>,
    secrets: Secrets,
}

static CURRENT: OnceLock<Telemetry> = OnceLock::new();

impl Telemetry {
    /// Open the sink at `path` (appending), or none. Becomes the process's sink.
    pub fn install(path: Option<&Utf8Path>, secrets: Secrets) -> Result<&'static Telemetry> {
        let telemetry = Telemetry::open(path, secrets)?;
        Ok(CURRENT.get_or_init(|| telemetry))
    }

    /// The process's sink, if one was installed.
    pub fn current() -> Option<&'static Telemetry> {
        CURRENT.get()
    }

    fn open(path: Option<&Utf8Path>, secrets: Secrets) -> Result<Telemetry> {
        let file = match path {
            Some(path) => Some(Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("failed to open the telemetry log {path}"))?,
            )),
            None => None,
        };
        Ok(Telemetry { file, secrets })
    }

    /// Write `event` as one line, every secret value redacted to its reference.
    pub fn emit(&self, event: &TelemetryEvent) {
        let Some(file) = &self.file else { return };
        let Ok(value) = serde_json::to_value(event) else {
            return;
        };
        let line = self.secrets.redact_value(&value);
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> TelemetryEvent {
        TelemetryEvent {
            version: SCHEMA_VERSION,
            kind: "llm_request".to_string(),
            run: "run-1".to_string(),
            step: 2,
            provider: "box".to_string(),
            tier: "local".to_string(),
            model: "small".to_string(),
            attempt: 1,
            ok: true,
            error: None,
            prompt_tokens: 120,
            completion_tokens: 30,
            tokens_estimated: false,
            cost_micros: 0,
            latency_ms: 840,
            route_reason: "first choice (local tier)".to_string(),
        }
    }

    /// An event survives a JSON round trip, and carries its schema version.
    #[test]
    fn an_event_round_trips_with_its_version() {
        let json = serde_json::to_string(&event()).unwrap();
        assert!(json.contains("\"version\":1"), "{json}");
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event());
    }

    /// A secret value in an event (an endpoint echoing a key in its error) is written as its
    /// reference.
    #[test]
    fn an_emitted_event_is_redacted() {
        let dir = std::env::temp_dir().join(format!(
            "hostd-telemetry-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.join("t.jsonl")).unwrap();
        let secrets = Secrets::new(vec![("api_key".to_string(), "sk-planted-77".to_string())]);
        let sink = Telemetry::open(Some(&path), secrets).unwrap();
        sink.emit(&event().with_error("401: key sk-planted-77 is invalid"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-planted-77"), "{text}");
        assert!(text.contains("{{secret:api_key}}"), "{text}");
        let line: TelemetryEvent = serde_json::from_str(text.trim()).unwrap();
        assert!(!line.ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_error_is_cut() {
        let long = event().with_error(&"e".repeat(5000));
        assert_eq!(long.error.unwrap().chars().count(), 512);
    }
}
