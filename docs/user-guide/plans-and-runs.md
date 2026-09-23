# Plans, runs, and repairs

INXM Local separates planning from execution. The configured LLM creates or edits a typed plan; the validator checks it; and the executor runs the saved plan and persists its progress. 

This separation is the core of the [determinism boundary](#determinism-boundary): language-model reasoning shapes the workflow before execution, while the executor follows the stored definition during a run.

## Plans

A plan is the versioned, executable definition of a workflow. It contains:

- **Metadata** such as its stable ID, version, original intent, compiler provenance, and draft or published status.
- **Inputs** supplied by a caller or schedule. Each input has a name, type, required/default behavior, and optional path semantics. Steps reference runtime values as `${input.<name>}`.
- **Configuration** that stays with the plan and is available through `${conf.<key>}`.
- **Steps** with stable IDs, dependencies, named outputs, timeouts, and optional bounded retry policies. Depending on the workflow, steps can call tools or code, make a bounded prompt call, wait for a person, branch on a condition, fan out over a list, collect results, or invoke an explicitly configured agent.
- **Published outputs** that point to named step outputs and become the plan's final result after a successful run.

Dependencies form a graph rather than a fixed list of commands. The executor runs a step only after its dependencies are complete, so later steps can consume validated inputs and earlier step outputs. A plan can be edited or repaired into a new version while keeping the plan identity and its history.

## Runs

A run is one execution of one plan version. Before execution, the invocation is checked against the plan's input contract: missing required values, unknown values, and incorrect types are rejected before work begins.

During execution, the run stores its resolved inputs and a record for each step. Step records include status, attempts, timing, outputs, standard output/error where applicable, token usage for reported model work, and failure details. The executor orders dependencies topologically, applies configured timeouts and retries, and persists state after each step. A completed step is not silently rerun because a later step failed.

The run status can be successful, failed, cancelled, or waiting for human input. A `HUMAN_INTERACTION` step creates a checkpoint with either an approval decision or free-text response. The desktop app collects the response in chat; an MCP caller receives a persisted run ID and resumes that same run with the response.

## Repairs

Repair is a proposal workflow, not an automatic rewrite. After a failed run is classified, the compiler proposes a constrained patch and the user applies or rejects it. Applying a patch creates a new plan version. Resuming then resets the failed step and its downstream dependents, while preserving unrelated successful work.

Not every failure means the plan is wrong. A repair can either change the plan or identify an external problem that the user must fix first. In the first case, resume against the newer patched version. In the second, resume the same version after the external system, credentials, file, or other dependency has been corrected.

## Human interaction

Approval and free-text steps are explicit in the plan. In the desktop app they are answered in chat. Through MCP, `execute_plan` returns `elicitation_required` with a persisted `run_id`; the caller supplies `human_responses` to resume that run.

## Determinism boundary

LLM calls can compile plans, perform bounded `PROMPT_CALL` steps, or propose repairs. The executor does not ask an LLM to improvise the next step. Tool calls, conditions, fan-out, human interaction, retries, and persistence are controlled by the saved plan and executor. A model-backed step is therefore explicit in the plan and visible in the run rather than an invisible controller of the whole workflow.

<h2>What's next</h2>

- [Run a workflow from chat](chat-workflows.md) to compile, invoke, inspect, and repair a plan.
- [Schedule a workflow](schedules.md) when you need recurring runs with captured inputs.
