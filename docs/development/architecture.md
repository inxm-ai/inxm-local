# Architecture

INXM Local is a native Rust desktop application with a shared workflow core and a local HTTP MCP surface.

```mermaid
flowchart LR
    UI[egui desktop UI] --> E[Application engine]
    MCP[Local MCP client] --> S[Loopback MCP server]
    S --> E
    E --> C[Compiler]
    E --> V[Validator]
    E --> X[Deterministic executor]
    X --> T[Tool catalog]
    X --> R[(Plans, runs, schedules)]
    C --> L[Configured LLM or account CLI]
    X --> H[Human interaction]

    classDef interface fill:var(--bg-raised),stroke:var(--accent),color:var(--fg-strong)
    classDef core fill:var(--bg-surface),stroke:var(--status-info),color:var(--fg-strong)
    classDef execution fill:var(--bg-raised),stroke:var(--status-ok),color:var(--fg-strong)
    classDef external fill:var(--bg-surface),stroke:var(--status-warn),color:var(--fg-strong)
    classDef data fill:#ffffff,stroke:var(--status-error),color:var(--fg-strong)
    class UI,MCP interface
    class S,E,C,V core
    class X,T execution
    class L external
    class R data
    class H interface
```

## Components

- The **desktop UI** renders chat, plans, runs, settings, schedules, and tools with egui.
- The **application engine** runs asynchronous work on a Tokio runtime and bridges commands and events to the UI.
- The **compiler** turns intent into a plan; the **validator** checks contracts before persistence or execution.
- The **executor** runs plan steps in dependency order and persists state through the storage layer.
- The **tool catalog** supplies subprocess, HTTP, and MCP-backed operations.
- The **local MCP server** exposes the same plan and execution operations to local clients.

## Data flow

Desktop and MCP requests enter the shared engine paths. Compilation produces a validated saved plan. Execution resolves inputs, runs dependencies, writes step state, and emits progress. A failed run can be sent to the repair path, which produces a patch for explicit approval.

## Trust boundaries

The MCP server listens on loopback and rejects non-loopback Host or Origin headers. It is unauthenticated, so local processes with access to the loopback endpoint should be treated as trusted. Subprocess tools and experimental agent steps execute with the process permissions and should be allowlisted deliberately.

Remote MCP OAuth credentials are kept in the operating-system credential vault. They are not serialized into the tool catalog, plans, or exported bundles.

<h2>What's next</h2>

- Trace a workflow through [plans, runs, and repairs](../user-guide/plans-and-runs.md).
- [Set up the repository](setup.md) and explore the implementation locally.
