# Infrastructure — release packaging + the telemetry Worker

Read this for any task involving GitHub release builds, OS installers/packaging, or
the Cloudflare Worker telemetry sink. There is no Kubernetes, Helm, Terraform, or
Ansible in this repo — `inxm-local` is a distributed desktop binary, not a hosted
service. Do not introduce container/orchestration infra unless the user explicitly
asks for a hosted component.

---

## Release builds (`.github/workflows/release.yml`)

- Triggered by a published GitHub Release, or manually via `workflow_dispatch` (which
  builds the same artifacts without publishing).
- A matrix build across `x86_64-unknown-linux-gnu` (`.deb`), `x86_64-pc-windows-msvc`
  (installer `.exe`), and `x86_64-apple-darwin` (`.app.zip`) — one native package per
  platform, produced by `cargo build --release` plus the platform-specific packaging
  step, not a cross-compilation shortcut.
- Version comes from `Cargo.toml` unless overridden via the manual dispatch input —
  don't hardcode a version anywhere else.
- Changes to this workflow are root-integration-owner territory per `Agents.md`
  (CI metadata), not a module owner's concern.

## OS packaging (`packaging/`)

- `packaging/install.sh` / `packaging/install.ps1` — user-facing install scripts.
- `packaging/linux/` — Linux-specific packaging assets (desktop icon, `.deb` install script).
- Keep these scripts POSIX-sh / plain PowerShell — no dependency on a package manager
  beyond what the target OS ships with, since they run before the app itself is installed.

## Telemetry sink (`telemetry-worker/`)

The only "hosted" component in this repo, and it's intentionally minimal:
- A single Cloudflare Worker (`worker.js`) fronting Workers Analytics Engine, deployed
  via `wrangler.toml` — no additional infrastructure-as-code layer on top.
- Served at a custom domain (`telemetry.inxm.ai`) so Cloudflare manages the DNS record.
- Analytics Engine gives ~90-day-retention aggregate counters with no per-row identity
  by design — don't propose swapping this for a database or a longer-retention store
  without the user explicitly asking, since retention/identity limits are a deliberate
  privacy choice (see `observability.md`).
- Changes here must stay in lockstep with `src/telemetry/schema.rs`'s allow-lists
  (`ALLOWED.os`, `ALLOWED.channel`, `ALLOWED.backend`, the version regex) — a new
  client-side enum variant that isn't also allow-listed here is silently dropped, not
  an error, so treat both sides as one change.
