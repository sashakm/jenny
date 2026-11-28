# ADR-031: Authenticating to NATS, mTLS First

**Status:** Accepted — implemented
**Date:** 2026-11-25

## Context

jenny opens connections to NATS in two places, and neither can authenticate:

- `Nats`, the handle behind the key-value stores (ADR-017), which the runtime builds from
  `messaging.nats.url`;
- `NatsConfig::create_client`, which a service calls to get a client for `NatsGateway`
  (ADR-006), configured by a `url`, a `stream` and a `consumer` of its own.

Both call `async_nats::connect(servers)` with the default options. A NATS that serves
more than one team accepts no anonymous clients, so a hardened deployment cannot connect
at all. Postgres already can, because sqlx reads `sslmode`, `sslrootcert`, `sslcert` and
`sslkey` from the URL. Object storage can too, through the standard `AWS_*` variables.

A NATS server authenticates a client by one of these:

| Mechanism | What the client holds | Typical deployment |
|---|---|---|
| TLS client certificate | a certificate and key; the server maps its subject or SAN to a user (`verify_and_map`) | Kubernetes with cert-manager, a mesh, SPIFFE |
| decentralised JWT | a `.creds` file: a user JWT and its NKey seed | operator mode, Synadia Cloud, multi-tenant clusters |
| NKey | an Ed25519 seed; the server challenges with a nonce | static users without passwords |
| user and password | both | small installations |
| token | one shared secret | development |

TLS client certificates come first because they need no shared secret. cert-manager
issues and rotates them, and the same certificate can identify the service to the mesh
and to Postgres. async-nats 0.53 supports every mechanism in the table. It reads a client
certificate, its key and the CA file from disk again at each TLS handshake, so a rotated
certificate is used from the next reconnect on. It reads a `.creds` file once, when the
options are built.

ADR-023 covers mTLS too, but in the other direction: callers proving who they are to
jenny. This ADR covers jenny proving who it is to NATS. The two share no code: ADR-023
lives in `jenny-security` and the runtime, this one in `jenny-messaging`, and no adapter
crate depends on another (ADR-002).

## Decision

### One connection, configured once

`messaging.nats` changes from a single URL into the connection's own section. The runtime
builds one client from it, and the key-value stores and the gateway share that client.
`NatsConfig` is removed. Its `stream` belongs to the gateway, and its `consumer` to
inbound dispatch, configured under `messaging.inbound` (ADR-006). Before this, a service that used both
opened two connections, and only one of them came from configuration.

The gateway is built from the `Nats` handle that `JennyServer::nats()` returns:
`nats.gateway(stream, subscription)`. Wiring is synchronous and the connection is
opened in `bind`, so there is no connected `Client` to hand over while wiring; the
gateway takes the shared client on its first publish. `NatsGateway::new(Client, …)`
stays, for a service that brings a client of its own.

The section is `NatsConnectionConfig` in `jenny-messaging`
(`jenny_messaging::connection`), as `PostgresConfig` lives with its adapter, and
`jenny-server`'s `messaging.nats` holds it. `NatsConnectionConfig::connect_options`
turns it into `async_nats::ConnectOptions`, a pure function the tests check without a
server.

```yaml
messaging:
  nats:
    url: tls://nats-1:4222,tls://nats-2:4222
    refresh_interval: 30              # seconds between looks at the files below
    tls:
      ca: /etc/nats-tls/ca.crt        # trust anchors; default: the system's roots
      cert: /etc/nats-tls/tls.crt     # client certificate, for mTLS
      key: /etc/nats-tls/tls.key
      handshake_first: false          # for a server with `handshake_first`
      server_name: nats.internal      # optional, when the URL names an IP address
    auth:                             # at most one of:
      creds: /etc/nats/service.creds  # user JWT + NKey seed
      # nkey: …                       # seed; JENNY__MESSAGING__NATS__AUTH__NKEY
      # user: notes
      # password: …                   # JENNY__MESSAGING__NATS__AUTH__PASSWORD
      # token: …                      # JENNY__MESSAGING__NATS__AUTH__TOKEN
```

`url` stays where it is, so existing configuration keeps working.

### TLS and authentication are separate

`tls` says how the connection is secured. `auth` says who the service claims to be, on
top of that. A certificate alone is enough when the server maps it to a user, which is
the common mTLS setup, and a `.creds` file over TLS with a server certificate only is
the common operator-mode setup. Validation at startup:

- `cert` without `key`, or `key` without `cert`, is an error;
- more than one entry under `auth` is an error, because a server uses only one, and a
  silently ignored secret is a bug that is hard to find;
- `user` without `password`, or `password` without `user`, is an error; the two count
  as one entry;
- every URL in the list must parse, and there must be one;
- `refresh_interval` is at least one second;
- `tls` asks for TLS on every server in the list and on every reconnect, even when a URL
  says `nats://`. A `tls` section that is present but empty does too, for a server whose
  certificate the system's roots already trust.

`ConfigSource::server_config` validates the section when the configuration is loaded,
and `Nats` again before it connects, for a configuration built in code. Every mistake is
reported at once, naming the keys at fault.

The secrets (`nkey`, `password`, `token`) are redacted in `Debug`, as
`BasicAuthConfig`'s password is, because the configuration is logged at startup. They
come from environment variables like every other secret (`JENNY__…`). Certificates,
keys and `.creds` are paths, because Kubernetes mounts secrets as files.

### Rotation forces a reconnect

Certificates are passed to async-nats as paths, never loaded by jenny first, so the
client reads them again at each handshake. A `.creds` file is read through
`with_auth_callback` instead of `credentials_file`, so it is also read again at each
connect. Every reconnect therefore uses the files as they are now.

