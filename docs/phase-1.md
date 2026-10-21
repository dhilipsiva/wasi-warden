## 0. Opinionated baseline

Use these defaults unless you have a strong reason not to:

* Runtime: **Wasmtime** (Rust, secure, component model, WASI Preview 2 ready).
* Language: **Rust** for both host and guest.
* Topology: **Single host daemon + one Wasm “brain” component**.
* Platform v0: **Linux only**, X11/Wayland aware. Add macOS/Windows later.
* LLM: **HTTP tool-calling style** (can be OpenAI, local OpenAI-compatible, Ollama, whatever).

Your unique angle: OS-level control via **capability imports** into Wasm, not Python scripts.

---

## 1. High-level architecture

One sentence: **Wasm agent = untrusted planner; Rust host = privileged executor with policies.**

Components:

1. `hostd` (native Rust)

   * Embeds Wasmtime.
   * Loads the Wasm agent (component).
   * Implements WIT interfaces:

     * `fs` (scoped)
     * `proc`
     * `browser` (WebDriver / CDP)
     * `input` (mouse/keyboard)
     * `llm` (proxy to whatever model user configures)
     * `policy` (budget, allow/deny)
   * Exposes a CLI / API for humans or other agents.

2. `agent-core.wasm` (WASI component, Rust-compiled)

   * Imports those WIT interfaces.
   * Implements plan–act–observe loop.
   * Uses only imported capabilities.
   * No direct syscalls, no ambient FS/net.

3. Config + logs

   * TOML/YAML for policies:

     * allowed paths, processes, domains
     * input allowed: on/off; scopes
   * Structured logs for every tool invocation.

---

## 2. Project layout

Create a mono-repo like this:

```text
os-agent/
  wit/
    fs.wit
    proc.wit
    browser.wit
    input.wit
    llm.wit
    policy.wit
    agent.wit         # entrypoint
  hostd/
    Cargo.toml
    src/main.rs
    src/engine.rs
    src/caps_fs.rs
    src/caps_proc.rs
    src/caps_browser.rs
    src/caps_input.rs
    src/caps_llm.rs
    src/policy.rs
  agent-core/
    Cargo.toml
    src/lib.rs        # compiled to wasm component
```

Single responsibility:

* `wit/` = contract.
* `hostd/` = privileged, audited, ugly OS stuff.
* `agent-core/` = pure logic, safe, testable, replaceable.

---

## 3. Define the WIT capabilities (minimal v0)

Keep v0 surgical. No raw “run shell(command)”.

### `fs.wit` (read/write inside workspace)

```wit
package osagent:fs

interface fs {
  /// List entries in a directory handle.
  list_dir: func(dir: u32) -> expected<list<string>, string>

  /// Read file by path relative to a directory handle.
  read_file: func(dir: u32, path: string) -> expected<string, string>

  /// Write file, create/overwrite inside allowed dir.
  write_file: func(dir: u32, path: string, contents: string) -> expected<(), string>

  /// Get a handle to the root workspace (host decides path).
  open_workspace: func() -> expected<u32, string>
}
```

No absolute paths. Only handles from `open_workspace`.

### `proc.wit` (controlled process exec)

```wit
package osagent:proc

interface proc {
  spawn: func(cmd: string, args: list<string>, cwd_handle: u32)
      -> expected<u32, string>  // pid handle
  wait: func(pid: u32, timeout_ms: u64)
      -> expected<s32, string>  // exit code
}
```

Host enforces allowlist on `cmd`.

### `browser.wit` (WebDriver / CDP proxy, v0)

```wit
package osagent:browser

interface browser {
  new_session: func() -> expected<u32, string>
  goto: func(sess: u32, url: string) -> expected<(), string>
  click: func(sess: u32, selector: string) -> expected<(), string>
  type: func(sess: u32, selector: string, text: string) -> expected<(), string>
  html: func(sess: u32) -> expected<string, string>
}
```

Backed by `thirtyfour` or CDP.

### `input.wit` (only when explicitly enabled)

```wit
package osagent:input

interface input {
  key_seq: func(text: string) -> expected<(), string>
  mouse_move: func(x: u32, y: u32) -> expected<(), string>
  mouse_click: func(button: string) -> expected<(), string>
}
```

Wire to `enigo`/`rdev` etc. Only export/import if policy says so.

### `llm.wit`

```wit
package osagent:llm

type Tool = record { name: string, schema_json: string }

interface llm {
  complete: func(prompt: string) -> expected<string, string>
  call_tools: func(prompt: string, tools: list<Tool>)
      -> expected<string, string> // model’s JSON tool calls
}
```

