use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
use crate::bindings;
use crate::bindings::exports::osagent::agent::planner::{AgentError, Observation, StepResponse};
use crate::budget::{self, Budgets, Kind, Limits};
use crate::cli::StepArgs;
use crate::config::HostConfig;
use crate::state::HostState;

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
            while !running.load(Ordering::Relaxed) {
                std::thread::sleep(EPOCH_TICK);
                engine.increment_epoch();
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
    let budgets = Budgets::shared(limits);
    let engine = build_engine()?;
    let component = load_component(&engine, &args.component)?;
    let _ticker = EpochTicker::start(&engine);

    let observation_json = validate_json(&args.observation)?;
    let mut current_step = args.step;
    let mut observation = Observation {
        step: current_step,
        summary: format!("host bootstrap step {}", current_step),
        data: observation_json,
    };

    let mut linker: Linker<HostState> = Linker::new(&engine);
    add_to_linker_sync(&mut linker)
        .map_err(anyhow::Error::from)
        .context("failed to add WASI to linker")?;
    bindings::Control::add_to_linker::<_, HasSelf<_>>(&mut linker, |state: &mut HostState| state)?;

    let mut executor =
        ActionExecutor::new(config.clone(), tokio_handle.clone(), Arc::clone(&budgets));

    let mut store = Store::new(
        &engine,
        HostState::new(config.clone(), Arc::clone(&budgets), tokio_handle),
    );
    store.limiter(|state| &mut state.limits);
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
        let planner_result = planner
            .call_step(&mut store, &args.task, &observation)
            .map_err(|err| guest_stopped(err, limits))
            .context("planner.step failed")?;
        let response = planner_result.map_err(agent_failure)?;

        match response {
            StepResponse::Continue(plan) => {
                info!(
                    step = current_step,
                    thought = plan.thought,
                    actions = plan.actions.len(),
                    "planner requested capability executions"
                );
                let reports = executor.execute(&plan.actions);
                log_action_reports(&reports);
                current_step = current_step.saturating_add(1);
                observation = Observation {
                    step: current_step,
                    summary: summarize_reports(&reports),
                    data: build_action_observation(&reports)?,
                };
            }
            StepResponse::Complete(done) => {
                info!(
                    reason = done.reason,
                    outcome = done.outcome,
                    total_steps = round,
                    "planner completed task"
                );
                return Ok(());
            }
        }
    }
    unreachable!("the step budget ends the loop")
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

/// Name a guest trap the operator caused by a limit: out of fuel, or past the wall-clock budget.
fn guest_stopped(err: wasmtime::Error, limits: Limits) -> anyhow::Error {
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
    Engine::new(&config)
        .map_err(anyhow::Error::from)
        .context("failed to build Wasmtime engine")
}

fn load_component(engine: &Engine, path: &PathBuf) -> Result<Component> {
    Component::from_file(engine, path)
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
            debug!(capability = report.capability, "action succeeded");
        } else {
            warn!(
                capability = report.capability,
                error = report.error.as_deref().unwrap_or("unknown failure"),
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
