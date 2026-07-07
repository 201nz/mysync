# mysync

Sync a local MySQL/MariaDB database to match a `mysqldump` file by touching
only the tables and rows that actually changed — instead of dropping the
database and restoring the whole dump from scratch every time.

If you regularly pull a production dump down to a dev machine, delete your
local database, and reload the whole thing, `mysync` is a drop-in
replacement for that last step that can be many times faster when most of
the data hasn't changed since your last sync.

```
mysqldump -h prod -u root myapp | mysync myapp
```

## Contents

- [Why this exists](#why-this-exists)
- [Performance](#performance)
- [Installing / building](#installing--building)
- [Usage](#usage)
  - [Transaction modes](#transaction-modes)
- [How it decides what "changed" means](#how-it-decides-what-changed-means)
- [Limitations](#limitations)
- [Design notes](#design-notes)
- [License](#license)

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

New to Rust? Building this is two steps: install the Rust toolchain, then
run one command. The steps below assume no prior Rust setup.

### 1. Install Rust

Rust is installed via `rustup`, the official installer/version manager
(this also gives you `cargo`, the build tool used below).

<details>
<summary><strong>macOS / Linux</strong></summary>

Open a terminal and run:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Accept the default options. Then close and reopen your terminal (or run
`source "$HOME/.cargo/env"`) so `cargo` is on your `PATH`.

</details>

<details>
<summary><strong>Windows</strong></summary>

Download and run [rustup-init.exe](https://win.rustup.rs) and accept the
default options. If it reports that the "MSVC linker" / C++ Build Tools
are missing, follow the link it gives you to install "Build Tools for
Visual Studio" with the **Desktop development with C++** workload, then
re-run `rustup-init.exe` (this is a one-time OS-level requirement for
compiling *any* Rust project on Windows, not specific to this one).

</details>

Verify it worked in a **new** terminal window:

```
rustc --version
cargo --version
```

### 2. Install a C compiler + zlib (one-time, OS package manager)

This project itself is pure Rust, but one dependency (dump decompression)
links against the system `zlib` compression library, which means a C
compiler is needed at build time to link it. Rust doesn't install this for
you — it comes from your OS's usual toolchain:

<details>
<summary><strong>Debian / Ubuntu</strong></summary>

```
sudo apt install build-essential pkg-config zlib1g-dev
```

</details>

<details>
<summary><strong>Fedora / RHEL</strong></summary>

```
sudo dnf install gcc pkgconf-pkg-config zlib-devel
```

</details>

<details>
<summary><strong>Arch</strong></summary>

```
sudo pacman -S base-devel zlib
```

</details>

<details>
<summary><strong>macOS</strong></summary>

Install the Xcode Command Line Tools (provides both the C compiler and
zlib — no Homebrew needed):

```
xcode-select --install
```

</details>

<details>
<summary><strong>Windows</strong></summary>

Nothing further — the MSVC Build Tools installed in step 1 are enough;
`cargo build` will compile zlib from source automatically.

</details>

### 3. Build

From the project directory, on any OS:

```
cargo build --release
```

The first build compiles all dependencies too, so expect it to take a
couple of minutes; later builds are much faster. The resulting binary is
at:

- macOS / Linux: `target/release/mysync`
- Windows: `target\release\mysync.exe`

Run it directly from there (e.g. `./target/release/mysync ...` on
macOS/Linux, `target\release\mysync.exe ...` on Windows/PowerShell), or
copy it somewhere on your `PATH`.

## Usage

```
mysync <database> [options]
```

`mysync` always reads the dump from stdin — plain SQL or gzip-compressed,
detected automatically. Use a pipe or redirect to feed it:

```bash
# pipe directly from mysqldump
mysqldump -h prod -u root myapp | mysync myapp

# pipe with compression
ssh prod "mysqldump myapp | gzip" | mysync myapp

# redirect from a file
mysync myapp < production.sql.gz
```

| flag | default | meaning |
|---|---|---|
| `--host` | `127.0.0.1` | MySQL/MariaDB host |
| `--port` | `3306` | port |
| `-u, --user` | `root` | username |
| `-p, --password` | *(empty, or `$MYSQL_PWD`)* | password |
| `database` | *(required)* | database to sync (positional argument) |
| `--batch-size` | `1000` | rows per `INSERT`/`DELETE` statement (auto-clamped down for very wide tables to stay under MySQL's 65535-placeholder limit) |
| `-j, --jobs` | `min(cores, 8)` | worker threads/connections for the read-only diff step |
| `--tables-per-commit` | `0` (= one commit at the end) | commit every N tables instead of a single transaction for everything |
| `--per-table-transactions` | off | trade the all-or-nothing guarantee for more write parallelism — see below |
| `--dry-run` | off | report what would change without touching the database |
| `-v, --verbose` | off | print progress, schema diff, per-table stats, a final summary, and total wall time (silent otherwise) |

By default `mysync` prints nothing and exits 0 on success, so it composes
cleanly in scripts/pipelines; pass `-v` to see what it's doing.

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

## Limitations

### What it doesn't do

- **Views, triggers, stored procedures/functions**: not supported. These
  statements are parsed just enough to be recognized and skipped — nothing
  in the target database is created or updated for them. If your dump has
  these, you'll need a separate step for them.
- **Column-level `ALTER TABLE`**: a schema change always means drop +
  recreate + full reload for that one table, not an in-place `ALTER`.
- **Cross-database consistency**: this syncs one database against one
  dump. It doesn't know about, or coordinate with, anything else.

### Known correctness edge cases (PK-less tables, multiple/drifted unique keys)

For a table with **no primary key**, row matching falls back to "the
first non-nullable unique key" (see above), and that same key is used to
build an `INSERT ... ON DUPLICATE KEY UPDATE` for anything that looks
changed. Two related situations make that upsert unsafe on such a table:

- **A second real unique key on the same table.** MySQL detects an
  `ON DUPLICATE KEY UPDATE` conflict against *any* unique constraint on
  the table, not just the one mysync picked. A dump row that looks new
  by the chosen key could otherwise collide with an unrelated existing
  row via a different unique key, silently merging the two into one row
  instead of inserting the new one.
- **A unique key that's drifted between the dump and the local copy.**
  Schema-change detection (see "How it decides what changed means" above)
  intentionally ignores index differences, so a table whose *unique key*
  differs between dump and local (columns/primary key otherwise
  unchanged) is never rebuilt — which would otherwise leave the upsert
  relying on a key that isn't actually enforced locally.

mysync checks for both before touching anything: before any DDL or writes,
it inspects each table's already-parsed schema (no data rows involved, so
this costs nothing per-row and doesn't slow down tables that don't have
this shape) and refuses the whole run — printing which table(s) and why,
with no changes made — if a table's key can't be proven to be the *only*
real unique-ish constraint, or if the dump's and local database's keys
don't match. Tables using the no-key/full-multiset diff path (see above)
aren't affected, since they never use an upsert.

### When *not* to use this

A plain `mysqldump | mysql` pipe-restore is still the better choice when:

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

## License

[0BSD](LICENSE) — do whatever you want with it, no attribution required, no
warranty provided.
