---
name: update-deps
description: Update jenny's dependencies to their latest releases, including breaking majors, then fix the fallout and keep the CLI templates, MSRV and topmesys in step. Use when asked to update, bump or upgrade dependencies, or when Dependabot raises a major-version PR.
---

# Update jenny's dependencies

Versions live in one place: `[workspace.dependencies]` in the root `Cargo.toml`. Crates
use `{ workspace = true }`.

## 1. topmesys first

If `../topmesys` has unreleased changes jenny needs, they must be released before jenny
can depend on them (see its `RELEASING.md`). For local co-development only, use
`[patch.crates-io] topmesys = { path = "../topmesys" }` and never commit it.

## 2. What is out of date

```sh
sed -n '/^\[workspace.dependencies\]/,$p' Cargo.toml | grep -oE '^[a-z_0-9-]+ =' \
  | sed 's/ =//' | grep -v '^jenny' | xargs bash .claude/skills/update-deps/latest.sh
```

`latest.sh` prints the newest release of each crate. A prerelease (`7.0.0-rc2`) is not a
target; stay on the newest stable. Compare against `Cargo.toml` and list the
semver-incompatible bumps (a new major, or a new minor before 1.0) separately. Those
need code changes.

## 3. Bump

- Raise requirements in `[workspace.dependencies]`.
- Move families together and check what each release depends on
  (`cargo info <crate>@<version>`): `opentelemetry*` with `tracing-opentelemetry`;
  `utoipa` with `utoipa-swagger-ui`; `syn` with `darling`.
- `cargo update`, then `cargo check --workspace --all-features --all-targets` and fix
  each breakage. Read the crate's changelog for every major instead of guessing.
- Delete dependencies nothing uses: grep the crate manifests and the code.

## 4. Keep the CLI templates in step

`crates/cargo-jenny/templates/project/Cargo.toml.tmpl` pins third-party crates for
generated projects. Bump those to the workspace's major too. The
`template_dependencies_track_the_workspace_majors` test fails if they drift.

## 5. MSRV

```sh
python3 .claude/skills/update-deps/msrv_floor.py
```

That prints the highest `rust-version` in the resolved graph. If it is above
`[workspace.package] rust-version`, raise it everywhere it appears: `Cargo.toml`, the
`msrv` recipe in `justfile`, the `msrv` job in `.github/workflows/ci.yml`, and the number
in `CLAUDE.md` and `RELEASING.md`. Then `just msrv` (`rustup toolchain install <ver>
--profile minimal` if missing).

## 6. Verify

Run the full set from the `verify` skill, including `just test-cli-features` and
`just package`: dependency bumps break scaffolded projects and packaging more often than
the workspace itself.

## 7. Commit

`chore(deps): …` for compatible updates. jenny re-exports `axum`, `tower_http`, `http`
types and emits `utoipa` impls, so a major bump of one of those changes jenny's own
public API. Mark that commit breaking: `chore(deps)!: …`.
