use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::config::HostConfig;

#[allow(dead_code)]
pub struct HostState {
    pub config: HostConfig,
    pub resources: ResourceTable,
    pub wasi_ctx: WasiCtx,
}

impl HostState {
    pub fn new(config: HostConfig) -> Self {
        // No preopened directories, environment, arguments or network: the agent reaches the host only
        // through the WIT capabilities.
        let wasi_ctx = WasiCtx::builder().build();
        Self {
            config,
            resources: ResourceTable::new(),
            wasi_ctx,
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
