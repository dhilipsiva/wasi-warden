# Repository Guidelines

`wasi-warden` tracks the design, policy, and soon the Rust host plus WASI guest that form the secure automation stack. Treat every change as part of defense-in-depth: keep scope tight, document assumptions, and prefer auditable code paths.

## Project Structure & Module Organization
The workspace has three code directories: `hostd/` (the privileged Rust host), `agent-core/` (the WASI component) and `wit/` (the shared interfaces), plus `tools/wit-check` (parses `wit/`). Host adapters live in `hostd/src/capabilities.rs` (the WIT host traits) and `hostd/src/actions.rs` (the executor for the planner's JSON actions); guest planning logic lives in `agent-core/src`. Keep experiments and threat-model notes under `docs/research/`, not beside production code. Read the README's Status section first: it lists what is not implemented yet.

## Build, Test, and Development Commands
Run these from the repository root inside `nix develop` (the flake pins Rust with the `wasm32-wasip2` target):
- `cargo build -p hostd` — compile the Rust host.
- `cargo build -p agent-core --target wasm32-wasip2 --release` — produce the WASI component (`wit-bindgen` generates the bindings; `cargo-component` is not used).
- `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings` — formatting and lints before every push (clippy does not pass yet).
- `cargo run -p hostd -- step --task "<task>" --observation '{}'` — run the plan-act loop against the compiled guest; it needs `hostd.toml` (copy `hostd.toml.example`) with an LLM endpoint.

## Coding Style & Naming Conventions
Rust code follows default `rustfmt` (4-space indent, trailing commas). Modules stay `snake_case`; exported types use `UpperCamelCase`. WIT packages live under `osagent:*` with verb-style functions (`open_workspace`, `list_dir`). Keep capability structs small, reuse `CapabilityImpl` naming for host adapters, and reserve comments for security rationale.

## Testing Guidelines
There are no tests yet. Add unit tests alongside modules using Rust’s `mod tests`. Integration coverage belongs in `hostd/tests/` and should exercise real capability flows via allowlisted commands (e.g., `cargo test -p hostd -- --nocapture`). Test the guest's prompt building and plan parsing as plain Rust unit tests in `agent-core`, with fixture observations. Name tests after behavior (`handles_missing_policy`, `denies_unlisted_proc`) and pair every bug fix with a regression.

## Commit & Pull Request Guidelines
History shows short, imperative commit titles (“add getting started guide”). Keep subject ≤50 chars, follow with a blank line, then detail the why/impact. Pull requests must cite related issues, describe policy implications, and include screenshots or logs when touching CLI UX. Call out any new capabilities, config knobs, or security-sensitive default changes.

## Security & Configuration Tips
Default to deny-by-default policies: keep new capabilities off until the config documents scope, allowlists, and budget knobs. The sample configuration is `hostd.toml.example`; `hostd.toml` is git-ignored because it holds an API key. Store other sample policies under `docs/`, never in user directories. Log every capability invocation with task IDs, and ensure new code paths return structured errors so auditors—and downstream agents—can reason about failures.
