use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, HasSelf, Linker},
};
use wasmtime_wasi::p2::add_to_linker_sync;

use tokio::runtime::Handle;

use crate::actions::{ActionExecutor, ActionReport};
use crate::audit::Audit;
use crate::bindings;
use crate::bindings::exports::osagent::agent::planner::{AgentError, Observation, StepResponse};
use crate::browser::Sessions;
use crate::budget::{self, Budgets, Kind, Limits};
use crate::cli::StepArgs;
use crate::config::HostConfig;
use crate::registry;
use crate::state::{HostState, OverMemory};

/// Run the plan-act loop for `args`. The loop blocks: wasmtime calls the component synchronously,
/// the `llm` capability makes blocking HTTP requests, and the executor drives WebDriver futures with
/// `Handle::block_on`, which panics on a runtime worker thread. So it runs on a blocking thread,
/// where `block_on` is allowed, and the executor's `Drop` (which quits browser sessions) runs there
/// too. A blocking-pool thread has tokio's 2 MiB stack (the main thread has 8 MiB); that is ample for
/// the host side of the loop, and the guest runs on wasmtime's own bounded wasm stack (512 KiB).
pub async fn run_step(args: StepArgs) -> Result<()> {
    let tokio_handle = Handle::current();
    tokio::task::spawn_blocking(move || run_step_blocking(args, tokio_handle))
        .await
        .context("the step thread panicked")?
}

/// How often the engine's epoch advances: the resolution of the wall-clock interrupt.
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// How long past its wall-clock budget a run may stay inside one host call before hostd ends it.
const OVERRUN_GRACE: Duration = Duration::from_secs(5);

/// The longest compiling the component may take. Compiling comes before the run's clock starts, and
/// the component is the agent's code, so it is capped here.
const COMPILE_LIMIT: Duration = Duration::from_secs(120);

/// The most actions one plan runs; the rest are reported as not run.
const MAX_ACTIONS_PER_PLAN: usize = 32;

/// The most of a guest-written string (a thought, an outcome, an error) a log line holds.
const LOG_FIELD_CHARS: usize = 256;

/// `text`, cut to [`LOG_FIELD_CHARS`] for a log line.
fn clip(text: &str) -> String {
    let chars = text.chars().count();
    if chars <= LOG_FIELD_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(LOG_FIELD_CHARS).collect();
    format!("{kept}… ({chars} chars)")
}

