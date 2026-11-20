---
name: write-adr
description: Write or update an architecture decision record in docs/adr for jenny. Use when a change introduces, reverses or materially changes a structural decision (a new crate, port, adapter, layering rule, wire format, runtime behaviour or public convention), or when asked to document a decision.
---

# Write an ADR

ADRs live in `docs/adr/ADR-NNN-kebab-title.md`, numbered consecutively. They are why
jenny looks the way it does, so read the related ones before proposing a change that
contradicts them. ADR-001 (layering) and ADR-002 (workspace) underlie everything.

## When an ADR is needed

- A new structural element: a crate, a port in `jenny_core::port`, an adapter type, a
  runtime endpoint, a configuration section.
- A rule other code must follow, such as "filter paths need an allowlist".
- Reversing or narrowing an earlier decision. Update the old ADR's status line
  (`**Status:** Superseded by ADR-NNN`) and leave its body alone.

A bug fix, a refactor that keeps behaviour, or a dependency bump does not need one.

## Shape

Follow the existing files exactly:

```markdown
# ADR-NNN: Title In Title Case

**Status:** Accepted
**Date:** YYYY-MM-DD

## Context

What forces are at play: the problem, the constraints, what makes it non-obvious.

## Decision

What was decided, in subsections (`### …`) per part. Code or diagrams where they carry
the argument. Say what was considered and rejected, and why.

## Consequences

Bullets: what becomes easier, what becomes harder, what is now someone's job.

## Not built yet

Optional. Parts of the decision that are agreed but unimplemented.
```

Status values in use: `Accepted`, `Accepted — implemented`, and
`Accepted — <part> not implemented` when part of it is still open. `Proposed` marks a
roadmap stub: the need is agreed, the design is not. A stub has `## Context`,
`## Proposal` (in place of `## Decision`), `## Open questions` and `## Consequences`.
When it is agreed, `Proposal` becomes `Decision`, its open questions are answered or
moved, and the status changes. Add a new stub to the README's Roadmap table.

## Style

- Explain *why*, with the alternative that lost. "We use X" without "instead of Y,
  because Z" is not a decision record.
- Plain declarative sentences, present tense for what holds now.
- Name the code: types, modules and recipes in backticks, so the ADR can be checked
  against the source.
- Link the ADR from the doc comment of the main type it governs when that helps a
  reader, and from `CLAUDE.md` if it adds a non-negotiable rule.
