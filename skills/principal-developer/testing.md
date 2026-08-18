# Testing

Testing pyramid, mocks philosophy, and structural patterns for writing tests that stay green through refactoring.

---

## Testing Pyramid

```
         /\
        /E2E\          ← few, slow, high-confidence
       /------\
      / Integ  \       ← moderate, test real component boundaries
     /----------\
    /    Unit    \     ← many, fast, test business logic
   /--------------\
```

In this repo:
- **Unit tests**: `#[cfg(test)] mod tests` at the bottom of the file they test — see
  `src/telemetry/mod.rs`, `src/telemetry/schema.rs`. Fast, no I/O, no async runtime.
- **Integration tests**: `tests/integration_*.rs` at the crate root — exercise real
  module boundaries (plan load → validate → execute) using fixtures under
  `tests/fixtures/`. See `tests/integration_plan.rs`, `tests/integration_executor.rs`.
- **E2E-ish tests**: `tests/live_spec_planning.rs` — the closest thing to an end-to-end
  test here, driving the compiler against a real backend. Keep these few and gated
  behind explicit opt-in (env var / feature) if they need network or an API key.

**Core rules:**
- Tests are written **from a business/behaviour perspective**, not an implementation perspective.
- Test *what* the code does, not *how* it does it internally.
- Avoid testing implementation details (private fields, internal state) — this makes tests brittle.
- Name tests in plain language: `fn cycle_plan_fails_validation()`, `fn env_var_disables_but_never_enables()`.
- Prefer TDD: write the failing test first, then implement.
- Avoid testing the same thing at multiple pyramid levels — no test duplication.

---

## Testing Without Mocks (James Shore)

**Avoid mocks by default.** Mocks lock in implementation details, make refactoring painful, and produce tests that verify nothing real. The preferred alternatives are:

### 1. State-Based Tests — always

Assert on *output and observable state*, never on *which methods were called*:

```rust
// ✅ State-based (tests what actually happened) — real pattern from
// tests/integration_plan.rs
let errors = validator::validate(&plan, &catalog);
assert!(
    errors.iter().any(|e| e.kind == ValidationErrorKind::CyclicDependency),
    "expected CyclicDependency error, got: {errors:?}"
);
```

### 2. Logic Sandwich — separate pure logic from I/O

Keep business logic in pure functions with no I/O dependencies. Push all I/O (filesystem,
subprocess, HTTP, MCP, clock) to the edges. This crate already draws this line at the
module level — `validator/` and `repair/classifier.rs` are explicitly "no AI, no I/O",
`plan/` normalization is pure. Test that pure logic with plain in-memory `Plan`/`ToolCatalog`
values — no filesystem, no subprocess needed:

```rust
// ✅ Pure logic — trivially testable, no infrastructure needed
let errors = validator::validate(&plan, &catalog);
```

### 3. Infrastructure Wrappers — own your boundaries

Never call a 3rd-party crate or perform I/O directly from business logic. Wrap it in a
thin owned function/module. This is already the shape of `tools/` — `execute_tool`
dispatches by `ToolConfig` variant to one adapter module per tool type
(`adapters::subprocess`, `adapters::http`, `adapters::mcp`, ...), and `storage/` has
one module per persisted entity (`plans.rs`, `runs.rs`, `patches.rs`) wrapping the
filesystem. This gives a seam to substitute behaviour in tests and a single place to
change when a 3rd-party API changes:

```rust
// ❌ Direct 3rd-party call from business logic — couples the executor to reqwest
async fn run_step(step: &PlanStep) -> Result<String, ExecutorError> {
    let resp = reqwest::get(&step.url).await?;
    Ok(resp.text().await?)
}

// ✅ Owned adapter module — substitutable, single seam (the tools/adapters/ shape)
pub async fn run(config: &HttpToolConfig, args: &IndexMap<String, Value>) -> Result<ToolOutput, ToolError> {
    let resp = reqwest::get(&config.url).await.map_err(|e| ToolError::Execution {
        tool: config.url.clone(),
        message: e.to_string(),
    })?;
    Ok(ToolOutput::from_text(resp.text().await?))
}
```

For tests, prefer exercising a real adapter against a real temp file/subprocess (via
`tempfile`, already a dev-dependency) over a hand-rolled mock — e.g. a subprocess
adapter run against a real temp script is more honest than a mock that just records
calls.

### When mocks ARE acceptable

Mocks are acceptable when wrapping a 3rd-party crate in a full adapter + fake would be
excessive relative to the value of the test — e.g. a thin, well-understood SDK call used
in exactly one place. In that case, mock only at the adapter boundary, never deep inside
`executor`/`compiler`/`repair` logic. Always note in the tradeoffs section why a mock was
chosen over a real/fake implementation.

### What to never do
- Never mock this crate's own domain types (`Plan`, `ToolCatalog`, `RunState`) — use the real thing built with plain constructors, or a fixture file under `tests/fixtures/`.
- Never write tests that only verify mocks call other mocks — that tests nothing real.
- Never assert on *how* something was done internally — only on *what* the observable outcome was.

---

## Default Expectation

When writing code, always include at minimum the unit test(s) for the core logic unless the user explicitly says not to.

---

## egui UI Testing (`src/app`)

egui is immediate-mode — there's no persistent DOM to snapshot. In this repo:
- Keep business logic (chat command parsing in `commands.rs`, mutation logic in
  `mutation.rs`) out of the `eframe::App::update` draw loop so it can be unit tested
  directly without a `Context`.
- `debug_shot.rs` provides a dev-only headless render-and-screenshot hook
  (`INXM_SCREENSHOT=/path/out.png`) for verifying visual changes without a human at
  the keyboard — use it for visual regressions, not as a substitute for unit-testing
  the underlying logic.
- Assert on the data a view renders (the `Vec<ChatMessage>`, the computed badge state),
  not on egui's internal widget tree.

---

## Fixtures

Prefer small, purpose-built JSON/YAML fixtures under `tests/fixtures/` (plans, tool
catalogs) over constructing large literal structs inline — see
`tests/fixtures/plans/valid_plan.json` and `tests/fixtures/tools/catalog.yaml`. This
keeps test intent readable and lets a fixture be reused across several tests.
