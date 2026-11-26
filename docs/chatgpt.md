# Use Beankeeper with local Codex

1. In the ChatGPT desktop app, choose **Codex**, then **Work in → This computer**.
2. Say **Install Beankeeper for me by following https://beankeeper.app/install.md**.
   If ChatGPT asks for permission to run the installer on your computer, approve
   the request in the app. Codex handles the installation and connection.
3. Type \`/mcp\` to see whether **beankeeper** and **beankeeper-service** are
   connected. If Codex says this chat still needs to load the connection,
   choose **Restart** in **Settings → MCP servers**, then open a new Codex chat
   on **This computer**.
4. Say **Set up my books** or **Create demo books**. You can then ask Codex to
   record a practice purchase, move it to a different category, undo the move,
   and explain your spending in chat.

Your books and connection credentials stay on your computer. The agent can
answer in chat and offer a Beankeeper Desktop link for details from your books.
Practice books stay in chat.
[OpenAI's guide to Codex environments](https://learn.chatgpt.com/docs/environments/modes) · [OpenAI's MCP settings guide](https://learn.chatgpt.com/docs/extend/mcp)

## What the agent verifies

Once the per-user service is running, the local agent runs
\`bk agents connect chatgpt-codex --json\` and
\`bk agents doctor chatgpt-codex --json\`. The \`codex\` client name uses the same
adapter and works with older installed versions. Registration creates or
updates \`~/.codex/config.toml\` (or the configured \`CODEX_HOME\`) and installs
the local skill. It can be repeated safely and preserves a server's explicit
\`enabled\` setting. A disabled connection is reported by name.

Doctor verifies the shared profile and makes an authenticated read-only call
through the local service. The agent then calls \`company_list\` through the
current chat to check that Codex has loaded the tools. It calls \`setup_status\`
to continue onboarding. The ledger and lifecycle connections use local stdio,
and the ledger bridge discovers and authenticates to Desktop's loopback MCP
endpoint using Beankeeper's managed credentials. Existing access grants
control which actions succeed. [Managed access and hosting](service.md)

## ChatGPT modes

Use local Codex in the desktop app when coming from regular Chat or ChatGPT
web. ChatGPT Work on **This computer** still needs a signed-in connection
check. The macOS and Windows first-run walkthroughs also remain pending.
