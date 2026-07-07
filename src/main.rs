mod ddl;
mod dumpfile;
mod db;
mod sqlstream;
mod sync;
mod values;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use clap::Parser;
use mysql::TxOpts;

use dumpfile::ParsedDump;

/// Sync a local MySQL/MariaDB database to match a mysqldump file, touching
/// only the rows/tables that actually changed.
///
/// Tokenizing the dump uses zero-copy `&[u8]` slices and `memchr`-based
/// scanning instead of per-byte loops (dump parsing is otherwise easily
/// the dominant cost against a multi-million-row dump), and each table's
/// SELECT+diff step (read-only and independent per table) is farmed out
/// across a thread pool, each worker with its own connection.
///
/// Default mode keeps every write in one shared transaction (applied
/// serially on the main thread after the parallel diff step) — the
/// original design goal: a single all-or-nothing sync, atomic against a
/// partial failure. `--per-table-transactions` trades that guarantee for
/// more parallelism: each worker both diffs *and* writes/commits its own
/// table independently, so writes to different tables can actually
/// happen concurrently instead of being serialized through one
/// connection. See the CLI help for the tradeoff.
#[derive(Parser, Debug)]
#[command(name = "mysync")]
struct Args {
    database: String,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 3306)]
    port: u16,

    #[arg(short, long, default_value = "root")]
    user: String,

    /// Falls back to the MYSQL_PWD environment variable if not given.
    #[arg(short, long, env = "MYSQL_PWD", default_value = "", hide_env_values = true)]
    password: String,

    /// Rows per INSERT/DELETE statement
    #[arg(long, default_value_t = 1000)]
    batch_size: usize,

    /// Commit every N tables instead of one commit at the end (0 = single
    /// transaction, default). Ignored under --per-table-transactions,
    /// which always commits once per table.
    #[arg(long, default_value_t = 0)]
    tables_per_commit: usize,

    /// Number of worker threads (and MySQL connections) used for the
    /// read-only SELECT+diff step. Defaults to the number of available
    /// cores, capped at 8 to stay polite to the server.
    #[arg(short = 'j', long, default_value_t = default_jobs())]
    jobs: usize,

    /// Give up the single-transaction all-or-nothing guarantee: each
    /// table gets its own transaction, computed and applied entirely on
    /// one worker thread, so writes to different tables happen
    /// concurrently instead of being serialized onto one connection. A
    /// failure partway through leaves some tables already synced and
    /// others not — fine if your schema has no cross-table consistency
    /// requirements (foreign keys, etc.) you care about mid-failure, not
    /// otherwise.
    #[arg(long)]
    per_table_transactions: bool,

    /// Report planned changes without executing them
    #[arg(long)]
    dry_run: bool,

    #[arg(short, long)]
    verbose: bool,
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
}