/// Ends hostd when the run outlasts its wall-clock budget by [`OVERRUN_GRACE`]. Epochs interrupt a
/// guest only while it runs wasm; one blocked in a host call (a WASI clock sleep of any length, a page
/// load, an LLM request) is ended here instead. The run's browser sessions are quit first; its
/// processes die with hostd (`sandbox::run_bounded`).
struct Watchdog {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(deadline: Option<Instant>, limits: Limits, sessions: Sessions, tokio: Handle) -> Self {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = deadline.and_then(|deadline| deadline.checked_add(OVERRUN_GRACE)).map(|end| {
            std::thread::spawn(move || {
                let wait = end.saturating_duration_since(Instant::now());
                if let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(wait) {
                    // hostd exits whatever happens here: a cleanup that panics or a stderr that
                    // blocks only costs the cleanup or the message.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        sessions.quit_all(&tokio)
                    }));
                    let message = format!(
                        "Error: the run outlasted its wall-clock budget ({} ms, [budgets] wallclock_ms) \
                         by {} s inside a host call; hostd stops it\n",
                        limits.wallclock_ms,
                        OVERRUN_GRACE.as_secs()
                    );
                    write_within(std::io::stderr(), message, Duration::from_secs(1));
                    std::process::exit(1);
                }
            })
        });
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Advances the engine's epoch every [`EPOCH_TICK`] until dropped, so a guest past its wall-clock
/// deadline is interrupted even when it never calls the host.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: &Engine) -> Self {
        let (engine, stop) = (engine.clone(), Arc::new(AtomicBool::new(false)));
        let running = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            // The epoch follows the clock, not the count of sleeps, so a late wakeup does not
            // push the deadline back.
            let started = Instant::now();
            let mut ticks: u128 = 0;
            while !running.load(Ordering::Relaxed) {
                std::thread::sleep(EPOCH_TICK);
                let due = started.elapsed().as_millis() / EPOCH_TICK.as_millis();
                while ticks < due {
                    engine.increment_epoch();
                    ticks += 1;
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An error that stops the loop because `kind`'s budget is spent, worded for the operator.
fn spent(budgets: &budget::Shared, kind: Kind, what: &str) -> Result<()> {
    let check = budgets
        .lock()
        .map_err(|_| anyhow!("the budgets are unavailable"))?
        .check(kind);
    check.map_err(|why| anyhow!("the {what} budget ran out before the planner completed: {why}"))
}

fn run_step_blocking(args: StepArgs, tokio_handle: Handle) -> Result<()> {
    let config = HostConfig::from_step_args(&args)?;
    let limits = config.budgets;
    let engine = build_engine()?;
    let component = load_component(&engine, &args.component)?;
    // The run's clock starts once the component is compiled: compiling is the host's work on the
    // file the operator chose, and its time depends on the host's load. Instantiation runs guest
    // code, so it is inside the budget.
    let budgets = Budgets::shared(limits);
    let sessions = Sessions::default();
    let deadline = budgets.lock().ok().and_then(|b| b.deadline());
    let watchdog = Watchdog::start(deadline, limits, sessions.clone(), tokio_handle.clone());
    let _ticker = EpochTicker::start(&engine);

    let audit = Audit::install(config.audit_path.as_deref(), config.secrets.clone())?;
    let tools = registry::catalog().to_string();
    let observation_json = validate_json(&args.observation)?;
    let mut current_step = args.step;
    let mut observation = Observation {
        step: current_step,
        summary: format!("host bootstrap step {}", current_step),
        data: observation_json,
        tools: tools.clone(),
    };

    let mut linker: Linker<HostState> = Linker::new(&engine);
    add_to_linker_sync(&mut linker)
        .map_err(anyhow::Error::from)
        .context("failed to add WASI to linker")?;
    bindings::Control::add_to_linker::<_, HasSelf<_>>(&mut linker, |state: &mut HostState| state)?;

    let mut executor = ActionExecutor::new(
        config.clone(),
        tokio_handle.clone(),
        Arc::clone(&budgets),
        sessions.clone(),
        audit,
    );

    let mut store = Store::new(
        &engine,
        HostState::new(
            config.clone(),
            Arc::clone(&budgets),
            tokio_handle.clone(),
            sessions.clone(),
        ),
    );
    store.limiter(|state| &mut state.limits);
    // Everything that runs guest code is in here, so every way out of it is cleaned up below.
    let result = (|| -> Result<()> {
        // Instantiation runs guest code too, so it is bounded like a round.
        bound_round(&mut store, &budgets, limits)?;
        let control = bindings::Control::instantiate(&mut store, &component, &linker)
            .map_err(|err| guest_stopped(err, limits))
            .context("failed to instantiate component")?;
        let planner = control.osagent_agent_planner();
        for round in 1.. {
            spent(&budgets, Kind::WallclockMs, "wall-clock")?;
            spent(&budgets, Kind::LlmTokens, "token")?;
            if budget::claim(&budgets, Kind::Steps, 1).is_err() {
                bail!(
                    "the step budget ({} rounds) ran out before the planner completed (last summary: {})",
                    limits.steps,
                    observation.summary
                );
            }
            bound_round(&mut store, &budgets, limits)?;
            audit.set_step(current_step);
            let planner_result = planner
                .call_step(&mut store, &args.task, &observation)
                .map_err(|err| guest_stopped(err, limits))
                .context("planner.step failed")?;
            let response = planner_result.map_err(agent_failure)?;

            match response {
                StepResponse::Continue(plan) => {
                    info!(
                        step = current_step,
                        thought = clip(&plan.thought),
                        actions = plan.actions.len(),
                        "planner requested capability executions"
                    );
                    let (run, past) = plan
                        .actions
                        .split_at(plan.actions.len().min(MAX_ACTIONS_PER_PLAN));
                    // Reports come back with every secret value redacted to its reference.
                    let mut reports = executor.execute(run);
                    if !past.is_empty() {
                        reports.push(ActionReport::failed(
                            "plan".to_string(),
                            anyhow!(
                                "{} more actions were not run: a plan runs at most {MAX_ACTIONS_PER_PLAN}",
                                past.len()
                            ),
                        ));
                    }
                    log_action_reports(&reports);
                    current_step = current_step.saturating_add(1);
                    observation = Observation {
                        step: current_step,
                        summary: summarize_reports(&reports),
                        data: build_action_observation(&reports)?,
                        tools: tools.clone(),
                    };
                }
                StepResponse::Complete(done) => {
                    info!(
                        reason = clip(&done.reason),
                        outcome = clip(&done.outcome),
                        total_steps = round,
                        "planner completed task"
                    );
                    return Ok(());
                }
            }
        }
        unreachable!("the step budget ends the loop")
    })();
    // The run is over: the watchdog stands down, and the sessions it would have quit are quit
    // here, each within browser::QUIT_WAIT, so a run that completed is not then ended as overdue.
    drop(watchdog);
    drop(executor);
    drop(store);
    sessions.quit_all(&tokio_handle);
    result
}

/// Give the guest its fuel for one round (`guest_fuel`; 0 for none) and an epoch deadline at the
/// end of the wall-clock budget.
fn bound_round(
    store: &mut Store<HostState>,
    budgets: &budget::Shared,
    limits: Limits,
) -> Result<()> {
    let fuel = if limits.guest_fuel == 0 {
        u64::MAX
    } else {
        limits.guest_fuel
    };
    store.set_fuel(fuel).map_err(anyhow::Error::from)?;
    let remaining = budgets
        .lock()
        .map(|b| b.remaining_wallclock())
        .unwrap_or_default();
    let ticks = (remaining.as_millis() / EPOCH_TICK.as_millis()).max(1) as u64;
    store.set_epoch_deadline(ticks);
    Ok(())
}

/// Name a guest trap the operator caused by a limit: out of fuel, past the wall-clock budget, or
/// past the memory budget.
fn guest_stopped(err: wasmtime::Error, limits: Limits) -> anyhow::Error {
    if let Some(over) = err.downcast_ref::<OverMemory>() {
        return anyhow!("{over}");
    }
    match err.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::OutOfFuel) => anyhow!(
            "the agent ran out of fuel ({} units per round, [budgets] guest_fuel)",
            limits.guest_fuel
        ),
        Some(wasmtime::Trap::Interrupt) => anyhow!(
            "the agent was interrupted at the wall-clock budget ({} ms, [budgets] wallclock_ms)",
            limits.wallclock_ms
        ),
        _ => anyhow::Error::from(err),
    }
}

