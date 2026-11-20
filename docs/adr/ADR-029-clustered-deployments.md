# ADR-029: Clustered Deployments

**Status:** Proposed — milestone 3.0, except for leases, which are day-2 work
**Date:** 2026-11-20

## Context

The goal is jenny instances deployed as a cluster. That means replicas of one service,
and ultimately several different services cooperating. The aims:

- fault tolerance;
- integrity assertion;
- distribution and availability;
- access scenarios more complex than one service behind one load balancer.

For a start, the service's robustness and the replication of its state matter most.

Today a jenny service keeps no state between requests. State lives in Postgres, NATS and
S3, and replicas scale out behind a load balancer, coordinating only through those:

- a save is a compare-and-swap (ADR-012);
- outbox relays share the work through `FOR UPDATE SKIP LOCKED` (ADR-016).

Some things do not hold across replicas today:

- The in-process bus delivers in the replica that published (ADR-006).
- Every replica runs every scheduled task, so a nightly job with three replicas runs
  three times (ADR-007).
- The API-key file, the JWKS, and later the policies, scopes and revocation lists
  (ADR-023 to ADR-025), are cached per replica, and each finds out about changes on its
  own schedule.

Every storage backend is a feature, off by default (ADR-013). A deployment without
Postgres or NATS therefore has nowhere to keep the framework's own state: revocations,
scopes, delegated policies, installed components (ADR-030). That state is small, changes
rarely, and must be the same on every replica. It is exactly what a replicated state
machine is for.

## Proposal

### Raft replicates the framework's state first, behind `Store`

Raft (`openraft`, say) replicates a log among the instances of one service, with
snapshots. The first state machine on it is a `Store` implementation like Memory,
Postgres and NATS KV, so replicated state is one more storage option, not a parallel
API:

- a save is a compare-and-swap, linearised through the leader;
- reads come from the local replica, or from the leader when they must be current.

The framework keeps its own state there when nothing else is configured, and an
implementor can keep small aggregates there too.

Making it a general-purpose database is not the aim. Compaction, backups and large data
sets are what Postgres is for, and the documentation says where the line is.

### Integrity is a separate property

Raft tolerates instances that crash, not instances that lie: a compromised leader can
write whatever it likes to the log. Integrity assertion therefore needs more:

- **Tamper evidence**: hash-chained log entries, each signed by the instance that
  proposed it, under ADR-023's algorithm policy. Any instance, or an auditor, can check
  that the state it holds is the history everyone agreed on. This is cheap on top of
  Raft.
- **Tolerating a compromised instance**: a Byzantine fault-tolerant protocol (the
  HotStuff or Tendermint family). That is much heavier, and needs at least 3f + 1
  instances to tolerate f bad ones.

Proposal: Raft with a signed hash chain. BFT only if the threat model includes an
attacker inside an instance.

### Order of work

1. **Leases over the stores that exist**: a Postgres advisory lock, or a NATS KV key with
   a time-to-live (ADR-017 lists per-key TTL as not built). They give singleton
   scheduled tasks and leader election without new infrastructure. Scheduled tasks need
   them now, so this is day-2 work.
2. **Change notification across replicas**: `LISTEN/NOTIFY` (an ADR-016 gap) or NATS
   subjects, so caches of policies, scopes and revocations are invalidated, not polled.
3. **The Raft store**, for the framework's state and small aggregates, with a signed
   hash chain. It brings a persistent log, so the chart's Deployment becomes a
   StatefulSet with volumes (ADR-027). Leases then also work without Postgres or NATS.
4. **Several services**: discovery, routing between them, and identity between them
   (mTLS with SPIFFE ids, ADR-023). This overlaps with what a service mesh does, and
   should use one where one exists.

### What Raft costs

Raft puts consensus into the application's deployment:

- losing quorum stops writes;
- replica counts should be odd;
- every instance needs a volume;
- a rolling update must keep quorum, so a PodDisruptionBudget of one, and membership
  changes during the rollout.

A service that has Postgres or NATS already may be better off without it. Raft is
therefore opt-in, and steps 1 and 2 must stand without it.

## Open questions

- The threat model behind integrity assertion: is a compromised instance in scope?
- Which aggregates, if any, may an implementor keep in the Raft store, and how large?
- Across several services: replicated state shared between them, or each with its own
  cluster, talking through events?

## Consequences

- Leases move into the day-2 work, because duplicated scheduled tasks already affect
  anyone running more than one replica.
- With step 3, jenny runs a consensus protocol, and its correctness becomes jenny's
  responsibility. That calls for testing under faults (partitions, restarts, clock
  jumps) of a kind the project does not do yet.
