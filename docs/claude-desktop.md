# Connect Beankeeper to Claude Desktop

1. When **Get it for the Claude app** is available, download Beankeeper from
   [beankeeper.app](https://beankeeper.app/).
2. Double-click the downloaded file and click **Install** in the Claude app.
   [Anthropic’s extension installation instructions](https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop)
3. Say **Set up my books** in Claude. To try fictional transactions first,
   say **Create demo books**.

Claude can set up your books and accounts, post opening balances, and import
a CSV, OFX or QFX statement you share in chat. It reports each change, and you
can ask it to undo supported changes in the same chat.
[Chat setup guide](../SKILL.md#guide-the-owner-through-book-setup)

The extension serves the Beankeeper guide over MCP before you create books,
and the guide updates with the extension. In local Claude Code sessions in
the Desktop app, Anthropic currently excludes Desktop extensions and
`claude_desktop_config.json` servers from automatic MCP skill loading. Those
connections supply brief setup and tool guidance in the MCP handshake.
[Anthropic's client support notes](https://code.claude.com/docs/en/mcp#where-mcp-skills-are-supported)

In Claude Desktop, you only need the **Beankeeper** extension. If Desktop also
shows `beankeeper` or `beankeeper-service` under **Local dev**, those are Claude
Code's connections for ledger access and background service controls.

On your first setup request, the extension creates encrypted books, saves the
passphrase in your computer’s credential store, and starts Beankeeper in the
background under your account. It saves a private recovery kit beside the
books. Setup continues in the same chat. Other supported apps detected during
setup can load their new connections the next time you open them.
[Service storage and recovery](service.md#platform-behavior-and-files)

## First-open messages

During testing, Claude may show **Developer: unknown** until its extension
signature verification supports the signed bundle. On macOS, Gatekeeper may
say **Apple cannot check ‘bk’ for malicious software** or **‘bk’ cannot be
opened because the developer cannot be verified**. On Windows, SmartScreen may
show **Windows protected your PC**.
These messages can appear while OS code signing is pending. If you downloaded
the official test bundle and independently confirmed it with your test
coordinator, macOS offers **System Settings > Privacy & Security > Open Anyway**
for eligible warnings. On Windows, the corresponding choice is
**More info > Run anyway** when available. Stop if the OS reports malware or
your organization blocks the extension.
[Apple’s Gatekeeper guidance](https://support.apple.com/en-us/102445)

If Claude says it can only read your books, say **Let my agent post** in chat
when you want it to record transactions. Claude uses the extension's
`access_set` tool. Say **Make me read-only** to turn posting off. Beankeeper
respects your choice to stay read-only. Restarting cannot grant posting access.

If a detail link asks you to pair the browser, say **Open Beankeeper Desktop**
in chat. The extension opens your default browser. Select **Pair this browser**
there to continue to the detail you requested.

If Claude can see the extension but setup fails, say **Check my Beankeeper
connection**. Claude can check setup progress and explain any issue with
the local connection. If Claude cannot see the extension at all, follow
Anthropic’s extension troubleshooting instructions.
[Anthropic’s extension troubleshooting](https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop)

## Remove the extension

Remove Beankeeper in Claude Desktop's **Extensions** settings. If you also
want to remove background hosting, ask your agent on a connected Beankeeper
lifecycle connection. Your books and recovery material stay on your machine.

## Advanced service commands

Use `bk service start` before reconnecting the extension to a service you
stopped. Use `bk service uninstall` to remove background hosting. If you
installed only the extension, its `bk` is in the `claude-extension`
subdirectory of the per-user service state.
[Service state directories](service.md#platform-behavior-and-files)
