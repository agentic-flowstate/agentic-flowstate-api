# Agentic Flowstate Observability Registry v0.1

Effective date: 2026-07-06

This directory is the versioned source of truth for the first Agentic
Flowstate observability contract registry. It is grounded in artifacts
`A-5E4F35CD` and `A-FBE61DC4` for ticket `T-A133CC95`.

The machine-readable registry lives in `registry.toml` and is loaded by the
API at startup. Parse or validation failures are process-fatal so unsafe
telemetry policy cannot silently drift from the checked-in contract.

## Contracts

| Contract ID | Name | Status | Scope |
|---|---|---|---|
| C-OBS-001 | iOS Responsiveness | draft | Client hangs, frames, MetricKit, lifecycle, and interaction telemetry |
| C-OBS-002 | Conversation Fanout and Scale | draft | SSE, fanout, hierarchy, payload, queue, and large conversation guardrails |
| C-OBS-003 | Backend Request Economics | draft | HTTP request logs, RED metrics, request IDs, route templates, and payload/query guardrails |
| C-OBS-004 | Agent Run Lifecycle | draft | Agent runtime, queue, child admission, wake, completion, and failure telemetry |

## Guardrails

The API enforces the registry in three places:

- Client telemetry ingest rejects unknown event types, forbidden detail field
  names, oversized detail payloads, raw body/prompt/message content markers,
  and secret-bearing strings.
- Metric emitters assert that each metric is registered and only uses its
  approved low-cardinality label keys.
- Request logs use normalized paths and never persist session cookie values.

Telemetry changes must update this registry and the Rust validator tests in
the same change.
