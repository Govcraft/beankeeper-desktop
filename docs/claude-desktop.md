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

## Install or upgrade from Beankeeper

If you already have `bk` or a local Beankeeper agent connection, ask your agent
to install or upgrade the Claude Desktop extension. The `agents_extension`
tool is available in Claude's Beankeeper extension and on the local lifecycle
connection. The command-line equivalent is `bk agents extension`.

Before setup, the extension lists `setup`, `setup_status`, `doctor`,
`update_check`, and `agents_extension`, alongside the license tools. You can prepare the verified installer before creating
books or starting the local service.

Beankeeper verifies the newest published release for your computer in-process,
without requiring a separate `cosign` installation.
It verifies the Sigstore signatures on the checksums and extension, then requires
the extension's SHA-256 to match the signed checksum. On Linux, Beankeeper
launches Claude directly through its XDG desktop entry
with `gio launch`, falling back to `claude-desktop` on the executable search
path. It does not depend on the `.mcpb` MIME association. If Claude is not
found, the agent reports `AGENT_EXTENSION_CLAUDE_NOT_FOUND` and asks you to
install or open Claude Desktop. On macOS and Windows, the operating system
opens the verified file. Select **Install** in Claude Desktop, then restart
Claude Desktop or toggle the extension off and on to load the new version.

When opening the extension, Beankeeper removes its managed stdio entries from
`claude_desktop_config.json` if they are present. It keeps other client settings
and saves the original file as `claude_desktop_config.json.beankeeper-backup` if
there is no backup yet. If you cancel installation and want the stdio connection
back, run `bk agents connect claude-desktop`.

Use `bk agents extension --path-only` for the local file path, or add `--json`
for a structured result. The MCP equivalent is `{"path_only":true}`. Path-only
mode leaves Claude's settings alone, so you can open the file later or select it
from Claude's extension settings if your system has no file association.

Desktop updates do not replace Claude's self-contained extension. Ask your agent
to check the extension after an update. `bk doctor` and `bk update-check` return
`extension.state` as `behind`, `current`, or `not_installed` (or `unavailable`
with a diagnostic code when the registry cannot be read). The MCP tools `doctor`
and `update_check` return the same extension data and explain the next step.
An extension newer than Desktop counts as current. Diagnostics compare against
the authenticated running service version when available, otherwise this `bk`
version. A stale extension never blocks the books. Ask your agent to upgrade it;
the agent opens the verified installer through `agents_extension`.

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

If first setup cannot use the computer’s keyring, the agent explains whether it
is locked or a Secret Service provider cannot be reached. A locked keyring needs
a sign-out and sign-in before retrying. On Ubuntu or Debian the agent can help
install `gnome-keyring` or enable an existing provider. No terminal instructions
are required from the owner. `bk agents doctor` also reads the native keyring
when the service is unavailable, without writing a credential; failures include a named code, an owner message,
and a separate sanitized `technical_detail` field in its JSON `data.keyring`
result. Backend error text and credential bytes are never included.

## Claude Code and local Cowork plugin

Download `claude-plugin.zip` from the signed Desktop release and upload it at
**Customize > Plugins > Add > Upload plugin**. Install Beankeeper Desktop with
its graphical installer first. The plugin carries the same guide as the chat
extension and runs the installed `bk mcp extension`. The host must find `bk`
on its executable search path; ask your agent or support to check the host's
environment if it cannot connect. The plugin contains no executable.

The local MCP server loads in Claude Code and Cowork sessions running on your
computer. Chat ignores it, so keep using `.mcpb` for Claude app chat. Uploading
the plugin alone does not connect chat to your local books.
[Plugin structure](https://claude.com/docs/plugins/build) and
[platform support](https://claude.com/docs/plugins/platform-support).

The marketplace publishing tree is prepared in this repository. Publishing
Govcraft/beankeeper-claude-plugins is a PM follow-up.
