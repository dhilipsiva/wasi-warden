use tracing_subscriber::filter::{EnvFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt};

use crate::audit::AuditLayer;

/// Initialize tracing: log lines to stderr per RUST_LOG (or a sensible default), colored only on a
/// terminal, and the audit layer, which sees every WIT call `bindgen!` traces. The WIT call traces
/// never reach the log lines, whatever RUST_LOG says: their arguments are for the redacted audit log
/// only.
pub fn init() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wasi_warden=debug,hostd=debug"))
        .add_directive("hostd::bindings=off".parse().expect("a valid directive"));
    let lines = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_filter(env_filter);
    let audit = AuditLayer
        .with_filter(Targets::new().with_target("hostd::bindings", tracing::Level::TRACE));
    let _ = tracing_subscriber::registry()
        .with(lines)
        .with(audit)
        .try_init();
}
