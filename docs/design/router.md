# The LLM router and its cost model

hostd's `llm` capability can sit in front of several OpenAI-compatible chat-completions endpoints. The
router (`hostd/src/router.rs`, driven from `hostd/src/capabilities.rs`) picks the order to try them in,
falls back when one fails, and charges the call that answers to the run's budgets. The agent sees one
`llm` interface either way. Telemetry, one line per attempt, is described in [telemetry.md](telemetry.md).

## Provenance

This design comes from wasmai, which was folded into wasi-warden (decision D8 in the products
repository's `DECISIONS.md`). The sources, at wasmai commit `67b9158` (2026-10-21), were:

- `README.md`, "Escalation cascade": a CPU-hosted org model first, then a GPU-hosted one, then frontier
  APIs, stopping at the first good-enough answer.
- `TODO.md`:
  - "Credit / cost philosophy": tiers from cheapest to dearest, and "never default to frontier".
  - "Multi-provider & credits": one provider abstraction; per turn, provider, model, tokens, estimated
    cost, latency and route reason; budget caps.
  - "2.9 Telemetry hooks": a versioned `TelemetryEvent`; never log raw secrets.
  - "Phase 5 — Router cascade": price tables, hard budget caps that stop frontier calls, `route_reason`.
- `docs/distillation-and-cluster-report.md` §8.3 ("TCO Analysis") and §8.4 ("Fallback Escalation"), and
  `docs/dual-mode-wasm-architecture.md` ("Economic Viability and Total Cost of Ownership"): the cost
  claims.

**Carried over:**

- Cheaper tiers are tried first and the frontier last.
- One interface for every provider.
- Each attempt is accounted for (tokens, cost, latency, route reason) against hard budgets.
- Telemetry events are versioned and redacted.

**Not carried over, and why:**

- **Escalation on answer quality.** wasmai's "not good enough" was a confidence below 0.6 from token
  probabilities, or a quality gate. An OpenAI-compatible endpoint does not reliably return token
  probabilities, and there is no quality gate yet. The router therefore falls back only when a provider
  fails, not when its answer is poor.
- **The complexity classifier.** It would score a prompt to start at a higher tier; it is not built.
- **The CPU and GPU sub-tiers.** Both are `local` here. Several local providers can be listed in the
  order wanted.
- **Training on captured sessions.** wasmai would queue every escalated conversation, with the weaker
  tiers' drafts as rejected examples and the winner as chosen, for scheduled fine-tuning. That is out:
  there is no training on model outputs, and telemetry records no prompts or replies.
- **The cost claims.**
  - §8.3 finds the hybrid CPU/GPU fleet ($1,720 a month) costing more than GPU-only ($1,140) by its own
    numbers. It reaches "≥ 60%" savings only by switching the baseline to an A100-only fleet.
  - The other figures (80% of the teacher's quality, ≤ 20% escalations, the p95 latencies) were targets,
    never measured.
  - None of these are repeated. The cost model below replaces them.

## The routing policy

For each `llm.complete` or `llm.call-tools` call:

1. **Order.** The router tries `local` providers first, then `frontier` ones. Within a tier it keeps the
   configured order. A provider without a `tier` is `frontier`, so it is tried late.
2. **Cost budget.** A paid provider (one with a non-zero price) is tried only while
   `[budgets] llm_cost_micros` has some left. Once that budget is spent, only free providers remain. If
   none is configured, the call is refused with the `limit` code.
3. **Fallback.** On any failure, the router tries the next provider. A failure is a refused connection,
   a timeout, an HTTP error, or a reply without content (without tool calls, for `call-tools`). When
   every provider fails, the call fails with each one's error.
4. **Time.** Each attempt is limited to the provider's `timeout_ms` (60 s by default) and to the run's
   time left.
5. **Tokens.**
   - The guest's `max_tokens`, when it gives one, is cut to the token budget left.
   - The answering call is charged against `llm_tokens`: the endpoint's prompt and completion counts, or,
     when it reports none, an estimate of one token per four bytes of the request and of the reply.
6. **Cost.** The answering call is also charged against `llm_cost_micros`:
   `prompt_tokens × input_price + completion_tokens × output_price`. Prices are per million tokens, so
   this is in millionths of the price unit, rounded up.
7. **What is not charged.** A failed attempt is charged neither tokens nor cost, since the endpoint
   reported nothing. It is still recorded, with its latency.

The policy decides by cost and availability only. It never sends a request to a provider the
configuration does not list. Secret values are redacted from every request, whichever provider it goes
to.

## Configuration

A single `[llm]` endpoint still works: it is one free `frontier` provider named `default`. Several
endpoints go in a list:

```toml
[[llm.providers]]
name = "box"                      # used in telemetry and errors
tier = "local"                    # tried before every frontier provider
api_base = "http://127.0.0.1:11434/v1"
api_key = "ollama"
model = "qwen2.5-coder:7b"
timeout_ms = 30000
input_price = 0.16                # per million tokens (electricity; see below)
output_price = 0.16

[[llm.providers]]
name = "sonnet"
tier = "frontier"
api_base = "https://gateway.example/v1"   # any OpenAI-compatible chat-completions endpoint
api_key = "..."
model = "claude-sonnet-5-5"
input_price = 2.0
output_price = 10.0

[budgets]
llm_cost_micros = 1000000         # 1.00 in the price unit per run (the default)

[telemetry]
path = "hostd-telemetry.jsonl"
```

A frontier provider must offer an OpenAI-compatible chat-completions endpoint, either directly or
through a gateway. hostd speaks no other API.

## The cost model

**Baseline: always-frontier.** Every LLM call goes to one frontier provider. Everything is measured per
call against that baseline.

**Inputs.** The first two are explicit assumptions, and they change the answer:

- a call's prompt and completion tokens, `p` and `c`;
- the local provider's cost per million tokens, `L`;
- the frontier provider's prices, `P_in` and `P_out`;
- the fraction of calls the local provider answers, `s`.

**Formulas:**

- Baseline cost per call: `B = p·P_in + c·P_out`.
- Routed cost per call: `R = (p + c)·L + (1 − s)·B`. This is an upper bound: a failed local attempt is
  assumed to cost as much compute as one that answers.
- Saving: `1 − R/B = s − (p + c)·L / B`.

The saving can never exceed `s`, the share of calls the local provider answers.

### Prices

The frontier prices below are Anthropic's first-party API list prices as of 2026-09-25, in USD per
million tokens:

| Model | ID | Input | Output |
|---|---|---|---|
| Claude Opus 5.5 | `claude-opus-5-5` | 4.00 | 20.00 |
| Claude Sonnet 5.5 | `claude-sonnet-5-5` | 2.00 | 10.00 |
| Claude Haiku 4.5 | `claude-haiku-4-5` | 1.00 | 5.00 |

Gateways and cloud platforms may price differently; use the price the endpoint actually charges.

**Local cost.** This is an assumption, and it counts electricity only: hardware is not amortized.

- A workstation draws 350 W.
- It takes 20 s for a call of 2,000 prompt and 500 completion tokens.
- Electricity costs 0.20 USD per kWh.
- One call: 350 W × 20 s = 1.94 Wh, which is 0.00039 USD.
- So `L` ≈ 0.16 USD per million tokens. That is the `input_price` and `output_price` in the example
  above.

### Worked example

Take a call of 2,000 prompt and 500 completion tokens:

| Frontier | B (per call) | Local cost (p + c)·L | Saving | `s` needed for 60% |
|---|---|---|---|---|
| Claude Sonnet 5.5 | 0.0090 USD | 0.0004 USD | s − 0.043 | 0.64 |
| Claude Haiku 4.5 | 0.0045 USD | 0.0004 USD | s − 0.086 | 0.69 |
| Claude Opus 5.5 | 0.0180 USD | 0.0004 USD | s − 0.022 | 0.62 |

**No saving is claimed.** `s` has not been measured on any workload. And because the router falls back
only on failure, not on poor answers, `s` alone overstates the saving: a local answer that is valid but
wrong costs more rounds, or a failed task. The number that matters is cost per *completed task*, with
and without the local tier, on the same tasks. Until that is measured, the table is a way to read
telemetry, not a forecast.

Each fallback also adds the failed attempt's latency, up to its `timeout_ms`, before the frontier
answers. A short local timeout bounds that delay.

## Open questions

These came over from wasmai unresolved:

- **Where per-request confidence would come from.** Escalating on poor answers, not only failed ones,
  needs a confidence signal, such as token log-probabilities. On wasmai's planned WASI-NN path the
  inference runtime would have to expose them. Over HTTP, only endpoints that honour the OpenAI
  `logprobs` option return them. Until one is chosen and measured, the router falls back on failure
  only.
- **Terms of service and consent for training on outputs.** wasmai planned to train org models on
  frontier answers and on employees' sessions. Many providers' terms restrict using outputs to train
  other models, and capturing people's sessions needs their notice and consent. The question is moot
  while nothing trains on model outputs. It is recorded here in case that is ever reconsidered: check
  each provider's terms and the people involved first.
- **Buying vs building the local tier.** The local tier needs only an OpenAI-compatible
  chat-completions server, and LlamaEdge's API server, llama.cpp's server and Ollama each provide one.
  So wasi-warden builds no inference runtime of its own. An in-process WASI-NN path (wasmai's design,
  with LlamaEdge's runtime) is worth revisiting only for a measured need, such as sandboxing the model
  runtime itself.
