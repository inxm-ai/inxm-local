# storage module

File-based local storage under `.inxm/`: plans, runs, patches, world fixes.

Files:
- `plans.rs` — plan version storage
- `runs.rs` — run state storage
- `patches.rs` — patch storage
- `world_fixes.rs` — world-fix storage (repair verdicts that blame the
  environment, not the plan; authorise same-version resumes)

You own this dir only. Read other `src/` dirs for context, never edit them.
