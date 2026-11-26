Outside code contributions to Beankeeper Desktop are currently closed. See the
[contribution guide](https://github.com/Govcraft/beankeeper-desktop/blob/main/docs/CONTRIBUTING.md).

Closes #N

<!-- Remove the notice above and replace N with the issue number before submitting. -->

Tenets: <!-- Name the tenets this change touches. -->

## Change and design

<!-- Summarize the change and explain design decisions. For applicable invariants, say what Rust enforces at compile time, what is checked at runtime, and why. -->

## Verification

<!-- List the checks you ran and their results. -->

- [ ] Local CI checks passed.

For a PM release PR:

- [ ] Stable release: commit a fresh issuer `export-revocations` output as `packaging/revocations.json` and run `python3 scripts/release.py revocations` (not required for RC releases). See [release preparation](../docs/RELEASE.md#preparing-a-release-pr).
