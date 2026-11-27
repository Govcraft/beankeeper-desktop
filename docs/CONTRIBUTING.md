# Contributing to Beankeeper Desktop

Beankeeper Desktop is source-available. See [LICENSE.md](../LICENSE.md).
We are currently closed to outside code contributions and will close external
pull requests. If we open code submissions, we will update this guide and
require a contributor agreement before merging contributions.

## Report a bug or suggest a feature

Issues and feedback are welcome in the [issue tracker](https://github.com/Govcraft/beankeeper-desktop/issues).

- **Bug reports.** Include the Desktop version, operating system, steps to
  reproduce, and what you expected and observed.
- **Feature requests.** Describe the bookkeeping task and the outcome you need.

Ledger, library, and schema changes belong in the open-source
[Beankeeper repository](https://github.com/Govcraft/beankeeper).

## Maintainer pull requests

Read the ratified **Beankeeper - Tenets** note before designing a change.

- **The agent is the interface.** Make new capabilities available through
  both the `bk` CLI and MCP tools. Keep data entry in the agent conversation.
- **Screens are optional.** Answer in plain words with chartable data and a
  link to the matching Desktop view.
- **The books can't be broken.** Encode invariants in Rust types wherever
  possible. Validate untrusted input once at the boundary and carry the
  validated type through the application.

For authorized writes, the agent acts directly and informs the owner afterward.
Give the owner a one-step undo that creates an audited reversal.

Start the submitted pull request body with `Closes #N` and name the applicable
tenets on the `Tenets:` line. If the change affects an invariant, explain what
Rust enforces at compile time, what requires a runtime check, and why.

For Rust changes, run these checks locally and report the results.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo test --workspace --locked --doc
```

See the [CI guide](CI.md) for helper tests and checks selected by changed paths.

Desktop builds against a released core: `beankeeper` and `beankeeper-cli` are pinned to
one `beankeeper-cli-vX.Y.Z` tag. Don't switch them back to `branch = "main"`; to
adopt a newer core release, see [Release packaging](RELEASE.md).
