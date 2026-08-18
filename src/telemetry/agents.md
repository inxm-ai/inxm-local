# telemetry module

Optional, opt-in usage telemetry. Off unless the user explicitly
enabled it during first-run setup or in Settings; a runtime kill switch
(`INXM_TELEMETRY=off` or `--no-telemetry`) always wins. Failures must never
affect normal operation — every send is fire-and-forget on its own thread.

Files:
- `schema.rs` — the exhaustive list of events and their fields. If a field
  is not here, it is not collected. No stable identifiers, ever.
- `sender.rs` — the exact sending code: one HTTP POST to the Cloudflare
  Worker, short timeout, errors swallowed (debug-traced only)
- `usage.rs` — consent-gated local counters (`telemetry-usage.json` in the
  data dir), flushed as one `usage_summary` event on the next app start.
  Reads exactly three settings keys (`backend`, `model`,
  `experimental_agent_calls`) so custom-CLI commands can never leak.
- `mod.rs` — consent resolution (settings → env → CLI) and the public
  `record_*` entry points

The receiving side lives in `telemetry-worker/` at the repo root; the user
story is documented in `docs/telemetry.md`. Keep schema, worker, and doc in
sync — the doc promises the schema is exhaustive.

You own this dir only. Read other `src/` dirs for context, never edit them.
