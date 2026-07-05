// This binary only exercises a slice of the modules it reuses via
// `#[path]` below, so the rest is expected to look unused from here.
#![allow(dead_code)]

use std::io::Read;
use std::time::Instant;

#[path = "../sqlstream.rs"]
mod sqlstream;
#[path = "../ddl.rs"]
mod ddl;
#[path = "../values.rs"]
mod values;

fn main() {
    let path = std::env::args().nth(1).expect("dump path");
    let t0 = Instant::now();
    let mut buf = Vec::new();
    let f = std::fs::File::open(&path).unwrap();
    flate2::read::GzDecoder::new(f).read_to_end(&mut buf).unwrap();
    println!("decompress: {:?}, {} bytes", t0.elapsed(), buf.len());

    // Parse every CREATE TABLE up front so we know each column's type,
    // same as the real dumpfile::parse_dump will.
    let t0 = Instant::now();
    let mut schemas: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let mut insert_spans: Vec<(String, usize, usize)> = Vec::new();
    let base = buf.as_ptr() as usize;
    for stmt in sqlstream::iter_statements(&buf) {
        let kw = sqlstream::statement_keyword(stmt);
        if kw == "CREATE" {
            // minimal CREATE TABLE parse just for this bench: table name + column types
            let body = sqlstream::strip_leading_comments(stmt);
            if !body.to_ascii_uppercase().starts_with(b"CREATE TABLE") {
                continue;
            }
            let name_start = memchr::memchr(b'`', body).unwrap() + 1;
            let name_end = name_start + memchr::memchr(b'`', &body[name_start..]).unwrap();
            let name = String::from_utf8_lossy(&body[name_start..name_end]).into_owned();
            let open = memchr::memchr(b'(', &body[name_end..]).unwrap() + name_end;
            let close = sqlstream::find_matching_paren(body, open);
            let inner = &body[open + 1..close];
            let mut types = Vec::new();
            for item in sqlstream::split_toplevel(inner, b',') {
                let item = item.trim_ascii();
                if item.first() == Some(&b'`') {
                    let cn_end = 1 + memchr::memchr(b'`', &item[1..]).unwrap();
                    let rest = item[cn_end + 1..].trim_ascii_start();
                    let type_end = rest.iter().position(|b| !b.is_ascii_alphabetic()).unwrap_or(rest.len());
                    types.push(String::from_utf8_lossy(&rest[..type_end]).to_lowercase());
                }
            }
            schemas.insert(name, types);
        } else if kw == "INSERT" {
            let table_start = memchr::memchr(b'`', stmt).unwrap() + 1;
            let table_end = table_start + memchr::memchr(b'`', &stmt[table_start..]).unwrap();
            let table = String::from_utf8_lossy(&stmt[table_start..table_end]).into_owned();
            let start = stmt.as_ptr() as usize - base;
            insert_spans.push((table, start, start + stmt.len()));
        }
    }
    println!("parse CREATE TABLEs + collect INSERT spans: {:?}  tables={}", t0.elapsed(), schemas.len());

    let _ = &schemas; // no longer needed for value parsing (see values.rs docs)
    let t0 = Instant::now();
    let mut n_rows = 0u64;
    let mut n_values = 0u64;
    for (_table, s, e) in &insert_spans {
        let stmt = &buf[*s..*e];
        let vpos = memchr::memmem::find(stmt, b"VALUES").map(|p| p + 6).unwrap();
        for group in sqlstream::iter_paren_groups(stmt, vpos) {
            n_rows += 1;
            for tok in sqlstream::split_toplevel(group, b',') {
                let _v = values::parse_value_token(tok).into_mysql_value();
                n_values += 1;
            }
        }
    }
    println!("parse+to_mysql_value all rows: {:?}  rows={n_rows} values={n_values}", t0.elapsed());
}
