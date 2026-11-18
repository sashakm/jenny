---
name: verify
description: Run the right jenny quality gates for the current change (fmt, clippy, rustdoc, feature combinations, tests, scaffolding, MSRV, packaging) and report the results. Use before declaring any code change in this repo done, before committing, and whenever asked to "run the tests" or "make CI green".
---

# Verify a jenny change

Pick the gates from what the change touched, run them, and report only what the output
shows. Never call something green without having seen it pass in this session.

## 1. Find what changed

```sh
git status --short && git diff --stat
```

## 2. Choose the gates

Always:

```sh
cargo fmt --all
just check       # fmt --check, clippy -D warnings, rustdoc -D warnings, `just features`
just test-all    # every test with all features
```

Then add whatever the change touched:

| Touched | Also run | Why |
|---|---|---|
| `crates/cargo-jenny/templates/**` or `crates/cargo-jenny/src/**` | `just test-cli` | templates are only checked by generating from them |
| a template inside `{{#if <feature>}}`, or the feature list in `scaffold.rs` | `just test-cli-features` | `test-cli` generates the default features only |
| any `Cargo.toml` (dependency, version, feature) | `just package`, `just msrv` | crates.io packaging and the declared `rust-version` |
| a new cargo feature or `#[cfg(feature = …)]` | add it to the lists in the `features` recipe | `--all-features` hides combination bugs |
| `examples/basic/**` | `cargo test --example basic --features server` | the example's end-to-end test boots the real composition root |
| `crates/jenny-persistence/**`, the runtime's `persistence` wiring, or a store's contract in `jenny-core` | `just test-postgres` | database tests skip without `JENNY_TEST_POSTGRES_URL`, so `test-all` never ran them |
| public API of any crate | `just package`; mark the commit breaking (`!`) if anything was removed or changed | release-plz versions from the commit header |

`just test` is what CI runs (doc tests, then everything under llvm-cov); use it when
coverage matters or to reproduce a CI failure exactly.

Known noise: `jenny-security`'s JWKS cache test fetches from googleapis.com and fails
without network. Say so rather than retrying it.

The slow gates (`test-cli`, `test-cli-features`, `package`) build whole projects under
`/tmp`, which has a per-user quota. If one fails with "Disk quota exceeded", rerun it with
`TMPDIR=$PWD/target/tmp`; that is the environment, not the change.

## 3. Fix, then rerun

On a failure, read the first error, fix the cause, and rerun the failing gate before the
full set again. Do not silence a lint with `#[allow]` without saying why in a comment,
and do not mark a test `#[ignore]` to get green.

## 4. Report

One line per gate that ran, pass or fail, with the test counts from the output (for
example `just test-all: 299 passed, 5 ignored`). List any gate the change needed but
that was skipped, and why.
