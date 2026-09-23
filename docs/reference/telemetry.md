# Telemetry

INXM Local can send **two anonymous events, both only at app start**: a launch ping and a batched usage summary, and only if you said yes on the setup screen. There is no real-time tracking: nothing is sent while you use the app. This page is the complete, binding description of what that means. If something is not listed here, it is not collected.

## Consent

- The first-run setup screen discloses the ping with a checkbox that is **checked by default**: telemetry is on unless you uncheck it there (or turn it off later, see below). Leaving the screen in any way, including "Get started", "Skip for now", or clicking a navigation item, keeps whatever the checkbox shows at that moment.
- **Nothing is sent while the setup screen is still open**, so unchecking the box is a real pre-collection opt-out.
- Installs that never see the setup screen send nothing: existing installs upgrading to a version with telemetry, and headless/agent-mode installs, have no recorded choice. No recorded choice always means **off**, because collection before disclosure is never allowed.

## What is sent

Exactly two events exist, both sent at process start and never during use.

**`app_started`** — one ping per launch:

```json
{
  "event": "app_started",
  "app_version": "0.1.0",
  "os": "linux",
  "channel": "desktop"
}
```

- `app_version`: the Cargo package version of the build
- `os`: `linux`, `macos`, or `windows` (`std::env::consts::OS`)
- `channel`: `desktop`, `headless`, or `mcp_only` (which entry point ran)

**`usage_summary`** — plain tallies accumulated locally since the previous launch, flushed as one event on the next start and then reset:

```json
{
  "event": "usage_summary",
  "app_version": "0.1.0",
  "os": "linux",
  "backend": "claude",
  "model": "claude-sonnet-5",
  "experimental_agent_calls": false,
  "plans_created_app": 2,
  "plans_created_mcp": 0,
  "plans_edited_app": 1,
  "plans_edited_mcp": 0,
  "runs_succeeded_app": 4,
  "runs_succeeded_mcp": 3,
  "runs_failed_app": 1,
  "runs_failed_mcp": 0,
  "runs_healed_app": 1,
  "runs_healed_mcp": 0,
  "seconds_in_chat": 840,
  "seconds_in_plans": 120,
  "seconds_in_schedules": 0,
  "seconds_in_mcp_tools": 30,
  "seconds_in_settings": 15
}
```

- `backend`: the configured compiler backend *kind* (for example, `claude`, `codex`, or `custom_cli`)
- `model`: the configured model **name only**, trimmed and capped at 64 characters. For custom CLIs this is still just the model field: the code reads exactly three settings keys (`backend`, `model`, and `experimental_agent_calls`), so an executable path or command template can never be included. A test asserts this.
- `experimental_agent_calls`: whether the experimental agent-step toggle is on
- `*_app` / `*_mcp`: the same action counted by surface: the desktop app (including its scheduler) versus the local MCP server. `runs_healed` counts successful post-repair resumes and is a subset of `runs_succeeded`.
- `seconds_in_*`: foreground time per view, in whole seconds. Time only accrues while the window is focused; tray/background time never counts.

The tallies live in `telemetry-usage.json` in the data dir until the next launch, so you can inspect exactly what would be sent. Counting itself is consent-gated: with telemetry off the file is never written, so opting in later can never ship earlier activity.

There are **no identifiers** (no machine id, install id, hostname, or username), **no client timestamps** (counters are totals with no ordering or session boundaries beyond "since the last launch"), and **no user content** (no plan names, run ids, prompts, or paths). The schema is enforced on both ends: the client types are [`src/telemetry/schema.rs`](https://github.com/inxm-ai/inxm-local/blob/main/src/telemetry/schema.rs), and the receiving worker rejects any payload with extra, missing, or out-of-range fields.

## How to disable it

Any one of these wins over everything else; none of them can *enable* telemetry:

| Mechanism | How |
| --- | --- |
| Settings UI | Untick *Settings → Anonymous usage ping* |
| Config file | Set `"telemetry_enabled": false` (or remove the key) in `settings.json` in the data dir |
| Environment | `INXM_TELEMETRY=off` (also accepts `0`, `false`, `no`) |
| CLI | Pass `--no-telemetry` |

## Where it goes, and for how long

Events go to `https://telemetry.inxm.ai/v1/event`, a Cloudflare Worker whose full source is in [`telemetry-worker/`](https://github.com/inxm-ai/inxm-local/tree/main/telemetry-worker). The worker writes one row per ping into Cloudflare **Workers Analytics Engine** and nothing else: the client IP is never read or stored, no cookies are set, and no request headers are persisted. Analytics Engine keeps data for roughly **90 days** and then drops it; there is no long-term store.

You can point the client at your own sink for inspection with `INXM_TELEMETRY_ENDPOINT=http://localhost:8080/v1/event`.

## Purpose

Telemetry answers product questions through aggregated counts only:

- How many installs start the app, by version and operating system, so we know which platforms and releases to support.
- Which entry points and surfaces people use, such as the desktop app, headless mode, and MCP, so we can prioritise polish.
- How the plan lifecycle behaves in the field, including created, edited, successful, failed, and healed runs, so we can judge compiler and repair quality.
- Which compiler backends and model names are most common.
- Whether the experimental agent-call mode is being used.
- Which views people actually spend time in.

None of this can be traced back to a machine.

## Non-blocking by construction

Every send runs on a detached thread with a 3-second timeout; failures are swallowed (visible only at `debug` trace level). Telemetry can never delay startup, block a run, or surface an error. The exact sending code is [`src/telemetry/sender.rs`](https://github.com/inxm-ai/inxm-local/blob/main/src/telemetry/sender.rs): it is the only place in the codebase that talks to the telemetry endpoint.
