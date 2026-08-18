# validator module

Deterministic plan validation only. Never modifies the plan. No AI here.

Files:
- `graph.rs` — structural/graph checks
- `placeholders.rs` — `${...}` placeholder checks
- `tool_binding.rs` — checks steps bind to valid catalog tools

You own this dir only. Read other `src/` dirs for context, never edit them.