fn build_engine() -> Result<Engine> {
    // Wasm backtraces are on by default (20 frames). Fuel bounds the guest's work in a round, and
    // epoch interruption bounds its wall-clock time (EpochTicker).
    let mut config = Config::default();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config.epoch_interruption(true);
    // Off, as the memory budget cannot hold them (GuestLimiter): 64-bit memories and tables (an
    // overflowing table growth reaches the limiter as a failure it never approved), shared memories
    // (never shown to it), several memories in one module, and the GC heap.
    config.wasm_memory64(false);
    config.wasm_features(wasmtime::WasmFeatures::THREADS, false);
    config.wasm_shared_everything_threads(false);
    config.wasm_multi_memory(false);
    config.wasm_gc(false);
    config.wasm_features(wasmtime::WasmFeatures::FUNCTION_REFERENCES, false);
    config.wasm_custom_page_sizes(false);
    config.wasm_stack_switching(false);
    Engine::new(&config)
        .map_err(anyhow::Error::from)
        .context("failed to build Wasmtime engine")
}

/// `work`'s result if it finishes within `limit`; otherwise none, and `work` is left running on its
/// thread (until hostd exits).
fn finish_within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    finished.recv_timeout(limit).ok()
}

/// Write `message` to `out`, waiting at most `wait`: a full pipe or a held lock costs the message,
/// not the caller.
fn write_within(mut out: impl std::io::Write + Send + 'static, message: String, wait: Duration) {
    let _ = finish_within(wait, move || {
        let _ = out.write_all(message.as_bytes());
        let _ = out.flush();
    });
}

/// Compile the component, within [`COMPILE_LIMIT`].
fn load_component(engine: &Engine, path: &Path) -> Result<Component> {
    let (engine, file) = (engine.clone(), path.to_path_buf());
    let component = finish_within(COMPILE_LIMIT, move || Component::from_file(&engine, &file))
        .ok_or_else(|| {
            anyhow!(
                "compiling the component {} took longer than {} s",
                path.display(),
                COMPILE_LIMIT.as_secs()
            )
        })?;
    component
        .map_err(anyhow::Error::from)
        .with_context(|| format!("failed to load component {}", path.display()))
}

