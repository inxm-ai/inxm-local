# Backend — the Rust compiler/executor/repair pipeline

Read this for any task involving `src/compiler`, `src/executor`, `src/plan`,
`src/repair`, `src/storage`, `src/tools`, `src/validator`, or MCP tool integration.

There is no separate "backend service" in `inxm-local` — it's a single Rust crate.
The desktop app (`src/app`) is the only consumer of this pipeline; there's no Kotlin,
no Airflow/Python orchestration, and no separate MCP-hosting service. If a task seems
to call for one of those, stop and confirm with the user — it's very unlikely to be
the right shape for this repo.

---

## The pipeline, in order

```
chat intent
    │
    ▼
compiler/    ← the only place an LLM is invoked; turns intent into a typed Plan
    │
    ▼
validator/   ← deterministic checks only, no AI, never mutates the plan
    │
    ▼
executor/    ← deterministic DAG execution, no AI, persists run state per step
    │
    ├──(success)──► storage/  (run + plan state under `.inxm/`)
    │
    └──(failure)──► repair/   ← diagnoses: bad plan → Patch (via compiler);
                                bad world → WorldFix (human remediation, plan untouched)
```

`tools/` sits underneath `executor/`: `execute_tool` dispatches by `ToolConfig` variant
to an adapter (subprocess, HTTP, MCP) — the executor never talks to a tool transport
directly.

The executor never calls back into `compiler/` at runtime — only `repair/` does, to
propose a patch for a failed run.

---

## Per-module conventions

| Module | Owns | Never contains |
|---|---|---|
| `compiler/` | `backend.rs` (profile-backed LLM backend; transport lives in `src/llm.rs`), `config.rs`, `extractor.rs` (pulls plan JSON out of raw output), `prompt.rs` | Execution or validation logic |
| `validator/` | `graph.rs` (structural/cycle checks), `placeholders.rs` (`${...}` checks), `tool_binding.rs` | Any mutation of the plan, any I/O, any LLM call |
| `executor/` | `dag.rs` (topological ordering), `step_runners/` (one runner per step type: `tool_call`, `prompt_call`, `code_call`, `condition`, `fan`/FAN_OUT, `human`) | LLM calls, plan mutation |
| `repair/` | `classifier.rs` (error → Patch vs WorldFix), `failure_packet.rs`, `patch.rs` (deterministic apply + re-validate) | Human-approval UI (that's a CLI/app-layer step) |
| `plan/` | `types.rs` (Plan/StepConfig IR), `normalization.rs`, `bundle.rs` | Execution or AI logic |
| `storage/` | `plans.rs`, `runs.rs`, `patches.rs`, `world_fixes.rs` — all file-based under `.inxm/` | Business logic — it persists and loads, nothing else |
| `tools/` | `catalog.rs` (`ToolConfig` + catalog), `adapters/` (subprocess, HTTP, MCP, mcp_http) | Plan/DAG logic |

Each module has its own `agents.md` — read it before editing; it's the authoritative,
short version of the table above for that directory.

---

## Language & library choices

| Concern | Library |
|---|---|
| Async runtime | `tokio` (the engine thread; the UI thread in `app/` stays sync) |
| Serialisation | `serde` + `serde_json` everywhere; `serde_yaml` for tool catalogs |
| Domain errors | `thiserror` (`src/error.rs`: `PlanError`, `ToolError`, `CompilerError`, `ExecutorError`, `RepairError`, `StorageError`) |
| Application-level aggregation | `anyhow`, only at true top-level points |
| Outgoing HTTP | `reqwest` (`json` feature; `blocking` only for the fire-and-forget telemetry send) |
| Local HTTP server | `axum` (the local MCP server in `app/mcp_server.rs`) |
| MCP client | `rmcp` (official SDK) for outbound Streamable HTTP + OAuth; a hand-written stdio client remains for local stdio MCP servers |
| DAG scheduling | `petgraph` |
| Tracing | `tracing` + `tracing-subscriber` (`EnvFilter`) — see `observability.md` |
| IDs | `uuid` v4 |
| Timestamps | `chrono` with `serde` |
| Cron parsing | `cron` (scheduled plan runs) |

- All error handling follows ROP (`Result<T, E>` + `?`) — see `engineering-principles.md`.
- Serialisation: `serde` on every type that crosses a module or process boundary. No
  manual JSON string-building.
- No gRPC, no REST API surface beyond the local MCP server that this app itself hosts
  for its own tool integrations — this is a desktop app, not a backend service.

---

## MCP (Model Context Protocol) integration

`inxm-local` is both an MCP **client** (calling out to MCP tool servers configured by
the user, via `tools/adapters/mcp.rs` and `mcp_http.rs`) and a local MCP **server**
(`app/mcp_server.rs`, via `axum`) exposing the app's own tools/plans to external MCP
clients. There is no separate bridge process or Python component:
- Outbound Streamable HTTP + OAuth goes through `rmcp`.
- Local stdio MCP servers are spoken to via the existing hand-written stdio client —
  don't replace it with `rmcp` without checking why it was hand-written first (see the
  `rmcp`/`reqwest`/`rustls` feature-unification notes in `Cargo.toml`).
- Each adapter wraps exactly one transport/concern (Shore Infrastructure Wrapper
  principle) — see `testing.md`.
- OAuth token storage goes through `keyring` (OS credential vault) — secrets never fall
  back to plain files.
