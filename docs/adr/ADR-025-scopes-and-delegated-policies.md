# ADR-025: Resource Scopes and Delegated Policies

**Status:** Proposed
**Date:** 2026-11-20

## Context

Users want to organise themselves, admins want policies finer than roles, and API keys
should be restricted to part of what their owner may do (ADR-023). Both point
to scopes: groupings of resources and principals that can be nested (a team inside a
department), associated (one resource in several), and merged. Delegation follows from
them. A user holding a role in a scope may contribute a Cedar policy or attribute
configuration (ADR-024) that applies within it.

What exists (ADR-011):

- Cedar entities come from the request: the resource with its parents, and the principal
  with its roles.
- Static entities, such as a role hierarchy, are loaded once at startup.
- Policies are read from files at startup and never reloaded.
- Policy templates are refused at load, because nothing links them.

## Proposal

### Scopes are Cedar entities with parents

A scope is a `Scope::"<id>"` entity whose parents are the scopes enclosing it. A resource
or principal belongs to a scope by having it as a parent. Cedar's `in` is transitive, so
`resource in Scope::"acme"` covers everything nested under `acme`. An entity may have
several parents, so a scope can be associated with more than one; the hierarchy must
stay acyclic, and the store refuses a cycle.

Scopes are kept through a port, on jenny's stores. Two things are needed first:

- entities and policies that reload without a restart, which ADR-011 lists as not built;
- per-request entity slicing (loading the ancestors of this request's principal and
  resource) once there are too many scopes to hold in memory.

Merging scope A into B re-parents A's members and children onto B, removes A, and
raises an event. The policies that named A then match nothing, so the merge must rewrite
them or refuse to proceed. "The union of A's and B's policies" is never what a merge
means, as it could grant B's members what only A's had.

### Delegation through Cedar templates

A Cedar template has slots, `?principal` and `?resource`. An admin writes the template,
saying what may be granted. A delegate links it, saying to whom and on which scope.
Linking needs no Cedar knowledge and cannot grant more than the template does. Proposal:
jenny accepts templates (ADR-011 refuses them today) and keeps the links through a port.

### Free-form policies, within bounds

A delegate's own Cedar policy is accepted only if all of these hold:

1. It parses and validates in strict mode against the schema.
2. Its head constrains the resource to the delegate's scope (`resource in
   Scope::"theirs"`). Cedar exposes a policy's head constraints, so this is a syntactic
   check. Its `when` conditions can only narrow what the head allows, never widen it.
3. Its principals are limited too, if the scope's configuration says so (open below).
4. It stays within limits on size and number.

Admins' `forbid` policies are the ceiling. In Cedar a `forbid` overrides any `permit`, so
no delegated policy reaches past them. Every addition, change and removal raises an
event, so there is an audit trail.

### Where policies live

A `Policies` port supplies policies and template links: files, as today, and a `Store`
for the delegated ones. The authorizer swaps its policy set atomically when they change.
A set that fails validation is not applied, and the last good one stays, as the API-key
store does with a file it cannot read.

### Templates first, free-form policies last

Delegated free-form Cedar is the riskiest item on the roadmap, because policies are code
and a mistake in one grants access. Templates cover most delegation with almost none of
that risk. Proposal: build scopes and templates first, and free-form policies only when a
use case needs what a template cannot express.

## Open questions

- Is a top-level scope the tenancy boundary? Proposal: yes; a tenant is a root scope, not
  a separate concept.
- May a delegate grant to anyone, or only to principals within their scope?
- Who may read which policies? A delegate who can read the admins' `forbid`s learns what
  is guarded.
- Merging: rewrite policies that name the merged scope, or refuse while any do?

## Consequences

- Policy reload, entity slicing and a policy store become prerequisites.
- Scoped API keys (ADR-023) use scopes as entities without needing them stored: a key
  names its scopes, and the request supplies the resource's parents.
- Authorization depends on stored data, so its availability is the store's (cached, with
  the same stale bounds as ADR-023's revocation list).
