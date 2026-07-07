//! Schema diff + row diff/sync engine. The per-table SELECT + diff step
//! is split from the actual writes (`compute_*_plan` vs `apply_table_plan`)
//! so the caller can farm the (read-only, independent-per-table) compute
//! step out across threads while still funneling every write through a
//! single shared transaction — see main.rs for the thread-pool
//! orchestration. Schema DDL still runs autocommit, outside any
//! transaction, before any row is touched.
//!
//! Rows are represented internally as `Vec<Cell>` (`Cell = Option<Vec<u8>>`)
//! rather than `Vec<mysql::Value>` — see values.rs's module docs for why
//! (`mysql::Value` isn't `Hash`/`Eq`, but every value here is only ever
//! `NULL` or `Bytes` in practice, verified against a real connection).
//! `Cell`s are converted to `mysql::Value` only at the point of binding a
//! query parameter.

use std::collections::{HashMap, HashSet};

use mysql::prelude::*;
use mysql::Conn;

use crate::ddl::TableSchema;
use crate::dumpfile::InsertStmt;
use crate::sqlstream;
use crate::values::{self, cell_to_value};

pub type Cell = Option<Vec<u8>>;

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[derive(Debug, Default)]
pub struct DdlPlan {
    pub to_drop: Vec<String>,
    pub to_create: Vec<String>,
    pub to_rebuild: Vec<String>,
    pub unchanged: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TableStats {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
}

/// The result of the (parallelizable, read-only) compute step for one
/// table: exactly what needs to change, with nothing executed yet.
pub enum TablePlan {
    /// Table was just CREATEd empty (new or rebuilt): no diffing needed,
    /// every dump row is new.
    New { rows: Vec<Vec<Cell>> },
    Keyed {
        to_insert: Vec<Vec<Cell>>,
        to_update: Vec<Vec<Cell>>,
        delete_keys: Vec<Vec<Cell>>,
    },
    /// No primary/unique key available: diffed as a row-value multiset.
    Unkeyed {
        to_insert: Vec<Vec<Cell>>,
        to_delete: Vec<(Vec<Cell>, u64)>,
    },
}

impl TablePlan {
    pub fn stats(&self) -> TableStats {
        match self {
            TablePlan::New { rows } => TableStats {
                inserted: rows.len() as u64,
                ..Default::default()
            },
            TablePlan::Keyed { to_insert, to_update, delete_keys } => TableStats {
                inserted: to_insert.len() as u64,
                updated: to_update.len() as u64,
                deleted: delete_keys.len() as u64,
            },
            TablePlan::Unkeyed { to_insert, to_delete } => TableStats {
                inserted: to_insert.len() as u64,
                updated: 0,
                deleted: to_delete.iter().map(|(_, n)| n).sum(),
            },
        }
    }
}

pub fn plan_ddl(
    dump_schemas: &HashMap<String, TableSchema>,
    dump_order: &[String],
    local_schemas: &HashMap<String, TableSchema>,
) -> DdlPlan {
    let local_names: HashSet<&str> = local_schemas.keys().map(|s| s.as_str()).collect();
    let dump_names: HashSet<&str> = dump_schemas.keys().map(|s| s.as_str()).collect();

    let mut plan = DdlPlan::default();
    let mut to_drop: Vec<&str> = local_names.difference(&dump_names).copied().collect();
    to_drop.sort_unstable();
    plan.to_drop = to_drop.into_iter().map(String::from).collect();

    for t in dump_order {
        if !local_names.contains(t.as_str()) {
            plan.to_create.push(t.clone());
        } else if dump_schemas[t].signature() != local_schemas[t].signature() {
            plan.to_rebuild.push(t.clone());
        } else {
            plan.unchanged.push(t.clone());
        }
    }
    plan
}

/// Finds tables where syncing by key would be unsafe: `key_columns()`
/// picks *a* usable key, but doesn't guarantee it's the *only* real
/// unique-ish constraint on the table — and `INSERT ... ON DUPLICATE KEY
/// UPDATE` conflicts on any of them, not just the one mysync picked. A
/// second real constraint (declared in the dump) or a constraint that's
/// drifted out of sync between the dump and the local database (invisible
/// to schema-change detection, which intentionally ignores index
/// differences — see `TableSchema::signature`) can both make that upsert
/// silently merge or duplicate rows instead of doing what was intended.
///
/// This only inspects already-parsed schema metadata (no data rows), so
/// it costs nothing per-row and runs once before any DDL or writes —
/// tables named here should cause the whole run to stop, not be silently
/// skipped or slowed down for tables that don't have this shape.
pub fn find_unsafe_key_tables(
    dump_schemas: &HashMap<String, TableSchema>,
    local_schemas: &HashMap<String, TableSchema>,
    plan: &DdlPlan,
) -> Vec<String> {
    let freshly_created: HashSet<&str> = plan
        .to_create
        .iter()
        .chain(&plan.to_rebuild)
        .map(String::as_str)
        .collect();

    let mut problems = Vec::new();
    for (name, schema) in dump_schemas {
        if schema.key_columns().is_none() {
            continue; // no usable key at all: falls back to unkeyed multiset diff, no upsert involved
        }
        match schema.effective_key() {
            None => problems.push(format!(
                "{name}: has more than one primary/unique key candidate — mysync can't tell \
                 which one INSERT ... ON DUPLICATE KEY UPDATE will actually conflict on"
            )),
            Some(dump_key) => {
                // Freshly created/rebuilt tables are loaded from the dump's
                // exact CREATE TABLE text, so they're guaranteed to match it.
                if freshly_created.contains(name.as_str()) {
                    continue;
                }
                let local_matches = local_schemas
                    .get(name)
                    .and_then(|local| local.effective_key())
                    .is_some_and(|local_key| local_key == dump_key);
                if !local_matches {
                    problems.push(format!(
                        "{name}: primary/unique key differs between the dump and the local \
                         database — the key mysync would sync by isn't actually enforced locally"
                    ));
                }
            }
        }
    }
    problems.sort();
    problems
}

pub fn execute_ddl(
    conn: &mut Conn,
    dump_schemas: &HashMap<String, TableSchema>,
    plan: &DdlPlan,
    dry_run: bool,
) -> mysql::Result<()> {
    for t in &plan.to_drop {
        if !dry_run {
            conn.query_drop(format!("DROP TABLE {}", quote_ident(t)))?;
        }
    }
    for t in &plan.to_rebuild {
        if !dry_run {
            conn.query_drop(format!("DROP TABLE {}", quote_ident(t)))?;
            let sql = String::from_utf8_lossy(&dump_schemas[t].raw_statement).into_owned();
            conn.query_drop(sql)?;
        }
    }
    for t in &plan.to_create {
        if !dry_run {
            let sql = String::from_utf8_lossy(&dump_schemas[t].raw_statement).into_owned();
            conn.query_drop(sql)?;
        }
    }
    Ok(())
}

/// Yields coerced rows (in `schema`'s natural column order) from a
/// table's INSERT statements.
fn iter_dump_rows<'a>(
    schema: &'a TableSchema,
    insert_stmts: &'a [InsertStmt<'a>],
) -> impl Iterator<Item = Vec<Cell>> + 'a {
    let natural_names = schema.column_names();
    insert_stmts.iter().flat_map(move |stmt| {
        // `reorder[i]` is the token position (in this statement's row
        // tuples) holding natural column `i`'s value, or `None` if that
        // column was left out of an explicit column list — e.g.
        // `INSERT INTO t (a,b) VALUES ...` on a table with columns
        // `(a,b,c)` misses `c`.
        let reorder: Option<Vec<Option<usize>>> = stmt.explicit_columns.as_ref().map(|cols| {
            let pos: HashMap<&str, usize> =
                cols.iter().enumerate().map(|(i, &c)| (c, i)).collect();
            natural_names.iter().map(|n| pos.get(n).copied()).collect()
        });
        // Bit width per *token position* (not per natural column), so a
        // token can be decoded as a bit-literal without first reordering.
        let token_bit_widths: Vec<Option<u32>> = match &stmt.explicit_columns {
            Some(cols) => cols
                .iter()
                .map(|c| {
                    schema
                        .columns
                        .iter()
                        .find(|col| col.name == *c)
                        .and_then(|col| col.bit_width)
                })
                .collect(),
            None => schema.columns.iter().map(|col| col.bit_width).collect(),
        };
        stmt.rows().map(move |row_bytes| {
            let tokens = sqlstream::split_toplevel(row_bytes, b',');
            let cells: Vec<Cell> = tokens
                .into_iter()
                .zip(token_bit_widths.iter().copied().chain(std::iter::repeat(None)))
                .map(|(t, bit_width)| values::parse_value_token_typed(t, bit_width).into_cell())
                .collect();
            match &reorder {
                None => cells,
                Some(map) => map
                    .iter()
                    .enumerate()
                    .map(|(col_idx, pos)| match pos {
                        Some(i) => cells.get(*i).cloned().unwrap_or(None),
                        None => schema.columns[col_idx].default.resolve(),
                    })
                    .collect(),
            }
        })
    })
}

