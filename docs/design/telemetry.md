# Telemetry

With `[telemetry] path` set, hostd appends one JSON line per LLM request attempt: every provider the
[router](router.md) asks, whether it answered or failed. The schema is `TelemetryEvent` in
`hostd/src/telemetry.rs`.

Its origin is wasmai's "2.9 Telemetry hooks" (wasmai `TODO.md` at `67b9158`). That section asked for a
versioned event carrying the provider, model, tokens, cost and cascade step, and no raw secrets. The
training-capture parts (cascade transcripts, preference pairs) are not carried over: telemetry records
no prompts and no replies.

## Schema, version 1

| Field | Type | Meaning |
|---|---|---|
| `version` | integer | The schema version, `1`. |
| `kind` | string | `llm_request` (the only kind so far). |
| `run` | string | The run's id, the same as in the audit log. |
| `step` | integer | The planner round. |
| `provider`, `tier`, `model` | string | Who was asked: the configured name, `local` or `frontier`, and the model. |
| `attempt` | integer | 1 for the first provider asked for this call, 2 for the next, and so on. |
| `ok` | boolean | Whether this provider answered. |
| `error` | string or null | Why it did not, cut to 512 characters. |
| `prompt_tokens`, `completion_tokens` | integer | What the answer used; 0 for a failed attempt. |
| `tokens_estimated` | boolean | The endpoint reported no usage, so the counts are estimates (a token per four bytes). |
| `cost_micros` | integer | What the attempt cost, in millionths of the price unit. |
| `latency_ms` | integer | How long the attempt took. |
| `route_reason` | string | Why this provider was asked: `first choice (local tier)`, `fallback after box failed`, and whether paid providers were skipped. |

An example of a fallback, as two lines (shown wrapped):

```json
{"version":1,"kind":"llm_request","run":"18f2…-4c1","step":0,"provider":"box","tier":"local",
 "model":"qwen2.5-coder:7b","attempt":1,"ok":false,"error":"llm error 503: …","prompt_tokens":0,
 "completion_tokens":0,"tokens_estimated":false,"cost_micros":0,"latency_ms":12,
 "route_reason":"first choice (local tier)"}
{"version":1,"kind":"llm_request","run":"18f2…-4c1","step":0,"provider":"sonnet","tier":"frontier",
 "model":"claude-sonnet-5-5","attempt":2,"ok":true,"error":null,"prompt_tokens":500,
 "completion_tokens":500,"tokens_estimated":false,"cost_micros":6000,"latency_ms":840,
 "route_reason":"fallback after box failed"}
```

## Rules

- **Versioning.** Adding a field keeps the version. Removing, renaming or retyping one bumps it. Readers
  should ignore fields they do not know.
- **Redaction.** Every line passes the configured secrets' redaction before it is written, like the audit
  log, so an endpoint that echoes a key in its error writes `{{secret:NAME}}`.
- **Not recorded.** Telemetry never holds prompts, replies or tool calls. The audit log records the
  agent's LLM calls by digest only.
