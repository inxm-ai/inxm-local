# Why INXM exists and what it can do

We at [INXM](https://www.inxm.ai/) are building a Process Execution Engine for AI workflows, which we call the Orchestrator. Its central idea is *Compiled AI*: use AI to understand intent and shape a workflow, then execute that workflow reliably, repeatably, and with a clear record of what happened. Agents reason; automation executes. Critical work needs both.

We built [INXM Local](https://www.inxm.ai/oss/inxm-local/) as the *open-source, local-first* version of that approach: a hands-on way to experience Compiled AI on your own machine. Describe what you want in chat or through a local client, then see the workflow before it runs and keep a record of what happened.

We believe this is a better starting point for AI workflows than asking a model to improvise every step of every run. Try INXM Local, take it apart, and see whether this way of working fits your own use case.

If you need the same approach at enterprise scale, with governance, auditability, and integration across the business systems you already run, learn more about [INXM Orchestrator](https://www.inxm.ai/) or [talk to us](https://www.inxm.ai/contact.html).

## What INXM Local is not

INXM Local is deliberately not another chat assistant that produces a fresh answer and loses the workflow behind it. It is not an unrestricted autonomous agent that silently changes its behavior while a run is in progress. It is not a black box that asks a model what to do next at every step.

Instead, INXM Local makes the workflow a saved, versioned artifact. The compiler may use a language model to create or repair that artifact, and a plan may explicitly include bounded model-backed work. Once execution begins, however, the deterministic executor follows the saved steps, validates inputs, persists progress, pauses for declared human interaction, and exposes failures for inspection or repair.

INXM Local is also not a replacement for the systems a workflow connects to. Compiler backends, configured tools, MCP servers, and account CLIs remain dependencies whose availability and permissions affect a run. Its MCP endpoint is a local, unauthenticated interface, so it is intended for trusted processes on the same machine rather than as a centrally governed enterprise service.

## How the pieces fit together

At a high level, you begin with an intent: a description of the outcome or process you want. The compiler turns that intent into a structured plan with declared inputs, ordered steps, tool references, and any human decisions the workflow needs. The validator checks the plan before it is saved, so the executor does not start with an ambiguous or malformed definition.

Once the plan is ready, a trigger supplies its inputs and starts a run. The deterministic executor follows the saved dependencies, calls the configured tools, and persists progress as each step completes. That gives you a live result during execution and a record you can inspect afterward. The same plan can be run again with different inputs, scheduled for repeated use, or invoked by another local client through MCP.

Some workflows intentionally stop for a human approval or free-text answer. Others fail because an external tool, service, or input is unavailable. In both cases, the state is explicit: a human can resume the paused run, and a failed run can be sent back to the compiler for a repair proposal that must be reviewed and applied before the workflow changes.

```mermaid
flowchart LR
    I([Intent]) --> C([Compiler])
    C --> P[Validated plan]
    P --> T([Trigger with inputs])
    T --> R[Run]
    R --> O([Inspectable result])
    R --> H{Human interaction}
    H -->|answer| R
    R --> F([Failure])
    F --> X[Repair proposal]
    X -->|apply| P

    classDef input fill:var(--bg-raised),stroke:var(--accent),color:var(--fg-strong)
    classDef control fill:var(--bg-surface),stroke:var(--status-info),color:var(--fg-strong)
    classDef result fill:var(--bg-raised),stroke:var(--status-ok),color:var(--fg-strong)
    classDef decision fill:var(--bg-surface),stroke:var(--status-error),color:var(--fg-strong)
    class I,T input
    class C,P,R,X control
    class O result
    class H,F decision
```

<h2>What's next</h2>

- [Follow a plan through its runs and repairs](../user-guide/plans-and-runs.md) to see the model in action.
- [Create your first workflow](first-workflow.md) and experience the full execution loop.
