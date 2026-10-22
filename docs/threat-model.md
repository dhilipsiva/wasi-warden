# Threat model

This page covers what wasi-warden defends against, how, and what it does not. The enforced rules are
listed in the README's "Security model", and the known limits are in its Status section. This page ties
them to the threats they answer.

## Assets

- The host's files outside the workspace, and the workspace's own integrity when it is read-only.
- Credentials: the LLM providers' API keys in hostd.toml, and the `[secrets]` the plan may use.
- The host's processes, network position and browser profile.
- The operator's money: LLM spend.
- The host's availability: CPU, memory, disk, time.

## Trust boundaries and adversaries

| Who | Trusted? | Reaches the host through |
|---|---|---|
| The operator (hostd.toml, the task, the component file) | Yes | Configuration |
| **The LLM** (any provider) | **No.** A prompt-injected or hostile model writes the plan | The planner's JSON actions; the guest's WIT calls |
| **The guest component** | **No.** Treated as hostile code | WIT imports, WASI (no preopens, environment or network) |
| **Web pages** the browser visits | **No** | Page content read back; navigation |
| Allowlisted programs | Trusted to be what they are, not to resist the workspace | Run with the host's permissions |
| LLM endpoints and the WebDriver | Trusted transport, untrusted content | Replies, page state |

The central assumption: whatever the planner reads, it may act on. Prompt injection is expected, not
prevented. The defences bound what any plan can do.

## Threats and what answers them

| Threat | Defence | Where |
|---|---|---|
| Read or write outside the workspace | Paths confined component by component through symlinks on every call; no absolute paths or `..`; `read_only`; `max_file_bytes` | `sandbox.rs`, `capabilities.rs` |
| Run an arbitrary program | Only `allow_proc` entries, resolved at load, none inside the workspace; no shell primitive | `sandbox.rs` |
| Hijack an allowed program through its environment | `allow_env` allowlist; `PATH`, the loader's, shells' and interpreters' hooks refused; the child's `PATH` set by the host | `sandbox.rs` |
| Outlive the run, or hold the host | Process timeouts cut to the time left; output caps; group kill before reaping; parent-death signal; a watchdog 5 s past the budget | `sandbox.rs`, `runtime.rs` |
| Exhaust the host from the guest | Fuel per round; epoch interruption; one memory budget over memories and tables; a refused 64-bit, shared or multi-memory module; at most 16 memories, 64 tables and 256 instances; a 120 s compile cap | `runtime.rs`, `state.rs` |
| Exhaust it through the host | Budgets for steps, wall-clock, tokens, cost, processes, fs operations and browser actions; 32 actions a plan; 4 browser sessions; event caps; guest strings cut in log lines | `budget.rs`, `runtime.rs`, `capabilities.rs` |
| Reach arbitrary hosts with the browser | `allowed_hosts` checked on visit and before every action; script evaluation refused; downloads only into a writable workspace | `browser.rs` |
| Exfiltrate a secret to the model | Values never sent: redacted in the one function that builds each request, in reports, in process output and page text returned to the guest | `capabilities.rs`, `secrets.rs` |
| Spend money | Paid providers only while `llm_cost_micros` lasts; `max_tokens` cut to the token budget left | `router.rs`, `capabilities.rs` |
| Act without a record | One audit line per capability call from either path; telemetry per LLM attempt; both redacted | `audit.rs`, `telemetry.rs` |
| Gain a capability at run time | The policy is fixed by hostd.toml; every grant request is refused | `capabilities.rs` |

## What is not defended

The README's Status section lists these in full. In summary:

- **The check-to-use window on paths.** A program running alongside could swap a directory for a symlink
  between the check and the use. Hard links into the workspace are followed.
- **Allowlisted programs.** A tool that reads configuration or hooks from its working directory (git,
  cargo, npm, make), or from an allowed variable (`HOME`), takes instructions from whatever the plan wrote
  there. A process that leaves its group (`setsid`) is not killed with it, and setuid programs drop the
  parent-death signal.
- **The browser between actions.** A page can load anything between two actions, and ports are not
  constrained.
- **Secrets the plan sends where it may.** A secret goes wherever the plan types it on an allowed host,
  and into any allowed environment variable. Redaction matches the exact value only.
- **The model's provider.** Whatever the planner reads, apart from configured secrets, goes to the
  configured providers. With a cloud provider, it leaves the machine.
- **Hostile components at compile time.** Compiling is capped at 120 s but otherwise trusted to
  wasmtime.
- **Denial by a slow page or endpoint.** Bounded only by the wall-clock budget and the watchdog, which
  ends hostd outright.
