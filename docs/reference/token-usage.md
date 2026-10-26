# Token usage

INXM Local tracks provider-reported token usage to help you understand workflow consumption. 

Records stay on your machine and are never part of [telemetry](telemetry.md).

## Available counts

There are two ways to count tokens in INXM Local: basic counts for workflow steps and optional detailed tracking for direct Claude API calls.

| | Basic count | Detailed Claude API tracking |
| --- | --- | --- |
| Availability | Always on across supported API and CLI connections, when usage is reported | Opt-in, direct Claude API only |
| Scope | Input and output totals for workflow steps | Individual calls for plan creation, edits, execution, and selected supporting actions |
| Level of detail | Step-level summary, not a complete history of billed attempts | Call history with retries, failures, cache usage, and metadata |

To enable detailed tracking, go to **Settings → Compiler → Token usage** and turn on **Record and show Claude token usage**. This setting is shown for **Claude API** and **Automatic** connections, but records only direct Claude API calls. It does not track **Claude account** (Claude Code CLI) calls. Basic counting remains on for both API and CLI connections.

Both use usage reported by the provider or CLI. Neither estimates missing usage or makes extra AI calls to count tokens.

## Basic count

Basic counting needs no setup. It collects reported input and output tokens from `PROMPT_CALL` and `AGENT_CALL` steps, including individual `FAN_OUT` iterations, and adds them to the run summary:

```text
Tokens: 1 AI step — 100 in / 20 out (120 total).
```

Supported sources include Claude / Anthropic, OpenAI / OpenAI-compatible, Vertex, Anthropic-compatible endpoints, and structured Claude Code or Codex CLI output. Custom CLIs must return recognized usage fields in JSON for `AGENT_CALL`; plain-text output and custom-CLI `PROMPT_CALL` responses do not provide counts.

These are step-level summaries, not a complete history of every billed attempt. They do not include plan compilation or other AI interactions outside workflow execution. Basic counting does not capture request or response bodies.

Claude API input counts include cache reads and writes.

## Detailed count for Claude API

Detailed tracking records individual requests to the Anthropic Messages API at `api.anthropic.com`. It adds cache details, retries, failures, and call metadata. It does not cover other providers, compatible endpoints, or CLI-internal requests.

An API-backed agent still runs through a CLI: its reported usage contributes to basic counts, not the detailed ledger. See [Agent steps](execution.md#agent-steps).

When enabled, it covers:

| Activity | Recorded calls |
| --- | --- |
| Plan creation and edits | Assessment, design, compilation, and edit compilation from chat or MCP, including retries and unapplied edit proposals |
| Workflow execution | Direct Claude API `PROMPT_CALL` attempts and `FAN_OUT` iterations, including resumed runs |
| Supporting actions | Schedule-to-cron conversion and tool generation from the MCP Tools page |

Plain chat answers and repair proposals are not recorded. Each completed attempt gets its own entry, even on failure; unknown usage is marked as unknown rather than zero. A call still in progress when cancelled has no entry because no response was received.

### Enable recording and optional message storage

Under **Settings → Compiler → Token usage** (shown for **Claude API** or **Automatic**, not **Claude account**):

1. Enable **Record and show Claude token usage** to record and display detailed calls. It is off by default.
2. Optionally enable **Store Claude API messages** to save request and response bodies as well. It is off by default and requires recording to be enabled.

Without message storage, detailed records contain usage and metadata, not message bodies. Ordinary step outputs are unaffected by either setting.

Turning recording off stops new detailed records and hides detailed displays. Existing records remain, and basic counting continues. Turning it back on cannot recover detailed calls made while it was off.

Environment variables can override these settings for the current app instance without changing the saved checkboxes:

- `INXM_CLAUDE_USAGE=off` (also `0`, `false`, or `no`) forces detailed recording and displays off.
- `INXM_STORE_CLAUDE_PROMPTS=1` (also `true`, `yes`, or `on`) enables message storage, but only while detailed recording is enabled.

Settings shows a warning when either override applies. Basic counting is unaffected.

### Stored records

Basic counts are saved as `token_usage` on steps and iterations in the ordinary run record. Detailed calls also appear under `claude_calls`; calls from removed or replaced steps are retained in `archived_claude_calls` and still contribute to detailed run totals.

The detailed ledger files are under the [data directory](configuration.md):

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

A call's `outcome` is `ok`, `no_text_block`, `http_error`, `transport_error`, or `invalid_json`. An answer without a text block can still have reported, billed usage. HTTP errors have no usage; transport failures and invalid responses have unknown usage.

#### Computing ledger totals

Per call:

- **Input** = `input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens` (the three do not overlap)
- **Output** = `output_tokens`. This already includes thinking, so do not add `thinking_tokens` on top.
- **Total** = input + output

Calls whose usage is `null` count as calls with unknown usage. They are never treated as zero tokens.

- **Plan creation total:** add up `plan_creation` calls whose `context.plan_id` matches the plan (including edits), plus calls whose `creation_session_id` is tied to the plan by a link line. Count each call only once.
- **Detailed API usage for a run:** add up every call whose `context.run_id` matches the run. This ledger-only calculation excludes basic usage from CLI agents or other providers.

For example, to add up the known detailed API tokens for one run with `jq`:

```sh
jq -s '[.[] | select(.context.run_id == "RUN_ID") | .call.usage
        | select(. != null and .input_tokens != null and .output_tokens != null)
        | .input_tokens + (.cache_creation_input_tokens // 0)
          + (.cache_read_input_tokens // 0) + .output_tokens] | add' \
  llm-usage/plans/*.jsonl
```

INXM Local never deletes these files. The usage record stays when you delete the plan, the same as past runs do, so you can always look up what earlier work used.


## Why track this: compiled vs. agentic AI

Usage records make the tradeoff between compiled and agentic AI workflows measurable: user experience, orchestration, and provider-reported token use can be compared using the same task.

### Compiled AI

`user -> INXM Local -> compile once -> execute N times`

The user describes a workflow once. INXM Local compiles it into a plan, then executes that plan deterministically as often as needed. The ledger captures the Claude API `usage` fields for the compilation and any bounded `PROMPT_CALL` work during execution.

### Agentic AI

`user -> Claude/ChatGPT + connectors -> reason -> tools -> reason -> tools ... on every run`

The user gives the same task to an agent such as Claude Code. The agent reasons and calls MCP servers or tools during every run. Compare the direct Anthropic API billing data with Claude Code's `/cost` output to understand the recurring agentic cost.