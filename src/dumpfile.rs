//! Loads a mysqldump file (plain or gzip) and splits it into the pieces
//! the sync engine needs: one CREATE TABLE per table, and the ordered
//! list of INSERT statements belonging to each table (as byte spans
//! borrowed straight from the loaded dump — no per-row copying at parse
//! time).

use std::collections::HashMap;
use std::io::Read;

use memchr::memchr;

use crate::ddl::{parse_create_table, TableSchema};
use crate::sqlstream;

pub fn read_dump_bytes(path: &str) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if path.ends_with(".gz") {
        let f = std::fs::File::open(path)?;
        flate2::read::GzDecoder::new(f).read_to_end(&mut buf)?;
    } else {
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
    }
    Ok(buf)
}

/// One `INSERT INTO ...` statement, pre-split at the point its `VALUES`
/// row list begins, and with any explicit column list captured (dumps
/// almost always omit it, relying on the table's natural column order,
/// but some tools emit `INSERT INTO t (a,b) VALUES ...` and we should
/// still cope with that).
pub struct InsertStmt<'a> {
    pub data: &'a [u8],
    pub values_start: usize,
    pub explicit_columns: Option<Vec<&'a str>>,
}

impl<'a> InsertStmt<'a> {
    pub fn rows(&self) -> sqlstream::ParenGroups<'a> {
        sqlstream::iter_paren_groups(self.data, self.values_start)
    }
}

/// Returns `(table_name, explicit_columns, values_start, stripped_stmt)`
/// where `values_start` is an offset into `stripped_stmt` (*not* the
/// original `stmt` passed in — comment-stripping narrows the slice, so
/// the caller must store `stripped_stmt` alongside `values_start`, not
/// the original).
fn parse_insert_header(stmt: &[u8]) -> (&str, Option<Vec<&str>>, usize, &[u8]) {
    let stmt = sqlstream::strip_leading_comments(stmt);
    // tolerate "INSERT IGNORE INTO" too
    let after_insert = if stmt[..7.min(stmt.len())].eq_ignore_ascii_case(b"INSERT ") {
        &stmt[7..]
    } else {
        stmt
    };
    let after_into = if after_insert[..7.min(after_insert.len())].eq_ignore_ascii_case(b"IGNORE ")
    {
        &after_insert[7..]
    } else {
        after_insert
    };
    let after_into = after_into.trim_ascii_start();
    assert!(
        after_into[..5.min(after_into.len())].eq_ignore_ascii_case(b"INTO "),
        "not an INSERT statement: {:?}",
        &stmt[..stmt.len().min(80)]
    );
    let rest = after_into[5..].trim_ascii_start();
    let name_start = memchr(b'`', rest).expect("INSERT missing table name");
    let name_end = name_start + 1 + memchr(b'`', &rest[name_start + 1..]).unwrap();
    let table_name = std::str::from_utf8(&rest[name_start + 1..name_end]).unwrap();

    let after_name = rest[name_end + 1..].trim_ascii_start();
    let (explicit_columns, after_columns) = if after_name.first() == Some(&b'(') {
        let close = sqlstream::find_matching_paren(after_name, 0);
        let inner = &after_name[1..close];
        let cols: Vec<&str> = sqlstream::split_toplevel(inner, b',')
            .into_iter()
            .map(|c| {
                let c = c.trim_ascii();
                let c = c.strip_prefix(b"`").unwrap_or(c);
                let c = c.strip_suffix(b"`").unwrap_or(c);
                std::str::from_utf8(c).unwrap()
            })
            .collect();
        (Some(cols), &after_name[close + 1..])
    } else {
        (None, after_name)
    };
    let after_columns = after_columns.trim_ascii_start();
    assert!(
        after_columns[..6.min(after_columns.len())].eq_ignore_ascii_case(b"VALUES"),
        "INSERT missing VALUES"
    );

    // Offset relative to `stmt` (the comment-stripped slice) — see the
    // doc comment above on why that, and not the original statement.
    let values_rel_start = (after_columns.as_ptr() as usize - stmt.as_ptr() as usize) + 6;
    (table_name, explicit_columns, values_rel_start, stmt)
}

pub struct ParsedDump<'a> {
    pub table_order: Vec<String>,
    pub schemas: HashMap<String, TableSchema>,
    pub inserts: HashMap<String, Vec<InsertStmt<'a>>>,
}

pub fn parse_dump(data: &[u8]) -> ParsedDump<'_> {
    let mut table_order = Vec::new();
    let mut schemas = HashMap::new();
    let mut inserts: HashMap<String, Vec<InsertStmt<'_>>> = HashMap::new();

    for stmt in sqlstream::iter_statements(data) {
        match sqlstream::statement_keyword(stmt).as_str() {
            "CREATE" => {
                let body = sqlstream::strip_leading_comments(stmt);
                if body.len() < 12 || !body[..12].eq_ignore_ascii_case(b"CREATE TABLE") {
                    continue; // CREATE VIEW/TRIGGER/etc: not supported, skipped
                }
                let schema = parse_create_table(stmt);
                table_order.push(schema.name.clone());
                inserts.entry(schema.name.clone()).or_default();
                schemas.insert(schema.name.clone(), schema);
            }
            "INSERT" => {
                let (table_name, explicit_columns, values_start, stripped) =
                    parse_insert_header(stmt);
                inserts
                    .entry(table_name.to_string())
                    .or_default()
                    .push(InsertStmt {
                        data: stripped,
                        values_start,
                        explicit_columns,
                    });
            }
            _ => {} // DROP/LOCK/UNLOCK/SET/ALTER...DISABLE KEYS: irrelevant, we derive
                    // our own DDL from the schema diff and our own DML from row diffs.
        }
    }

    ParsedDump {
        table_order,
        schemas,
        inserts,
    }
}
