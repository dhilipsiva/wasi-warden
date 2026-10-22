use tracing_subscriber::{EnvFilter, fmt};

/// Initialize tracing using RUST_LOG or a sensible default. The log lines go to stderr, colored only
/// on a terminal.
pub fn init() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wasi_warden=debug,hostd=debug"));

    let _ = fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
}
