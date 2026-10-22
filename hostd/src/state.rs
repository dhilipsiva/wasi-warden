use tokio::runtime::Handle;
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::budget;
use crate::config::HostConfig;

/// What the agent's store holds: the host's configuration and budgets, the resources the agent
/// holds handles to, an empty WASI context, and the limiter that caps its memory.
pub struct HostState {
    pub config: HostConfig,
    pub resources: ResourceTable,
    pub wasi_ctx: WasiCtx,
    pub budgets: budget::Shared,
    /// For the browser, whose WebDriver calls block on the runtime (from the loop's blocking thread).
    pub tokio: Handle,
    pub limits: StoreLimits,
}

impl HostState {
    pub fn new(config: HostConfig, budgets: budget::Shared, tokio: Handle) -> Self {
        // No preopened directories, environment, arguments or network: the agent reaches the host only
        // through the WIT capabilities.
        let wasi_ctx = WasiCtx::builder().build();
        let memory = usize::try_from(config.budgets.guest_memory_bytes).unwrap_or(usize::MAX);
        Self {
            config,
            resources: ResourceTable::new(),
            wasi_ctx,
            budgets,
            tokio,
            limits: StoreLimitsBuilder::new().memory_size(memory).build(),
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resources,
        }
    }
}