/// Converts a `mysql::Value` row (from a text-protocol query result,
/// which per our empirical check only ever contains `NULL`/`Bytes`) into
/// our `Cell` representation.
fn row_to_cells(row: mysql::Row) -> Vec<Cell> {
    row.unwrap()
        .into_iter()
        .map(|v| match v {
            mysql::Value::NULL => None,
            mysql::Value::Bytes(b) => Some(b),
            other => panic!(
                "unexpected non-text value from a text-protocol query result: {other:?} \
                 (mysync assumes query_iter always returns NULL/Bytes; see values.rs docs)"
            ),
        })
        .collect()
}

/// MySQL/MariaDB caps a single prepared statement at 65535 `?`
/// placeholders. A wide table (this schema has one with 122 columns)
/// times a 1000-row batch blows past that easily, so the configured
/// batch size is a ceiling, not a guarantee — clamp it down per
/// statement shape (`values_per_row` = columns for INSERT/UPSERT, key
/// column count for a keyed DELETE).
fn safe_batch_size(requested: usize, values_per_row: usize) -> usize {
    requested.min(65535 / values_per_row.max(1)).max(1)
}

fn chunked<T>(items: Vec<T>, n: usize) -> impl Iterator<Item = Vec<T>> {
    let mut items = items;
    std::iter::from_fn(move || {
        if items.is_empty() {
            None
        } else {
            let tail = items.split_off(items.len().min(n));
            Some(std::mem::replace(&mut items, tail))
        }
    })
}

