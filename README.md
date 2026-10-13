# quorum

**quorum** is a distributed SQL database written from scratch in Rust — a
mini CockroachDB/TiDB, built layer by layer with **zero external crate
dependencies**. The binary codec, write-ahead log, MVCC engine, SQL parser,
planner, executor, wire protocol, and Raft consensus core are all hand-written
in this repository.

> Status: **Phase 1 complete-ish** — a single-node SQL engine that parses,
> plans, and executes statements against a crash-safe MVCC store, served over a
> TCP protocol. Multi-node Raft replication is implemented as a testable core
> and is wired into the server in a later phase. See `docs/ROADMAP.md`.

---

## Why from scratch?

Using `crates.io` for everything would hide the interesting parts. quorum is an
exercise in building the hard parts ourselves:

- a compact little-endian binary **codec** (`quorum-types::codec`),
- a framed, fsync-durable **write-ahead log** with torn-tail recovery,
- a **multi-version concurrency control** store with snapshot isolation,
- a recursive-descent **SQL parser** and binder,
- a **volcano-style executor**,
- a deterministic **Raft** state machine (no I/O, no wall clock),
- a length-prefixed **TCP protocol** with a hand-rolled request/response codec.

The nine core crates have **no external dependencies at all** — only the
optional desktop client (`quorum-ui`) pulls in a GUI toolkit (`egui`/`eframe`).

---

## Architecture

```
                         ┌───────────────────────────────────────────┐
                         │                 quorum-cli                 │
                         │        (repl, ASCII table printer)         │
                         └───────────────────────┬───────────────────┘
                                                 │ TCP ("QRM1" frames)
                         ┌───────────────────────▼───────────────────┐
                         │                quorum-proto                │
                         │   framing + ClientRequest/ClientResponse    │
                         └───────────────────────┬───────────────────┘
                                                 │
                         ┌───────────────────────▼───────────────────┐
                         │                quorum-node                 │
                         │   TCP server; quorumd binary entrypoint    │
                         └───────────────────────┬───────────────────┘
                                                 │
                         ┌───────────────────────▼───────────────────┐
                         │                quorum-exec                 │
                         │   Database facade + volcano operators      │
                         │   + three-valued expression evaluation     │
                         └───────────┬───────────────────┬───────────┘
                                     │                   │
             ┌───────────────────────▼──────┐   ┌────────▼────────────┐
             │        quorum-planner         │   │   quorum-storage     │
             │  binder → logical → physical  │   │  MVCC + WAL+catalog  │
             └───────────────┬──────────────┘   └────────┬────────────┘
                             │                           │
                 ┌───────────▼──────────┐       ┌────────▼─────────┐
                 │      quorum-sql       │       │   quorum-types    │
                 │ lexer/parser/AST      │       │ Value, Schema,    │
                 └───────────┬──────────┘       │ codec, errors     │
                             └──────────────────┘
                                       │
                             ┌─────────▼─────────┐
                             │    quorum-raft     │
                             │ deterministic core │
                             └───────────────────┘
```

### Crates

| Crate | Responsibility |
| --- | --- |
| `quorum-types` | `Value`, `DataType`, `ColumnDef`, `Schema`, `TableDef`, `DbError`, and the hand-rolled `codec` module (values, rows, schemas). |
| `quorum-sql` | Lexer, recursive-descent parser, and AST. `parse(sql) -> Result<Vec<Statement>>`. |
| `quorum-storage` | Durable engine: framed+fsynced `wal`, `MvccStore` with snapshot isolation, and the `catalog` (tables, rowid allocation, key layout). |
| `quorum-planner` | Binds AST to schemas (`binder`), produces a `LogicalPlan`, optimizes, and lowers to a `PhysicalPlan`. |
| `quorum-exec` | Volcano `operators`, expression `eval` with SQL three-valued logic, and the `Database` facade. |
| `quorum-raft` | Pure deterministic Raft core (`tick`/`handle`) — elections, log replication, commit; unit-tested as a simulated cluster. |
| `quorum-proto` | Wire framing (`"QRM1"` + length) and client/server message types. |
| `quorum-node` | TCP server library + the `quorumd` binary. |
| `quorum-cli` | The `quorum` client REPL. |
| `quorum-ui` | Optional native desktop workbench (`egui`/`eframe`): query editor, schema browser, connection manager, and results grid — speaking the same `QRM1` protocol. |

