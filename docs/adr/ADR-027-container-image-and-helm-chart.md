# ADR-027: A Container Image and a Helm Chart from the Scaffold

**Status:** Proposed
**Date:** 2026-11-20

## Context

`cargo jenny new` writes a `Dockerfile`: a build in `rust:1-slim`, then a distroless
runtime as a non-root user, exposing port 8000. It also writes a `compose.yaml` with
the local dependencies the features need: Postgres, AGE, NATS and RustFS (ADR-009). There
is nothing for Kubernetes.

Implementors start from very different places. Some have a cluster with Postgres, an
observability stack and S3 already running, and only need the service deployed. Others
want to roll everything: Postgres, NATS, S3, and an observability stack of Loki, Tempo,
Prometheus, Grafana and Alloy.

## Proposal

### The container image

- **Dependency caching with `cargo-chef`**, replacing today's trick of building with
  stub sources. That trick breaks as soon as a project is a workspace (ADR-028's
  `frontend/`) or has a `build.rs`.
- **Distroless `cc`**, as today, non-root, and compatible with a read-only root
  filesystem: nothing is written outside a mounted temp directory.
- **Configuration** from a mounted file and the environment. No secret is baked into the
  image.
- **Two ports**, API and management (ADR-020).
- **Multi-arch builds** (amd64, arm64) and an SBOM, from a CI template the scaffold
  writes. Kept open.

The file name is open. Podman and Buildah read a `Containerfile`; with Docker,
`-f Containerfile` is the safe spelling.

### A chart for the service, and only the service

`deploy/helm/<name>/` contains:

- a Deployment with:
  - probes on the management port;
  - a security context: non-root, a read-only root filesystem, all capabilities dropped,
    `RuntimeDefault` seccomp;
  - resource requests;
- a Service with both ports;
- an `HTTPRoute` (Gateway API) or an `Ingress` for the API port only;
- a `ServiceMonitor` for `/j/metrics`, rendered only when the Prometheus Operator's CRDs
  exist;
- a PodDisruptionBudget, an optional HorizontalPodAutoscaler, a NetworkPolicy and a
  ServiceAccount;
- a ConfigMap from `config.yaml`, with Secrets referenced by name (`existingSecret`)
  rather than created by the chart.

`values.yaml` maps onto jenny's configuration sections: the database URL from a secret,
the NATS URL, `objects.url`, the OTLP endpoint.

### Rolling everything: a platform chart, built from upstream charts

Postgres, NATS, S3 and the observability stack do not belong in each service's chart.
Many services share them, they have lifecycles of their own (upgrades, backups,
retention), and upstream projects maintain charts for them. Proposal: `cargo jenny add
platform` writes an umbrella chart, `deploy/platform/`, whose dependencies are upstream
charts, each switched by `enabled:`:

- CloudNativePG for Postgres (with an AGE image when `graph` is on);
- the NATS chart, with JetStream;
- an S3 server;
- Grafana's charts for Loki, Tempo and Alloy;
- kube-prometheus-stack for Prometheus, Grafana and the operator.

The service chart's values then point at the platform. With existing infrastructure,
nobody installs the platform chart, and the values hold the URLs.

Bitnami's charts and images, the usual shortcut, stopped being a dependable free default
in 2025. MinIO's community images are gone too, which is why the tests already moved to
RustFS (ADR-018). The platform chart therefore names neither.

### Templates that contain templates

The scaffold's renderer fails on any `{{…}}` it does not know (ADR-009), and every Helm
template is full of `{{ .Values… }}`. Chart templates must therefore be copied verbatim,
with only `Chart.yaml` and `values.yaml` rendered, or the renderer needs a raw block.
Proposal: verbatim, as it keeps the renderer as small as ADR-009 wants it.

### Tested like the rest of the scaffold

`just test-cli` runs `helm lint` and `helm template` on the generated chart and checks
the output against Kubernetes schemas (`kubeconform`). An end-to-end install into a
`kind` cluster is a slow gate of its own.

## Open questions

- `Containerfile` or `Dockerfile`?
- Which S3 server goes in the platform chart: RustFS, which the tests use, or Garage?
- `HTTPRoute`, `Ingress`, or both behind a switch?
- Who bumps upstream chart versions in a generated project? Proposal: the scaffold writes
  a Renovate or Dependabot configuration.
- Is the platform chart for production, or a starting point for development and staging
  clusters? Backups and retention would make it a product of its own.

## Consequences

- `cargo jenny new` produces something deployable to a cluster, not just to a laptop.
- The scaffold depends on `helm` (and `kubeconform`) in its own tests.
- Upstream chart versions become something a generated project has to maintain.