fn bulk_insert<Q: Queryable>(
    q: &mut Q,
    table: &str,
    columns: &[&str],
    rows: impl Iterator<Item = Vec<Cell>>,
    batch_size: usize,
) -> mysql::Result<u64> {
    let col_sql = columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(",");
    let row_ph = format!("({})", vec!["?"; columns.len()].join(","));
    let batch_size = safe_batch_size(batch_size, columns.len());
    let mut count = 0u64;
    let mut batch: Vec<Vec<Cell>> = Vec::with_capacity(batch_size);
    for row in rows {
        batch.push(row);
        if batch.len() >= batch_size {
            count += flush_insert(q, table, &col_sql, &row_ph, std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        count += flush_insert(q, table, &col_sql, &row_ph, batch)?;
    }
    Ok(count)
}

fn flush_insert<Q: Queryable>(
    q: &mut Q,
    table: &str,
    col_sql: &str,
    row_ph: &str,
    batch: Vec<Vec<Cell>>,
) -> mysql::Result<u64> {
    let n = batch.len() as u64;
    let sql = format!(
        "INSERT INTO {} ({}) VALUES {}",
        quote_ident(table),
        col_sql,
        vec![row_ph; batch.len()].join(",")
    );
    let params: Vec<mysql::Value> = batch.into_iter().flatten().map(cell_to_value).collect();
    q.exec_drop(sql, params)?;
    Ok(n)
}

/// Computes the plan for a table that was just CREATEd empty (new or
/// rebuilt): no DB access needed at all, just parse every dump row. Safe
/// to run on any thread without a connection.
pub fn compute_new_table_plan(schema: &TableSchema, insert_stmts: &[InsertStmt<'_>]) -> TablePlan {
    TablePlan::New {
        rows: iter_dump_rows(schema, insert_stmts).collect(),
    }
}

pub fn compute_keyed_table_plan<Q: Queryable>(
    q: &mut Q,
    schema: &TableSchema,
    insert_stmts: &[InsertStmt<'_>],
    key_cols: &[String],
) -> mysql::Result<TablePlan> {
    let columns = schema.column_names();
    let key_idx: Vec<usize> = key_cols
        .iter()
        .map(|c| columns.iter().position(|n| n == c).unwrap())
        .collect();

    let select_sql = format!(
        "SELECT {} FROM {}",
        columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(","),
        quote_ident(&schema.name)
    );
    let mut local_by_key: HashMap<Vec<Cell>, Vec<Cell>> = HashMap::new();
    for row in q.query_iter(select_sql)? {
        let row = row_to_cells(row?);
        let key: Vec<Cell> = key_idx.iter().map(|&i| row[i].clone()).collect();
        local_by_key.insert(key, row);
    }

    let mut to_insert = Vec::new();
    let mut to_update = Vec::new();
    for row in iter_dump_rows(schema, insert_stmts) {
        let key: Vec<Cell> = key_idx.iter().map(|&i| row[i].clone()).collect();
        match local_by_key.remove(&key) {
            None => to_insert.push(row),
            Some(local_row) => {
                if local_row != row {
                    to_update.push(row);
                }
            }
        }
    }

    Ok(TablePlan::Keyed {
        to_insert,
        to_update,
        delete_keys: local_by_key.into_keys().collect(),
    })
}

/// No primary/unique key available: diff as a row-value multiset.
pub fn compute_unkeyed_table_plan<Q: Queryable>(
    q: &mut Q,
    schema: &TableSchema,
    insert_stmts: &[InsertStmt<'_>],
) -> mysql::Result<TablePlan> {
    let columns = schema.column_names();
    let select_sql = format!(
        "SELECT {} FROM {}",
        columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(","),
        quote_ident(&schema.name)
    );
    let mut local_counts: HashMap<Vec<Cell>, i64> = HashMap::new();
    for row in q.query_iter(select_sql)? {
        let row = row_to_cells(row?);
        *local_counts.entry(row).or_insert(0) += 1;
    }

    let mut dump_counts: HashMap<Vec<Cell>, i64> = HashMap::new();
    for row in iter_dump_rows(schema, insert_stmts) {
        *dump_counts.entry(row).or_insert(0) += 1;
    }

    let mut to_insert = Vec::new();
    for (row, &count) in &dump_counts {
        let extra = count - local_counts.get(row).copied().unwrap_or(0);
        for _ in 0..extra.max(0) {
            to_insert.push(row.clone());
        }
    }

    let mut to_delete = Vec::new();
    for (row, &count) in &local_counts {
        let extra = count - dump_counts.get(row).copied().unwrap_or(0);
        if extra > 0 {
            to_delete.push((row.clone(), extra as u64));
        }
    }

    Ok(TablePlan::Unkeyed { to_insert, to_delete })
}

fn upsert_batch<Q: Queryable>(
    q: &mut Q,
    schema: &TableSchema,
    key_cols: &[String],
    rows: Vec<Vec<Cell>>,
    batch_size: usize,
) -> mysql::Result<()> {
    let columns = schema.column_names();
    let col_sql = columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(",");
    let row_ph = format!("({})", vec!["?"; columns.len()].join(","));
    let key_set: HashSet<&str> = key_cols.iter().map(String::as_str).collect();
    let non_key: Vec<&str> = columns.iter().filter(|c| !key_set.contains(*c)).copied().collect();
    let update_clause = if non_key.is_empty() {
        String::new()
    } else {
        format!(
            " ON DUPLICATE KEY UPDATE {}",
            non_key
                .iter()
                .map(|c| format!("{0}=VALUES({0})", quote_ident(c)))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let batch_size = safe_batch_size(batch_size, columns.len());
    for chunk in chunked(rows, batch_size) {
        let sql = format!(
            "INSERT INTO {} ({}) VALUES {}{}",
            quote_ident(&schema.name),
            col_sql,
            vec![row_ph.as_str(); chunk.len()].join(","),
            update_clause
        );
        let params: Vec<mysql::Value> = chunk.into_iter().flatten().map(cell_to_value).collect();
        q.exec_drop(sql, params)?;
    }
    Ok(())
}

fn delete_by_key<Q: Queryable>(
    q: &mut Q,
    table: &str,
    key_cols: &[String],
    keys: Vec<Vec<Cell>>,
    batch_size: usize,
) -> mysql::Result<()> {
    let batch_size = safe_batch_size(batch_size, key_cols.len());
    if key_cols.len() == 1 {
        let col_sql = quote_ident(&key_cols[0]);
        for chunk in chunked(keys, batch_size) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("DELETE FROM {} WHERE {} IN ({})", quote_ident(table), col_sql, placeholders);
            let params: Vec<mysql::Value> = chunk
                .into_iter()
                .map(|k| cell_to_value(k.into_iter().next().unwrap()))
                .collect();
            q.exec_drop(sql, params)?;
        }
    } else {
        let col_sql = format!(
            "({})",
            key_cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(",")
        );
        let row_ph = format!("({})", vec!["?"; key_cols.len()].join(","));
        for chunk in chunked(keys, batch_size) {
            let placeholders = vec![row_ph.as_str(); chunk.len()].join(",");
            let sql = format!(
                "DELETE FROM {} WHERE {} IN ({})",
                quote_ident(table),
                col_sql,
                placeholders
            );
            let params: Vec<mysql::Value> = chunk.into_iter().flatten().map(cell_to_value).collect();
            q.exec_drop(sql, params)?;
        }
    }
    Ok(())
}

fn delete_n_matching<Q: Queryable>(
    q: &mut Q,
    table: &str,
    columns: &[&str],
    row: &[Cell],
    n: u64,
) -> mysql::Result<()> {
    let mut conds = Vec::new();
    let mut params = Vec::new();
    for (col, val) in columns.iter().zip(row) {
        match val {
            None => conds.push(format!("{} IS NULL", quote_ident(col))),
            Some(b) => {
                conds.push(format!("{} <=> ?", quote_ident(col)));
                params.push(mysql::Value::Bytes(b.clone()));
            }
        }
    }
    let sql = format!(
        "DELETE FROM {} WHERE {} LIMIT {}",
        quote_ident(table),
        conds.join(" AND "),
        n
    );
    q.exec_drop(sql, params)
}

/// Executes a previously-computed plan. This is the only part of the
/// sync that touches the shared write transaction, so it must run on a
/// single thread (the caller drives this serially — see main.rs).
pub fn apply_table_plan<Q: Queryable>(
    q: &mut Q,
    schema: &TableSchema,
    key_cols: Option<&[String]>,
    plan: TablePlan,
    batch_size: usize,
) -> mysql::Result<()> {
    let columns = schema.column_names();
    match plan {
        TablePlan::New { rows } => {
            if !rows.is_empty() {
                bulk_insert(q, &schema.name, &columns, rows.into_iter(), batch_size)?;
            }
        }
        TablePlan::Keyed { to_insert, to_update, delete_keys } => {
            let key_cols = key_cols.expect("Keyed plan requires key_cols");
            let mut upsert_rows = to_insert;
            upsert_rows.extend(to_update);
            if !upsert_rows.is_empty() {
                upsert_batch(q, schema, key_cols, upsert_rows, batch_size)?;
            }
            if !delete_keys.is_empty() {
                delete_by_key(q, &schema.name, key_cols, delete_keys, batch_size)?;
            }
        }
        TablePlan::Unkeyed { to_insert, to_delete } => {
            if !to_insert.is_empty() {
                bulk_insert(q, &schema.name, &columns, to_insert.into_iter(), batch_size)?;
            }
            for (row, n) in to_delete {
                delete_n_matching(q, &schema.name, &columns, &row, n)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dumpfile;

    fn schema_map(schemas: Vec<TableSchema>) -> HashMap<String, TableSchema> {
        schemas.into_iter().map(|s| (s.name.clone(), s)).collect()
    }

    #[test]
    fn unsafe_key_check_passes_a_clean_single_key_table() {
        let dump = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`id` int NOT NULL, PRIMARY KEY (`id`))",
        )]);
        let local = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`id` int NOT NULL, PRIMARY KEY (`id`))",
        )]);
        let plan = DdlPlan { unchanged: vec!["t".to_string()], ..Default::default() };
        assert!(find_unsafe_key_tables(&dump, &local, &plan).is_empty());
    }

    #[test]
    fn unsafe_key_check_flags_a_second_unique_key_in_the_dump() {
        let dump = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`id` int NOT NULL, `a` int NOT NULL, \
              PRIMARY KEY (`id`), UNIQUE KEY `uk_a` (`a`))",
        )]);
        // even if local matches the dump exactly, it's still ambiguous —
        // both keys are real, either one can trigger ON DUPLICATE KEY UPDATE
        let local = dump.clone();
        let plan = DdlPlan { unchanged: vec!["t".to_string()], ..Default::default() };
        let problems = find_unsafe_key_tables(&dump, &local, &plan);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains('t'));
        assert!(problems[0].contains("more than one"));
    }

    #[test]
    fn unsafe_key_check_flags_drift_between_dump_and_local_key() {
        // dump: unique key on `a`. local: same columns, but the real
        // constraint is on `b` instead (schema-diff signature ignores this).
        let dump = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`a` int NOT NULL, `b` int NOT NULL, UNIQUE KEY `uk_a` (`a`))",
        )]);
        let local = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`a` int NOT NULL, `b` int NOT NULL, UNIQUE KEY `uk_b` (`b`))",
        )]);
        let plan = DdlPlan { unchanged: vec!["t".to_string()], ..Default::default() };
        let problems = find_unsafe_key_tables(&dump, &local, &plan);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("differs between the dump and the local database"));
    }

    #[test]
    fn unsafe_key_check_skips_drift_check_for_freshly_created_tables() {
        // dump/local keys mismatch, but `t` is being created fresh from the
        // dump's own CREATE TABLE text, so it's guaranteed to match once done.
        let dump = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`a` int NOT NULL, UNIQUE KEY `uk_a` (`a`))",
        )]);
        let local = HashMap::new(); // table doesn't exist locally yet
        let plan = DdlPlan { to_create: vec!["t".to_string()], ..Default::default() };
        assert!(find_unsafe_key_tables(&dump, &local, &plan).is_empty());
    }

    #[test]
    fn unsafe_key_check_ignores_tables_with_no_usable_key_at_all() {
        // no PK, no unique key: falls back to unkeyed multiset diff, which
        // never does an upsert, so ambiguity here doesn't matter.
        let dump = schema_map(vec![crate::ddl::parse_create_table(
            b"CREATE TABLE `t` (`a` int NOT NULL)",
        )]);
        let local = dump.clone();
        let plan = DdlPlan { unchanged: vec!["t".to_string()], ..Default::default() };
        assert!(find_unsafe_key_tables(&dump, &local, &plan).is_empty());
    }

    #[test]
    fn explicit_column_list_fills_omitted_column_with_its_default() {
        // dump row omits `status`; a real `INSERT INTO t (id) VALUES (1)`
        // would apply the column's DEFAULT '1', not NULL.
        let data = b"CREATE TABLE `t` (\n\
            `id` int NOT NULL,\n\
            `status` int NOT NULL DEFAULT '1',\n\
            PRIMARY KEY (`id`)\n\
        ) ENGINE=InnoDB;\n\
        INSERT INTO `t` (`id`) VALUES (1),(2);\n";
        let dump = dumpfile::parse_dump(data);
        let schema = &dump.schemas["t"];
        let TablePlan::New { rows } = compute_new_table_plan(schema, &dump.inserts["t"]) else {
            panic!("expected TablePlan::New");
        };
        assert_eq!(
            rows,
            vec![
                vec![Some(b"1".to_vec()), Some(b"1".to_vec())],
                vec![Some(b"2".to_vec()), Some(b"1".to_vec())],
            ]
        );
    }

    #[test]
    fn explicit_column_list_omitted_column_without_default_is_still_null() {
        let data = b"CREATE TABLE `t` (\n\
            `id` int NOT NULL,\n\
            `note` varchar(10) DEFAULT NULL,\n\
            PRIMARY KEY (`id`)\n\
        ) ENGINE=InnoDB;\n\
        INSERT INTO `t` (`id`) VALUES (1);\n";
        let dump = dumpfile::parse_dump(data);
        let schema = &dump.schemas["t"];
        let TablePlan::New { rows } = compute_new_table_plan(schema, &dump.inserts["t"]) else {
            panic!("expected TablePlan::New");
        };
        assert_eq!(rows, vec![vec![Some(b"1".to_vec()), None]]);
    }

    #[test]
    fn bit_literal_row_value_decodes_correctly_through_full_pipeline() {
        let data = b"CREATE TABLE `t` (\n\
            `id` int NOT NULL,\n\
            `flags` bit(4) NOT NULL,\n\
            PRIMARY KEY (`id`)\n\
        ) ENGINE=InnoDB;\n\
        INSERT INTO `t` VALUES (1,b'1010');\n";
        let dump = dumpfile::parse_dump(data);
        let schema = &dump.schemas["t"];
        let TablePlan::New { rows } = compute_new_table_plan(schema, &dump.inserts["t"]) else {
            panic!("expected TablePlan::New");
        };
        assert_eq!(rows, vec![vec![Some(b"1".to_vec()), Some(vec![0x0A])]]);
    }
}
