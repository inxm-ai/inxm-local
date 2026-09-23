# Run workflows from chat

Use a plan's chat to edit the plan, supply inputs, run it, and handle repair or approval steps.

Each chat is a conversation owned by at most one plan. A new chat starts without a plan: describe a workflow in plain language or use `/compile` to create one. After a plan is created, that conversation becomes its plan chat; opening the plan or one of its runs returns to the same conversation. Start a new chat when you want to create a separate plan.

## Compile or edit a plan

In a new chat, enter plain-language intent or use `/compile <intent>`. In a chat that owns a plan, use `/edit <change>` to request an LLM-assisted edit. Review proposed edits before applying them.

## Run with inputs

Run the attached plan with optional JSON inputs:

```text
/run --inputs '{"query":"Rust workflow engines","limit":5}'
```

Input names and types come from the plan. Unknown, missing, or incorrectly typed values are rejected before execution.

## Handle human interaction

When a plan reaches a `HUMAN_INTERACTION` step, the plan card pauses and displays an approval or free-text prompt. Answer it in the card to continue the same run.

## Inspect, repair, and resume

Use `/inspect [run-id]` for step details. For a failed run, `/repair [run-id]` asks the compiler to propose a patch. Apply or reject the proposal with `/apply <patch-id>` or `/reject <patch-id> [reason]`. After applying a repair, `/resume <run-id>` reruns the failed step and its downstream steps against the new plan version.

## Find plans and runs

`/plans` lists stored plans. `/show <plan>` opens a plan by name or ID prefix, and `/runs` lists recent runs. The complete command syntax is in the [chat command reference](../reference/chat-commands.md).

<h2>What's next</h2>

- [Understand plans, runs, and repairs](plans-and-runs.md) before you build a larger workflow.
- [Schedule a workflow](schedules.md) when it is ready to run repeatedly.
- [Connect tools and MCP servers](tools.md) to give your plans useful capabilities.