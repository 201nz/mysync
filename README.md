# mysync

Sync a local MySQL/MariaDB database to match a `mysqldump` file by touching
only the tables and rows that actually changed — instead of dropping the
database and restoring the whole dump from scratch every time.

If you regularly pull a production dump down to a dev machine, delete your
local database, and reload the whole thing, `mysync` is a drop-in
replacement for that last step that can be many times faster when most of
the data hasn't changed since your last sync.

```
mysync production.sql.gz -u root -D myapp
```

## Why this exists

The usual dev workflow — `mysqldump` on the server, copy the file down,
`DROP DATABASE` / restore — always pays the full cost of reloading every
row, every time, no matter how small the actual day-to-day change is. For a
multi-gigabyte database that's minutes spent rewriting millions of rows
that were already correct.

`mysync` instead:

1. Parses the dump file directly (no intermediate database needed to diff
   against).
2. Compares its schema against your local database's `information_schema`
   and only creates, drops, or rebuilds the tables that actually differ.
3. For every table whose schema didn't change, reads the existing local
   rows and computes the minimal set of `INSERT`/`UPDATE`/`DELETE`s needed
   to match the dump — nothing is rewritten unless it actually changed.

## Performance

Measured on a real production dump: 169 tables, ~12.7M rows, 2.2GB
decompressed. Baseline is a plain `mysqldump | mysql` restore into an empty
database.

