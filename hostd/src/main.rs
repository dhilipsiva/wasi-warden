mod actions;
mod resources;
mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit",
        world: "control",
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
mod runtime;
mod sandbox;
mod state;

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
