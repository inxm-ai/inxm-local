# MCP server examples

Use these examples to configure MCP servers in **MCP Tools**

## GitHub

In this example, INXM Local runs `mcp-remote` locally to connect to GitHub's hosted MCP server using your personal access token (PAT).

<details>
  <summary>Why a PAT and not OAuth?</summary>
  <p>INXM Local's remote HTTP OAuth flow with a blank client ID uses dynamic client registration, which <a href="https://github.com/github/github-mcp-server/issues/1404">GitHub's MCP server does not support</a>. The local proxy passes the PAT as a bearer token instead. This limitation is specific to GitHub's endpoint; other servers may support dynamic registration.</p>
</details>

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Local stdio |
| Name | `GitHub` |
| Server command | `npx` |
| Server args | `-y mcp-remote https://api.githubcopilot.com/mcp/ --header "Authorization: Bearer ${GITHUB_TOKEN}"` |
| Server env | `GITHUB_TOKEN=ghp_yourTokenHere` |

1. Make sure Node.js is installed and `npx` is available.
2. Create a GitHub PAT with the permissions needed for the tools you plan to use. Replace `ghp_yourTokenHere` in **Server env** with your token. This value is saved in the local tool catalog; protect that file and do not commit it to a repository.
3. In **MCP Tools**, click **+ Add** and enter the fields above. Click **List tools on server**, then select and import the tools you need. When in doubt, refer to [the guide on local stdio](tools.md#local-stdio).


## Granola

Granola supports browser-based OAuth without a pre-registered client ID.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Granola` |
| Remote endpoint | `https://mcp.granola.ai/mcp` |
| Connect with OAuth | Checked |
| Public client ID | Leave blank |

1. In **MCP Tools**, click **+ Add** and enter the fields above.
2. Click **Connect**, then click on **Open authorization page** when the link appears. Sign in to Granola and authorize access in your browser.
3. Once the connection shows **Connected**, INXM Local lists the tools automatically. Select the ones you need and click **Import N selected**. For more details, refer to [the guide on Remote HTTP](tools.md#remote-http).

## Notion

Notion's hosted MCP server connects to your workspace through browser-based OAuth.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Notion` |
| Remote endpoint | `https://mcp.notion.com/mcp` |
| Connect with OAuth | Checked |
| Public client ID | Leave blank |

1. In **MCP Tools**, click **+ Add** and enter the fields above.
2. Click **Connect**, then **Open authorization page** when the link appears. Sign in to Notion and authorize access to the workspace you want to use.
3. Once the connection shows **Connected**, select and import the tools you need. See [Remote HTTP](tools.md#remote-http) for more details.

## Slack

This example runs the Slack MCP server locally with a user OAuth token from a Slack app installed in your workspace.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Local stdio |
| Name | `Slack` |
| Server command | `npx` |
| Server args | `-y slack-mcp-server@latest --transport stdio` |
| Server env | `SLACK_MCP_XOXP_TOKEN=xoxp-your-token-here` |

1. Make sure Node.js is installed and `npx` is available.
2. Login at [Slack API apps](https://api.slack.com/apps), create a **Blank app** and choose the workspace where you want to install it.
3. In the app's left sidebar, open **OAuth & Permissions**. Under **User Token Scopes**, select the scopes needed for the Slack features you plan to use. For example, an app might use:
    - `channels:history`, `channels:read`, `channels:write`
    - `groups:history`, `groups:read`
    - `im:history`, `im:read`, `im:write`
    - `mpim:history`, `mpim:read`, `mpim:write`
    - `users:read`
    - `chat:write`
    - `search:read`
    - `usergroups:read`, `usergroups:write`
4. Scroll up to **Install to Workspace** (or **Install App**) and approve the permissions. Copy the **User OAuth Token** (starting with `xoxp-`) from the same **OAuth & Permissions** page.
5. In INXM Local **MCP Tools**, click **+ Add** and enter the fields above, replacing `xoxp-your-token-here` in **Server env** with your token. Click **List tools on server**, then select and import the tools you need. See [Local stdio](tools.md#local-stdio) for more details. The token is saved in the local tool catalog; protect that file and do not commit it to a repository.

## Microsoft 365

In this case, a single server configuration covers multiple M365 services, so you can choose their tools from one server.

<details>
  <summary>Tool selection and optional server flags</summary>
  <p>With no extra flags, the command below offers personal-account tools by default. This includes Outlook mail and calendar, contacts, OneDrive, Excel, OneNote, To Do, Planner, profile, and search.<br>
  To change what the server offers, append flags to <strong>Server args</strong>:</p>
  <ul>
    <li><code>--org-mode</code> enables work/school features such as Teams, SharePoint, meetings, and shared mailboxes. Without it, those tools are not offered. <code>--org-mode</code> works as a switch; do not pass an email address, tenant ID, or <code>true</code> after it.</li>
    <li><code>--preset [NAME]</code> limits the offered tools to a named category. Use comma-separated names to combine categories, for example <code>--preset mail,calendar</code>. Presets do not enable work/school features on their own; add <code>--org-mode</code> when those tools are needed.</li>
  </ul>
  <p>Available presets: <code>mail</code>, <code>calendar</code>, <code>files</code>, <code>personal</code>, <code>work</code>, <code>excel</code>, <code>contacts</code>, <code>tasks</code>, <code>onenote</code>, <code>search</code>, <code>users</code>, <code>outlook</code>, <code>onedrive</code>, <code>teams</code>, <code>teams-write</code>, <code>all</code>. See the <a href="https://github.com/Softeria/ms-365-mcp-server#tool-presets">Softeria preset documentation</a> for the most up-to-date list, or run <code>npx @softeria/ms-365-mcp-server --list-presets</code>.</p>
  <p>Presets still filter tools with <code>--org-mode</code>. Your account permissions may limit what you can use.</p>
</details>

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Local stdio |
| Name | `m365` |
| Server command | `npx` |
| Server args | `-y @softeria/ms-365-mcp-server` |

1. Make sure Node.js is installed and `npx` is available.
2. In **MCP Tools**, click **+ Add** and enter the fields above. Click **List tools on server**, then select and import the tools you need. See [Local stdio](tools.md#local-stdio) for more details.