| scenario | plain restore | mysync (default mode) |
|---|---|---|
| Full load into an empty database | ~200s | ~115s |
| Daily sync (3 days of real accumulated changes: ~50k inserts, ~9k updates, ~2k deletes across ~60/169 tables) | ~200s (restore doesn't care how much changed) | **~30s** |
| No-op (nothing changed since last sync) | ~200s | ~30-35s (drops to ~8s with `--per-table-transactions`, since there's nothing to serialize writes for) |

The daily-sync number is the one that matters for the workflow this is
built for: a plain restore costs the same ~200s regardless of how much
actually changed, while `mysync`'s cost scales with the size of the actual
diff. In the tested scenario that's roughly a **6-7x speedup**, and it only
gets better the smaller the day-to-day change is relative to the database
size.

OS page cache / MariaDB buffer pool state (cold vs. warm) made no
measurable difference in any of these numbers — the bottleneck is real
database write throughput (commits, index maintenance, redo log I/O), not
file I/O.

These numbers are from one schema and one change pattern — see
[When *not* to use this](#when-not-to-use-this) below for how that can
shift.

## Installing / building

Requires a Rust toolchain (`rustup` is the easiest way to get one).

```
cargo build --release
# binary at target/release/mysync
```

## Usage

```
mysync <dump_file> -D <database> [options]
```

| flag | default | meaning |
|---|---|---|
| `--host` | `127.0.0.1` | MySQL/MariaDB host |
| `--port` | `3306` | port |
| `-u, --user` | `root` | username |
| `-p, --password` | *(empty)* | password |
| `-D, --database` | *(required)* | database to sync |
| `--batch-size` | `1000` | rows per `INSERT`/`DELETE` statement (auto-clamped down for very wide tables to stay under MySQL's 65535-placeholder limit) |
| `-j, --jobs` | `min(cores, 8)` | worker threads/connections for the read-only diff step |
| `--tables-per-commit` | `0` (= one commit at the end) | commit every N tables instead of a single transaction for everything |
| `--per-table-transactions` | off | trade the all-or-nothing guarantee for more write parallelism — see below |
| `--dry-run` | off | report what would change without touching the database |
| `-v, --verbose` | off | also print unchanged tables |

Always try `--dry-run` first against a database you care about.

### Transaction modes

By default, every schema change (`CREATE`/`DROP TABLE`) happens outside any
transaction (autocommit — this is unavoidable, since DDL implicitly commits
in MySQL anyway), and then **all** row-level `INSERT`/`UPDATE`/`DELETE`
work across every table happens in a single transaction. If anything fails
partway through, the whole sync rolls back and your database is exactly
where it started — nothing partially applied.

`--per-table-transactions` gives up that guarantee in exchange for
concurrency: each worker thread computes *and* commits its own table's
changes independently, so writes to different tables can happen at the
same time instead of being serialized onto one connection. In testing this
was consistently 15-20% faster than the single-transaction mode. The
tradeoff: a failure partway through leaves some tables already synced to
the new dump and others not. If your schema has cross-table consistency
you care about (foreign keys, invariants spanning tables), a partial
failure here could leave it inconsistent in a way the single-transaction
mode never would. Use it if you're comfortable with that risk for the
extra speed, or if you can just re-run the sync to converge on failure
(since re-running is idempotent — it'll pick up wherever it left off).

Increasing `-j` past the default rarely helps much beyond a point (tested
up to physical core count) — the ceiling is typically the largest single
table's read+compare time and the database server's own capacity, not
your CPU count. Worth trying a couple of values on your own hardware/data
rather than assuming higher is better.

## How it decides what "changed" means

- **Schema**: column names, types, nullability, and primary key are
  compared against `information_schema`. If they match, only rows are
  diffed. If they don't, the table is dropped and recreated from the
  dump's exact `CREATE TABLE` statement, then fully reloaded (no partial
  `ALTER TABLE` reconciliation — schema changes are assumed to be rare).
  Index/engine/charset differences are intentionally ignored.
- **Rows**: matched by primary key if the table has one, otherwise by the
  first unique key with no nullable columns. Tables with neither are
  diffed as a full-row multiset (every column together identifies a "row";
  duplicate identical rows are tracked by count) — correct, but means the
  whole table is read into memory for that specific table.
- **Foreign keys**: checks are disabled for the whole run (mirroring what
  `mysqldump` itself does in its dump header), since neither table
  creation order nor row-insert order here is dependency-aware.

## What it doesn't do

- **Views, triggers, stored procedures/functions**: not supported. These
  statements are parsed just enough to be recognized and skipped — nothing
  in the target database is created or updated for them. If your dump has
  these, you'll need a separate step for them.
- **Column-level `ALTER TABLE`**: a schema change always means drop +
  recreate + full reload for that one table, not an in-place `ALTER`.
- **Cross-database consistency**: this syncs one database against one
  dump. It doesn't know about, or coordinate with, anything else.

## When *not* to use this

A plain `mysqldump | mysql` restore is still the better choice when:

- **Most of the data changes every time.** This tool's cost is roughly
  `read + compare every existing row` plus `write only what changed`. A
  plain restore only pays the write cost. If most rows across most tables
  differ between syncs, you're paying for the comparison without getting
  much benefit from it — at that point a full restore may be as fast or
  faster. If you're not sure which regime you're in, just try both once
  and compare.
- **The database is small.** If a full restore already takes a few
  seconds, there's nothing to win and no reason to add the complexity.
- **The dump includes views, triggers, or stored routines you need kept
  in sync.** Not handled at all currently (see above).
- **You need bit-for-bit restore semantics** (exact `AUTO_INCREMENT`
  counters on untouched tables, exact index/constraint definitions, etc.)
  — this tool preserves data, not every incidental schema detail.
- **First-time load onto an empty database.** It still works (and was
  faster than a plain restore in testing, since it parallelizes the bulk
  insert), but there's no diffing advantage on an empty target — it's
  just a well-parallelized bulk load at that point, so use whichever tool
  you're more comfortable operating.

Where it clearly wins: a large, mostly-stable database that a dev machine
re-syncs from a production-like dump on a recurring (daily/weekly)
schedule, where only a modest fraction of rows actually change between
syncs.

## Design notes

For anyone reading the source: the dump parser is a zero-copy,
`memchr`-accelerated tokenizer (`sqlstream.rs`) operating directly on the
loaded dump buffer — no per-value allocation in the common case. Row
values are compared as `Option<Vec<u8>>` rather than typed
ints/dates/decimals; this was a deliberate simplification after checking
empirically that MySQL's text-protocol query results represent every
column type uniformly as raw bytes or `NULL`, so there's no need for a
parallel type-coercion layer to keep in sync with the driver's own
behavior — dump-parsed values and live-fetched values are already
comparable byte-for-byte.
