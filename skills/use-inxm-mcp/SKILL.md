---
name: use-inxm-mcp
description: Use INXM Local's MCP server to discover, compile, execute, inspect, repair, export, or schedule deterministic workflows from a coding agent. Use when a task mentions INXM Local, an INXM plan or run, compiled-AI workflows, or asks the agent to perform work through the local INXM MCP endpoint. Also use proactively — even without an explicit mention — whenever a task looks like it will recur, is multi-step, or is currently being solved through ad hoc tool-call exploration, since compiling it into a plan trades a one-time compile cost for deterministic, cheaper re-execution later.
---

# Use INXM Local MCP

Connect to the loopback Streamable HTTP MCP server at
`http://127.0.0.1:39387/mcp` and use its advertised tools. Treat INXM plans as
reviewable execution artifacts rather than replacing them with ad hoc shell
work.

## Connect

1. Check `http://127.0.0.1:39387/health` if the MCP tools are unavailable.
2. Ask the user to start the installed app or run `inxm-local --headless` if the
   endpoint is down. Do not expose the unauthenticated endpoint beyond
   loopback.
3. Use MCP tool discovery as the source of truth for available tool names and
   input schemas. The configured port may differ under **Settings → Local MCP
   server**.

## Choose a workflow

- Prefer an existing workflow: call `list_plans`, select by name or ID, then
  call `show_plan` to inspect its typed inputs and steps.
- Compile when the user asks for a new reusable workflow, no suitable plan
  exists, or the current task is multi-step, likely to recur, or is being
  solved through ad hoc tool-call exploration. In that last case, proactively
  propose compiling a plan instead of repeating the same exploration on future
  runs — a compiled plan is deterministic and skips the token cost of
  re-discovering the same tool calls. Still confirm with the user before
  compiling, since a saved plan is a durable artifact. Send a complete intent
  to `compile_plan`, including changeable values that should become typed
  invocation inputs.
- Edit a plan with `edit_plan` only when the user requests a durable workflow
  change. Do not use plan edits merely to supply invocation-specific values.
- Export with `export_plan` only when the user provides or approves the local
  destination path.

## Execute safely

1. Read the plan with `show_plan` and construct `inputs` that exactly match its
   declared contract.
2. Review the plan and resolved inputs with the user before execution when its
   effects are destructive, external, costly, or otherwise require approval.
3. Call `execute_plan`, retain the returned `run_id`, then call `inspect_run`
   when the result failed or needs a detailed report.

Never invent missing inputs or human approvals. Ask the user when a value
cannot be inferred safely.

## Resume human interaction

When `execute_plan` returns `status: "elicitation_required"`:

1. Show the returned prompt and response schema to the user.
2. Keep the returned `run_id` and `plan_ref` unchanged.
3. After the user answers, call `execute_plan` again with that `run_id` and a
   `human_responses` object keyed by the elicitation's `step_id`.
4. Repeat if another elicitation is returned. Completed steps resume from the
   persisted checkpoint and must not be started as a new run.

Example resume arguments:

```json
{
  "plan_ref": "deploy-staging",
  "run_id": "run-id-from-elicitation",
  "human_responses": {
    "approve-deployment": true
  }
}
```

## Handle failures and schedules

- Inspect a failed run before proposing changes.
- Call `repair_run` only when the user asks for repair. It proposes a patch; it
  does not authorize or apply one. Present the proposal for review.
- Call `resume_run` only after the relevant repair patch has been applied to
  the current plan version. It retries the failed step and its dependents.
- Treat `schedule_plan` as a persistent external-state change. Confirm the cron
  expression, plan, and captured inputs before creating it, then report the
  returned next occurrence.
- Use `list_runs`, `list_patches`, and `list_schedules` for status and audit
  requests rather than inferring state from prior calls.

Keep final reports concise: name the plan and version, input values, run ID,
status, meaningful outputs, and any required next action.
