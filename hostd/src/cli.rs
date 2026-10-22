use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

/// Command line interface for the host daemon.
#[derive(Parser, Debug)]
#[command(name = "hostd", version, about = "wasi-warden host daemon")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the task: planner rounds until the planner completes it or a budget runs out.
    Run(StepArgs),
    /// Run one planner round and print where it got to as JSON: the next step and its observation
    /// (pass them back with --step and --observation), or the completion.
    Step(StepArgs),
}

#[derive(clap::Args, Debug)]
pub struct StepArgs {
    /// Path to the compiled agent-core component (.wasm/.cwasm).
    #[arg(long, default_value = "./target/wasm32-wasip2/release/agent_core.wasm")]
    pub component: PathBuf,

    /// Path to a host configuration file (TOML). Defaults to ./hostd.toml.
    #[arg(long, default_value = "hostd.toml")]
    pub config: PathBuf,

    /// Root directory the agent may access via the fs capability (overrides config).
    #[arg(long)]
    pub workspace: Option<PathBuf>,

    /// Human task description supplied to the planner.
    #[arg(long)]
    pub task: String,

    /// JSON observation from the previous step.
    #[arg(long, default_value = "{}")]
    pub observation: String,

    /// Step index for logging/budgeting.
    #[arg(long, default_value_t = 0)]
    pub step: u32,

    /// Commands the proc capability may execute (repeat flag to allow multiple, overrides config).
    #[arg(long = "allow-proc", value_name = "CMD", action = ArgAction::Append)]
    pub allow_proc: Vec<String>,
}
