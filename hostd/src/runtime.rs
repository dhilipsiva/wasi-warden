use std::path::PathBuf;

use anyhow::{Context, Result, bail};
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
use crate::cli::StepArgs;
use crate::config::HostConfig;
use crate::state::HostState;

const MAX_HOST_STEPS: u32 = 8;

pub async fn run_step(args: StepArgs) -> Result<()> {
    let config = HostConfig::from_step_args(&args)?;
    let engine = build_engine()?;
    let component = load_component(&engine, &args.component)?;

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

    let tokio_handle = Handle::current();
    let mut executor = ActionExecutor::new(config.clone(), tokio_handle.clone());

    let mut store = Store::new(&engine, HostState::new(config.clone()));
    let control = bindings::Control::instantiate(&mut store, &component, &linker)
        .map_err(anyhow::Error::from)
        .context("failed to instantiate component")?;
    let planner = control.osagent_agent_planner();

    for iteration in 0..MAX_HOST_STEPS {
        let planner_result = planner
            .call_step(&mut store, &args.task, &observation)
            .map_err(anyhow::Error::from)
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
                    total_steps = iteration + 1,
                    "planner completed task"
                );
                return Ok(());
            }
        }
    }

    bail!(
        "planner did not complete within {} steps (last summary: {})",
        MAX_HOST_STEPS,
        observation.summary
    )
}

fn build_engine() -> Result<Engine> {
    // Wasm backtraces are on by default (20 frames).
    let mut config = Config::default();
    config.wasm_component_model(true);
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
