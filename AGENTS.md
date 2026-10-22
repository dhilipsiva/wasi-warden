# Repository Guidelines

`wasi-warden` tracks the design, policy, and soon the Rust host plus WASI guest that form the secure automation stack. Treat every change as part of defense-in-depth: keep scope tight, document assumptions, and prefer auditable code paths.

## Project Structure & Module Organization
The workspace has three code directories: `hostd/` (the privileged Rust host), `agent-core/` (the WASI component) and `wit/` (the shared interfaces), plus `tools/wit-check` (parses `wit/`). Host adapters live in `hostd/src/capabilities.rs` (the WIT host traits) and `hostd/src/actions.rs` (the executor for the planner's JSON actions); guest planning logic lives in `agent-core/src`. Keep experiments and threat-model notes under `docs/research/`, not beside production code. Read the README's Status section first: it lists what is not implemented yet.

## Build, Test, and Development Commands
Run these from the repository root inside `nix develop` (the flake pins Rust with the `wasm32-wasip2` target):
- `just ci` — the gate CI runs: fmt, clippy `-D warnings`, the unit tests, docs, `cargo deny`, the WIT parse, the component builds and the three end-to-end tests (`just --list` shows each recipe).
- `cargo build -p hostd` — compile the Rust host.
- `cargo build -p agent-core --target wasm32-wasip2 --release` — produce the WASI component (`wit-bindgen` generates the bindings; `cargo-component` is not used).
- `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings` — formatting and lints before every push.
- `cargo run -p hostd -- run --task "<task>"` — run the plan-act loop against the compiled guest (`step` runs one round and prints the next observation); it needs `hostd.toml` (copy `hostd.toml.example`) with an LLM endpoint.

## Coding Style & Naming Conventions
Rust code follows default `rustfmt` (4-space indent, trailing commas). Modules stay `snake_case`; exported types use `UpperCamelCase`. WIT packages live under `osagent:*` with verb-style functions (`open_workspace`, `list_dir`). Keep capability structs small, reuse `CapabilityImpl` naming for host adapters, and reserve comments for security rationale.

## Testing Guidelines
Unit tests live beside their modules (`hostd/src/*.rs`, `agent-core/src/lib.rs`). The end-to-end tests, `hostd/tests/step_e2e.rs` (a step against a scripted LLM), `hostd/tests/browser_e2e.rs` (a browser session against a fake WebDriver) and `hostd/tests/probe_e2e.rs` (the WIT capabilities, through the probe guest in `tests/probe-agent`), share helpers in `hostd/tests/support/` and are ignored until the component is built (`just e2e step_e2e`). Tests marked `known gap` record security gaps and are un-ignored by the fix that closes each. Add unit tests alongside modules using Rust’s `mod tests`. Integration coverage belongs in `hostd/tests/` and should exercise real capability flows via allowlisted commands (e.g., `cargo test -p hostd -- --nocapture`). Test the guest's prompt building and plan parsing as plain Rust unit tests in `agent-core`, with fixture observations. Name tests after behavior (`handles_missing_policy`, `denies_unlisted_proc`) and pair every bug fix with a regression.

## Commit & Pull Request Guidelines
History shows short, imperative commit titles (“add getting started guide”). Keep subject ≤50 chars, follow with a blank line, then detail the why/impact. Pull requests must cite related issues, describe policy implications, and include screenshots or logs when touching CLI UX. Call out any new capabilities, config knobs, or security-sensitive default changes.

## Security & Configuration Tips
Default to deny-by-default policies: keep new capabilities off until the config documents scope, allowlists, and budget knobs. The sample configuration is `hostd.toml.example`; `hostd.toml` is git-ignored because it holds an API key. Store other sample policies under `docs/`, never in user directories. A new planner action is added to `hostd/src/registry.rs`, which both generates the catalog the planner sees and drives the executor's exhaustive dispatch. Every capability call is audited (`hostd/src/audit.rs`): the planner's actions in `execute_action`, the WIT calls through the tracing spans `bindgen!` emits, so a new WIT argument that carries data rather than describing the call belongs in its `DATA_ARGUMENTS`. Secrets resolve only where they leave the host (typed text, process environment values); everything bound for the model goes through `build_chat_request`, which redacts it. Ensure new code paths return structured errors so auditors—and downstream agents—can reason about failures.
