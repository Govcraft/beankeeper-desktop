# Connect Beankeeper to Claude Desktop

1. Download `bk-VERSION-TARGET.mcpb` and its `.sigstore` bundle from the same
   GitHub release. Choose Apple Silicon or Intel on macOS, x64 on Windows, or
   x64 or arm64 on Linux. Verify the bundle with the release checksum and
   Sigstore identity before opening it. [Desktop releases](https://github.com/Govcraft/beankeeper-desktop/releases)
2. Open the verified bundle in Claude Desktop and confirm the installation. If
   opening the file does not launch Claude, use **Settings → Desktop app →
   Extensions → Install Extension** and select the file. Linux requires the
   Claude Desktop beta and a system compatible with the Beankeeper release
   binary, including glibc 2.39 or newer. [Claude Desktop extension installation](https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop)
3. Enable the extension in a Claude Desktop chat and ask **Set up my books**.
   Beankeeper answers in the chat. Open a Desktop link when you want to see
   the detail behind an answer.

On its first launch, the extension sets up encrypted local books and starts
per-user background hosting if Beankeeper is not already configured. It uses
the OS secret store and creates a private recovery kit beside the books. Keep
an offline copy under your control. Claude receives the ledger's agent access
over stdio and can request the tools that its grants allow. [Service storage
and recovery](service.md#platform-behavior-and-files)

An existing Beankeeper service keeps its books, credentials, and lifecycle
settings. If you stopped that service, start it again with `bk service start`
before reconnecting the extension. If setup failed, check the extension's log
in Claude Desktop and make sure your OS login session and keychain are
available. [Service diagnostics](service.md#commands)

## Remove the extension

Remove Beankeeper in Claude Desktop's **Extensions** settings. To remove
background hosting too, run `bk service uninstall`. This preserves your books
and recovery material. If you installed only the extension, its `bk` is in the
`claude-extension` subdirectory of the per-user service state listed in the
service documentation. [Service state directories](service.md#platform-behavior-and-files)