fn main() -> mysql::Result<()> {
    let args = Args::parse();
    let wall_t0 = Instant::now();

    let t0 = Instant::now();
    let data = dumpfile::read_dump_bytes().expect("failed to read dump");
    if args.verbose {
        println!("{:.1} MB decompressed in {:.1}s", data.len() as f64 / 1e6, t0.elapsed().as_secs_f64());
    }

    let t0 = Instant::now();
    let dump = dumpfile::parse_dump(&data);
    if args.verbose {
        let total_inserts: usize = dump.inserts.values().map(|v| v.len()).sum();
        println!(
            "Parsed {} tables, {} INSERT statements in {:.1}s",
            dump.schemas.len(),
            total_inserts,
            t0.elapsed().as_secs_f64()
        );
    }

    let conn_params = db::ConnParams {
        host: args.host.clone(),
        port: args.port,
        user: args.user.clone(),
        password: args.password.clone(),
        database: args.database.clone(),
    };

    let mut ddl_conn = db::connect(&conn_params)?;
    let local_schemas = db::fetch_local_tables(&mut ddl_conn, &conn_params.database)?;

    let plan = sync::plan_ddl(&dump.schemas, &dump.table_order, &local_schemas);
    if args.verbose {
        println!(
            "Schema diff: {} to create, {} to drop, {} to rebuild, {} unchanged",
            plan.to_create.len(),
            plan.to_drop.len(),
            plan.to_rebuild.len(),
            plan.unchanged.len()
        );
        for t in &plan.to_create {
            println!("  + create {t}");
        }
        for t in &plan.to_drop {
            println!("  - drop   {t}");
        }
        for t in &plan.to_rebuild {
            println!("  ~ rebuild {t} (schema changed)");
        }
    }

    let unsafe_tables = sync::find_unsafe_key_tables(&dump.schemas, &local_schemas, &plan);
    if !unsafe_tables.is_empty() {
        eprintln!("mysync: refusing to sync — unsafe primary/unique key situation:");
        for problem in &unsafe_tables {
            eprintln!("  - {problem}");
        }
        eprintln!(
            "See the README's \"Known correctness edge cases\" section. No changes have been made."
        );
        std::process::exit(1);
    }

    sync::execute_ddl(&mut ddl_conn, &dump.schemas, &plan, args.dry_run)?;
    drop(ddl_conn);

    let to_drop: HashSet<&str> = plan.to_drop.iter().map(String::as_str).collect();
    let new_tables: HashSet<&str> = plan
        .to_create
        .iter()
        .chain(&plan.to_rebuild)
        .map(String::as_str)
        .collect();
    let tables_to_sync: Vec<&String> = dump
        .table_order
        .iter()
        .filter(|t| !to_drop.contains(t.as_str()))
        .collect();

    let n_jobs = args.jobs.max(1).min(tables_to_sync.len().max(1));
    if args.verbose {
        let mode = if args.per_table_transactions { "one transaction per table" } else { "single shared transaction" };
        println!(
            "Syncing {} tables across {} worker connection(s) [{}]...",
            tables_to_sync.len(),
            n_jobs,
            mode
        );
    }

    let t0 = Instant::now();
    let (total_inserted, total_updated, total_deleted) = if args.per_table_transactions {
        run_per_table_transactions(&tables_to_sync, &dump, &new_tables, &conn_params, n_jobs, &args)?
    } else {
        run_shared_transaction(&tables_to_sync, &dump, &new_tables, &conn_params, n_jobs, &args)?
    };

    if args.verbose {
        let elapsed = t0.elapsed().as_secs_f64();
        println!(
            "{} rows in {:.1}s: {} inserted, {} updated, {} deleted",
            if args.dry_run { "Would sync" } else { "Synced" },
            elapsed,
            total_inserted,
            total_updated,
            total_deleted
        );
        println!("Total wall time: {:.1}s", wall_t0.elapsed().as_secs_f64());
    }

    Ok(())
}