Host implementation:

* Reads end-user config (OpenAI / local / whatever).
* Performs HTTP.
* Returns raw JSON; agent-core interprets.

### `agent.wit`

```wit
package osagent:agent

interface agent {
  /// Single-step: given a task + last observation, decide next actions.
  step: func(task: string, observation_json: string) -> expected<string, string>
}
```

`step` returns JSON:

* either `{ "done": true, "reason": "...", "result": ... }`
* or `{ "done": false, "actions": [ ... ] }`

Host enforces schema.

---

## 4. Implement `hostd` (v0 roadmap)

Order matters. Do this:

1. **Wire Wasmtime + WIT bindings**

   * Use `wasmtime` + `wasmtime::component` + `wit-bindgen`.
   * Load `agent-core` as a component.
   * Call `agent::step` from Rust with dummy data.

2. **Implement `fs` capability**

   * Hardcode workspace root: e.g. `/var/lib/os-agent/workspace` or `$HOME/os-agent`.
   * Map `open_workspace` to handle `1`.
   * Implement `list_dir/read_file/write_file` with strict path join + normalization.
   * No other FS access.

3. **Implement `llm` capability**

   * Read config from `~/.os-agent/config.toml`.
   * Support: `type = "openai"` or `type = "openai_compatible"` or `type = "none"`.
   * Call API, return content. No model-specific logic in `agent-core`.

4. **Host loop**

   * `hostd step --task "run tests" --observation '{}'`
   * Calls `agent.step`.
   * Parses returned actions JSON.
   * Executes allowed actions via `fs`/`proc` modules.
   * Feeds result as next `observation_json`.
   * Repeat until `done`.

At this point you have:

* No input automation.
* No browser.
* But a **real Wasm-based agent** that can edit files and run commands under strict scope.

Then:

5. Add **`proc`** with allowlisted commands (`cargo`, `pytest`, etc.).
6. Add **`browser`** with Playwright/WebDriver for specific tasks.
7. Add **`input`** only behind a scary flag.

---

## 5. Implement `agent-core` logic

`agent-core` is pure Rust compiled to component:

* Use `wit-bindgen` guest side.
* Structure:

```rust
// agent-core/src/lib.rs
mod bindings; // generated

use bindings::{
    exports::osagent::agent::agent,
    osagent::{fs, proc, browser, llm},
};

struct Agent;

impl agent::Guest for Agent {
    fn step(task: String, observation_json: String)
        -> Result<String, String>
    {
        // 1. Build prompt with task + last obs + tool schema.
        // 2. Call llm::call_tools.
        // 3. Return its JSON tool-plan directly.
        // Minimal logic v0: thin translator.
    }
}

bindings::export!(Agent);
```

v0: agent-core is mostly a broker:

* Encodes tools list.
* Asks LLM: “Given tools X, Y, Z, output JSON actions.”
* Returns that JSON to host.

Future:

* Add local validation, self-checks, heuristics.

---

## 6. Security/policy essentials from day one

Do not postpone these:

* **Deny-by-default**:

  * No ambient FS.
  * No outbound network except `llm` if allowed.
  * No env vars.
* **Allowlists**:

  * `proc.spawn`: only from a configured set.
  * `browser.goto`: optional domain allowlist.
* **Budgets**:

  * Max steps per task.
  * Max wall-clock per step.
* **Audit**:

  * Log: timestamp, task_id, action, args, result.

This is what differentiates you from “Auto-GPT but Rust”.

---

## 7. Concrete “first commit” checklist

You want something you can code in a weekend:

1. New repo `os-agent`.
2. Add `wit/agent.wit` + `wit/fs.wit` + `wit/llm.wit`.
3. Implement:

   * `agent-core`:

     * expose `step` that calls `llm.complete` and returns hardcoded no-op actions.
   * `hostd`:

     * loads component
     * implements `fs.open_workspace` (fixed path)
     * implements `llm.complete` (just echoes for now)
     * CLI: `os-agent step "echo hello"`.
4. Verify end-to-end: CLI → hostd → wasm → hostd → stdout.

Then iterate:

* Replace echo LLM with real HTTP.
* Teach agent-core to emit a simple action schema.
* Implement a single real tool: `proc.spawn("echo", ["hello"])`.
* Add logs and config.

When you reach that, we can design the precise JSON schemas for actions and the initial policy file.
