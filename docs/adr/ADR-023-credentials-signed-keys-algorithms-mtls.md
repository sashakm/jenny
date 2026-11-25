# ADR-023: Credentials — Signed API Keys, Key Algorithms and mTLS

**Status:** Proposed
**Date:** 2026-11-20

## Context

`jenny-security` verifies two kinds of credential: JWTs against a JWKS
(`JwtAuthContextConfig`), and API keys looked up by their SHA-256 in a store that is
loaded from a file and re-read when it changes (`ApiKeyAuthContextConfig`). Three
things are missing:

- **API keys depend on their store.** Every request looks its key up. With the store in
  a database, authentication becomes exactly as available as that database. A key that
  carries its own proof verifies against a public key held in memory.
- **There is no algorithm policy.** `verify_token` accepts any key the JWKS serves, with
  the algorithm the token's header names, unless the JWK declares one.
  `DecodingKey::from_jwk` accepts symmetric `oct` keys, so a JWKS containing one turns a
  published document into a shared secret. RSA moduli are not checked. `jsonwebtoken` 12
  itself refuses `none`, has no DSA, and refuses an algorithm from another key family.
  It offers HS256/384/512, RS256/384/512, PS256/384/512, ES256, ES384 and EdDSA
  (Ed25519).
- **There are no certificates.** The server does not terminate TLS at all.

## Proposal

### One algorithm policy, for what jenny mints and what it verifies

| Family | Accepted | Refused |
|---|---|---|
| EdDSA | Ed25519 | — |
| ECDSA | P-256 (ES256), P-384 (ES384) | other curves |
| RSA | PS256/384/512, RS256/384/512, modulus ≥ 2048 bits | shorter moduli |
| HMAC | — | `oct` keys in a fetched JWKS |
| DSA, `none` | — | always |

The policy is applied when a JWKS is loaded and when a token is verified. A key outside
it is dropped with a warning, so one bad key in an issuer's set does not take
authentication down. A token's `alg` must be in the policy and match its key.

**2048 bits for RSA**, because NIST SP 800-57 accepts 2048-bit RSA through 2030. The big
identity providers commonly sign with keys of that size: Google, Microsoft Entra ID, and
Keycloak's default realm keys, for instance. A higher floor would refuse their tokens.
`min_rsa_bits` can be raised, and the default is revisited before 2030. The keys jenny
mints default to Ed25519.

### Signed API keys

A signed key is a token minted by the service. It is a JWS under a signing key's `kid`,
carrying:

- the key's own id (`jti`);
- its owner;
- when it was issued and when it expires;
- its scopes (below);
- a snapshot of the owner's attributes at minting (ADR-024).

Verifying it takes a public key in memory and no lookup, so authentication works
without a key store, or through an outage of one. Hashed keys stay: they are simpler, and
can be revoked at once.

### A key carries its scopes

Implementors and operators will want keys that can do less than their owner can: one
for a CI job that may only publish to one project, one for a dashboard that may only
read. Every store is optional, so the restriction has to travel in the key itself.

A key lists the scopes it is good for (ADR-025), and optionally the actions. What it
may do is what its owner may do, within those scopes. `CedarAuthorizer` enforces this
with a `forbid` of its own, which no policy of the service's can override:

```cedar
forbid (principal, action, resource)
when { context.credential has scopes }
unless { resource in context.credential.scopes };
```

The scopes are entity references, and `in` follows the resource's parents, which the
request supplies. So a scoped key works without a store of scopes, too. Letting a holder
narrow a key further, offline, is what Biscuit tokens do; that stays open (below).

### Minting, rotation and revocation

Signing keys come from configuration (a PEM, through a secret). They are never generated
at startup, because two replicas would generate different ones. The public keys are
published as a JWKS, so other services can verify what this one mints.

Keys are minted by a use case behind an authenticated endpoint, and by a subcommand of
the service's own binary (`notes keys mint --owner … --scope … --expires …`). The
subcommand needs only the signing key, so an operator can mint a key with no store and
no running service.

A signed key is good until it expires unless something says otherwise:

- **Without a store**, revocation is expiry, plus retiring a signing key's `kid`, which
  revokes every key it signed. Expiries should be short, and rotation routine: an old
  `kid` is accepted until the last key it signed has expired.
- **With a store** (Postgres or NATS KV, through a `Store`), there is also a revocation
  list of `jti`s, cached in every replica and refreshed on an interval. A configured
  bound says how stale that cache may get, and whether to fail open or closed beyond it.

Alternatives considered:

- **PASETO v4.public**: Ed25519 only, so no algorithm choice at all. JWS was preferred
  because the JWKS machinery and the policy exist anyway.
- **Biscuit**: a holder can derive a narrower key offline, without asking the service.
  It is attractive once keys are passed down a chain of tools, and heavier. Kept open.
- **HTTP Message Signatures (RFC 9421)**: the client keeps a private key and signs each
  request. Strongest, and it needs a signing library in every client. Later, if at all.

### mTLS

The server learns to terminate TLS (rustls): `server.tls.cert`, `key`, and `client_ca`,
with `client_auth: required | optional`. A verified chain becomes a new
`Credentials::Certificate`, and its subject, SANs (DNS, email, URI, so SPIFFE ids),
issuer, serial and validity become attributes (ADR-024). Certificates are reloaded when
the files change, because cert-manager rotates them. CRLs are supported by rustls'
client verifier, and short-lived certificates make revocation matter less.

TLS often ends at the ingress or the mesh instead, which forwards the client certificate
in a header (`x-forwarded-client-cert` from Envoy and Istio, `ssl-client-cert` from
ingress-nginx). That header can only be trusted from that hop. Proposal: off by default;
when on, it is accepted only from configured proxy addresses and refused from anyone
else.

TLS also gives gRPC its ALPN (ADR-021) and the management listener a way to require
client certificates (ADR-020).

This section is about callers presenting certificates to the service. The service
presenting its own certificate to NATS is the other direction, and is covered by ADR-031.

## Open questions

- With an external proxy, trust the forwarded certificate header, or require TLS
  passthrough to the service?
- Past the stale bound, fail open or closed?
- May a scoped key mint keys itself, within its own scopes?

## Consequences

- Refusing `oct` keys from a fetched JWKS and checking RSA sizes is a hardening that
  stands on its own, and can land before the rest.
- The runtime gains TLS configuration and certificate reloading.
- Minting needs a signing key in every deployment that mints, and a rotation procedure.
- Scoped keys need ADR-025's scopes as entities, though not a store for them.