/// Default mode: parallel read-only diff, one shared transaction applied
/// serially on the main thread (see module docs for the tradeoff vs.
/// `run_per_table_transactions`).
fn run_shared_transaction(
    tables_to_sync: &[&String],
    dump: &ParsedDump,
    new_tables: &HashSet<&str>,
    conn_params: &db::ConnParams,
    n_jobs: usize,
    args: &Args,
) -> mysql::Result<(u64, u64, u64)> {
    let mut total_inserted = 0u64;
    let mut total_updated = 0u64;
    let mut total_deleted = 0u64;

    let chunk_size = if args.tables_per_commit == 0 {
        tables_to_sync.len().max(1)
    } else {
        args.tables_per_commit
    };

    let mut main_conn = db::connect(conn_params)?;
    let mut completed_overall = 0usize;

    for table_chunk in tables_to_sync.chunks(chunk_size) {
        let next_idx = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel::<mysql::Result<(usize, sync::TablePlan)>>();

        std::thread::scope(|scope| -> mysql::Result<()> {
            for _ in 0..n_jobs {
                let tx = tx.clone();
                let next_idx = &next_idx;
                let table_chunk = &table_chunk;
                let dump = &dump;
                let new_tables = &new_tables;
                scope.spawn(move || {
                    let mut conn = match db::connect(conn_params) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    loop {
                        let i = next_idx.fetch_add(1, Ordering::Relaxed);
                        if i >= table_chunk.len() {
                            break;
                        }
                        let table = table_chunk[i].as_str();
                        let schema = &dump.schemas[table];
                        let empty = Vec::new();
                        let insert_stmts = dump.inserts.get(table).unwrap_or(&empty);

                        let result = if new_tables.contains(table) {
                            Ok(sync::compute_new_table_plan(schema, insert_stmts))
                        } else if let Some(key_cols) = schema.key_columns() {
                            sync::compute_keyed_table_plan(&mut conn, schema, insert_stmts, key_cols)
                        } else {
                            sync::compute_unkeyed_table_plan(&mut conn, schema, insert_stmts)
                        };
                        if tx.send(result.map(|p| (i, p))).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);

            let mut db_tx = main_conn.start_transaction(TxOpts::default())?;
            for msg in rx {
                let (i, table_plan) = msg?;
                let table = table_chunk[i].as_str();
                let schema = &dump.schemas[table];
                let stats = table_plan.stats();

                if !args.dry_run {
                    sync::apply_table_plan(&mut db_tx, schema, schema.key_columns(), table_plan, args.batch_size)?;
                }

                total_inserted += stats.inserted;
                total_updated += stats.updated;
                total_deleted += stats.deleted;
                completed_overall += 1;
                print_progress(completed_overall, tables_to_sync.len(), table, &stats, args.verbose);
            }
            if args.dry_run {
                db_tx.rollback()?;
            } else {
                db_tx.commit()?;
            }
            Ok(())
        })?;
    }

    Ok((total_inserted, total_updated, total_deleted))
}

/// `--per-table-transactions` mode: each worker both computes *and*
/// applies/commits its own table's transaction, so writes to different
/// tables run concurrently across connections instead of being
/// serialized through one. See the CLI help / module docs for the
/// atomicity tradeoff this gives up.
fn run_per_table_transactions(
    tables_to_sync: &[&String],
    dump: &ParsedDump,
    new_tables: &HashSet<&str>,
    conn_params: &db::ConnParams,
    n_jobs: usize,
    args: &Args,
) -> mysql::Result<(u64, u64, u64)> {
    let next_idx = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<mysql::Result<(usize, sync::TableStats)>>();

    let mut total_inserted = 0u64;
    let mut total_updated = 0u64;
    let mut total_deleted = 0u64;

    std::thread::scope(|scope| -> mysql::Result<()> {
        for _ in 0..n_jobs {
            let tx = tx.clone();
            let next_idx = &next_idx;
            let dump = &dump;
            let new_tables = &new_tables;
            scope.spawn(move || {
                let mut conn = match db::connect(conn_params) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                loop {
                    let i = next_idx.fetch_add(1, Ordering::Relaxed);
                    if i >= tables_to_sync.len() {
                        break;
                    }
                    let table = tables_to_sync[i].as_str();
                    let schema = &dump.schemas[table];
                    let empty = Vec::new();
                    let insert_stmts = dump.inserts.get(table).unwrap_or(&empty);

                    let result = (|| -> mysql::Result<sync::TableStats> {
                        let mut db_tx = conn.start_transaction(TxOpts::default())?;
                        let plan = if new_tables.contains(table) {
                            sync::compute_new_table_plan(schema, insert_stmts)
                        } else if let Some(key_cols) = schema.key_columns() {
                            sync::compute_keyed_table_plan(&mut db_tx, schema, insert_stmts, key_cols)?
                        } else {
                            sync::compute_unkeyed_table_plan(&mut db_tx, schema, insert_stmts)?
                        };
                        let stats = plan.stats();
                        if args.dry_run {
                            db_tx.rollback()?;
                        } else {
                            sync::apply_table_plan(&mut db_tx, schema, schema.key_columns(), plan, args.batch_size)?;
                            db_tx.commit()?;
                        }
                        Ok(stats)
                    })();

                    if tx.send(result.map(|s| (i, s))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut completed = 0usize;
        for msg in rx {
            let (i, stats) = msg?;
            let table = tables_to_sync[i].as_str();
            total_inserted += stats.inserted;
            total_updated += stats.updated;
            total_deleted += stats.deleted;
            completed += 1;
            print_progress(completed, tables_to_sync.len(), table, &stats, args.verbose);
        }
        Ok(())
    })?;

    Ok((total_inserted, total_updated, total_deleted))
}

fn print_progress(completed: usize, total: usize, table: &str, stats: &sync::TableStats, verbose: bool) {
    if !verbose {
        return;
    }
    if stats.inserted > 0 || stats.updated > 0 || stats.deleted > 0 {
        println!(
            "  [{completed}/{total}] {table}: +{} ~{} -{}",
            stats.inserted, stats.updated, stats.deleted
        );
    } else {
        println!("  [{completed}/{total}] {table}: unchanged");
    }
}
