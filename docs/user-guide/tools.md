# Configure tools and MCP servers

The tool catalog defines the external operations that compiled plans can call. Manage it in the application from **MCP Tools** section or edit the YAML catalog in the application data directory.

## Built-in catalog

On first launch, INXM Local seeds a catalog containing an `echo` tool. The repository includes a broader example at [`examples-config/tools.yaml`](https://github.com/inxm-ai/inxm-local/blob/main/examples-config/tools.yaml), including subprocess and HTTP tool shapes.

## Add tools to the catalog

In **MCP Tools**, click **+ Add** to create a catalog entry.

### Choose a tool kind

- **Subprocess**: run an allowlisted executable with fixed arguments and dynamic input values.
- **HTTP**: call an HTTP endpoint using a base URL, method, and path template.
- **MCP**: connect to a local stdio server or a remote Streamable HTTP endpoint.

Tools imported from an MCP server include their input schemas. When adding a tool manually, use **Input schema (JSON Schema)** to describe the inputs it accepts. In a plan, a `TOOL_CALL` step calls a tool by its catalog name.

Subprocess tools receive inputs as environment variables: `INXM_ARGS` contains the full JSON input, and `INXM_ARG_<NAME>` contains an individual value. Only add commands you trust; they run with the permissions of INXM Local.

### Add MCP server tools

Choose **MCP server**, then follow the steps for its transport:

#### Local stdio

1. Select **Local stdio** and enter the server command, arguments, and any required environment variables.
2. To import tools, click **List tools on server**. If the server is available, its tools appear in a checklist, all selected by default. Deselect any you do not need, then click **Import N selected** (where N is the selected count) to add them to your catalog.
3. If you already know the tool name, you can skip discovery. Instead, expand **Add a single tool manually**, enter its exact name in **Tool on server**, fill in the required tool fields, and click **Save tool**.

#### Remote HTTP

1. Select **Remote HTTP** and enter the **Remote endpoint**.
2. If the server does not require OAuth, leave **Connect with OAuth** unchecked. If it requires OAuth, check **Connect with OAuth**, enter a name for the connection, and click **Connect**. Complete authorization in the browser; when connected, INXM Local lists the server's tools automatically. If authorization fails, resolve the connection before adding tools.
3. To import tools, click **List tools on server** if OAuth is off; otherwise, use the list that appears after connecting. All tools are selected by default. Deselect any you do not need, then click **Import N selected** (where N is the selected count) to add them to your catalog.
4. To add just one known tool instead, skip **List tools on server**. If OAuth has already listed tools automatically, click **Dismiss** to close the checklist. Expand **Add a single tool manually**, enter its exact name in **Tool on server**, fill in the required tool fields, and click **Save tool**.

OAuth endpoints must use HTTPS; loopback HTTP is accepted for local development. OAuth tokens and dynamic client registrations are stored in the operating-system credential vault, not in `tools.yaml`. Headless execution can reuse or refresh existing credentials but cannot begin a new interactive authorization flow.

## Verify a tool

Use the catalog view to inspect its schema, then run a small plan that calls the tool with representative inputs. Tool execution errors appear in the run inspection and are eligible for the repair workflow.

<h2>What's next</h2>

- [Follow the guide to set up MCP servers](mcp-server-examples.md) to connect INXM Local to external services.
- [Call the Local MCP server](../reference/mcp-server.md) from another trusted local client.
- [Connect Hermes to INXM Local](hermes.md) when you want to combine both workflows.