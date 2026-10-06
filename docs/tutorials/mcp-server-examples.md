# Adding MCP servers

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
3. In **MCP Tools**, click **+ Add** and enter the fields above. Click **List tools on server**, then select and import the tools you need. When in doubt, refer to [the guide on local stdio](../user-guide/tools.md#local-stdio).

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
3. Once the connection shows **Connected**, INXM Local lists the tools automatically. Select the ones you need and click **Import N selected**. For more details, refer to [the guide on Remote HTTP](../user-guide/tools.md#remote-http).

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
3. Once the connection shows **Connected**, INXM Local lists the tools automatically. Select the ones you need and click **Import N selected**. See [Remote HTTP](../user-guide/tools.md#remote-http) for more details.

## Linear

Linear's hosted MCP server connects to your workspace through browser-based OAuth.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Linear` |
| Remote endpoint | `https://mcp.linear.app/mcp` |
| Connect with OAuth | Checked |
| Public client ID | Leave blank |

1. In **MCP Tools**, click **+ Add** and enter the fields above.
2. Click **Connect**, then **Open authorization page** when the link appears. Sign in to Linear and authorize access to your workspace.
3. Once the connection shows **Connected**, INXM Local lists the tools automatically. Select the ones you need and click **Import N selected**. See [Remote HTTP](../user-guide/tools.md#remote-http) for more details.

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
5. In INXM Local **MCP Tools**, click **+ Add** and enter the fields above, replacing `xoxp-your-token-here` in **Server env** with your token. Click **List tools on server**, then select and import the tools you need. See [Local stdio](../user-guide/tools.md#local-stdio) for more details. The token is saved in the local tool catalog; protect that file and do not commit it to a repository.

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
2. In **MCP Tools**, click **+ Add** and enter the fields above. Click **List tools on server**, then select and import the tools you need. See [Local stdio](../user-guide/tools.md#local-stdio) for more details.

## Gmail

Google's hosted Gmail MCP server is available through the Google Workspace Developer Preview.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Gmail` |
| Remote endpoint | `https://gmailmcp.googleapis.com/mcp/v1` |
| Connect with OAuth | Checked |
| Public client ID | Enter a public OAuth client ID accepted by Google |

1. Join the [Google Workspace Developer Preview Program](https://developers.google.com/workspace/preview), create or select a Google Cloud project, and enable the Gmail API and Gmail MCP API.
2. Configure the Google Auth Platform consent screen and Gmail scopes as described in [Google's Gmail MCP setup guide](https://developers.google.com/workspace/gmail/api/guides/configure-mcp-server).
3. In **MCP Tools**, click **+ Add** and enter the fields above. Click **Connect** and complete Google's authorization flow.
4. When the connection shows **Connected**, select the Gmail tools you need and click **Import N selected**.

INXM uses OAuth with PKCE and accepts a public client ID, but does not accept an OAuth client secret. Google's setup guide may show a client type that requires a secret; use a public client supported by the MCP server, or the connection cannot complete in INXM.

## Google Calendar

Google's hosted Calendar MCP server is available through the Google Workspace Developer Preview.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Google Calendar` |
| Remote endpoint | `https://calendarmcp.googleapis.com/mcp/v1` |
| Connect with OAuth | Checked |
| Public client ID | Enter a public OAuth client ID accepted by Google |

1. Join the [Google Workspace Developer Preview Program](https://developers.google.com/workspace/preview), create or select a Google Cloud project, and enable the Google Calendar API and Google Calendar MCP API.
2. Configure the Google Auth Platform consent screen and Calendar scopes as described in [Google's Calendar MCP setup guide](https://developers.google.com/workspace/calendar/api/guides/configure-mcp-server).
3. In **MCP Tools**, click **+ Add** and enter the fields above. Click **Connect** and complete Google's authorization flow.
4. When the connection shows **Connected**, select the Calendar tools you need and click **Import N selected**.

INXM uses OAuth with PKCE and accepts a public client ID, but does not accept an OAuth client secret. Google's setup guide may show a client type that requires a secret; use a public client supported by the MCP server, or the connection cannot complete in INXM.

## Google Drive

Google's hosted Drive MCP server is available through the Google Workspace Developer Preview.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Google Drive` |
| Remote endpoint | `https://drivemcp.googleapis.com/mcp/v1` |
| Connect with OAuth | Checked |
| Public client ID | Enter a public OAuth client ID accepted by Google |

1. Join the [Google Workspace Developer Preview Program](https://developers.google.com/workspace/preview), create or select a Google Cloud project, and enable the Google Drive API and Google Drive MCP API.
2. Configure the Google Auth Platform consent screen and Drive scopes as described in [Google's Drive MCP setup guide](https://developers.google.com/workspace/drive/api/guides/configure-mcp-server).
3. In **MCP Tools**, click **+ Add** and enter the fields above. Click **Connect** and complete Google's authorization flow.
4. When the connection shows **Connected**, select the Drive tools you need and click **Import N selected**.

INXM uses OAuth with PKCE and accepts a public client ID, but does not accept an OAuth client secret. Google's setup guide may show a client type that requires a secret; use a public client supported by the MCP server, or the connection cannot complete in INXM.

## Todoist

Todoist's official hosted MCP server connects through browser-based OAuth.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Todoist` |
| Remote endpoint | `https://ai.todoist.net/mcp` |
| Connect with OAuth | Checked |
| Public client ID | Leave blank |

1. In **MCP Tools**, click **+ Add** and enter the fields above.
2. Click **Connect**, then open the authorization page and sign in to Todoist.
3. When the connection shows **Connected**, select the Todoist tools you need and click **Import N selected**. See [Remote HTTP](../user-guide/tools.md#remote-http) for details.

## Apple Reminders

This example uses the community-maintained [apple-mcp](https://github.com/baileywickham/apple-mcp) server on the Mac that has access to your Reminders. It requires macOS 14 or later on Apple silicon.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Apple Reminders` |
| Remote endpoint | `http://127.0.0.1:8000/mcp` |
| Connect with OAuth | Unchecked |

1. Install `apple-mcp` by following the project's [installation instructions](https://github.com/baileywickham/apple-mcp#install).
2. Start its HTTP server on loopback:

    ```sh
    apple-mcp --http --host 127.0.0.1 --port 8000
    ```

3. On first use, grant Apple MCP access to Reminders when macOS prompts you. Keep the server bound to `127.0.0.1`; without a token, any local process that can reach it can use its tools.
4. In **MCP Tools**, click **+ Add**, enter the fields above, and click **List tools on server**. Select the reminder tools you want and click **Import N selected**.

## Google Maps / Places

Google's hosted Maps Tools MCP endpoint requires a Google Maps Platform API key in the `X-Goog-Api-Key` request header. INXM's Remote HTTP form does not currently expose custom headers, so connect through the local `mcp-remote` stdio bridge instead.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Local stdio (bridge to the hosted Remote HTTP server) |
| Name | `Google Maps` |
| Server command | `npx` |
| Server args | `-y mcp-remote https://mapstools.googleapis.com/mcp --header "X-Goog-Api-Key: ${GOOGLE_MAPS_API_KEY}"` |
| Server env | `GOOGLE_MAPS_API_KEY=your_api_key_here` |

1. Create a Google Maps Platform API key and enable the Maps Platform APIs required by the tools you plan to use. See the [Google Maps MCP sample](https://github.com/googlemaps/a2ui) for the endpoint and API-key header pattern.
2. Make sure Node.js and `npx` are available.
3. In **MCP Tools**, click **+ Add** and enter the fields above, replacing the example value with your key. Click **List tools on server**, select the tools you need, and click **Import N selected**.

The API key is saved in the local tool catalog. Protect that file and do not commit it to a repository.

## Home Assistant

Home Assistant's built-in Model Context Protocol Server integration exposes a Streamable HTTP endpoint from your own instance.

| Field | Value |
| --- | --- |
| Kind | MCP server |
| Transport | Remote HTTP |
| Name | `Home Assistant` |
| Remote endpoint | `https://your-home-assistant-url/api/mcp` |
| Connect with OAuth | Checked |
| Public client ID | Leave blank |

1. In Home Assistant, add the **Model Context Protocol Server** integration and choose which entities it may expose. See [Home Assistant's MCP Server documentation](https://www.home-assistant.io/integrations/mcp_server/).
2. In **MCP Tools**, click **+ Add** and enter the fields above. Replace the endpoint with your configured external HTTPS URL or Home Assistant Cloud URL.
3. Click **Connect** and complete authorization, then import the tools you need.

**OAuth compatibility:** Home Assistant uses OAuth Client ID Metadata Documents rather than the Dynamic Client Registration flow INXM currently uses when the client ID is blank. Direct OAuth may therefore fail. For a local Home Assistant instance, use the token bridge below instead.

Configure INXM as a local stdio client of `mcp-remote`, which forwards a Home Assistant long-lived access token:

| Field | Value |
| --- | --- |
| Transport | Local stdio |
| Server command | `npx` |
| Server args | `-y mcp-remote http://homeassistant.local:8123/api/mcp --header "Authorization: Bearer ${HOME_ASSISTANT_TOKEN}"` |
| Server env | `HOME_ASSISTANT_TOKEN=your_long_lived_access_token` |

Create the token in your Home Assistant user profile under **Security**. Replace the URL with the address of your instance, keep the token private, and do not expose an unauthenticated Home Assistant endpoint. In **MCP Tools**, add this as an MCP server using **Local stdio**, then list and import the tools.
