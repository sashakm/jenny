# print this overview
@default:
  just --list

# Broken intra-doc links fail the build: docs.rs is the first thing users read.
[doc('build rustdoc for every crate, warnings as errors')]
lint-documentation:
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

[doc('check formatting')]
lint-quick:
  cargo fmt --all --check

[doc('run clippy on every target and feature, warnings as errors')]
clippy:
  cargo clippy --workspace --all-targets --all-features -- -D warnings

# Compile each feature combination on its own.
#
# `--all-features` cannot catch a `#[cfg]` that only holds when two features are on
# together — which is exactly how the telemetry backends were broken for anyone who
# enabled one of them alone.
[doc('compile every supported feature combination on its own')]
features:
  #!/usr/bin/env bash
  set -euo pipefail
  for f in "" telemetry-otlp telemetry-loki telemetry-prometheus \
           telemetry-otlp,telemetry-prometheus telemetry-loki,telemetry-prometheus \
           telemetry-otlp,telemetry-loki telemetry-otlp,telemetry-loki,telemetry-prometheus \
           persistence persistence,messaging; do
    echo "==> jenny-server [${f:-none}]"
    cargo check -p jenny-server --no-default-features ${f:+--features "$f"}
  done
  for f in "" rest server messaging persistence security openapi \
           server,openapi server,swagger-ui server,messaging server,messaging,openapi \
           server,persistence server,telemetry-otlp server,telemetry-prometheus \
           server,telemetry-loki full; do
    echo "==> jenny [${f:-none}]"
    cargo check -p jenny --no-default-features ${f:+--features "$f"}
  done

[doc('all lints, as CI runs them')]
lint-ci: lint-documentation lint-quick clippy features

[doc('alias of lint-ci, used by CI')]
check: lint-ci

[doc('run every test with all features')]
test-all:
  cargo test --workspace --all-features

[doc('run the doc tests only')]
doc-tests:
  cargo test --doc --workspace --all-features

# Generate a project with the CLI and prove it builds and passes its own tests.
#
# The scaffolding is only worth having if what it emits actually works, and templates
# are exactly the kind of thing that rots silently.
[doc('scaffold a default project and prove it builds, tests and lints clean')]
test-cli:
  #!/usr/bin/env bash
  set -euo pipefail
  root="$(pwd)"
  workdir="$(mktemp -d)"
  trap 'rm -rf "$workdir"' EXIT
  cargo build -p cargo-jenny
  "$root/target/debug/cargo-jenny" jenny new scaffolded --resource note \
      --path "$workdir/scaffolded" --jenny-path "$root"
  cd "$workdir/scaffolded"
  cargo test
  "$root/target/debug/cargo-jenny" jenny add resource order
  cargo test
  cargo clippy --all-targets -- -D warnings

# Scaffold one project per optional feature and prove each builds, tests and lints clean.
#
# `test-cli` covers the default features only, which is how scaffolded `openapi`
# projects shipped without the `utoipa` dependency their own DTOs need.
[doc('scaffold one project per optional feature, same checks as test-cli')]
test-cli-features:
  #!/usr/bin/env bash
  set -euo pipefail
  root="$(pwd)"
  workdir="$(mktemp -d)"
  trap 'rm -rf "$workdir"' EXIT
  cargo build -p cargo-jenny
  # One target dir for every generated project, so dependencies build once.
  export CARGO_TARGET_DIR="$workdir/target"
  for features in persistence messaging security openapi swagger-ui telemetry \
                  persistence,messaging,security,swagger-ui,telemetry; do
    name="svc-${features//,/-}"
    echo "==> cargo jenny new --features $features"
    "$root/target/debug/cargo-jenny" jenny new "$name" --features "$features" \
        --path "$workdir/$name" --jenny-path "$root"
    (cd "$workdir/$name" && cargo test && cargo clippy --all-targets -- -D warnings)
  done

# The database tests read `JENNY_TEST_POSTGRES_URL` and skip without it (fail, under
# CI). This gives them one: a disposable Postgres 18 on a free port, removed on exit.
[doc('run the Postgres tests against a throwaway database (needs docker)')]
test-postgres:
  #!/usr/bin/env bash
  set -euo pipefail
  name="jenny-test-postgres-$$"
  docker run -d --rm --name "$name" -e POSTGRES_USER=jenny -e POSTGRES_PASSWORD=jenny \
      -e POSTGRES_DB=jenny -p 127.0.0.1::5432 postgres:18 >/dev/null
  trap 'docker stop "$name" >/dev/null' EXIT
  port="$(docker port "$name" 5432/tcp | head -n1 | sed 's/.*://')"
  # Over TCP: the image's first server, during initdb, listens on the socket only.
  for _ in $(seq 1 60); do
    docker exec "$name" pg_isready -h 127.0.0.1 -U jenny -q && break
    sleep 1
  done
  export JENNY_TEST_POSTGRES_URL="postgres://jenny:jenny@127.0.0.1:${port}/jenny"
  cargo test -p jenny-persistence --test postgres_test
  cargo test -p jenny-server --features persistence --lib -- postgres configured_pool
  # The example's end-to-end suite, with its notes in Postgres instead of memory.
  JENNY__PERSISTENCE__POSTGRES__URL="$JENNY_TEST_POSTGRES_URL" \
      cargo test -p jenny --features server,persistence --example basic

[doc('doc tests, then all tests under coverage (writes coverage.xml)')]
ci-test: doc-tests
  @cargo llvm-cov --all-targets --all-features --examples --cobertura --output-path coverage.xml
  @cargo llvm-cov report --json --summary-only | jq -c '.data[].totals.lines'

[doc('alias of ci-test, used by CI')]
test: ci-test

# Package every publishable crate and build each tarball against the others, the way
# crates.io will see them. Nothing is uploaded; release-plz publishes (release-plz.toml).
#
# The tarballs are verified through a temporary local registry, which cargo extracts
# into `registry/src/-<hash>/` once per crate *version*. Without a version bump a second
# run would build against the first run's sources, so those extractions are cleared.
#
# The compiled crates have the same problem: cargo trusts a registry crate's version and
# never rechecks its sources, so a jenny-core built by the last run is reused even when
# its tarball changed. Verification therefore builds in its own target dir, with every
# jenny artifact cleared first; third-party dependencies stay cached.
[doc('package and verify every publishable crate without uploading')]
package *args:
  rm -rf "${CARGO_HOME:-$HOME/.cargo}"/registry/src/-*/jenny-* "${CARGO_HOME:-$HOME/.cargo}"/registry/src/-*/cargo-jenny-*
  rm -rf target/package-verify/debug/.fingerprint/*jenny* target/package-verify/debug/deps/*jenny* target/package-verify/debug/build/*jenny*
  cargo package --workspace --target-dir target/package-verify {{args}}

# Build against the declared `rust-version` (needs `rustup toolchain install 1.94`).
[doc('check the workspace on the declared rust-version (1.94)')]
msrv:
  cargo +1.94 check --workspace --all-features --all-targets --locked
