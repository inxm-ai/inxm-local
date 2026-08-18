// INXM Local telemetry sink — a Cloudflare Worker in front of Workers
// Analytics Engine. This is the complete receiving side: whatever is not
// written here is not stored anywhere.
//
// Privacy invariants (mirrors docs/telemetry.md):
// - only events matching the exact client schema are accepted
// - the client IP is never read, stored, or logged
// - no cookies, no headers persisted, no per-install identifiers
// - Analytics Engine rows are aggregated counters with ~90 day retention

const ALLOWED = {
  os: ["linux", "macos", "windows"],
  channel: ["desktop", "headless", "mcp_only"],
  backend: [
    "auto",
    "claude",
    "open_ai",
    "codex",
    "claude_code",
    "google_vertex",
    "open_ai_compatible",
    "anthropic_compatible",
    "custom_cli",
  ],
};

// e.g. "0.1.0" or "0.1.0-rc.1" — anything else is rejected, so arbitrary
// strings can never be smuggled into the version column.
const VERSION_RE = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]{1,20})?$/;

// Model *names* only (the client already refuses to send commands or
// executables); anything longer or with exotic characters is rejected.
const MODEL_RE = /^[0-9A-Za-z._\/: -]{0,64}$/;

// The flattened counters of a usage_summary, in a fixed order that defines
// the Analytics Engine double columns (double2..double16; double1 stays the
// sample count, as in app_started).
const COUNTERS = [
  "plans_created_app",
  "plans_created_mcp",
  "plans_edited_app",
  "plans_edited_mcp",
  "runs_succeeded_app",
  "runs_succeeded_mcp",
  "runs_failed_app",
  "runs_failed_mcp",
  "runs_healed_app",
  "runs_healed_mcp",
  "seconds_in_chat",
  "seconds_in_plans",
  "seconds_in_schedules",
  "seconds_in_mcp_tools",
  "seconds_in_settings",
];

const isCount = (value) =>
  typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 1e9;

function parseAppStarted(body) {
  const { event, app_version, os, channel, ...rest } = body ?? {};
  const valid =
    Object.keys(rest).length === 0 &&
    event === "app_started" &&
    ALLOWED.os.includes(os) &&
    ALLOWED.channel.includes(channel) &&
    typeof app_version === "string" &&
    VERSION_RE.test(app_version);
  return valid
    ? { blobs: [event, app_version, os, channel], doubles: [1] }
    : null;
}

function parseUsageSummary(body) {
  const {
    event,
    app_version,
    os,
    backend,
    model,
    experimental_agent_calls,
    ...rest
  } = body ?? {};
  const valid =
    event === "usage_summary" &&
    ALLOWED.os.includes(os) &&
    ALLOWED.backend.includes(backend) &&
    typeof app_version === "string" &&
    VERSION_RE.test(app_version) &&
    typeof model === "string" &&
    MODEL_RE.test(model) &&
    typeof experimental_agent_calls === "boolean" &&
    Object.keys(rest).length === COUNTERS.length &&
    COUNTERS.every((name) => isCount(rest[name]));
  return valid
    ? {
        blobs: [
          event,
          app_version,
          os,
          backend,
          model,
          String(experimental_agent_calls),
        ],
        doubles: [1, ...COUNTERS.map((name) => rest[name])],
      }
    : null;
}

export default {
  async fetch(request, env) {
    if (request.method !== "POST" || new URL(request.url).pathname !== "/v1/event") {
      return new Response("not found", { status: 404 });
    }

    let body;
    try {
      body = await request.json();
    } catch {
      return new Response("invalid json", { status: 400 });
    }

    const point = parseAppStarted(body) ?? parseUsageSummary(body);
    if (!point) {
      return new Response("schema mismatch", { status: 400 });
    }

    // No indexes (an index would be a grouping key we don't need), one row
    // per event. The write is best-effort on the server side too.
    env.TELEMETRY.writeDataPoint(point);
    return new Response(null, { status: 204 });
  },
};
