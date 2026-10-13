# quorum roadmap

quorum is built skeleton-first: stand up the whole architecture so it compiles
and passes end-to-end tests, then deepen each subsystem. This document tracks
what exists and what comes next.

Legend: ✅ done · 🟡 partial · ⬜ not started

---

## Phase 0 — Skeleton (current)

Goal: a single-node, crash-safe SQL database served over TCP, with a
deterministic Raft core ready to be wired in.

| Area | Status | Notes |
| --- | --- | --- |
| Workspace & crates | ✅ | 9 crates, zero external dependencies |
| Binary codec | ✅ | values, rows, schemas |
| SQL lexer/parser/AST | ✅ | recursive descent |
| Binder & logical plan | ✅ | aggregates, `GROUP BY`, `ORDER BY` |
| Physical plan | 🟡 | 1:1 lowering, no cost model |
| Volcano executor | ✅ | filter, project, sort, limit, aggregate, insert/update/delete |
| Expression eval | ✅ | SQL three-valued logic |
| MVCC + snapshot isolation | ✅ | first-committer-wins |
| WAL + fsync + replay | ✅ | torn-tail tolerant |
| Catalog | ✅ | durable, versioned |
| Wire protocol | ✅ | `QRM1` framing |
| `quorumd` server + `quorum` CLI | ✅ | single node |
| Raft core | ✅ | deterministic, unit-tested standalone |
| Raft wired into server | ⬜ | Phase 3 |

Phase-0 scope cuts (intentional, tracked below): no JOINs, no `DISTINCT`, no
subqueries, no `HAVING` with aggregates, no secondary indexes, no primary-key
uniqueness enforcement, no multi-node server.

---

## Phase 1 — Storage depth

Make the storage engine genuinely competitive rather than a flat sorted map.

- ⬜ **LSM-tree / memtable + SSTables** behind the current `MvccStore` API
  (keep the API stable; swap the backing store).
- ⬜ **Block-based SSTable format** with prefix compression and bloom filters.
- ⬜ **Background compaction** (size-tiered, then leveled).
- ⬜ **Checkpoints / snapshots** to bound WAL growth and speed up recovery.
- ⬜ **Log-structured manifest** for atomic multi-file metadata changes.
- ⬜ **MVCC garbage collection** driven by a safe-point / lease mechanism.
- 🟡 **Key encoding** — switch to order-preserving typed key encoding so range
  scans are index-friendly.
- ⬜ **Crash-injection tests**: kill mid-`fsync`, mid-compaction; assert
  recovery invariants.

## Phase 2 — Query engine depth

- ⬜ **Hash joins** and **merge joins** (`INNER`, `LEFT`, `RIGHT`, `FULL`).
- ⬜ **Subqueries** (correlated + uncorrelated), `EXISTS`/`IN` decorrelation.
- ⬜ **`DISTINCT`** and `HAVING` with aggregates.
- ⬜ **Secondary indexes** + index selection in the planner.
- ⬜ **Primary-key / unique constraint enforcement** with write conflicts.
- ⬜ **Cost-based optimizer**: statistics collection, selectivity estimation,
  join ordering, access-path selection.
- ⬜ **More scalar functions**, `CASE`, `CAST`, date/time types.
- ⬜ **Prepared statements / parameter binding**.

## Phase 3 — Real consensus & replication

Wire the deterministic Raft core into the node.

- ⬜ **Multi-node server**: replace direct `Database::execute` with a
  Raft-replicated state machine (apply committed log entries deterministically).
- ⬜ **TCP transport** for Raft `RequestVote`/`AppendEntries` + snapshots.
- ⬜ **Membership changes** (single-server joint consensus).
- ⬜ **Log compaction / snapshots** on the consensus side.
- ⬜ **Linearizable reads** (read-index / lease reads).
- ⬜ **Leader leases** and follower reads.
- ⬜ **Partition simulation** test harness (drop/delay/reorder/duplicate
  messages) and invariants: election safety, log matching, leader completeness,
  state-machine safety.

## Phase 4 — Distributed transactions

- ⬜ **Range partitioning** and **replica placement** (range descriptors).
- ⬜ **Two-phase commit** across ranges/shards, coordinated with Raft.
- ⬜ **Distributed MVCC** with transaction records and intents.
- ⬜ **Timestamp oracle** (hybrid logical clocks + `HLC`) and serializable
  snapshot isolation.
- ⬜ **Deadlock detection / transaction priorities**.
- ⬜ **Follower reads** at a snapshot timestamp.

## Phase 5 — Operability & hardening

- ⬜ **Structured logging** and metrics/`EXPLAIN`/`EXPLAIN ANALYZE`.
- ⬜ **Jepsen-style nemesis tests**: partitions, clock skew, process pauses,
  membership churn — assert linearizability and no data loss.
- ⬜ **Fuzzing** for the parser and codec; property tests for Raft and MVCC.
- ⬜ **Graceful shutdown & draining** (the current `NodeHandle::shutdown` is
  best-effort).
- ⬜ **Authentication / TLS** on the wire protocol.
- ⬜ **Benchmarks** (YCSB-like) and performance budgets.

---

## Guiding principles

1. **Deterministic core, effects at the edges.** Consensus, the planner, and
   the executor should be pure and unit-testable; I/O lives in thin adapters.
2. **Durability before visibility.** A write is visible only after its WAL
   record is fsynced.
3. **Keep seams honest.** `PhysicalPlan` is the place cost-based decisions go;
   `Database::execute` is the place replication will intercept.
4. **Zero dependencies.** Every hard subsystem is ours to build and ours to
   debug.
