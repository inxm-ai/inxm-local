# Engineering Principles

Fundamentals that apply to every piece of code in `inxm-local`, regardless of module.

---

## Error Handling — Railway Oriented Programming

Model errors as data. Never panic for expected failure paths. Think in two tracks —
happy path and error path — and keep them explicit in the code structure.

### Rust — the default everywhere in `src/`

- Functions that can fail return `Result<T, E>`, propagated with `?`.
- Domain errors are `thiserror` enums, one per module family (see `src/error.rs`:
  `PlanError`, `ToolError`, `CompilerError`, `ExecutorError`, `RepairError`, `StorageError`).
  Each variant carries the context needed to act on it — not just a string:

```rust
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {name}")]
    NotFound { name: String },

    #[error("tool timed out after {secs}s: {tool}{captured_output}")]
    Timeout {
        tool: String,
        secs: u64,
        captured_output: String,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
```

- Use `#[from]` to make `?` work across module boundaries (e.g. `ExecutorError::Tool(#[from] ToolError)`)
  instead of hand-rolled `.map_err()` at every call site.
- Reach for `anyhow` only at true application-aggregation points (top-level `main.rs`
  branches) where the caller genuinely doesn't need to match on a specific variant.
- `.unwrap()` / `.expect()` are for tests, examples, and invariants that truly cannot
  fail (`expect("why this can't happen")` — never a bare `unwrap()` in library code).
- Programmer errors / invariant violations → panic is acceptable. Expected failures
  (missing file, bad tool config, LLM returned malformed JSON, network timeout) →
  always modelled as `Result`.

### Plain JavaScript (`telemetry-worker/worker.js` only)

The one non-Rust corner of this repo. Same defensive style already used there:

```javascript
// ✅ Right — validate, return early, never throw across the boundary
function parseEvent(body) {
  try {
    const event = JSON.parse(body);
    if (!ALLOWED.os.includes(event.os)) return null;
    return event;
  } catch {
    return null;
  }
}
```

- Be defensive: guard inputs, fail gracefully, return `null`/early on failure.
- No custom `Error` subclasses, no `Result`-like objects — this file stays small and boring.

### Universal rules
- Never swallow errors silently (an `Err` matched and discarded without at least a
  `tracing::warn!`/`error!`, or a genuinely-intentional comment explaining why).
- Log at the boundary where you *handle* the error, not where you create it.
- When adding a new failure mode, extend the existing error enum for that module
  rather than inventing a new ad hoc error type.

---

## Functional-First Style

Default to functions and data transformation pipelines. Reach for a `struct` with methods
only when:
- You need to encapsulate mutable state with a clear lifecycle (a `ShotState`, an
  engine handle, a lock guard).
- You're modelling a well-known pattern that genuinely simplifies the design (a state
  machine, a builder).
- The framework idiom demands it (`eframe::App`, `TokenSource` trait impls, etc.).

Prefer:
- Pure functions with no side effects — especially in `validator/`, `plan/`, and
  `repair/classifier.rs`, which are explicitly "no AI, no I/O" by design.
- Small, single-purpose functions with descriptive names.
- Iterator adapters (`map`, `filter`, `fold`) over manual loops where clarity is equal
  or better; a plain `for` loop is still the right call when it reads more clearly
  (e.g. early-return control flow, or accumulating into multiple collections).

---

## Comments — "Why", Never "What"

```rust
// ❌ Wrong: explains what the code does (already obvious)
// Increment retry count
retries += 1;

// ✅ Right: explains why a non-obvious decision was made
// TIME-based, not frame-based: continuous repaints make frames essentially
// free, so a frame count would fire before the engine bootstrap finishes.
const CAPTURE_AFTER_SECS: f64 = 3.0;
```

Doc comments (`///`) on all public API surfaces — exported functions, public struct
fields, module entry points (`//!` at the top of the file). Explain param/return
rationale when non-obvious, matching the style already in `src/error.rs` and
`src/telemetry/schema.rs`.

No commented-out code in final output. Use `TODO:`/`FIXME:` with a brief explanation
if something is intentionally incomplete.

---

## Code Structure

This crate is already organised by domain, not by technical layer — keep new code
consistent with that:

```
src/
  compiler/    ← chat intent → typed Plan (the only place an LLM is invoked)
  validator/   ← deterministic plan validation, no AI, no mutation
  executor/    ← deterministic DAG execution, no AI
  repair/      ← diagnose + patch/world-fix a failed run
  plan/        ← Plan IR: types, load/save, normalization
  storage/     ← file-based persistence under `.inxm/`
  tools/       ← tool catalog + per-type adapters (subprocess, HTTP, MCP)
  app/         ← egui desktop UI, chat-first surface over the above
  telemetry/   ← opt-in usage telemetry schema + sender
```

Each module owns its own `agents.md` describing its files and boundaries — read it
before editing. Keep nesting shallow inside a module (e.g. `tools/adapters/`,
`app/views/`) — more than one extra level deep is a signal to reconsider the split.

---

## Dependencies

Pragmatic — use well-maintained crates freely, don't reinvent what `Cargo.toml`
already provides. Check `Cargo.toml` before adding a new dependency; this crate
already has canonical choices for most concerns:

