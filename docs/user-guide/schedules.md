# Schedule workflows

Create a local cron schedule for a plan when the same workflow should run repeatedly with fixed invocation inputs.

## Before you begin

Compile and test the plan once. Identify its required input values and choose a cron expression interpreted in local time.

## Create a schedule

From the plan chat, run:

```text
/schedule <plan> <cron> [--inputs '<json>']
```

For example:

```text
/schedule daily-summary "0 8 * * *" --inputs '{"recipient":"team@example.com"}'
```

The captured inputs are validated when the schedule is created and reused each time it fires.

### Understanding the cron expression

A cron expression has five parts, in this order:

```text
minute hour day-of-month month day-of-week
```

For example, `0 8 * * MON-FRI` means “at 8:00 a.m. every Monday through Friday.” The `*` means “every,” and ranges select a span of values. Numeric weekdays in INXM start at Sunday = 1, so `1-5` means Sunday through Thursday. Schedule times use your computer's local time zone.


## Manage schedules

Use `/schedules` in any chat to list schedules. The Plans view provides controls for schedule state; the MCP API also exposes `list_schedules`, `delete_schedule`, and `set_schedule_enabled`.

## Keep schedules running

Schedules run only while an INXM Local scheduler is running. Choose how you want to keep it running:

- **Use the desktop app:** Leave **Keep schedules running in the background** enabled in Settings (it is on by default). Closing the window hides INXM Local in the system tray; it does not quit the app. From the tray menu, you can reopen the window, pause or resume all schedules, or quit INXM Local. Pausing does not change each schedule's enabled state.
- **Run without the desktop app:** Keep this command running in a terminal or managed process:
	`inxm-local --headless` or `INXM_HEADLESS=1 inxm-local`

Only one INXM Local instance can run schedules for a data directory at a time. If no scheduler is running when a scheduled time passes, that occurrence is skipped; it will not run later as a catch-up.

<h2>What's next</h2>

- [Troubleshoot common problems](troubleshooting.md) if your schedules do not run as expected.
- [Connect tools and MCP servers](tools.md) to give your plans useful capabilities.