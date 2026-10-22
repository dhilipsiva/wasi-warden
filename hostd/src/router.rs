//! The LLM router behind the `llm` capability (docs/design/router.md): the order the configured
//! providers are tried in, and what an answered call costs. The capability asks the providers in
//! that order until one answers; a failure of any kind (refused connection, timeout, an HTTP
//! error, a reply without content) moves on to the next.

use crate::config::{LlmProvider, Tier};

/// The providers to try, in order: local before frontier, in the configured order within a tier.
/// A paid provider is tried only while the cost budget has some left (`cost_left`, in
/// micro-units); returns the candidates and whether any paid provider was left out.
pub fn candidates(providers: &[LlmProvider], cost_left: u64) -> (Vec<&LlmProvider>, bool) {
    let mut skipped_paid = false;
    let mut order: Vec<&LlmProvider> = Vec::with_capacity(providers.len());
    for tier in [Tier::Local, Tier::Frontier] {
        for provider in providers.iter().filter(|p| p.tier == tier) {
            if provider.is_paid() && cost_left == 0 {
                skipped_paid = true;
            } else {
                order.push(provider);
            }
        }
    }
    (order, skipped_paid)
}

/// The tokens one answered call used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spent {
    pub prompt: u64,
    pub completion: u64,
    /// The endpoint reported no usage, so these are estimates.
    pub estimated: bool,
}

impl Spent {
    pub fn total(&self) -> u64 {
        self.prompt.saturating_add(self.completion)
    }
}

/// What `spent` costs at `provider`'s prices, in millionths of the price unit (prices are per
/// million tokens, so a token at price p costs p micro-units), rounded up.
pub fn cost_micros(provider: &LlmProvider, spent: &Spent) -> u64 {
    let cost = spent.prompt as f64 * provider.input_price
        + spent.completion as f64 * provider.output_price;
    if cost.is_finite() && cost > 0.0 {
        cost.ceil().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

/// Why `provider` was asked: first choice, or after the ones that failed.
pub fn route_reason(provider: &LlmProvider, failed: &[String], skipped_paid: bool) -> String {
    let mut reason = if failed.is_empty() {
        format!("first choice ({} tier)", provider.tier.name())
    } else {
        format!("fallback after {} failed", failed.join(", "))
    };
    if skipped_paid {
        reason.push_str("; paid providers skipped: the llm_cost_micros budget is spent");
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, tier: Tier, price: f64) -> LlmProvider {
        LlmProvider {
            name: name.to_string(),
            tier,
            api_base: "http://127.0.0.1:1/v1".to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            timeout: std::time::Duration::from_secs(1),
            input_price: price,
            output_price: price * 5.0,
        }
    }

    fn names(order: &[&LlmProvider]) -> Vec<String> {
        order.iter().map(|p| p.name.clone()).collect()
    }

    /// Local providers come first, each tier in the configured order.
    #[test]
    fn local_providers_are_tried_first_in_configured_order() {
        let providers = [
            provider("api-a", Tier::Frontier, 2.0),
            provider("box-a", Tier::Local, 0.0),
            provider("api-b", Tier::Frontier, 0.0),
            provider("box-b", Tier::Local, 0.0),
        ];
        let (order, skipped) = candidates(&providers, 1);
        assert_eq!(names(&order), ["box-a", "box-b", "api-a", "api-b"]);
        assert!(!skipped);
    }

    /// With the cost budget spent, only free providers remain.
    #[test]
    fn a_spent_cost_budget_leaves_only_free_providers() {
        let providers = [
            provider("api", Tier::Frontier, 2.0),
            provider("box", Tier::Local, 0.0),
            provider("free-api", Tier::Frontier, 0.0),
        ];
        let (order, skipped) = candidates(&providers, 0);
        assert_eq!(names(&order), ["box", "free-api"]);
        assert!(skipped);
        let (none, skipped) = candidates(&providers[..1], 0);
        assert!(none.is_empty() && skipped);
    }

    /// Prices are per million tokens, so tokens times price is micro-units, rounded up.
    #[test]
    fn cost_is_tokens_times_price_in_micro_units() {
        let p = provider("api", Tier::Frontier, 2.0); // 2.0 in, 10.0 out
        let spent = Spent {
            prompt: 500,
            completion: 500,
            estimated: false,
        };
        assert_eq!(cost_micros(&p, &spent), 6000);
        let fraction = LlmProvider {
            input_price: 0.15,
            output_price: 0.6,
            ..p.clone()
        };
        assert_eq!(
            cost_micros(
                &fraction,
                &Spent {
                    prompt: 3,
                    completion: 0,
                    estimated: true
                }
            ),
            1
        );
        assert_eq!(cost_micros(&provider("box", Tier::Local, 0.0), &spent), 0);
    }

    #[test]
    fn the_route_reason_names_what_failed() {
        let p = provider("api", Tier::Frontier, 2.0);
        assert_eq!(route_reason(&p, &[], false), "first choice (frontier tier)");
        let reason = route_reason(&p, &["box".to_string()], true);
        assert!(reason.starts_with("fallback after box failed"), "{reason}");
        assert!(reason.contains("llm_cost_micros"), "{reason}");
    }
}
