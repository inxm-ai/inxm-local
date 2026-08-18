# inxm-local telemetry worker

The complete receiving side of INXM Local's opt-in telemetry.
One Cloudflare Worker, one Workers Analytics Engine dataset — nothing else.

- `worker.js` — validates the exact client schema, writes one aggregated
  row, never reads the client IP, never sets cookies or logs headers.
- `wrangler.toml` — the dataset binding and route.

Deploy with `npx wrangler deploy` from this directory (requires a Cloudflare
account with the `inxm.ai` zone).

What the client sends, why, and how users disable it is documented in
[`docs/telemetry.md`](../docs/telemetry.md); the sending code lives in
[`src/telemetry/`](../src/telemetry/).
