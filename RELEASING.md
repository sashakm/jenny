# Releasing jenny

Releases are automated with [release-plz](https://release-plz.dev). Configuration lives in
`release-plz.toml`; the pipeline is `.github/workflows/release-plz.yml`.

## How a release happens

1. Every push to `main` opens or updates one **release PR**. It bumps the version, writes
   the new section of `CHANGELOG.md` from the commits since the last tag, and runs
   cargo-semver-checks against the published API.
2. Review the PR: is the bump right, and does the changelog read well? Edit the PR if
   needed.
3. Merge it. The release job publishes every crate to crates.io in dependency order, tags
   `v<version>` and creates the GitHub release.

All nine crates (`jenny`, `jenny-core`, `jenny-macros`, `jenny-rest`, `jenny-server`,
`jenny-messaging`, `jenny-persistence`, `jenny-security`, `cargo-jenny`) share one version,
`[workspace.package] version` in `Cargo.toml`. They are released together, even when only
one of them changed (ADR-002).

## Commit messages decide the version

The version bump and the changelog come from [conventional commits](https://www.conventionalcommits.org).
release-plz reads **only the first line** of each commit:

| Commit | Before 1.0 | From 1.0 |
|---|---|---|
| `fix: …`, `perf: …`, `refactor: …` | patch | patch |
| `feat: …` | patch | minor |
| `feat!: …`, or a `BREAKING CHANGE:` footer | **minor** | **major** |

A breaking change listed on the third line of a multi-line commit message is invisible to
the version calculation, so the release would go out as a patch. Put one change in each
commit, or squash-merge PRs under a conventional title that carries the `!`.

## One-time setup

1. **Let Actions open PRs.** Repository settings → Actions → General → Workflow
   permissions: enable *Allow GitHub Actions to create and approve pull requests*.
2. **Optional: `RELEASE_PLZ_TOKEN`.** PRs opened with the default `GITHUB_TOKEN` do not
   trigger other workflows, so CI does not run on the release PR. A fine-grained PAT (or
   a GitHub App token) with *contents* and *pull requests* write access, stored as the
   `RELEASE_PLZ_TOKEN` secret, fixes that. The workflow falls back to `GITHUB_TOKEN`
   without it.
3. **First publish.** Trusted publishing cannot create a crate, so the first release
   needs a registry token once:
   1. Create a crates.io API token with the *publish-new* and *publish-update* scopes and
      store it as the `CARGO_REGISTRY_TOKEN` repository secret.
   2. Uncomment the `CARGO_REGISTRY_TOKEN` line in the `release` job.
   3. Merge the release PR for `0.1.0`.
4. **Switch to trusted publishing.** For each of the nine crates on crates.io: Settings →
   Trusted Publishing → add GitHub, repository `sashakm/jenny`, workflow
   `release-plz.yml`. Then delete the `CARGO_REGISTRY_TOKEN` secret, comment the line out
   again, and revoke the token. From then on the job authenticates with OIDC
   (`id-token: write`) and no long-lived credential exists.

## Checks before merging a release PR

CI runs these on every PR; locally:

```sh
just check              # fmt, clippy, rustdoc, every feature combination
just test               # doc tests + all tests under coverage
just test-cli           # a scaffolded project builds, tests and lints clean
just test-cli-features  # the same, once per optional feature
just msrv               # rust-version (1.94) still holds
just package            # every crate packages and builds the way crates.io sees it
```

## The topmesys dependency

jenny depends on the published `topmesys`. Its 0.2.1 release changed the consumer API
under a patch number, so it is being re-released as 0.3.0. Once topmesys 0.3.0 is on
crates.io, change `topmesys = "0.2.1"` to `topmesys = "0.3"` in `Cargo.toml` (the API is
identical, no code changes). Only then can 0.2.1 be yanked without breaking fresh
resolution here.
