# Observability

`tracing` instrumentation, structured wide-ish events, and the opt-in telemetry pipeline.
Read this for any task involving logging, the `telemetry/` module, or diagnosing a run
locally.

---

## Context: a local-first desktop app, not a hosted service

`inxm-local` runs on a user's machine. There is no backend fleet, no on-call rotation,
no SLA to page against, and (by design, see `docs/telemetry.md`) no default visibility
into what any individual user is doing. Generic SRE playbooks built around SLOs, error
budgets, and burn-rate alerting **do not apply here** — don't introduce them. What does
apply, and is already built:

1. **Structured `tracing` events**, visible locally via `tracing-subscriber` (console,
   and a compile log file — see `app/console.rs::open_log_file`) and inspectable by
   the user or by an agent debugging a report.
2. **Opt-in, privacy-first aggregate telemetry** (`src/telemetry/`), sent only when the
   user explicitly says yes, with an exhaustively-documented schema.

Both exist so a human *or an LLM agent* can answer "what happened, and why" — locally
first, and only in aggregate/anonymous form if the user opted in.

---

## Structured `tracing` Events

Instrument as you write the feature, not after — the engineer (or agent) who writes the
code writes its instrumentation.

This crate already has a consistent field vocabulary for significant operations (see
`app/engine.rs`'s scheduler and command-dispatch logging). Follow it for any new
operation worth being able to diagnose after the fact:

```rust
tracing::info!(
    command_kind = trace.command_kind,
    session_id = ?env.session_id,
    run_id = ?trace.run_id,
    plan_id = ?trace.plan_id,
    app_version = env!("CARGO_PKG_VERSION"),
    triggered_by,       // "application" | "scheduler" | "user"
    duration_ms,
    outcome = "success", // or "failure"
    "scheduled occurrence completed"
);
```

**Conventions to reuse, not reinvent:**
- `operation` — a short, stable name for what ran (`"scheduler_lock.acquire"`, `"schedule.claim"`).
- `outcome` — `"success"` | `"failure"`, always present on operations that can fail.
- `duration_ms` — measured with `Instant::now()` at the start, `.elapsed()` at the end.
- `triggered_by` — causal context: what caused this to run (user action, scheduler, application startup).
- `app_version` — `env!("CARGO_PKG_VERSION")`, for correlating reports against a release.
- Domain IDs as fields, not string-interpolated into the message: `run_id`, `plan_id`,
  `schedule_id`, `step_id` — this is what makes an event greppable/filterable instead
  of just readable.
- On failure, prefer `tracing::error!`/`warn!` with the `error` field via `%error`
  (`Display`) so the message stays structured: `tracing::warn!(%error, dir = %dir.display(), "...")`.

**Ask before shipping a feature**: *if this fails silently in a user's install, how would
either of us find out?* If the answer is "we wouldn't", add the event.

**Never log secrets or user content** — tokens, prompts, plan contents, and file paths
with personal information are not tracing-event material. IDs and reference keys only.
This is the same invariant the telemetry schema documents explicitly (see below); hold
`tracing` calls to it too, even though they never leave the machine.

---

## Opt-in Telemetry (`src/telemetry/`)

This is the one place data can leave the machine, and it is deliberately narrow:

- **Off unless explicitly on**: the persisted setting is `Option<bool>`; only
  `Some(true)` counts. Two runtime kill switches (`INXM_TELEMETRY=off` env var,
  `--no-telemetry` CLI flag) can only ever *disable*, never enable — see
  `telemetry::resolve` for the exact precedence rules, and its unit tests for the
  behaviour that must never regress.
- **Exhaustive schema**: every field that can ever leave the machine is declared in
  `telemetry/schema.rs`'s `Event` enum. Adding a field there is a documentation change
  too — `docs/telemetry.md` promises the list is complete.
- **No stable identifiers, ever**: no machine ID, no install ID, no username, no
  hostname, no IP (the receiving Cloudflare Worker discards it), no timestamps from
  the client (the sink assigns a coarse server-side one), no free-form strings from
  user data (plan names, prompts, file paths).
- **Fire-and-forget**: sending is entirely decoupled from normal operation — a failed
  or slow send can never affect the app (`telemetry::sender::send_detached`).

When a task touches `telemetry/`, treat the schema and the privacy rules above as hard
constraints — see `SKILL.md`'s hard rules. Any new field must be justified against
"does this help understand aggregate product usage without identifying anyone", and
must be added to `docs/telemetry.md` and to the Cloudflare Worker's allow-list
(`telemetry-worker/worker.js`) in the same change.

---

## Core Analysis Loop — Debugging from First Principles

When something goes wrong (a bug report, a failed run, a crash), never grep-and-pray.
Use the core analysis loop:

1. **Form a hypothesis** — what do you believe is happening and why?
2. **Find the data** — the user's compile log file (`app/console.rs`), `RUST_LOG`-driven
   `tracing` output, the persisted run/patch/world-fix state under `.inxm/` (`storage/`),
   or a `tests/fixtures/` reproduction.
3. **Refine** — if refuted, update your hypothesis and repeat. If confirmed, dig deeper.
4. **Resolve** — act only when you have evidence-backed confidence in the cause.

When helping debug an issue, apply this loop explicitly: state the hypothesis, identify
the log field/run state/fixture that would test it, reason from evidence — don't guess
at a fix.

---

## Per-topic Implementation

- **Rust**: `tracing` crate, `tracing-subscriber` with `EnvFilter::from_default_env()`
  (respects `RUST_LOG`). No OTel SDK, no collector, no exporter in this codebase —
  don't introduce one without the user asking; it doesn't match a single-user desktop
  app with no backend to receive traces.
- **Plain JS** (`telemetry-worker/worker.js`): no logging of request bodies or headers;
  the worker's job is to validate against the allow-list and forward aggregate counters
  to Workers Analytics Engine, nothing else.
- **Span/event naming**: `<module>.<entity>.<action>` for the `operation` field —
  e.g. `scheduler_lock.acquire`, `schedule.claim`, `storage.event`.
- **Never log sensitive data** (tokens, prompts, plan/file contents, paths that embed
  personal information) — log IDs and reference keys only, in both `tracing` events and
  any future telemetry field.
