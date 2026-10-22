# Architecture

wasi-warden runs an untrusted planner, an LLM driven through a WebAssembly component, against a narrow
set of capabilities that a privileged Rust host implements and polices. This page describes the code as
it stands. The README's Status section lists what does not work yet, and
[threat-model.md](threat-model.md) covers what the design defends against.

## The pieces

| Directory | What it is |
|---|---|
| `wit/` | The interfaces, package `osagent:*`. The `control` world imports `fs`, `proc`, `browser`, `input`, `llm` and `policy` (plus `common` types) and exports `planner`. |
| `agent-core/` | The planner, a WASI Preview 2 component built for `wasm32-wasip2`. It renders the host's capability catalog into a system prompt, calls `llm.complete`, parses the reply into a plan, and asks once more if the reply is not a plan. It calls no other capability itself. |
| `hostd/` | The host binary: wasmtime with the component model, the capability implementations, the plan-act loop and the CLI (`hostd run`, `hostd step`). |
| `tests/probe-agent/` | A test guest that calls the WIT capabilities and WASI directly, for the end-to-end tests. |
| `tools/wit-check/` | Parses `wit/` (part of `just ci`). |

## The loop

`hostd run --task T` does the following:

1. It loads hostd.toml (`config.rs`) and compiles the component, within a 120 s cap.
2. Only then does the run's clock start, together with the budgets (`budget.rs`), the epoch ticker and
   the watchdog.
3. It instantiates the component in a store whose limiter caps the agent's memory (`state.rs`).
4. It runs planner rounds (`runtime.rs`). Each round:
   - calls `planner.step(task, observation)`, bounded by fuel and the epoch deadline;
   - on a plan, runs up to 32 of its actions natively (`actions.rs`) and builds the next observation from
     their reports, with every secret value redacted;
   - on completion, stops.
5. It stops when the planner completes the task or a budget runs out: steps, wall-clock, tokens, cost,
   processes, filesystem operations or browser actions.

`hostd step` runs one round and prints the next step and observation as JSON. A caller drives the loop by
passing them back.

So there are two ways in, and they share one set of rules:

- **The planner's JSON actions** (`actions.rs`): `fs.list_dir`, `fs.read_file`, `proc.spawn` and the
  browser actions. They are listed once in `registry.rs`, which drives the executor's exhaustive dispatch
  and generates the catalog the planner is shown, so the set advertised is the set executed.
- **The WIT capabilities the guest calls** (`capabilities.rs`): the full `fs`, `proc`, `browser`, `llm`
  and `policy` interfaces, through handles held in the store's resource table (`resources.rs`). `input`
  and the browser's script evaluation are refused.

Both go through the same modules for the rules that matter:

- **`sandbox.rs`.** Paths are confined component by component through symlinks. Programs come from
  `allow_proc`, resolved once at load. Environment names are checked against `allow_env` and a denylist.
  The child gets a host-built `PATH`. `run_bounded` runs a process in its own group, with a timeout, an
  output cap, and a parent-death signal.
- **`browser.rs`.**
  - Only `allowed_hosts` are visited, and the page is re-checked before every action.
  - At most four sessions are open at once, and quits are bounded.
  - A read-only workspace gets no downloads.
- **`budget.rs`.** One set of budgets, shared by both paths and the loop.

## Around the loop

- **Guest limits.** These live in `runtime.rs` and `state.rs`:
  - The guest gets fuel per round.
  - An epoch deadline interrupts running wasm at the wall-clock budget.
  - A watchdog ends hostd 5 s past the budget if the run is stuck inside a host call.
  - A limiter caps linear memories and tables together.
  - The engine refuses 64-bit memories, shared memories, multiple memories and GC, which the memory
    budget cannot hold.
- **Secrets** (`secrets.rs`). A `{{secret:NAME}}` reference resolves only in typed text and in process
  environment values. Every value is redacted:
  - in the one function that builds each LLM request;
  - in the planner's reports;
  - in process output and page text returned to the guest;
  - in the audit log and telemetry.
- **The audit log** (`audit.rs`) has one JSON line per capability call:
  - The planner's actions are recorded in the executor.
  - The WIT calls come from the tracing spans `bindgen!` emits, collected by a tracing layer. Those
    traces never reach the log lines.
  - Each line has the run, step, capability, redacted input, outcome, duration, and a digest of the
    output.
- **The LLM router** (`router.rs`, `capabilities.rs`; [design/router.md](design/router.md)) handles
  several OpenAI-compatible providers:
  - local ones first;
  - fallback on any failure;
  - paid providers only while the cost budget lasts.

  Each attempt is a versioned telemetry event (`telemetry.rs`; [design/telemetry.md](design/telemetry.md)).
- **Logging** (`logging.rs`). Log lines go to stderr, and guest-written fields are cut to 256 characters.
  `hostd step` writes its JSON to stdout.

## Data flow

```text
 operator ── hostd.toml, --task ──▶ hostd ──── chat/completions ────▶ LLM providers
                                     │  ▲      (router; redacted)
                       planner.step  │  │ plan (JSON actions)
                                     ▼  │
                              agent-core.wasm ── WIT calls ──▶ hostd capabilities
                              (fuel, epochs,                  (fs, proc, browser,
                               memory limiter)                 llm, policy)
                                     │
            reports (redacted) ◀─────┘ actions run natively: fs, proc, browser
                                       ├─ sandbox.rs rules, budgets
                                       └─ audit log, telemetry (redacted)
```

## History

This page replaces `docs/phase-1.md` and `docs/getting-started.md`. Those were two near-identical
planning notes from November 2025, which sketched the design before it was built. They are in the git
history; [status.md](status.md) is a snapshot from the same month.
