# Troubleshoot common problems

Use these checks to diagnose common setup and workflow issues. 

> If you need help, you can create an anonymized support ticket from the app with `/support [run-id]`. For a failed run, the command includes run context in a prefilled GitHub issue.

## The compiler is not configured

Open **Settings -> Compiler** and choose a backend. For automatic API-key selection, set `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` before starting the app. Account-backed connections require the corresponding `codex` or `claude` CLI to be installed and signed in.

## The MCP server cannot bind its port

The default endpoint is `http://127.0.0.1:39387/mcp`. If the configured port cannot be bound, the server tries an available ephemeral port instead; check the reported listening port and update your client URL. To choose a different fixed port, change it under **Settings -> Local MCP server**, save, and restart. `INXM_MCP_ONLY=1 inxm-local` is useful when the native window cannot start.

## A remote MCP tool needs authorization

Start authorization from **MCP Tools -> Connect** while the desktop app is available. Scheduled and headless runs cannot open an interactive OAuth flow. Reconnect if the credential expired or the server requested new scopes.

## A schedule does not fire

Check that the schedule is enabled and that either the desktop app is allowed to keep running in the tray or `inxm-local --headless` is running. A second process using the same data directory will not start a duplicate scheduler.
