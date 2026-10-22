//! The audit log (`[audit] path` in hostd.toml): one JSON line per capability call, from both
//! executors. Each line carries:
//! - `run`, `step` and `source` (`action` for the planner's JSON actions, `wit` for the agent's WIT
//!   calls);
//! - `capability`, the call's `input` (redacted, long strings cut), `ok` and `error`;
//! - `output_sha256`, a digest of the output instead of the output itself, and `ms`.
//!
//! WIT calls are captured from the tracing spans `bindgen!` emits for every import (see
//! [`AuditLayer`]).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8Path;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::secrets::Secrets;

/// WIT arguments that carry a file's data rather than describe the call (write-string's).
const DATA_ARGUMENTS: [&str; 1] = ["contents"];

/// The longest any recorded string is kept.
const MAX_FIELD_CHARS: usize = 512;

/// The run's audit log.
pub struct Audit {
    file: Option<Mutex<File>>,
    run: String,
    step: AtomicU32,
    secrets: Secrets,
}

/// The audit log of the run in progress, for the tracing layer (one run per process).
static CURRENT: OnceLock<Audit> = OnceLock::new();

impl Audit {
    /// Open the log at `path` (appending), or none. Becomes the process's audit log.
    pub fn install(path: Option<&Utf8Path>, secrets: Secrets) -> Result<&'static Audit> {
        let file = match path {
            Some(path) => Some(Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("failed to open the audit log {path}"))?,
            )),
            None => None,
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let audit = Audit {
            file,
            run: format!("{nanos:x}-{:x}", std::process::id()),
            step: AtomicU32::new(0),
            secrets,
        };
        Ok(CURRENT.get_or_init(|| audit))
    }

    pub fn set_step(&self, step: u32) {
        self.step.store(step, Ordering::Relaxed);
    }

    /// The process's audit log, if one was installed.
    pub fn current() -> Option<&'static Audit> {
        CURRENT.get()
    }

    /// The run's id, shared with the telemetry.
    pub fn run(&self) -> &str {
        &self.run
    }

    /// The planner round in progress.
    pub fn step(&self) -> u32 {
        self.step.load(Ordering::Relaxed)
    }

    /// Record one capability call.
    pub fn record(
        &self,
        source: &str,
        capability: &str,
        input: &Value,
        output: &str,
        error: Option<&str>,
        ms: u64,
    ) {
        let Some(file) = &self.file else { return };
        let line = json!({
            "run": self.run,
            "step": self.step.load(Ordering::Relaxed),
            "source": source,
            "capability": capability,
            "input": cut(&self.secrets.redact_value(input)),
            "ok": error.is_none(),
            "error": error.map(|e| cut_str(&self.secrets.redact(e))),
            "output_sha256": hex(&Sha256::digest(output.as_bytes())),
            "ms": ms,
        });
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn cut_str(s: &str) -> String {
    if s.chars().count() <= MAX_FIELD_CHARS {
        return s.to_string();
    }
    let kept: String = s.chars().take(MAX_FIELD_CHARS).collect();
    format!("{kept}…")
}

/// `value` with every string cut to [`MAX_FIELD_CHARS`].
fn cut(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(cut_str(s)),
        Value::Array(items) => Value::Array(items.iter().map(cut).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), cut(v))).collect()),
        other => other.clone(),
    }
}

/// What the layer gathers for one WIT import span.
struct Call {
    capability: String,
    started: Instant,
    input: Map<String, Value>,
    result: Option<String>,
}

/// Collects an event's fields as strings, apart from its message.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    values: Map<String, Value>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.values
                .insert(field.name().to_string(), Value::String(text));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.values
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }
}

/// Turns the `wit-bindgen import` spans (one per WIT call, with `call` and `return` events) into
/// audit lines.
pub struct AuditLayer;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for AuditLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        if attrs.metadata().name() != "wit-bindgen import" {
            return;
        }
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let module = fields
            .values
            .get("module")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let function = fields
            .values
            .get("function")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let capability = format!(
            "{}.{}",
            module.trim_matches('"'),
            function.trim_matches('"')
        );
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Call {
                capability,
                started: Instant::now(),
                input: Map::new(),
                result: None,
            });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(call) = extensions.get_mut::<Call>() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        match fields.message.as_deref() {
            Some("call") => {
                // A file's contents are data, not part of the call's shape: only their size is kept.
                // (Arguments holding lists, such as bytes and messages, are elided by bindgen.)
                for name in DATA_ARGUMENTS {
                    if let Some(Value::String(v)) = fields.values.get_mut(name) {
                        *v = format!("<{} chars not recorded>", v.chars().count());
                    }
                }
                call.input = fields.values
            }
            Some("return") => {
                call.result = fields
                    .values
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
            _ => {}
        }
    }

    fn on_close(&self, id: Id, ctx: LayerContext<'_, S>) {
        let (Some(audit), Some(span)) = (CURRENT.get(), ctx.span(&id)) else {
            return;
        };
        let Some(call) = span.extensions_mut().remove::<Call>() else {
            return;
        };
        let result = call.result.unwrap_or_default();
        let error = result.starts_with("Err(").then_some(result.as_str());
        audit.record(
            "wit",
            &call.capability,
            &Value::Object(call.input),
            &result,
            error,
            call.started.elapsed().as_millis() as u64,
        );
    }
}
