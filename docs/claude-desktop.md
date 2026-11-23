# Connect Beankeeper to Claude Desktop

1. Download `bk-VERSION-TARGET.mcpb` and its `.sigstore` bundle from the same
   GitHub release. Choose Apple Silicon or Intel on macOS, x64 on Windows, or
   x64 or arm64 on Linux. Verify the bundle with the release checksum and
   Sigstore identity before opening it. [Desktop releases](https://github.com/Govcraft/beankeeper-desktop/releases)
2. Sign in to Claude Desktop, open the verified bundle, and confirm the
   installation. If the installation prompt does not appear, open
   **Settings → Desktop app**, choose **Manage** next to **Extensions**, then
   choose **Advanced settings → Install Extension** and select the file.
   Linux requires the Claude Desktop beta and a system compatible with the
   Beankeeper release binary. The release build targets glibc 2.35 or newer.
   The published 0.1.0-rc.11 bundle requires glibc 2.39.
   [Claude Desktop extension installation](https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop)
3. Enable the extension in a Claude Desktop chat and ask **Set up my books**.
   Beankeeper answers in the chat. Open a Desktop link when you want to see
   the detail behind an answer.

In Claude Desktop, you only need the **Beankeeper** extension. If Desktop also
shows `beankeeper` or `beankeeper-service` under **Local dev**, those are Claude
Code's connections for ledger access and background service controls.

On its first launch, the extension sets up encrypted local books and starts
per-user background hosting if Beankeeper is not already configured. It uses
the OS secret store and creates a private recovery kit beside the books. Keep
an offline copy under your control. Claude receives the ledger's agent access
over stdio and can request the tools that its grants allow. [Service storage
and recovery](service.md#platform-behavior-and-files)

If Claude says it can only read your books, say **Let my agent post** in chat
when you want it to record transactions. Claude uses the extension's
`access_set` tool. Say **Make me read-only** to turn posting off. Beankeeper
respects your choice to stay read-only. Restarting cannot grant posting access.

If setup fails, check the extension's log in Claude Desktop and make sure
your OS login session and keychain are available.

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
