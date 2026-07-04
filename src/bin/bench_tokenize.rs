use std::io::Read;
use std::time::Instant;

#[path = "../sqlstream.rs"]
mod sqlstream;

fn main() {
    let path = std::env::args().nth(1).expect("dump path");
    let t0 = Instant::now();
    let mut buf = Vec::new();
    let f = std::fs::File::open(&path).unwrap();
    flate2::read::GzDecoder::new(f).read_to_end(&mut buf).unwrap();
    println!("decompress: {:?}, {} bytes", t0.elapsed(), buf.len());

    let t0 = Instant::now();
    let mut n_stmt = 0u64;
    let mut n_create = 0u64;
    let mut n_insert = 0u64;
    let mut n_rows = 0u64;
    let mut n_tokens = 0u64;
    for stmt in sqlstream::iter_statements(&buf) {
        n_stmt += 1;
        let kw = sqlstream::statement_keyword(stmt);
        if kw == "CREATE" {
            n_create += 1;
        } else if kw == "INSERT" {
            n_insert += 1;
            if let Some(vpos) = find_values_pos(stmt) {
                for group in sqlstream::iter_paren_groups(stmt, vpos) {
                    n_rows += 1;
                    n_tokens += sqlstream::split_toplevel(group, b',').len() as u64;
                }
            }
        }
    }
    println!(
        "tokenize (single fused pass): {:?}  statements={n_stmt} creates={n_create} inserts={n_insert} rows={n_rows} tokens={n_tokens}",
        t0.elapsed()
    );
}

fn find_values_pos(stmt: &[u8]) -> Option<usize> {
    memchr::memmem::find(stmt, b"VALUES").map(|p| p + 6)
}