---

## Building

Requires a stable Rust toolchain (edition 2021). Then, from the workspace root:

```sh
cargo build
cargo test
```

The first build compiles the nine core crates with **no third-party
downloads**. Building the optional desktop client (`-p quorum-ui`) additionally
pulls in `egui`/`eframe`.

---

## Quickstart

Start a server (in-memory storage):

```sh
cargo run -p quorum-node --bin quorumd -- --addr 127.0.0.1:7300
```

Or with durable storage:

```sh
cargo run -p quorum-node --bin quorumd -- --addr 127.0.0.1:7300 --data ./data
```

In another shell, connect the client:

```sh
cargo run -p quorum-cli --bin quorum -- --addr 127.0.0.1:7300
```

Then run SQL:

```sql
CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT);
INSERT INTO users (id, name, age) VALUES (1, 'ada', 36), (2, 'grace', 45);
SELECT name, age FROM users WHERE age > 40 ORDER BY age DESC;
UPDATE users SET age = 37 WHERE id = 1;
DELETE FROM users WHERE id = 2;
SELECT COUNT(*), AVG(age) FROM users;
```

One-shot mode is also supported:

```sh
cargo run -p quorum-cli --bin quorum -- --addr 127.0.0.1:7300 -e "SELECT 1 + 1 AS two"
```

### Desktop UI (optional, early)

An `egui` workbench lives in `quorum-ui` and talks to a running `quorumd` over
the same `QRM1` protocol:

```sh
cargo run -p quorum-ui --bin quorum-ui
```

It offers a query editor (`Ctrl+Enter` to run), a schema browser, a connection
manager, and a results grid. It is an optional add-on and the only crate in the
workspace with third-party dependencies.

---

## What's implemented today

- **SQL**: `CREATE TABLE` / `DROP TABLE`, `INSERT`, `SELECT`, `UPDATE`,
  `DELETE`, `BEGIN`/`COMMIT`/`ROLLBACK`; expressions with arithmetic,
  comparison, `AND`/`OR`/`NOT`, `IS NULL`, `IN`, `BETWEEN`; `ORDER BY`,
  `LIMIT`, `GROUP BY`, aggregates (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`), scalar
  functions (`UPPER`, `LOWER`, `LENGTH`, `ABS`).
- **Storage**: MVCC snapshot isolation with first-committer-wins conflict
  detection; WAL append + fsync before visibility; torn-tail-tolerant replay;
  durable system catalog.
- **Raft**: a deterministic, I/O-free consensus core with leader election, log
  replication, and the current-term commit rule; tested over an in-process
  simulated network.

## Deliberately out of scope (for now)

JOINs, `DISTINCT`, subqueries, `HAVING` with aggregates, secondary indexes,
primary-key uniqueness enforcement (only `NOT NULL` and PK-non-null are
enforced), and multi-node wiring in the server. These are roadmap items, not
oversights — see `docs/ROADMAP.md`.

---

## Repository layout

```
quorum/
├── Cargo.toml            # workspace manifest (10 members)
├── README.md
├── docs/ROADMAP.md
├── examples/demo.sql
└── crates/
    ├── quorum-types/
    ├── quorum-sql/
    ├── quorum-storage/
    ├── quorum-planner/
    ├── quorum-exec/
    ├── quorum-raft/
    ├── quorum-proto/
    ├── quorum-node/
    ├── quorum-cli/
    └── quorum-ui/         # optional desktop workbench
```

## License

MIT.
