# wasi-warden

> **Status: design spike, Nov 2025.** Policy, budgets, audit and fuel are unimplemented, and known
> sandbox gaps are open; do not run it on a machine or data you care about.
> What works and what does not is listed under [Status](#status).

Secure, local-first OS automation for LLMs: the design.

`wasi-warden` is meant to run an untrusted "brain" inside a WebAssembly/WASI sandbox and expose a narrow, auditable set of capabilities from a Rust host: files, processes, browser automation, and (optionally) keyboard/mouse input. The goal is that an LLM can plan and execute tasks on a machine without ever getting raw, ambient system access.

Think: **computer-use / RPA / ops runbooks**, but:

* local-first
* capability-based
* WASI-sandboxed
* policy-driven
* LLM-agnostic

For instance, a browser login task (the planner's prompt does not list the browser actions yet, so a real model has to guess their names; see [Status](#status)):

```
cargo run -p hostd -- step \
  --task "Log into https://demo.example/login with creds in secrets/demo-login.json and confirm the dashboard greets the user."
```

The sample task is described in [docs/browser-login-task.md](docs/browser-login-task.md).

---

## Why this exists

Most “LLM controls your computer” stacks are:

* closed-source,
* cloud-run,
* or built as ad-hoc Python glue with weak isolation.

`wasi-warden` targets a different quadrant:

* Organizations that need **local or air-gapped** automation.
* Engineers who want **verifiable guardrails** around powerful agents.
* Tooling ecosystems (MCP, IDEs, CI, ops) that need a **standardized, secure OS agent** instead of bespoke scripts.

If you are fine with a hosted black box, you do not need this project.

---

## Core ideas

1. **Untrusted brain, trusted hands**

   * The planner/agent logic runs as a WASI component (WebAssembly).
   * The Rust host is the only thing allowed to touch the real OS.

2. **Deny-by-default capabilities**

   * No ambient filesystem, network, or env by default.
   * All access goes through explicit, typed capabilities defined in WIT.

3. **Narrow, composable tool surface**

   * Files: scoped to configured workspaces.
   * Processes: allowlisted commands.
   * Browser: controlled via WebDriver.
   * Input: optional and off by default (not implemented yet).

4. **LLM-agnostic control**

   * Any LLM that can speak JSON / tool-calls can drive `wasi-warden`.
   * Local models and cloud APIs both work via a single `llm` capability.

5. **Audit, budgets, verification** (planned; see [Status](#status))

   * Every action is logged.
   * Per-task limits on steps, time, and scope.
   * Tasks are “done” only when explicit success predicates are met.

---

## High-level architecture

```text
+-----------------------------+
|        LLM (any)            |
|  - gets tool schema         |
|  - returns actions JSON     |
+--------------+--------------+
               |
         (local HTTP / pipe)
               |
+--------------v--------------+
|        wasi-warden hostd    |   Rust (native)
|  - loads Wasm agent-core    |
|  - enforces policy          |
|  - logs all actions         |
|  - implements capabilities: |
|    fs / proc / browser /    |
|    input / llm              |
+--------------+--------------+
               |
        (WIT imports / WASI)
               |
+--------------v--------------+
|       agent-core.wasm       |   Rust -> WASI component
|  - untrusted planner        |
|  - uses only imported APIs  |
|  - plan/act/observe loop    |
+-----------------------------+
```

The agent never talks to the OS directly. It can only call what `hostd` exposes.

---

## Status

A four-day spike (2025-11-11 to 2025-11-15). What exists:

* WIT interfaces for `fs`, `proc`, `browser`, `input`, `llm`, `policy` and the `agent` planner (`wit/`).
* A Rust host (`hostd`) on the Wasmtime component model that loads the agent component.
* A Rust guest (`agent-core`) built as a WASI Preview 2 component. It formats the prompt, calls the `llm`
  capability and parses the returned JSON plan; it calls no other capability itself.
* A bounded loop: `hostd step` asks the planner for a plan, executes its actions natively (`hostd/src/actions.rs`)
  and feeds the results back, for at most 8 rounds (`MAX_HOST_STEPS`).

What does not work yet:

* **Tests cover the host's own logic, not its security**: 33 unit tests (path resolution, the fs and proc
  actions, configuration, the plan parser) and two end-to-end tests (a step against a scripted LLM, a browser
  session against a fake WebDriver) run in CI. Three more tests record the security gaps below and are ignored
  until they are fixed.
* **The planner's prompt lists only `fs.list_dir`, `fs.read_file` and `proc.spawn`**, though the host runs eleven
  actions, browser ones included; a model has to guess the rest.
* **Nothing is enforced beyond the round cap**: no wasm fuel or epoch limit, no wall-clock or token budget, no
  audit log. The `policy` capability and every `input` call return `Denied`.
* **The sandbox isolates only prompt-building code**: the planning happens in the LLM and the actions run in the
  native host, outside the sandbox.
* **The host's rules have known limits** (both the planner's actions and the WIT capabilities follow them):
  * **Paths.** They are checked component by component through symlinks on every call, a stale directory
    handle included. But the check and the use are separate calls, so a process swapping a directory for a
    symlink in between could still escape. Only an allowlisted program running alongside could do that.
    Hard links into the workspace are followed.
  * **Processes.** A process that leaves its process group (`setsid`, a daemon) is not killed with it. The
    parent-death signal does not survive exec of a setuid, setgid or file-capability program (such as those
    under NixOS's `/run/wrappers`), so do not allowlist one.
  * **Allowlisted programs trust the workspace.** A program that reads configuration or hooks from its working
    directory (git, cargo, npm, make) takes instructions from whatever the plan wrote there. The same holds
    for any variable in `allow_env` that points a tool at its configuration (`HOME`, `XDG_CONFIG_HOME`,
    `CARGO_HOME`). Allowlist accordingly.
  * **Browser.** The page is re-checked before every action, but between two actions it could load
    anything, and ports are not constrained.

Expect breaking changes. The progress snapshot is in [docs/status.md](docs/status.md).

---

## Features (planned)

* **Filesystem**

  * Workspace-scoped read/write via directory handles.
  * No arbitrary absolute paths.
* **Process**

  * Spawn and wait for allowlisted commands.
  * Used for tests, formatters, build tools, etc.
* **Browser**

  * Sessions over WebDriver.
  * Navigate, click, type, scrape DOM/HTML.
* **Input (opt-in, high-risk)**

  * Global keyboard and mouse via OS-specific backends.
  * Fully disabled by default.
* **LLM integration**

  * Single interface for OpenAI-compatible and local models.
  * Supports tool-calling / JSON plans.
* **Policies**

  * TOML config:

    * allowed paths, binaries, domains,
    * per-task budgets,
    * capability toggles.
* **Observability**

  * Structured logs for all calls.
  * Optional JSON event stream for external auditors/UIs.

---

## Getting started (dev)

Prerequisites: [Nix](https://nixos.org/) with flakes, whose dev shell provides Rust 1.99.1 with the
`wasm32-wasip2` target, chromedriver with Chromium, and Ollama (or a Rust toolchain with that target installed),
and an OpenAI-compatible LLM endpoint (a local [Ollama](https://ollama.com/) works). `nix build .#hostd` builds
the host binary alone.

From the repository root:

```bash
git clone https://github.com/dhilipsiva/wasi-warden.git
cd wasi-warden
nix develop                                               # the pinned toolchain
cargo build -p hostd                                      # the host
cargo build -p agent-core --target wasm32-wasip2 --release  # the agent component
cp hostd.toml.example hostd.toml                          # then edit: workspace, allowlist, LLM endpoint
cargo run -p hostd -- step --task "List workspace files" --observation '{}'
```

`hostd step` loads `./target/wasm32-wasip2/release/agent_core.wasm` (`--component` to change it), asks the
planner for a JSON plan, runs the allowed actions, and repeats with their results until the planner reports
the task complete or 8 rounds pass. `hostd step --help` lists the flags.

To try browser tasks, point the host at a running WebDriver (Chromedriver, Geckodriver, etc.) in `hostd.toml`:

```toml
[browser]
webdriver_url = "http://127.0.0.1:9515"
default_profile = "default"
# The hosts the browser may visit (`*.example.com` for subdomains); none when unset.
allowed_hosts = ["demo.example"]
```

Each browser action in planner JSON must assign an `alias` for new sessions/elements so follow-up actions (click/type/etc.) can reference them. See [docs/browser-login-task.md](docs/browser-login-task.md) for a worked example.

---

## Security model

The design's rules:

* Sandbox first:

  * WASI with **no** inherited FS, env, or network.
* Capabilities only:

  * Every sensitive operation is a WIT-defined import.
  * No “run_shell(string)” primitive.
* Configurable scope:

  * Per-install policies define what is reachable.
* Bounded execution:

  * Max steps, max runtime per task, and wasm “fuel” limits.
* Full audit:

  * Every capability call is logged with inputs, outputs, and task ID.

**Enforced today:**

* **Paths.** They stay inside the workspace: no absolute paths, no `..`, and no symlink out of it, dangling
  ones included.
* **Programs.** A program runs only as `allow_proc` resolved it when the config loaded: a bare name through
  `PATH` (relative entries skipped), or an absolute path. A program planted elsewhere under an allowed name does
  not run, and no entry may resolve inside the workspace, where the plan could rewrite it.
* **Environment.** The plan sets only the variables named in `allow_env`. It never sets `PATH` (the host
  gives the child its own, minus workspace and relative entries), the loader's `LD_*`/`DYLD_*`, exported shell
  functions, or the hooks shells and interpreters run code from.
* **Process limits.** A process gets 30 s unless the plan asks for less (300 s at most), and each output stream
  is capped at 1 MiB. When it exits or times out, its whole process group (its background children included)
  is killed before it is reaped, and it dies if the host does.
* **Reads.** A read returns at most 1 MiB.
* **Browser.** It visits only `allowed_hosts` over http(s): `goto` sends the URL as checked, and every later
  action first re-checks the page and abandons one that wandered off. Chrome keeps its sandbox unless
  `chrome_no_sandbox` is set, and downloads, when allowed, land in the workspace.
* **Rounds.** The loop stops after 8 rounds (`MAX_HOST_STEPS`).

**Not enforced yet:** fuel, wall-clock and token budgets, and an audit log. The limits listed under
[Status](#status) apply. Whatever the planner reads, a
secrets file included, is sent to the configured LLM provider, so with a cloud model it leaves the machine.

---

## Roadmap

Short-term:

* Solidify WIT contracts.
* Basic fs + proc + llm loop.
* Simple JSON action format for LLMs.
* Example: “clone repo, run tests, fix lint, summarize” workflow.
* Browser login sample task using `browser.*` selectors (see `docs/browser-login-task.md`).

Medium-term:

* Browser automation.
* Pluggable LLM backends.
* Policy DSL.
* MCP / tool protocol integration.
* Hardened CI examples and threat model documentation.

Long-term:

* Cross-platform input automation (Linux/Windows/macOS).
* Visual runner and policy inspector.
* Library mode for embedding into other tools.

---

## Contributing

Contributions welcome once the initial skeleton lands.

Planned expectations:

* No unsafe OS shortcuts that bypass the model.
* Every new capability must be:

  * WIT-defined,
  * scoped,
  * logged,
  * behind config.

Open issues and discussions will drive the initial design; send PRs that tighten guarantees, not weaken them.

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
