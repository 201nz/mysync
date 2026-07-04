//! Local MySQL/MariaDB connection helpers and schema introspection.

use std::collections::HashMap;

use mysql::prelude::*;
use mysql::{Conn, OptsBuilder};

use crate::ddl::{Column, TableSchema};

pub struct ConnParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

pub fn connect(params: &ConnParams) -> mysql::Result<Conn> {
    // mysqldump captures TIMESTAMP columns under `SET TIME_ZONE='+00:00'`
    // (its standard header); match that session time zone here so reading
    // existing TIMESTAMP values back gives the same wall-clock text the
    // dump has, instead of both drifting by the server's local UTC offset.
    // DATETIME columns are unaffected (MySQL never converts those) —
    // without this, every TIMESTAMP column looks "changed" even when
    // the underlying data is identical.
    //
    // mysqldump also wraps its whole restore in
    // `SET FOREIGN_KEY_CHECKS=0` / `=1`, since it dumps tables in
    // whatever order and can't guarantee a referenced table is created
    // (or a referenced row inserted) before the table/row that points to
    // it — and our own CREATE TABLE / INSERT ordering isn't
    // dependency-aware either (worse, tables are diffed across a thread
    // pool in whatever order workers finish, so a dependent table can
    // easily get its rows inserted before the table it references does).
    // Matching that same disable-for-the-whole-restore convention here
    // is what a real mysqldump restore already relies on, not a new risk.
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(params.host.clone()))
        .tcp_port(params.port)
        .user(Some(params.user.clone()))
        .pass(Some(params.password.clone()))
        .db_name(Some(params.database.clone()))
        .init(vec!["SET time_zone='+00:00'", "SET FOREIGN_KEY_CHECKS=0"]);
    Conn::new(opts)
}

/// Returns `{table_name: TableSchema}` for all base tables currently in
/// `database`, built from information_schema (no raw CREATE TABLE text —
/// not needed since we only ever recreate tables from the dump's own
/// CREATE TABLE statement, never from a locally-derived one).
pub fn fetch_local_tables(
    conn: &mut Conn,
    database: &str,
) -> mysql::Result<HashMap<String, TableSchema>> {
    let table_names: Vec<String> = conn.exec(
        "SELECT TABLE_NAME FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA=? AND TABLE_TYPE='BASE TABLE'",
        (database,),
    )?;

    let column_rows: Vec<(String, String, String, String)> = conn.exec(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=? \
         ORDER BY TABLE_NAME, ORDINAL_POSITION",
        (database,),
    )?;
    let mut columns_by_table: HashMap<String, Vec<Column>> = HashMap::new();
    for (table_name, col_name, data_type, is_nullable) in column_rows {
        columns_by_table
            .entry(table_name)
            .or_default()
            .push(Column {
                name: col_name,
                r#type: data_type.to_lowercase(),
                nullable: is_nullable == "YES",
            });
    }

    let pk_rows: Vec<(String, String)> = conn.exec(
        "SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA=? AND CONSTRAINT_NAME='PRIMARY' \
         ORDER BY TABLE_NAME, ORDINAL_POSITION",
        (database,),
    )?;
    let mut pk_by_table: HashMap<String, Vec<String>> = HashMap::new();
    for (table_name, col_name) in pk_rows {
        pk_by_table.entry(table_name).or_default().push(col_name);
    }

    let uk_rows: Vec<(String, String, String)> = conn.exec(
        "SELECT TABLE_NAME, INDEX_NAME, COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA=? AND NON_UNIQUE=0 AND INDEX_NAME<>'PRIMARY' \
         ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
        (database,),
    )?;
    let mut uk_by_table: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    for (table_name, index_name, col_name) in uk_rows {
        uk_by_table
            .entry(table_name)
            .or_default()
            .entry(index_name)
            .or_default()
            .push(col_name);
    }

    let mut schemas = HashMap::new();
    for name in table_names {
        let unique_keys = uk_by_table
            .remove(&name)
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        schemas.insert(
            name.clone(),
            TableSchema {
                name: name.clone(),
                columns: columns_by_table.remove(&name).unwrap_or_default(),
                primary_key: pk_by_table.remove(&name).unwrap_or_default(),
                unique_keys,
                raw_statement: Vec::new(),
            },
        );
    }
    Ok(schemas)
}
