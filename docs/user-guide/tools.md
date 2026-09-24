# Configure tools and MCP servers

The tool catalog defines the external operations that compiled plans can call. Manage it from **MCP Tools** or edit the YAML catalog in the application data directory.

## Built-in catalog

On first launch, INXM Local seeds a catalog containing an `echo` tool. The repository includes a broader example at [`examples-config/tools.yaml`](https://github.com/inxm-ai/inxm-local/blob/main/examples-config/tools.yaml), including subprocess and HTTP tool shapes.

## Add a local tool

In **MCP Tools**, add one of these supported kinds:

- **Subprocess**: run an allowlisted executable with fixed arguments and dynamic input values.
- **HTTP**: call an HTTP endpoint using a base URL, method, and path template.
- **MCP**: connect to a local stdio server or a remote Streamable HTTP endpoint.

Give each tool an input schema and output schema. Plans reference tools by name through `TOOL_CALL` steps.

Subprocess inputs are also exposed to the process as `INXM_ARGS` (the complete JSON object) and `INXM_ARG_<NAME>` variables. Only add commands you trust; an allowlisted subprocess can execute with the permissions of the INXM process.

## Connect a remote MCP server

For an unauthenticated remote endpoint, configure an MCP tool with an endpoint and tool name. For a protected endpoint, enable OAuth and choose **Connect**. Tokens and dynamic client registrations are stored in the operating-system credential vault, not in `tools.yaml`.

OAuth endpoints must use HTTPS; loopback HTTP is accepted for local development. Headless execution can reuse or refresh existing credentials but cannot begin a new interactive authorization flow.

## Verify a tool

Use the catalog view to inspect its schema, then run a small plan that calls the tool with representative inputs. Tool execution errors appear in the run inspection and are eligible for the repair workflow.

<h2>What's next</h2>

- [Run a plan with your configured tools](plans-and-runs.md) and inspect what each step produces.
- [Call the Local MCP server](../reference/mcp-server.md) from another trusted local client.
- [Connect Hermes to INXM Local](hermes.md) when you want to combine both workflows.