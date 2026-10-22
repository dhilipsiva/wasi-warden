//! The run's budgets, shared by both executors (the planner's JSON actions and the WIT
//! capabilities) and the plan-act loop. Each counted kind is claimed before the operation it pays
//! for and refused past its limit. LLM tokens are spent after the call that used them, so one call
//! can cross the limit; the next is refused. Wall-clock time is measured from the start of the run.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a budget limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Planner rounds.
    Steps,
    /// Milliseconds since the run started.
    WallclockMs,
    /// LLM tokens, as the endpoint reports them.
    LlmTokens,
    /// Processes spawned.
    Processes,
    /// Filesystem operations.
    FilesystemOps,
    /// Browser actions.
    BrowserActions,
}

impl Kind {
    /// Every kind, in the order the policy snapshot lists them.
    pub const ALL: [Kind; 6] = [
        Kind::Steps,
        Kind::WallclockMs,
        Kind::LlmTokens,
        Kind::Processes,
        Kind::FilesystemOps,
        Kind::BrowserActions,
    ];

    /// The `[budgets]` key that sets this kind's limit.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Steps => "steps",
            Kind::WallclockMs => "wallclock_ms",
            Kind::LlmTokens => "llm_tokens",
            Kind::Processes => "processes",
            Kind::FilesystemOps => "filesystem_ops",
            Kind::BrowserActions => "browser_actions",
        }
    }

    fn index(self) -> usize {
        Kind::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }
}

/// The limits a run starts with (`[budgets]` in hostd.toml).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub steps: u64,
    pub wallclock_ms: u64,
    pub llm_tokens: u64,
    pub processes: u64,
    pub filesystem_ops: u64,
    pub browser_actions: u64,
    /// Wasm fuel the agent may burn in one planner round; 0 for no limit (the wall-clock budget
    /// still interrupts it).
    pub guest_fuel: u64,
    /// The most memory (linear memories, in bytes) the agent's instance may hold.
    pub guest_memory_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            steps: 8,
            wallclock_ms: 10 * 60 * 1000,
            llm_tokens: 200_000,
            processes: 32,
            filesystem_ops: 10_000,
            browser_actions: 200,
            guest_fuel: 10_000_000_000,
            guest_memory_bytes: 256 << 20,
        }
    }
}

impl Limits {
    /// The limit for `kind`.
    pub fn of(&self, kind: Kind) -> u64 {
        match kind {
            Kind::Steps => self.steps,
            Kind::WallclockMs => self.wallclock_ms,
            Kind::LlmTokens => self.llm_tokens,
            Kind::Processes => self.processes,
            Kind::FilesystemOps => self.filesystem_ops,
            Kind::BrowserActions => self.browser_actions,
        }
    }
}

/// A run's budgets: the limits and what has been used of each.
#[derive(Debug)]
pub struct Budgets {
    limits: Limits,
    used: [u64; 6],
    started: Instant,
}

/// The budgets of one run, shared by everything that spends them.
pub type Shared = Arc<Mutex<Budgets>>;

impl Budgets {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            used: [0; 6],
            started: Instant::now(),
        }
    }

    pub fn shared(limits: Limits) -> Shared {
        Arc::new(Mutex::new(Self::new(limits)))
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// What has been used of `kind` (for wall-clock time, the milliseconds elapsed).
    pub fn used(&self, kind: Kind) -> u64 {
        match kind {
            Kind::WallclockMs => self.started.elapsed().as_millis() as u64,
            _ => self.used[kind.index()],
        }
    }

    /// Claim `units` of `kind` before spending them; refused, and nothing recorded, past the limit.
    /// Returns what is used afterwards.
    pub fn claim(&mut self, kind: Kind, units: u64) -> Result<u64, String> {
        let limit = self.limits.of(kind);
        let after = self.used(kind).saturating_add(units);
        if after > limit {
            return Err(format!(
                "the {} budget is spent ({} of {limit} used; {units} more asked)",
                kind.name(),
                self.used(kind)
            ));
        }
        if kind != Kind::WallclockMs {
            self.used[kind.index()] = after;
        }
        Ok(after)
    }

    /// Record `units` of `kind` already spent (LLM tokens, known only after the call).
    pub fn spend(&mut self, kind: Kind, units: u64) {
        if kind != Kind::WallclockMs {
            let i = kind.index();
            self.used[i] = self.used[i].saturating_add(units);
        }
    }

    /// An error if `kind` is used up (at or past its limit).
    pub fn check(&self, kind: Kind) -> Result<(), String> {
        let (used, limit) = (self.used(kind), self.limits.of(kind));
        if used >= limit {
            Err(format!(
                "the {} budget is spent ({used} of {limit} used)",
                kind.name()
            ))
        } else {
            Ok(())
        }
    }

    /// Wall-clock time left in the run.
    pub fn remaining_wallclock(&self) -> Duration {
        Duration::from_millis(self.limits.wallclock_ms).saturating_sub(self.started.elapsed())
    }
}

/// Claim `units` of `kind` from `budgets` (a poisoned lock refuses).
pub fn claim(budgets: &Shared, kind: Kind, units: u64) -> Result<u64, String> {
    budgets
        .lock()
        .map_err(|_| "the budgets are unavailable".to_string())?
        .claim(kind, units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            processes: 2,
            llm_tokens: 1000,
            wallclock_ms: 60_000,
            ..Limits::default()
        }
    }

    #[test]
    fn a_claim_past_the_limit_is_refused_and_not_recorded() {
        let mut b = Budgets::new(limits());
        assert_eq!(b.claim(Kind::Processes, 1), Ok(1));
        assert_eq!(b.claim(Kind::Processes, 1), Ok(2));
        assert!(b.claim(Kind::Processes, 1).is_err());
        assert_eq!(
            b.used(Kind::Processes),
            2,
            "the refused claim was not recorded"
        );
        assert!(
            b.check(Kind::Processes).is_err(),
            "and the budget reads as spent"
        );
    }

    #[test]
    fn spending_may_cross_the_limit_once_and_then_checks_fail() {
        let mut b = Budgets::new(limits());
        b.spend(Kind::LlmTokens, 600);
        assert!(b.check(Kind::LlmTokens).is_ok());
        b.spend(Kind::LlmTokens, 600);
        assert_eq!(b.used(Kind::LlmTokens), 1200);
        assert!(b.check(Kind::LlmTokens).is_err());
    }

    #[test]
    fn wall_clock_time_is_measured_not_claimed() {
        let mut b = Budgets::new(Limits {
            wallclock_ms: 50,
            ..limits()
        });
        assert!(b.check(Kind::WallclockMs).is_ok());
        assert!(b.remaining_wallclock() <= Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.check(Kind::WallclockMs).is_err());
        assert_eq!(b.remaining_wallclock(), Duration::ZERO);
        assert!(b.claim(Kind::WallclockMs, 1).is_err());
    }
}
