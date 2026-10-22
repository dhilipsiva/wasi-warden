use std::fmt;

use tokio::runtime::Handle;
use wasmtime::ResourceLimiter;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::browser::Sessions;
use crate::budget;
use crate::config::HostConfig;

/// The most instances, tables and linear memories the agent may create. A component built from Rust
/// needs a handful; each memory reserves gigabytes of address space, so wasmtime's default of 10,000
/// is far too many.
const MAX_INSTANCES: usize = 256;
const MAX_TABLES: usize = 64;
const MAX_MEMORIES: usize = 16;

/// What the agent's store holds: the host's configuration and budgets, the resources the agent
/// holds handles to, an empty WASI context, the run's browser sessions, and the limiter that caps
/// its memory.
pub struct HostState {
    pub config: HostConfig,
    pub resources: ResourceTable,
    pub wasi_ctx: WasiCtx,
    pub budgets: budget::Shared,
    /// For the browser, whose WebDriver calls block on the runtime (from the loop's blocking thread).
    pub tokio: Handle,
    pub limits: GuestLimiter,
    /// The run's browser sessions, shared with the planner's executor.
    pub sessions: Sessions,
    /// Events and grant requests the agent has logged ([`crate::capabilities`] caps them).
    pub events: u32,
}

impl HostState {
    pub fn new(
        config: HostConfig,
        budgets: budget::Shared,
        tokio: Handle,
        sessions: Sessions,
    ) -> Self {
        // No preopened directories, environment, arguments or network: the agent reaches the host only
        // through the WIT capabilities.
        let wasi_ctx = WasiCtx::builder().build();
        let limits = GuestLimiter::new(config.budgets.guest_memory_bytes);
        Self {
            config,
            resources: ResourceTable::new(),
            wasi_ctx,
            budgets,
            tokio,
            limits,
            sessions,
            events: 0,
        }
    }
}

/// A growth that would take the agent past `guest_memory_bytes`. It traps the guest, and the host
/// names it ([`crate::runtime`]).
#[derive(Debug)]
pub struct OverMemory {
    pub limit: u64,
}

impl fmt::Display for OverMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the agent would pass its memory budget ({} bytes, [budgets] guest_memory_bytes)",
            self.limit
        )
    }
}

impl std::error::Error for OverMemory {}

/// Caps the agent's memory in total: its linear memories and its tables (a pointer per element)
/// together. wasmtime's own `StoreLimits` caps each linear memory on its own, and a component may
/// hold many, so a guest could multiply its share.
///
/// A growth is counted once allowed, and stays counted even if it then fails: wasmtime also reports
/// failures it never asked about (a 64-bit table growth whose size overflows), so taking one back
/// would let a guest grow for free.
pub struct GuestLimiter {
    limit: u64,
    used: u64,
}

impl GuestLimiter {
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    fn grow(&mut self, current: u64, desired: u64) -> wasmtime::Result<bool> {
        let added = desired.saturating_sub(current);
        let total = self.used.saturating_add(added);
        if total > self.limit {
            return Err(wasmtime::Error::new(OverMemory { limit: self.limit }));
        }
        self.used = total;
        Ok(true)
    }
}

/// The bytes `elements` table entries take.
fn table_bytes(elements: usize) -> u64 {
    (elements as u64).saturating_mul(std::mem::size_of::<usize>() as u64)
}

impl ResourceLimiter for GuestLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|max| desired > max) {
            // Fails anyway, past the memory's own maximum: nothing to count.
            return Ok(false);
        }
        self.grow(current as u64, desired as u64)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        self.grow(table_bytes(current), table_bytes(desired))
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
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

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    /// Two memories share one budget: each may stay under it while together they would not.
    #[test]
    fn the_memory_budget_is_a_total_across_memories() {
        let mut limiter = GuestLimiter::new(64 << 20);
        assert!(limiter.memory_growing(0, 40 * MIB, None).unwrap());
        let err = limiter.memory_growing(0, 40 * MIB, None).unwrap_err();
        assert!(err.downcast_ref::<OverMemory>().is_some(), "{err}");
        assert!(limiter.memory_growing(0, 20 * MIB, None).unwrap());
    }

    /// Tables count against the same budget, at a pointer per element.
    #[test]
    fn tables_count_against_the_memory_budget() {
        let mut limiter = GuestLimiter::new(64 << 20);
        assert!(limiter.memory_growing(0, 60 * MIB, None).unwrap());
        assert!(limiter.table_growing(0, 10_000_000, None).is_err());
        assert!(limiter.table_growing(0, 1_000, None).unwrap());
    }

    /// A growth past the memory's own maximum fails without being counted. One that fails after it
    /// was allowed stays counted: wasmtime also reports failures it never asked about (a table
    /// growth whose size overflows), so taking a growth back would let a guest grow for free.
    #[test]
    fn a_failed_growth_stays_counted() {
        let mut limiter = GuestLimiter::new(64 << 20);
        assert!(!limiter.memory_growing(0, 60 * MIB, Some(MIB)).unwrap());
        assert!(limiter.memory_growing(0, 60 * MIB, None).unwrap());
        limiter
            .table_grow_failed(wasmtime::Error::msg("overflow calculating new table size"))
            .unwrap();
        limiter
            .memory_grow_failed(wasmtime::Error::msg("no memory"))
            .unwrap();
        assert!(
            limiter.memory_growing(0, 10 * MIB, None).is_err(),
            "the 60 MiB is still counted"
        );
    }

    #[test]
    fn instances_tables_and_memories_are_few() {
        let limiter = GuestLimiter::new(1);
        assert!(limiter.instances() <= 256 && limiter.tables() <= 64 && limiter.memories() <= 16);
    }
}
