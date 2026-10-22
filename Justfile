# The local gate (`just ci`) is what CI runs, recipe by recipe, inside `nix develop` (.github/workflows/ci.yml).

set shell := ["bash", "-euo", "pipefail", "-c"]

export RUST_BACKTRACE := "1"

# List the recipes.
default:
    @just --list

# Type-check every crate and target.
check:
    cargo check --locked --workspace --all-targets

# Formatting and Clippy, with warnings as errors.
lint:
    cargo fmt --all -- --check
    cargo clippy --locked --workspace --all-targets -- -D warnings

# The unit tests (the end-to-end tests are ignored here; see `e2e`).
test:
    cargo test --locked --workspace

# Build the docs, with warnings as errors.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps

# Advisories, bans, licences and sources against deny.toml.
deny:
    cargo deny check advisories bans licenses sources

# Parse the WIT contracts in wit/.
wit:
    cargo run --locked -p wit-check

# The WASI Preview 2 components: the agent hostd loads, and the probe guest the end-to-end tests use.
wasm:
    cargo build --locked -p agent-core -p probe-agent --target wasm32-wasip2 --release

# One end-to-end test (step_e2e, browser_e2e or probe_e2e) against freshly built components.
e2e name: wasm
    cargo test --locked -p hostd --test {{ name }} -- --include-ignored

# Everything CI checks.
ci: lint test doc deny wit (e2e "step_e2e") (e2e "browser_e2e") (e2e "probe_e2e")
