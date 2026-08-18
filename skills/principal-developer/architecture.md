# Architecture

Strategic and structural principles. Read this for any task involving system design, new services, module boundaries, or architectural decisions.

---

## Design Stamina Hypothesis (Fowler)

Good design is not a trade-off against speed — it *is* speed, past the payoff line.

A codebase with neglected design produces features faster initially. But as design degrades, productivity falls. At some point — often within weeks, not months — a well-designed codebase permanently overtakes the no-design codebase in cumulative delivered functionality, and never looks back.

**Implications for every design decision:**
- "We need to move fast so we're skipping design" is almost always false economy once you're past the payoff line.
- Technical debt is only worth taking deliberately and temporarily *below* the design payoff line — i.e. for the very earliest prototypes. Once you're shipping to real users, you are almost certainly above the line.
- Neglecting design above the payoff line doesn't make you ship faster — it makes you ship later. The interest payments exceed the principal.
- When suggesting shortcuts: always flag explicitly whether the tradeoff is below or above the design payoff line.

This hypothesis underpins why all the principles in this skill exist — not as aesthetics or idealism, but as productivity multipliers.

---

## Software Engineering Principles (Dave Farley)

These principles apply at every level — function, module, service, system.

### Manage Complexity

Complexity is the root cause of most software failures. Fight it actively:

- **High cohesion, low coupling**: each module/function does one thing well and knows as little as possible about others. If a change in one place requires changes in many others, the coupling is too high — flag in tradeoffs.
- **Shallow module hierarchies**: avoid deep inheritance chains or deep abstraction layers. If you need more than 2–3 levels of abstraction to explain what something does, simplify.
- **Separate domain model from infrastructure**: business logic must never depend on databases, HTTP, message queues, or any I/O mechanism. Infrastructure depends on the domain, not the other way around.

### Ports & Adapters (Hexagonal Architecture)

Structure code so the core domain is isolated from the outside world:

```
         ┌─────────────────────────────┐
         │        Domain / Core        │
         │  (pure logic, no I/O deps)  │
         └──────────┬──────────────────┘
                    │ Ports (interfaces)
         ┌──────────▼──────────────────┐
         │         Adapters            │
         │  (DB, HTTP, queue, CLI...)  │
         └─────────────────────────────┘
```

- Core domain defines **ports** (interfaces/abstract types) — it never imports adapters.
- **Adapters** implement ports and live at the edges of the system.
- This makes the domain independently testable with zero infrastructure.
- Apply even at small scale — a single function that accepts a callback/strategy instead of calling a DB directly is hexagonal thinking.

**In `inxm-local`**, this split is already the module boundary, not just a pattern to
reach for:
- `validator/` and `executor/`'s core loop are pure domain — no LLM calls, no I/O.
- `compiler/backend.rs` is the port to the one place an LLM is invoked; swapping the
  configured backend (Claude, OpenAI, a custom CLI) never touches `executor/` or `validator/`.
- `tools/` dispatches by `ToolConfig` variant to an adapter module per tool type
  (subprocess, HTTP, MCP) — the executor calls `execute_tool`, never a specific adapter.
- `storage/` is the only place that touches the filesystem (`.inxm/`); every other
  module receives already-loaded `Plan`/`RunState` values.

When adding a new capability, ask which side of this line it belongs on before writing
any code — a new tool type is a new adapter under `tools/adapters/`, not a new branch
in `executor/`.

### Fast Feedback Loops

- Design for testability from the start — if something is hard to test, treat that as a design signal, not a testing problem.
- Prefer designs that can be verified locally without spinning up infrastructure.
- Keep functions and modules small enough that failures are easy to pinpoint.
- Consider CI/CD implications: avoid designs that require long build/deploy cycles to validate.

### Small Steps & Continuous Deployability

- Write code in small, complete, integrable increments. Every commit leaves the codebase in a deployable state.
- **Feature flags over long-lived branches**: when a feature isn't ready to be exposed, hide it behind a flag rather than maintaining a divergent branch. Trunk-based development is the goal.
- Avoid "big bang" refactors — prefer incremental strangler-fig style improvements.
- When scaffolding new modules or services, always consider: *could this be deployed independently right now?* If not, why not?

### Optimise for Learning

- Treat every design decision as a hypothesis.
- Prefer reversible decisions over irreversible ones — keep options open.
- When multiple approaches are viable, choose the one that gives faster feedback on whether it's right.
- In the tradeoffs section, explicitly note where a decision is a deliberate bet that may need revisiting.

---

## Evolutionary Architecture

Architecture is never finished. Treat every structural decision as a hypothesis to be validated, not a contract to be honoured indefinitely.

- **No big upfront design**: start with the simplest structure that could work. Let architecture emerge from real usage and real constraints — don't speculate about future needs.
- **Prefer reversible decisions**: when two approaches are equally valid, pick the one that is easier to undo. Explicitly flag irreversible decisions in the tradeoffs section.
- **Architecture is always a work in progress**: when suggesting structure, frame it as "current best fit" not "the right answer". Note what signals would prompt revisiting it.
- **Fitness functions**: every significant architectural constraint should be verifiable. When making or reviewing an architectural decision, ask: *how would we know if this constraint was violated?* Suggest a fitness function (a test, a lint rule, a metric threshold, a CI check) for any architectural invariant that matters.

---

## AI-Replaceable Architecture

Code should be structured so that any module can be confidently regenerated or replaced by an AI with minimal context. This is an active design constraint, not a theoretical goal.

- **Explicit contracts at every boundary**: every module exposes its public API through a barrel `index.js` / `index.ts`. Nothing outside a module imports from internal paths. If you can't describe a module's contract in 3 sentences, it's too big or too vague.
- **Self-contained modules**: each module must be independently testable with no knowledge of its consumers. No implicit globals, no ambient dependencies, no reaching across module boundaries.
- **Thin, explicit interfaces over deep integrations**: prefer passing dependencies explicitly (ports & adapters) over modules that wire themselves together internally.
- **Prefer boring over clever**: the best code for AI-assisted replacement is code that is obvious. Clever abstractions, metaprogramming, and dynamic dispatch all raise the context requirement for safe regeneration.
- **Small, complete units**: a module should do one coherent thing. If regenerating it requires understanding three other modules first, split or simplify.
- **Flag replacement risk in tradeoffs**: when a design choice would make a module harder to safely regenerate (e.g. tight coupling, implicit shared state, non-obvious ordering dependencies), note it explicitly.
