# storage module

File-based local storage under `.inxm/`: plans, runs, patches, world fixes.

Files:
- `plans.rs` — plan version storage
- `runs.rs` — run state storage and per-run OS lifecycle locks (`runs/.locks/`)
- `patches.rs` — patch storage
- `llm_usage.rs` — append-only per-plan Claude API ledger (`llm-usage/`), inline message bodies, and token totals
- `world_fixes.rs` — world-fix storage (repair verdicts that blame the
  environment, not the plan; authorise same-version resumes)

You own this dir only. Read other `src/` dirs for context, never edit them.
