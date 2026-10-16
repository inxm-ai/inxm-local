# Claude API token usage

INXM Local keeps a local record of the token usage that the Anthropic API reports for each Claude API call it makes. You can use it to see exactly how many tokens it took to create a plan and to run it. The numbers are what Anthropic reports, not local estimates.

This record never leaves your machine and is never part of [telemetry](telemetry.md).

## Storage controls

The first toggle, **Record and show Claude token usage**, is off by default and controls the entire local usage record. When it is off, INXM Local writes no new Claude API ledger entries, request bodies, or response bodies.

The second toggle, **Store Claude API messages**, is also off by default and is available only while usage recording is on. It controls message content: when off, the ledger keeps token counts, IDs, status, and response headers, but stores neither the request body nor the response body. When on, both bodies are stored inline with their call in the plan ledger.

The Settings checkboxes are persistent defaults: their values are saved in `settings.json` and stay unchanged. The **Token usage** section appears under **Settings → Compiler** when the backend is Claude or Automatic. Environment variables only override the **effective behavior** of the app instance launched with them. For example, when the checkbox is on but INXM Local starts with `INXM_CLAUDE_USAGE=off`, the checkbox remains on in Settings while that running instance records nothing. Restarting normally restores the saved checkbox behavior.

- `INXM_CLAUDE_USAGE=off` (also `0`, `false`, or `no`) disables all usage recording for that instance, even if the saved checkbox is on.
- `INXM_STORE_CLAUDE_PROMPTS=1` (also `true`, `yes`, or `on`) enables message storage for that instance, even if the saved checkbox is off, but only while usage recording is enabled.

**Settings → Compiler → Token usage** warns when `INXM_CLAUDE_USAGE=off` forces recording off or `INXM_STORE_CLAUDE_PROMPTS=1` enables message storage for the current app instance.

Turning either setting off does not delete files that were already saved.

## What is recorded

INXM Local records Claude API calls made to the Anthropic Messages API (`api.anthropic.com`) in these situations:

| Situation | What counts |
| --- | --- |
| Plan creation | Assessing your request, writing the design (including design corrections), and compiling (including retries after invalid JSON or failed validation), from chat or MCP |
| Plan runs | Every `PROMPT_CALL` step, including each retry attempt and each `FAN_OUT` iteration, for new and resumed runs |
| Plan support | Converting a schedule description to a cron expression, and generating tools (during plan import or on the Tools page) |

Not recorded:

- Other providers (OpenAI, OpenAI-compatible, Vertex) and CLI backends (Codex CLI, Claude Code CLI, custom CLI)
- Anthropic-compatible endpoints, meaning any base URL whose host is not `api.anthropic.com`
- Answers to plain questions in chat (insight answers)

A call is recorded even when it fails, so retries and errors show up as their own entries:

| Outcome | Meaning | Usage |
| --- | --- | --- |
| `ok` | Text answer received | Reported |
| `no_text_block` | Answer had no text, for example thinking used up the token limit. Anthropic still bills it. | Reported |
| `http_error` | Anthropic returned an HTTP error | None |
| `transport_error` | Timeout or connection failure | Unknown |
| `invalid_json` | The response could not be parsed | Unknown |

If you cancel a run while a call is in progress, INXM Local never receives a response for that call, so it is not recorded. Calls that finished before you cancelled stay recorded.

## Where it is stored

All files are under the [data directory](configuration.md):

| Path | Contents |
| --- | --- |
| `llm-usage/plans/<id>.jsonl` | One workflow ledger: it begins under a temporary creation ID, is renamed to the plan ID when saved, then receives that plan's run and support calls. Request and response bodies are inline when message storage is on. |
| `llm-usage/tools.jsonl` | Tool-synthesis calls started from the Tools page, which do not belong to a plan |

If the temporary creation file cannot be renamed, it remains under its creation ID and later runs write to the plan-ID file. INXM Local logs a warning and continues saving the plan; it does not currently retry or repair that rename automatically.

Each ledger line has the call's attribution (`context`) and these fields from Anthropic:

- `message_id` (the response `id`) and `request_id` (the `request-id` header)
- `model_requested`, `model_reported`, `http_status`, `stop_reason`, `outcome`
- `response_headers`: every response header except `set-cookie`, including `anthropic-ratelimit-*` and `retry-after`
- `usage`: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, cache writes by TTL (5 minutes and 1 hour), `thinking_tokens`, server tool request counts, `service_tier`, `inference_geo`, and the unmodified `usage` object as `raw`
- `latency_ms`, `started_at`, and `prompt_stored`

Fields Anthropic leaves out or sets to `null` are stored as `null`, not `0`. API keys and request headers are never written.

Run files also list the Claude API calls for each step under `claude_calls`, so you can read a run's usage without opening the ledger.

Repairing and resuming a run does not erase its recorded token usage. Calls from retried steps stay in that step's `claude_calls` list. If a repair removes a step or replaces it with one that has a different ID, its earlier calls are kept under the original step ID in the run's `archived_claude_calls`. All of these calls still count toward the run's token totals.

## Computing totals

Per call:

- **Input** = `input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens` (the three do not overlap)
- **Output** = `output_tokens`. This already includes thinking, so do not add `thinking_tokens` on top.
- **Total** = input + output

Calls whose usage is `null` count as calls with unknown usage. They are never treated as zero tokens.

- **Plan creation total:** find the link lines for the plan's `plan_id` to get its `creation_session_id` values, then add up every call with one of those session ids.
- **Run total:** add up every call whose `context.run_id` matches the run.

For example, to add up the total tokens of one run with `jq`:

```sh
jq -s '[.[] | select(.context.run_id == "RUN_ID") | .call.usage
        | select(. != null and .input_tokens != null and .output_tokens != null)
        | .input_tokens + (.cache_creation_input_tokens // 0)
          + (.cache_read_input_tokens // 0) + .output_tokens] | add' \
  llm-usage/plans/*.jsonl
```

## Retention

INXM Local never deletes these files. The usage record stays when you delete the plan, the same as past runs do, so you can always look up what earlier work used.


## Why track this

Usage records make the tradeoff between compiled and agentic AI workflows measurable: user experience, orchestration, and provider-reported token use can be compared using the same task.

### A - Compiled

`user -> INXM Local -> compile once -> execute N times`

The user describes a workflow once. INXM Local compiles it into a plan, then executes that plan deterministically as often as needed. The ledger captures the Claude API `usage` fields for the compilation and any bounded `PROMPT_CALL` work during execution.

### B - Agentic

`user -> Claude/ChatGPT + connectors -> reason -> tools -> reason -> tools ... on every run`

The user gives the same task to an agent such as Claude Code. The agent reasons and calls MCP servers or tools during every run. Compare the direct Anthropic API billing data with Claude Code's `/cost` output to understand the recurring agentic cost.