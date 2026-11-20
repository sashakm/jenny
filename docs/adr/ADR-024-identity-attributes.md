# ADR-024: Identity Attributes and Where They Come From

**Status:** Proposed
**Date:** 2026-11-20

## Context

A `Principal` is an id and a flat map of JSON attributes, filled by whichever
authenticator recognised the credential: a JWT's claims, an API-key store entry. Cedar
sees them as the principal's attributes, and with a schema only the declared ones
(ADR-011).

What a service can know about a caller depends on how the caller authenticated:

| Credential | What is known |
|---|---|
| API key | what was recorded when an authenticated user created it, and whatever they added since |
| JWT | its claims |
| OIDC / OAuth 2 | the claims, plus `userinfo`, plus token introspection (RFC 7662) for opaque tokens |
| client certificate (ADR-023) | the subject and SANs, plus whatever the issuer will disclose |

Attributes also differ in how far they can be trusted. A signed credential asserted
some, an identity provider returned others over TLS, an admin granted some, and some the
user typed themselves. Today all of them land in the same map, so a policy that reads
`principal.department` cannot tell whether the identity provider said so or the user did.

jenny will not ship an account database. Services still need somewhere to keep what
credentials do not carry, and the implementor decides what that is.

## Proposal

### An attribute schema the implementor configures

Each attribute is declared with:

- its type;
- the sources that may supply it (a claim path, a `userinfo` field, a certificate field,
  the stored profile), in order of precedence;
- conditions on a source (`email` only with `email_verified: true`);
- whether the user may set it;
- whether Cedar sees it.

A sketch:

```yaml
identity:
  attributes:
    email:
      type: string
      from: [{ claim: email, if: email_verified }, { userinfo: email }, { certificate: san.email }]
    department:
      type: string
      from: [{ claim: dept }, { profile: department }]
    display_name:
      type: string
      from: [{ claim: name }, { profile: display_name }]
      user_editable: true
```

Types are Cedar's (string, long, bool, set, record, entity reference), so the schema can
generate the `User` part of the Cedar schema instead of being written twice.

### Provenance a policy can see

Attributes from a credential or an identity provider appear as `principal.<name>`.
Attributes the user set themselves appear under `principal.claimed.<name>`, so a policy
cannot trust what a user typed by accident. A provenance tag on each attribute was
considered. Cedar cannot read metadata about an attribute, so a separate namespace is
the only distinction a policy can actually make.

### Stored attributes go through a port

`Profiles` (name open) in `jenny_core::port` loads and saves a principal's stored
attributes, versioned like any aggregate (ADR-012). jenny ships the port, the schema,
the merge, and an implementation over `Store`, so Memory, Postgres or NATS KV can keep
profiles. An external directory (LDAP, SCIM, an identity provider's admin API) is an
adapter the implementor writes. There are no user tables, no sign-up flow and no
passwords.

### Resolution per request

authenticate → attributes from the credential → enrich from the configured sources →
check against the schema → `Principal`.

- `userinfo` and introspection results are cached per token until it expires.
- Profiles are cached per principal for a configured time.
- An attribute that cannot be fetched is absent, so a policy that needs it denies.
  Nothing serves a stale value past its cache time.

An API key's snapshot (ADR-023) carries provenance `api_key`. Per attribute, the
implementor chooses whether the stored profile is re-read when the key is used, or the
snapshot from minting stands.

## Open questions

- Who writes profiles: the user (for `user_editable` attributes), admins, a sync job?
  This meets ADR-025's delegation.
- Is SCIM provisioning in scope? It is the standard way for an identity provider to
  push users and their attributes before their first login.
- Introspection and `userinfo` are a network call per new token. What happens when the
  identity provider is slow or down?

## Consequences

- Policies can rely on where an attribute came from.
- Authentication may make outbound calls, which needs caching, timeouts and metrics
  (ADR-026).
- The attribute schema becomes the one place that says what a principal can carry.
