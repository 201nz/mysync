use std::io::Read;

#[path = "../sqlstream.rs"]
mod sqlstream;
#[path = "../values.rs"]
mod values;
#[path = "../ddl.rs"]
mod ddl;

fn main() {
    let path = std::env::args().nth(1).expect("dump path");
    let mut buf = Vec::new();
    let f = std::fs::File::open(&path).unwrap();
    flate2::read::GzDecoder::new(f).read_to_end(&mut buf).unwrap();

    let mut n = 0;
    let mut no_key = Vec::new();
    for stmt in sqlstream::iter_statements(&buf) {
        if sqlstream::statement_keyword(stmt) == "CREATE" {
            let body = sqlstream::strip_leading_comments(stmt);
            if !body.to_ascii_uppercase().starts_with(b"CREATE TABLE") {
                continue;
            }
            let schema = ddl::parse_create_table(stmt);
            n += 1;
            if schema.key_columns().is_none() {
                no_key.push(schema.name.clone());
            }
        }
    }
    println!("parsed {n} CREATE TABLE statements");
    println!("tables with no usable key ({}): {:?}", no_key.len(), no_key);
}
