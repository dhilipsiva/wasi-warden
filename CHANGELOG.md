# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow semantic
versioning. The release workflow takes a version's notes from its section here.

## [Unreleased]

The first release, `0.1.0`, will cover everything below. These are the changes since the November 2025
design spike.

### Added

- `hostd run` runs a task: rounds until the planner completes it or a budget runs out. `hostd step` runs
  one round and prints where it got to as JSON.
- Budgets for steps, wall-clock, LLM tokens, LLM cost, processes, filesystem operations and browser
  actions, shared by the planner's actions and the WIT capabilities.
- Guest limits:
  - fuel per round and an epoch interrupt at the wall-clock budget;
  - a watchdog past the budget;
  - a total memory budget, with the engine refusing what it cannot hold;
  - a compile cap.
- `[workspace]` `read_only` and `max_file_bytes`.
- The WIT `policy` and `browser` interfaces, and the `input` interface refused as disabled.
- Host-side secrets (`[secrets]`, `{{secret:NAME}}`), resolved only where they leave the host and
  redacted everywhere else.
- An audit log (`[audit]`) of every capability call.
- The capability catalog in the planner's prompt, and one retry when a reply is not a plan.
- An LLM router over several providers (`[[llm.providers]]`): local first, fallback on failure, and a
  cost budget. Telemetry (`[telemetry]`) records each attempt.
- docs/architecture.md, docs/threat-model.md, docs/design/router.md, docs/design/telemetry.md, and
  CONTRIBUTING.md.

### Changed

- wasmtime 49, and a flake on nixpkgs-unstable with Rust 1.99.1.
- Licensed MIT OR Apache-2.0.
- Log lines go to stderr.
- hostd.toml rejects unknown keys and sections.

### Fixed

- Symlink escapes from the workspace.
- Programs resolved by base name.
- Environment hijacks of allowed programs.
- Processes outliving their bounds or the host.
- A browser panic on every action.
- Pages leaving the allowed hosts.
