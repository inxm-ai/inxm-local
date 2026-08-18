# executor module

Deterministic plan executor. No AI here. Runs steps in topological order,
persists run state before and after each step.

Files:
- `dag.rs` — topological step ordering
- `step_runners/` — one runner per step type: `tool_call`, `prompt_call`,
  `code_call`, `condition`, `fan` (FAN_OUT), `human`

Note: a FAN_OUT step owns its `spawn_steps`; the main loop skips them.

You own this dir only. Read other `src/` dirs for context, never edit them.
