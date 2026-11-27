# Use Beankeeper with the ChatGPT desktop app

Installing needs the ChatGPT desktop app, because a regular ChatGPT chat can't
run programs on your computer.

1. In the desktop app, start a Work task (or open Codex).
2. Say **Install Beankeeper for me by following https://beankeeper.app/get.md**.
   If ChatGPT asks for permission to run the installer on your computer, approve
   the request in the app. The agent follows the developer steps at
   https://beankeeper.app/install/agent.md and handles the installation and
   connection.
3. Type `/mcp` to see whether **beankeeper** and **beankeeper-service** are
   connected. If the agent says this chat still needs to load the connection,
   choose **Restart** in **Settings → MCP servers**, then open a new Work or
   Codex chat on this computer.
4. Say **Set up my books** or **Set up demo books**. You can then ask the agent
   to record a demo purchase, move it to a different category, undo the move,
   and explain the spending in chat.

Your books and connection credentials stay on your computer. The agent can
answer in chat and offer a Beankeeper Desktop link for details from your books.
Demo books stay on your computer, separate from your real books.
[OpenAI's guide to Codex environments](https://learn.chatgpt.com/docs/environments/modes) · [OpenAI's MCP settings guide](https://learn.chatgpt.com/docs/extend/mcp)

## What the agent verifies

Once the per-user service is running, the local agent runs
`bk agents connect chatgpt-codex --json` and
`bk agents doctor chatgpt-codex --json`. The `codex` client name uses the same
adapter and works with older installed versions. Registration creates or
updates `~/.codex/config.toml` (or the configured `CODEX_HOME`) and installs
the local skill. It can be repeated safely and preserves a server's explicit
`enabled` setting. A disabled connection is reported by name.

Doctor verifies the shared profile and makes an authenticated read-only call
through the local service. The agent then calls `company_list` through the
current chat to check that the tools have loaded. It calls `setup_status`
to continue onboarding. The ledger and lifecycle connections use local stdio,
and the ledger bridge discovers and authenticates to Desktop's loopback MCP
endpoint using Beankeeper's managed credentials. Existing access grants
control which actions succeed. [Managed access and hosting](service.md)

## ChatGPT modes

Use a Work task or local Codex in the desktop app when coming from regular Chat
or ChatGPT web. ChatGPT Work on this computer still needs a signed-in connection
check. The macOS and Windows first-run walkthroughs also remain pending.
