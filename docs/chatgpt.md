# Use Beankeeper with local Codex

1. In the ChatGPT desktop app, choose **Codex** in the ChatGPT menu. Under
   **Work in**, choose **This computer**.
2. Say **Install Beankeeper for me by following https://beankeeper.app/install.md.**
   Codex runs the signed installer in your local session and registers the
   connection. You can approve local execution if ChatGPT asks.
3. If the current chat still lacks the tools, open **Settings → MCP servers**,
   select **Restart**, and start a new local Codex chat. Type `/mcp` to see the
   Beankeeper servers. Say **Set up my books** or **Create demo books**.

The agent handles installation and connection from your local session. You
can keep your terminal closed and leave the connection credentials in Beankeeper.
[Choose a local Codex environment](https://learn.chatgpt.com/docs/environments/modes)
· [MCP settings](https://learn.chatgpt.com/docs/extend/mcp)

## What the agent sets up

`bk agents connect chatgpt-codex --json` uses the same adapter as
`bk agents connect codex --json`. Both merge the two stdio server entries into
`~/.codex/config.toml` (or the configured `CODEX_HOME`) and install the local
Beankeeper skill. The desktop app’s local Codex, CLI, and IDE use the shared
profile. The agent can repeat the command to repair setup. On older versions
of `bk` it can use `codex` as the client name.

`bk agents doctor chatgpt-codex --json` checks the profile and uses the managed
credential for a read-only probe of Desktop. To verify the loaded session,
the agent should call `company_list` in Codex and then `setup_status`. The
ledger bridge supplies Beankeeper’s MCP instructions and serves setup, posting,
categorization, audited undo, and reporting through the same local service
used by other agents. The existing access grant controls which calls succeed.
The lifecycle server can diagnose or start the local service when it is down.
The separate `.mcpb` installer configures Claude Desktop.
[Managed access and hosting](service.md) · [Claude Desktop extension](claude-desktop.md)

## What other ChatGPT surfaces require

| Surface | Connection |
| --- | --- |
| **Codex on this computer** | OpenAI documents local stdio and Streamable HTTP and says Codex reads server `instructions` at initialization. Beankeeper uses stdio, which reaches Desktop’s authenticated loopback `/mcp` through the existing bridge. [Local MCP configuration](https://learn.chatgpt.com/docs/extend/mcp) |
| **Regular Chat and Work in the desktop app** | The shared Codex configuration is documented for the Codex host only. OpenAI describes a desktop-only plugin path for threads that run locally. Submitting a plugin with a local stdio server requires OpenAI partner allowlisting. Beankeeper has no published ChatGPT plugin. [ChatGPT plugins](https://learn.chatgpt.com/docs/plugins) · [Desktop-only submissions](https://developers.openai.com/plugins/deploy/desktop-only) |
| **ChatGPT web and hosted Chat or Work** | Use **Plugins → Add custom MCP server** with a reachable HTTPS URL or a Secure MCP Tunnel. ChatGPT accepts remote SSE or Streamable HTTP. A loopback URL such as `http://127.0.0.1:8750/mcp` is private to the owner’s machine, so ChatGPT cannot reach it directly. Workspace settings, including permissions to create and use MCP plugins and Lockdown, control access. [Add custom MCP server](https://developers.openai.com/api/docs/guides/custom-mcp-server) · [Connect and test](https://developers.openai.com/plugins/deploy/connect-chatgpt) · [Workspace plugin controls](https://learn.chatgpt.com/docs/enterprise/apps-and-connectors) |

OpenAI’s custom-server flow accepts OAuth, anonymous access, or mixed OAuth and
anonymous access. OAuth supports static client credentials, Client ID Metadata
Documents (CIMD) with `none` or `private_key_jwt` client authentication, and
Dynamic Client Registration (DCR). Beankeeper’s existing local agent credential
does not implement that flow. ChatGPT also defaults to confirming plugin writes.
[Custom server auth and write behavior](https://developers.openai.com/api/docs/guides/custom-mcp-server)
· [OAuth details](https://developers.openai.com/plugins/build/auth)

A Secure MCP Tunnel can carry traffic to the private server over an outbound
connection while the ledger stays on the owner’s computer. It requires a
Platform tunnel ID, a runtime API key, and tunnel and workspace permissions.
The tunnel is a transport. User authorization still belongs to the MCP server.
For Beankeeper, use local Codex. A future private web integration would need
OAuth that maps to existing access grants and a way to provision and run the
tunnel for the owner. Public plugin distribution has separate requirements.
[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
