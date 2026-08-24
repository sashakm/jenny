lint-documentation:
  cargo doc --workspace --all-features --no-deps

lint-quick:
  cargo fmt --all --check

clippy:
  cargo clippy --workspace --all-targets --all-features

# Compile each feature combination on its own.
#
# `--all-features` cannot catch a `#[cfg]` that only holds when two features are on
# together — which is exactly how the telemetry backends were broken for anyone who
# enabled one of them alone.
features:
  #!/usr/bin/env bash
  set -euo pipefail
  for f in "" telemetry-otlp telemetry-loki telemetry-prometheus \
           telemetry-otlp,telemetry-prometheus telemetry-loki,telemetry-prometheus \
           telemetry-otlp,telemetry-loki telemetry-otlp,telemetry-loki,telemetry-prometheus; do
    echo "==> jenny-core [${f:-none}]"
    cargo check -p jenny-core --no-default-features ${f:+--features "$f"}
  done
  for f in "" rest server messaging persistence security openapi \
           server,openapi server,swagger-ui server,messaging server,messaging,openapi \
           server,telemetry-otlp server,telemetry-prometheus server,telemetry-loki full; do
    echo "==> jenny [${f:-none}]"
    cargo check -p jenny --no-default-features ${f:+--features "$f"}
  done

lint-ci: lint-documentation lint-quick clippy features

# Alias used by CI (.github/workflows/ci.yml)
check: lint-ci

test-all:
  cargo test --workspace --all-features

doc-tests:
  cargo test --doc --workspace --all-features

# Generate a project with the CLI and prove it builds and passes its own tests.
#
# The scaffolding is only worth having if what it emits actually works, and templates
# are exactly the kind of thing that rots silently.
test-cli:
  #!/usr/bin/env bash
  set -euo pipefail
  root="$(pwd)"
  workdir="$(mktemp -d)"
  trap 'rm -rf "$workdir"' EXIT
  cargo build -p jenny-cli
  "$root/target/debug/cargo-jenny" jenny new scaffolded --resource note \
      --path "$workdir/scaffolded" --jenny-path "$root"
  cd "$workdir/scaffolded"
  cargo test
  "$root/target/debug/cargo-jenny" jenny add resource order
  cargo test
  cargo clippy --all-targets -- -D warnings

ci-test: doc-tests
  @cargo llvm-cov --all-targets --all-features --examples --cobertura --output-path coverage.xml
  @cargo llvm-cov report --json --summary-only | jq -c '.data[].totals.lines'

# Alias used by CI (.github/workflows/ci.yml)
test: ci-test

publish: test-all
  cargo publish
