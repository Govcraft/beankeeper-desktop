# Govcraft Beankeeper plugins

This marketplace offers Beankeeper's guide and local MCP connection for Claude
Code and Cowork sessions running on your computer. Install Beankeeper Desktop
first using its graphical installer. Chat ignores the plugin's local server;
Claude app chat uses the signed `.mcpb` extension instead.

Publishing is a PM follow-up. No marketplace repository is created by this change.
Run `python scripts/release.py claude-plugin` in the Desktop source repository.
The release tooling validates this catalog and builds `staging/claude-marketplace/`
with this README, the catalog, and `plugins/beankeeper/` from the same files as
`claude-plugin.zip`. Publish that complete tree as Govcraft/beankeeper-claude-plugins.
Do not publish this source template alone, because the generated plugin subtree
is required. After publication an agent can add that marketplace and install
`beankeeper@govcraft-beankeeper`; owners can use Customize > Plugins > Add >
Add marketplace with the published repository URL.
