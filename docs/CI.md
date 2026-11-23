# Continuous integration

CI runs only the checks a change can affect, restores compiler caches that only
`main` writes, and reports one required status, `ci-gate`.

## Workflows

| Workflow | Runs on | Does |
| --- | --- | --- |
| `ci.yml` | pull requests, `main` pushes, manual, daily | Runs selected Rust, service and packaging checks behind `ci-gate`, and checks the latest Beankeeper main on the daily schedule |
| `packaging.yml` | called by `ci.yml` and `release.yml` | Builds, packages and exercises installers for a list of targets |
| `live-install.yml` | pull requests changing the guide, installers, or smoke test, manual | Runs the guide with the checked-out installers and verifies public signed releases on all five native targets |
| `release.yml` | tags, manual, nightly | Packages every target; tags also sign and publish |
| `cache-maintenance.yml` | `main` pushes, nightly, manual | The only writer of compiler caches |

## Change planning

`scripts/ci/plan.py` classifies every changed path with an ordered rule table and
takes the union of what they require. The planning job always runs the Python
helper tests (`scripts/tests` and `scripts/ci/tests`). Examples:

| Change | Checks |
| --- | --- |
| `docs/`, `README.md`, `CHANGELOG.md` | none beyond the Python helper tests |
| `crates/`, `ui/`, `Cargo.lock` | stable, MSRV, latest Beankeeper main and native service checks |
| `install.ps1`, `packaging/windows.iss` | Windows packaging only |
| `scripts/tests/service-lifecycle.py` | service checks and one package per OS |
| `scripts/release.py`, `build.rs` | every release target |

A path no rule recognises selects everything, as do manual runs, force pushes,
an unavailable diff and `[full-ci]` in a pull request's title or body. A new
tracked file must be classified: `test_every_tracked_file_is_classified` fails
otherwise. Dependency changes do not package on pull requests; the nightly
`release.yml` run packages every target from `main`.

`ci-gate` fails on any failed or cancelled job, and on any job that was skipped
or ran contrary to the plan, so it is the only check branch protection needs.

## Reusing validation

When `ci-gate` passes, it uploads a `ci-evidence` artifact recording the
validated source tree and the checks that ran. On a `main` push,
`scripts/ci/evidence.py` looks for a successful same-repository pull request
run whose head commit has exactly the pushed tree and whose evidence includes
every check `main`'s plan needs. If one exists, the push runs only planning and
the gate. This applies when a pull request was up to date with `main` at merge;
otherwise `main` validates normally. Force pushes never reuse evidence.

The nightly `release.yml` run likewise skips packaging when an earlier
scheduled or manual release run already qualified the same commit.

## Action pinning

Third-party actions and container images are pinned to a commit SHA or image
digest, with the version in a comment; GitHub's own `actions/*` keep version
tags. `test_third_party_actions_are_pinned_to_immutable_revisions` enforces it.

## Caches

`scripts/ci/cache.py` names each cache family (for example `dev-linux`,
`msrv-linux`, `release-x86_64-pc-windows-msvc`) and derives its key from the
manifests, `Cargo.lock`, the compiler and the warm recipe, never from source.
Validation jobs restore through `.github/actions/rust-setup` and never save, so
pull requests cannot evict `main`'s caches. `cache-maintenance.yml` warms only
the families whose key is missing on `main`, trims workspace artifacts and
executables, then deletes the entries it replaced. Builds share the settings in
`CARGO_ENV` (no incremental state or debug info), which keep caches small.

## Compiler upgrades

`rust-toolchain.toml` pins the exact compiler for CI and local builds. To
upgrade, change its `channel`; the next `main` push warms new caches. The MSRV
job follows `rust-version` in `Cargo.toml`.

## Running the helpers locally

```sh
python3 -B -m unittest discover -s scripts/ci/tests -v
python3 scripts/ci/plan.py --paths install.ps1 crates/beankeeper-desktop/src/ui.rs
ruff check scripts/ci && ruff format --check scripts/ci
```
