# Harness Engineering — Working with LLMs

Context engineering, mechanical constraints, agent-first mindset, and GitHub workflow. Read this for any task involving AI agent setup, CLAUDE.md files, CI/linting rules, or repository workflow.

---

## What a Harness Is

A harness is the complete set of constraints, feedback loops, documentation structures, linting rules, and context artifacts that allow AI coding agents to operate reliably. It is not a prompt. It is not a CLAUDE.md file. It is the engineered environment in which AI-assisted development happens.

> "Building software still demands discipline, but the discipline shows up more in the scaffolding rather than the code." — OpenAI Harness Engineering team

The goal: make it hard for an LLM to do the wrong thing, and easy for it to do the right thing — automatically, everywhere, at once.

---

## The Three Pillars

### 1. Context Engineering

Agents can only reason about what they can see. Knowledge in Slack threads, Google Docs, or people's heads is operationally invisible. Any knowledge you expect to influence agent behaviour must be materialised in the repository — versioned, reviewable, and testable.

**Layered context with progressive disclosure:**
- **Entry point** (`CLAUDE.md` / `AGENTS.md` at repo root): short, always-loaded. Covers build steps, test commands, key conventions, architectural constraints, and common pitfalls. This is a living feedback loop — updated every time an agent makes a mistake that could be prevented next time.
- **Domain-level context** (per subdirectory or domain): deeper conventions specific to that area, loaded on demand.
- **Architecture documentation**: a top-level map of domains, package layering, dependency rules, and a quality/gap tracker. Versioned and co-located in the repo.
- **Execution plans**: for complex work, checked-in plans with progress and decision logs. Active plans, completed plans, and known technical debt all live in the repo — not in chat history.

### 2. Mechanical Architectural Constraints

Documentation that is not mechanically enforced will drift. If a constraint matters enough to document, it matters enough to enforce with a linter or structural test.

**Rules to encode mechanically in CI:**
- Dependency direction (in this repo: `plan → validator → executor → repair`, with
  `compiler` as the only LLM entry point and `storage`/`tools` as the only I/O edges —
  lower layers never import `app`, and `validator`/`executor` never import `compiler`).
- Module boundary enforcement — no reaching into internals across the `Agents.md`
  module-ownership boundaries.
- Naming conventions for error enums, events, and schema types.
- File size limits (a file that's too large to fit in context is too large to exist).
- Structured logging format — statically enforced, not aspirational (see `observability.md`).
- Data shape validation at every boundary — `serde` derives on every type that crosses
  a module or process boundary, not optional.

**Write linter error messages as remediation instructions.** When an agent violates a constraint, the error message should tell it exactly how to fix it. The tooling teaches the agent while it works.

### 3. Entropy Management ("Garbage Collection")

Agent-generated codebases accumulate drift: stale docs, inconsistent naming, violated constraints. Fight entropy continuously:
- CI jobs validate that the knowledge base is up to date, cross-linked, and structured correctly.
- Periodic agent runs find documentation inconsistencies and constraint violations.
- Technical debt is tracked as versioned, co-located artifacts — not in a backlog tool that agents can't see.
- When an agent struggles, treat it as an environment design problem: what context, tool, or guardrail is missing? Fix the harness, not just the prompt.

---

## Agent-First Engineering Mindset

The scarce resource in agent-first development is **human time and attention**, not computation. This changes every engineering tradeoff:

- **Waiting is expensive; corrections are cheap.** Invest in fast feedback loops so corrections happen in seconds, not hours.
- **Constraints enable speed, not slow it down.** Architectural rigidity that would feel pedantic in a human-first workflow becomes a multiplier with agents — once encoded, it applies everywhere at once.
- **The bottleneck is never the model.** When an agent produces bad output, the fix is almost always in the environment: better context, tighter constraints, clearer feedback.
- **Design for legibility, not cleverness.** The codebase is the agent's context. Every naming decision, every abstraction, every comment is either signal or noise.

---

## What This Means in Practice

- **Every context file is a living artifact.** `CLAUDE.md` files at root and domain level are updated as part of the PR when agent failures reveal missing context. Never let them rot.
- **Enforce constraints mechanically.** Every architectural rule that can be expressed as a lint rule or structural test should be. Aspirational rules that aren't enforced don't exist for agents.
- **Plans are first-class artifacts.** For any non-trivial task, write a brief plan file in the repo before generating code. Include decision rationale. Check it in alongside the code.
- **Make the application observable to the agent.** Logs and traces are part of the agent's feedback loop — not just for humans. This is why the `tracing`/telemetry practices in `observability.md` are foundational, not optional.
- **When I struggle with a task, tell me what was missing.** If I produce something wrong, the right response is to identify what context, constraint, or feedback was absent and add it to the harness. Not just to correct the output.

---

## GitHub Workflow

### Non-negotiables
- **Clear, imperative commit messages** — short subject line, body only when it adds
  real information (see the personal commit-message instructions this agent already
  follows). This repo does not enforce a Conventional Commits prefix scheme — don't
  invent one unasked.
- **CI must pass before merge** — no exceptions, no force pushes to main.
- **Small PRs** — one concern per PR, respecting the single-module ownership model in
  `Agents.md`. If the description needs more than a short paragraph, it's probably too large.
- **PR descriptions explain the why** — not just what changed, but why this change, why now, what alternatives were considered.
- **Trunk-based development** preferred — short-lived branches, merge frequently.

### LLM-generated PRs
- LLMs always create PRs — never push directly to main.
- LLM-generated code is held to **exactly the same review bar as human code** — no exceptions.
- **Clean atomic commits** — no AI slop history ("fix", "update", "try again" chains). Squash or rebase before merging.
- PR descriptions for LLM-generated changes must still explain the *why* — the prompt alone is not sufficient context.

### AI context files (this repo already does this)
- `Agents.md` at the root plus a per-module `agents.md` (`src/app/agents.md`,
  `src/compiler/agents.md`, etc.) is exactly the layered-context pattern described
  above, already implemented — root file assigns ownership and points at the short
  domain-level files; keep both updated in the same PR that changes the module's shape.
- `CLAUDE.md` at the root defers to `Agents.md` — don't duplicate rules across the two;
  add new repo-wide rules to `Agents.md`.
- Write context files to be **LLM-agnostic** — avoid tool-specific syntax where possible so they work across Claude, Copilot, Cursor, and others.
- For **architectural decisions**: always explain tradeoffs before generating code.