An open connection keeps the credentials it was opened with until it reconnects, and
nothing makes it reconnect until a server restarts or the network fails. So jenny
watches the files and calls `Client::force_reconnect` when they change:

- It watches the way the API-key store already does (`ApiKeyAuthContextConfig`): a task
  (`jenny_messaging::rotation`), started once the connection is made,
  polls each file's modification time and length on an interval
  (`messaging.nats.refresh_interval`, in seconds, as for API keys; default 30). The metadata follows
  symlinks, so it also sees Kubernetes swapping a secret mount's `..data` link.
- It reconnects only when the new files are complete: the certificate and key parse and
  belong together (rustls' `CertifiedKey`, already in the tree through async-nats), the
  CA parses, and the `.creds` file holds a JWT and a seed. cert-manager writes the
  certificate and the key separately, and a reconnect between the two writes would fail
  its handshake. Files that do not pass are logged and checked again on the next poll,
  and the open connection stays as it is.
- A forced reconnect is a short disconnect, as when a server restarts. The client
  buffers publishes through it.

### An escape hatch: `ConnectOptions`

The section covers credentials kept in files and environment variables. For credentials
from somewhere else, such as Vault, a cloud secret manager or a sidecar's socket, a
service can adjust the options itself:

```rust,ignore
server.nats_options(move |options: ConnectOptions| {
    ConnectOptions::with_auth_callback(|nonce| vault::nats_auth(nonce))
        .require_tls(true)
        .add_client_certificate(cert.clone(), key.clone())
})?;
let nats = server.nats();
```

The hook receives the options built from the section and returns the ones to connect
with. It may extend them or replace them. Replacing them is the only way to set an auth
callback, because async-nats offers that only as a constructor. It must be called before
`nats()`. Calling it afterwards is a wiring error, in the same way that
`accept_json_patch()` must precede `mount()`. File watching applies only to files that
the section names. The hook is an `Fn`, run again before each attempt at the first
connection, because a failed attempt consumes the options it was given.

### Failures are loud

The first connect used to happen in `bind`, and only when a bucket was registered
(ADR-017). With one shared connection, `bind` connects whenever `messaging.nats.url` is
set, even if `nats()` was never called, and registers the `nats` readiness check. An
authentication or authorisation failure stops the service there with the server's
reason (a TLS alert such as `CertificateRequired` or `UnknownCA`, or the server's
`authorization violation`), rather than starting with a broker it cannot use. Later, the `nats` readiness
check reports anything but `Connected` as not ready, which includes a connection that
has lost its authorisation and is trying again.

At shutdown, `Nats::close` stops the watcher and drains the connection after the broker
has stopped, since its listeners (a gateway among them) may still be publishing; the
order is HTTP drain, outbox relay, broker, NATS, Postgres pool.

### The scaffold's development NATS stays plain

The scaffold's `compose.yaml` (ADR-009) runs NATS without TLS, and a generated project
gets no certificates: generating and trusting a CA is more than a first `just up`
should ask for. Its `config.yaml` shows the `tls` and `auth` keys, commented out. TLS
is exercised by jenny's own tests instead: `scripts/nats-test-certs.sh` writes a CA,
a server certificate, two client certificates mapped to one user and one from another
CA, and `just test-nats` and CI start a second NATS with `verify_and_map` from them.

### Alternatives considered

- **Only the hook, with no configuration.** Every service would write the same code,
  and neither the scaffold nor the Helm chart (ADR-027) could set it. The hook is kept for
  what the section cannot express.
- **Credentials in the URL.** That covers user and password, but no certificates, and it
  puts a secret in a value that is easy to log.
- **Loading the certificates in jenny** and handing async-nats a `rustls::ClientConfig`.
  async-nats would then keep that config across reconnects, and jenny would need a
  certificate resolver of its own. Passing paths lets async-nats do the loading, and
  jenny only decides when to reconnect.
- **Waiting for the next natural reconnect.** A connection can stay up for weeks, longer
  than a short-lived certificate is valid, so a rotation would only take effect at the
  next network failure.
- **A second, named connection**, so that the gateway could publish under a NATS account
  with different permissions from the account that holds the key-value buckets. One NATS
  user with both sets of permissions is simpler. A service that really needs two accounts
  can build a client of its own and pass it to `NatsGateway::new`.

## Consequences

- A service can reach a NATS that refuses anonymous clients.
- `NatsConfig` is removed, and the gateway takes the runtime's connection: breaking.
  `Nats::new` takes a `NatsConnectionConfig` instead of a URL, and `NatsGateway::ctx`
  is async, because a gateway from the `Nats` handle connects on first use.
- `async_nats::ConnectOptions` becomes part of `jenny-server`'s public API through
  `nats_options`, as `async_nats::Client` already is of `jenny-messaging`'s, so a new
  async-nats major is a breaking change.
- The runtime runs one more background task, the file watcher. It stops with the
  connection.
- With NATS permissions per user, only this service can publish to its subjects. That
  makes headers on its events as trustworthy as the account that publishes them, which
  ADR-032 relies on.
- `just test-nats` needs a second NATS, with TLS and `verify_and_map`, and certificates
  generated for the test (`JENNY_TEST_NATS_TLS_URL`, `JENNY_TEST_NATS_TLS_DIR`); CI
  starts it the same way.
- The scaffold's configuration template gains a commented `tls`/`auth` block.

## Not built yet

- Secrets set as values (`nkey`, `password`, `token`) come from the environment at
  startup and change only with a restart; only files rotate.
- Files a `nats_options` hook reads are not watched; a hook that needs rotation forces
  its own reconnect.
