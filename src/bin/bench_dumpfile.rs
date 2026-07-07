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
#[path = "../dumpfile.rs"]
mod dumpfile;

fn main() {
    let path = std::env::args().nth(1).expect("dump path");
    let t0 = Instant::now();
    let mut buf = Vec::new();
    if path.ends_with(".gz") {
        let f = std::fs::File::open(&path).unwrap();
        flate2::read::GzDecoder::new(f).read_to_end(&mut buf).unwrap();
    } else {
        std::fs::File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
    }
    println!("read: {:?}, {} bytes", t0.elapsed(), buf.len());

    let t0 = Instant::now();
    let dump = dumpfile::parse_dump(&buf);
    println!(
        "parse_dump: {:?}  tables={} insert_stmts={}",
        t0.elapsed(),
        dump.schemas.len(),
        dump.inserts.values().map(|v| v.len()).sum::<usize>()
    );

    let t0 = Instant::now();
    let mut n_rows = 0u64;
    let mut n_values = 0u64;
    for (_table, stmts) in &dump.inserts {
        for stmt in stmts {
            for row in stmt.rows() {
                n_rows += 1;
                for tok in sqlstream::split_toplevel(row, b',') {
                    let _ = values::parse_value_token(tok).into_mysql_value();
                    n_values += 1;
                }
            }
        }
    }
    println!("full row parse+coerce: {:?}  rows={n_rows} values={n_values}", t0.elapsed());
}
