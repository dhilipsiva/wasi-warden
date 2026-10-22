mod actions;
mod audit;
mod resources;
mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit",
        world: "control",
        // A tracing span per WIT call, which the audit layer turns into audit lines. Without
        // `verbose_tracing`, arguments and results holding lists (file bytes, messages) are elided.
        imports: { default: tracing },
        with: {
            "osagent:fs/fs.dir-handle": crate::resources::DirHandleResource,
            "osagent:fs/fs.file-handle": crate::resources::FileHandleResource,
            "osagent:proc/proc.process": crate::resources::ProcessResource,
            "osagent:browser/browser.session": crate::resources::BrowserSessionResource,
            "osagent:browser/browser.element-handle": crate::resources::BrowserElementResource,
        },
    });
}
mod browser;
mod budget;
mod capabilities;
mod cli;
mod config;
mod logging;
mod registry;
mod router;
mod runtime;
mod sandbox;
mod secrets;
mod state;
mod telemetry;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::Step(args) => runtime::run_step(args).await?,
    }
    Ok(())
}