| Concern | Crate |
|---|---|
| Error types / aggregation | `thiserror` + `anyhow` |
| Async runtime | `tokio` |
| Serialisation | `serde` + `serde_json` (+ `serde_yaml` for tool catalogs) |
| Outgoing HTTP | `reqwest` |
| MCP client | `rmcp` |
| Local HTTP server | `axum` |
| Tracing | `tracing` + `tracing-subscriber` |
| DAG scheduling | `petgraph` |
| IDs | `uuid` |
| Timestamps | `chrono` |

When introducing a *new* dependency beyond this list, briefly note why — especially
if an existing crate already covers the need.

---

## YAGNI — Full Detail

Never build a capability because you *presume* you'll need it in the future. Build it when it is actually needed.

Every presumptive feature carries three costs:
- **Cost of build**: effort on something that may never be used. Analysis at Microsoft found roughly ⅔ of carefully-planned features don't improve the metrics they were designed to improve.
- **Cost of delay**: while building the presumptive feature, something with real current value wasn't built.
- **Cost of carry**: the extra code adds complexity that slows down everything else indefinitely.

In practice:
- Before building something speculative, imagine the refactoring needed to add it later. It is almost always cheaper than the carry cost of building it now.
- Something cheap that meaningfully reduces *future* cost with minimal complexity today (e.g. a named constant, a clean interface boundary) is acceptable.
- Any extensibility point that is never used isn't just wasted effort — it actively gets in the way.
- When in doubt: don't build it.

YAGNI applies equally to LLM-generated code. The ease of generating large volumes of code with AI makes speculative building *more* tempting. Hold the same standard regardless of who (or what) writes the code.

---

## Thin Vertical Slices

Deliver every feature as the thinnest possible slice through the full stack that produces real, observable value — from the egui UI (if applicable) down through compiler/executor/validator logic to storage.

**Why slices, not layers:**
Building the entire storage layer first, then the executor, then the UI means nothing is demonstrably working until the end. A thin vertical slice is working — testable, runnable, shippable — from day one.

**What thin means:**
- Implement exactly the happy path needed to satisfy the current requirement. No edge cases that aren't yet required, no generalisation that isn't yet needed (YAGNI).
- A slice should be completable in a single PR. If it isn't, it's not thin enough — split it.
- Incomplete slices live behind a feature flag or an env-gated dev-only path (see `debug_shot.rs`'s `INXM_SCREENSHOT` pattern), not in a long-lived branch.

**In practice:**
- When given a feature to build, identify the thinnest path from input to output that delivers observable value.
- Propose that slice explicitly before writing code: "Here's the slice I'm implementing — [description]. This excludes [X, Y] which would be follow-up slices."
- Each slice gets its own tests, its own `tracing` instrumentation, and leaves the codebase in a runnable state.
- Resist the pull toward "let me just also handle..." — that's the next slice.

This works directly with evolutionary architecture: each slice is a validated hypothesis. The architecture grows from real slices, not anticipated ones.

---

## Refactoring (Martin Fowler)

### Core Practices

- **Small, safe steps**: each refactor must leave `cargo test --all-targets --all-features` green. Never refactor and change behaviour in the same step — if the tests break, the step was too large.
- **Separate refactoring from feature work**: refactoring commits and feature commits must not be mixed. A commit either changes behaviour (feature/fix) or improves structure (refactor) — never both.
- **Refactor freely around the feature**: fine to refactor before *or* after adding a feature — the key is keeping the two concerns in separate commits.
- **Rule of Three for abstractions**: do not propose extracting an abstraction until a pattern appears in **3 or more distinct locations**. One = leave it. Two = note it. Three = propose it. Premature abstraction is worse than duplication.

### Code Smells — Actively Flag These

When working on any code task, scan for these smells. **Do not silently fix them** — flag in the tradeoffs section and offer to address separately:

**Primitive Obsession**
Raw primitives (`String`, `usize`, `bool`) used where a domain type or newtype should exist:
```rust
// ❌ Smell
fn dispatch_tool(tool_name: String, timeout_secs: u64, plan_id: String) { ... }

// ✅ Better
fn dispatch_tool(tool: ToolName, timeout: Duration, plan_id: PlanId) { ... }
```

**Feature Envy**
A function that seems more interested in another module's data than its own. If
`executor` logic reaches deeply into `plan` internals it doesn't own, that logic
probably belongs closer to `plan`.

**Data Clumps**
The same group of 3+ fields or parameters appearing together repeatedly is a type waiting to be born:
```rust
// ❌ Smell: these three always travel together
fn run_step(step_id: String, run_id: String, plan_version: u32) { ... }
fn log_step(step_id: String, run_id: String, plan_version: u32) { ... }

// ✅ Better
struct StepContext { step_id: String, run_id: String, plan_version: u32 }
```

**Divergent Change / Shotgun Surgery**
- *Divergent Change*: one module changes for many different reasons (low cohesion).
- *Shotgun Surgery*: one change requires edits scattered across many modules (high coupling) — a red flag given the strict one-module-per-agent ownership model in `Agents.md`.
Both flagged in tradeoffs with a suggested structural remedy — not silently fixed.

### What Not To Do
- Do not extract an abstraction speculatively. Inline duplication is preferable to the wrong abstraction.
- Do not refactor code that has no tests — stabilise with tests first, then refactor.
- Do not rename things "while you're in there" during a feature commit — schedule as a separate step.
