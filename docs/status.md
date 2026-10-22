## Project Status — Host Execution Loop + Browser Automation

> A snapshot of 2025-11-14, kept as history; much of it is out of date. The README's Status section is
> current.

_Last updated: 2025-11-14_

### What’s Working

- **Planner ↔ host loop (`hostd/src/runtime.rs`)**  
  `hostd step` now instantiates the component, requests a plan, executes the returned `planned-action`s via the `ActionExecutor`, and feeds structured observations back into the next `planner.step` call. Execution is bounded by `MAX_HOST_STEPS` to prevent runaway planners.

- **Filesystem + process capabilities (`hostd/src/actions.rs`)**  
  Scoped FS read/list and allowlisted process spawning are implemented in the action executor. The executor enforces workspace-relative paths, UTF-8 handling, truncation limits, and command allowlists from `hostd.toml`.

- **LLM plumbing (`hostd/src/capabilities.rs`, `agent-core/src/lib.rs`)**  
  The guest planner composes system prompts and calls `llm.complete`. Host-side `llm.complete` proxies to OpenAI-compatible APIs defined in config, returning the raw response for planner parsing.

- **Browser automation MVP (`hostd/src/actions.rs`)**  
  Backed by `thirtyfour` + WebDriver. Supports:
  - Session creation (`browser.open_session`)
  - Navigation (`browser.session.goto`)
  - Element lookup with aliases (`browser.session.find`)
  - Element interaction (`browser.element.click`, `.type_text`, `.inner_text`)
  - Page describe + screenshot.
  Sessions/elements are stored in the executor and survive across planner steps until the process exits.

- **Documentation**  
  - `docs/browser-login-task.md` — prescriptive flow for login tasks using the new capability set.  
  - `README.md` — high-level architecture + updated instructions for `[browser]` config.

### Open Gaps / Next Up

1. **Capability coverage**  
   - File writes, directory management, streaming process IO, policy budgeting hooks, browser query-all + screenshot by element, and input automation remain stubbed in `hostd/src/capabilities.rs`.
2. **Policy/audit integration**  
   - `HostConfig` only handles workspace, allowlists, LLM and browser endpoints. None of the policy WIT functions are wired to storage or approval workflows.
3. **Planner intelligence**  
   - `agent-core` simply forwards LLM JSON with minimal validation. Needs schema enforcement, alias bookkeeping, tool catalog discovery, and error handling for mis-specified action plans.
4. **Testing**  
   - No automated tests yet. Need unit coverage for path validation, process allowlists, and a mocked browser run; plus e2e tests for planner-host loop using stubbed components.

### Execution Flow (file/src overview)

1. **CLI entry** — `hostd/src/main.rs` parses `Commands::Step` -> `runtime::run_step`.
2. **Host config** — `HostConfig::from_step_args` (`hostd/src/config.rs`) loads `hostd.toml`, merges CLI allowlists, creates `BrowserSettings` if `[browser]` is present.
3. **Runtime setup** — `run_step` (`hostd/src/runtime.rs`)
   - Builds Wasmtime engine + linker (`wasmtime::component`)
   - Instantiates guest component (`bindings::Control`)
   - Initializes `ActionExecutor` with cloned config + `tokio::runtime::Handle`.
4. **Planner loop** — up to `MAX_HOST_STEPS` iterations:
   - Call `planner.step` with current `Observation`.
   - If `StepResponse::Continue`, pass `plan.actions` into `ActionExecutor::execute`.
   - Translate `Vec<ActionReport>` to the next `Observation`.
   - If `StepResponse::Complete`, exit successfully.
5. **Action dispatch** — `ActionExecutor` (`hostd/src/actions.rs`)
   - Uses `serde_json::Value` to parse each `planned-action` input.
   - Routes to specific helpers (fs/proc/browser).
   - Maintains WebDriver sessions & element aliases (`HashMap`s). Handles resource cleanup in `Drop`.

### How to Continue

1. **Policy + logging wiring**  
   - Implement `bindings::osagent::policy::policy::Host` in `hostd/src/capabilities.rs`, tie into a log sink, and update `ActionExecutor` to emit audit events per invocation.
2. **Capability parity**  
   - Port remaining fs/proc/browser/input operations from `capabilities.rs` into the executor (or refactor so executor delegates back into the existing trait impls).
3. **Planner robustness**  
   - Teach `agent-core` to validate alias usage and to recover when the host returns failure reports (e.g., re-plan when selectors fail).
4. **End-to-end test harness**  
   - Add a mocked WebDriver (or feature-flagged fake) for CI; integrate `cargo test -p hostd` scenarios.

Document these when advancing so future sessions can pick up quickly. File references in this note refer to repo-relative paths.