fn validate_json(input: &str) -> Result<String> {
    let json: Value = serde_json::from_str(input)
        .with_context(|| format!("observation is not valid JSON: {input}"))?;
    Ok(json.to_string())
}

fn agent_failure(err: AgentError) -> anyhow::Error {
    anyhow::anyhow!(
        "agent-core reported error (retryable={}): {}",
        err.retryable,
        err.message
    )
}

fn log_action_reports(reports: &[ActionReport]) {
    for report in reports {
        if report.success {
            debug!(capability = clip(&report.capability), "action succeeded");
        } else {
            warn!(
                capability = clip(&report.capability),
                error = clip(report.error.as_deref().unwrap_or("unknown failure")),
                "action failed"
            );
        }
    }
}

fn summarize_reports(reports: &[ActionReport]) -> String {
    if reports.is_empty() {
        return "planner returned no actions".to_string();
    }
    let failures = reports.iter().filter(|r| !r.success).count();
    format!(
        "executed {} action(s) with {} failure(s)",
        reports.len(),
        failures
    )
}

fn build_action_observation(reports: &[ActionReport]) -> Result<String> {
    let payload = json!({ "actions": reports });
    serde_json::to_string(&payload).context("failed to serialize action observation")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work that outlasts its limit is given up on; work within it returns its result.
    #[test]
    fn finish_within_gives_up_at_the_limit() {
        let started = std::time::Instant::now();
        let late = finish_within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(5));
            1
        });
        assert_eq!(late, None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(finish_within(Duration::from_secs(5), || 2), Some(2));
    }

    /// A writer that never finishes (a full pipe) does not hold the caller past the wait.
    #[test]
    fn a_blocked_writer_does_not_hold_the_watchdog() {
        struct Stuck;
        impl std::io::Write for Stuck {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_secs(10));
                Ok(0)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let started = std::time::Instant::now();
        write_within(Stuck, "message".to_string(), Duration::from_millis(100));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// A core module of `sections` (raw section bytes).
    fn module(sections: &[u8]) -> Vec<u8> {
        let mut bytes = b"\0asm\x01\0\0\0".to_vec();
        bytes.extend_from_slice(sections);
        bytes
    }

    /// The engine refuses the proposals the memory budget cannot hold: 64-bit memories and tables
    /// (an overflowing 64-bit table growth is reported to the limiter as a failure it never
    /// approved), shared memories (never shown to the limiter) and multiple memories.
    #[test]
    fn the_engine_refuses_proposals_the_memory_budget_cannot_hold() {
        let engine = build_engine().expect("engine");
        let one_memory = module(b"\x05\x03\x01\x00\x01");
        assert!(
            wasmtime::Module::new(&engine, &one_memory).is_ok(),
            "the control loads"
        );
        for (what, sections) in [
            ("a 64-bit memory", &b"\x05\x03\x01\x04\x01"[..]),
            ("a 64-bit table", &b"\x04\x04\x01\x70\x04\x01"[..]),
            ("a shared memory", &b"\x05\x04\x01\x03\x01\x01"[..]),
            ("two memories", &b"\x05\x05\x02\x00\x01\x00\x01"[..]),
        ] {
            assert!(
                wasmtime::Module::new(&engine, module(sections)).is_err(),
                "{what} is refused"
            );
        }
    }

    fn report(capability: &str, success: bool) -> ActionReport {
        ActionReport {
            capability: capability.to_string(),
            success,
            output: json!({ "ok": success }),
            error: (!success).then(|| "boom".to_string()),
        }
    }

    #[test]
    fn an_observation_must_be_json_and_is_normalised() {
        assert!(validate_json("not json").is_err());
        assert_eq!(validate_json("{ \"a\" : 1 }").unwrap(), "{\"a\":1}");
    }

    #[test]
    fn a_summary_counts_actions_and_failures() {
        assert_eq!(summarize_reports(&[]), "planner returned no actions");
        assert_eq!(
            summarize_reports(&[report("fs.list_dir", true), report("proc.spawn", false)]),
            "executed 2 action(s) with 1 failure(s)"
        );
    }

    #[test]
    fn the_next_observation_carries_every_report() {
        let data =
            build_action_observation(&[report("fs.list_dir", true), report("proc.spawn", false)])
                .unwrap();
        let value: Value = serde_json::from_str(&data).unwrap();
        let actions = value["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[1]["capability"], "proc.spawn");
        assert_eq!(actions[1]["error"], "boom");
    }
}
