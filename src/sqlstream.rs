//! Splits a mysqldump byte buffer into top-level statements, and tokenizes
//! CREATE TABLE / INSERT VALUES bodies. Everything here borrows from the
//! original buffer (`&'a [u8]` in, `&'a [u8]` slices out) — no per-value
//! allocation.
//!
//! `memchr` jumps straight to the next byte that could possibly matter (a
//! quote, a paren, a separator, a comment starter) — each jump is one
//! SIMD-accelerated native call covering however many "boring" bytes lie
//! in between, instead of visiting every one of them with a manual
//! byte-at-a-time loop.

use memchr::{memchr, memchr2, memchr3};

/// Find the nearest position at or after `from` matching any byte in any
/// of `groups` (each a 1-3 byte needle set), *without* letting a rare
/// needle force an unbounded scan whose result gets discarded anyway.
///
/// The naive version of this (independently `memchr`-search each group
/// over the full remaining buffer, then take the min of the results) is
/// pathological: if `groups[1]` doesn't occur again for the next 500KB
/// but `groups[0]` matches 3 bytes away, that 500KB scan for `groups[1]`
/// was wasted — and gets repeated at every subsequent position until the
/// scan finally passes it, i.e. near-quadratic blowup. This was the
/// actual bug behind an early 100s+ benchmark of the statement splitter.
/// Scanning in bounded chunks means no single probe ever scans past a
/// point a *previous* probe already ruled out as unnecessary.
fn find_nearest(data: &[u8], from: usize, groups: &[&[u8]]) -> Option<usize> {
    const CHUNK: usize = 1 << 16;
    let len = data.len();
    let mut lo = from;
    while lo < len {
        let hi = (lo + CHUNK).min(len);
        let window = &data[lo..hi];
        let mut best: Option<usize> = None;
        for &needle in groups {
            let found = match *needle {
                [a] => memchr(a, window),
                [a, b] => memchr2(a, b, window),
                [a, b, c] => memchr3(a, b, c, window),
                _ => unreachable!("needle groups must be 1-3 bytes"),
            };
            if let Some(o) = found {
                best = Some(best.map_or(o, |b: usize| b.min(o)));
            }
        }
        if let Some(o) = best {
            return Some(lo + o);
        }
        lo = hi;
    }
    None
}

/// Skip over a quoted span starting at `pos` (`data[pos]` must be `'`,
/// `"`, or `` ` ``). Returns the index just past the closing quote.
/// Handles `\`-escaping (not recognized inside backtick identifiers,
/// matching MySQL) and doubled-quote-of-the-same-kind escaping (`''`,
/// `""`, `` `` ``).
pub(crate) fn skip_quoted(data: &[u8], pos: usize) -> usize {
    let quote = data[pos];
    let len = data.len();
    let mut i = pos + 1;
    loop {
        if i >= len {
            return len; // unterminated; defensive, shouldn't happen in a valid dump
        }
        let next = if quote == b'`' {
            memchr(b'`', &data[i..]).map(|o| o + i)
        } else {
            memchr2(quote, b'\\', &data[i..]).map(|o| o + i)
        };
        let Some(j) = next else { return len };
        if data[j] == b'\\' {
            i = j + 2;
            continue;
        }
        // data[j] == quote
        if j + 1 < len && data[j + 1] == quote {
            i = j + 2; // doubled quote: literal quote char, string continues
            continue;
        }
        return j + 1;
    }
}

fn find_block_comment_end(data: &[u8], start: usize) -> usize {
    // data[start..start+2] == "/*"
    let len = data.len();
    let mut i = start + 2;
    loop {
        match memchr(b'*', &data[i..]) {
            Some(o) => {
                let star = i + o;
                if star + 1 < len && data[star + 1] == b'/' {
                    return star + 2;
                }
                i = star + 1;
            }
            None => return len,
        }
    }
}

/// Splits `data` into top-level SQL statements (semicolon-terminated,
/// quote/comment aware).
pub struct Statements<'a> {
    data: &'a [u8],
    pos: usize,
}

pub fn iter_statements(data: &[u8]) -> Statements<'_> {
    Statements { data, pos: 0 }
}

impl<'a> Statements<'a> {
    /// Scan from self.pos for the next top-level `;`, skipping quotes and
    /// comments. On success, advances self.pos past the `;` and returns
    /// its index; on reaching the end of the buffer with no more `;`,
    /// returns None (the trailing fragment, if any, is discarded, matching
    /// mysqldump output which always terminates its last real statement
    /// with a semicolon).
    fn scan_to_semicolon(&mut self) -> Option<usize> {
        let data = self.data;
        let len = data.len();
        let mut i = self.pos;
        loop {
            if i >= len {
                return None;
            }
            let Some(j) = find_nearest(data, i, &[b"'\"`", b"-/#", b";"]) else {
                return None;
            };
            match data[j] {
                b'\'' | b'"' | b'`' => i = skip_quoted(data, j),
                b';' => {
                    self.pos = j + 1;
                    return Some(j);
                }
                b'-' if data.get(j + 1) == Some(&b'-') => {
                    i = memchr(b'\n', &data[j..]).map_or(len, |o| j + o + 1);
                }
                b'#' => {
                    i = memchr(b'\n', &data[j..]).map_or(len, |o| j + o + 1);
                }
                b'/' if data.get(j + 1) == Some(&b'*') => {
                    i = find_block_comment_end(data, j);
                }
                _ => i = j + 1, // lone '-' or '/', not a comment starter
            }
        }
    }
}

impl<'a> Iterator for Statements<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let stmt_start = self.pos;
            let semi_pos = self.scan_to_semicolon()?;
            let stmt = self.data[stmt_start..semi_pos].trim_ascii();
            if !stmt.is_empty() {
                return Some(stmt);
            }
            // empty (e.g. a run of pure comments between two ';'s): keep scanning
        }
    }
}

/// Skip any leading `--`/`#`/`/* */` comments and whitespace, so a
/// statement block that starts with mysqldump's decorative "Table
/// structure for table `x`" comments still classifies correctly.
pub fn strip_leading_comments(stmt: &[u8]) -> &[u8] {
    let mut rest = stmt;
    loop {
        let trimmed = rest.trim_ascii_start();
        if trimmed.starts_with(b"--") {
            match memchr(b'\n', trimmed) {
                Some(nl) => {
                    rest = &trimmed[nl + 1..];
                    continue;
                }
                None => return rest, // no trailing newline: leave the comment intact, unconsumed
            }
        } else if trimmed.starts_with(b"#") {
            match memchr(b'\n', trimmed) {
                Some(nl) => {
                    rest = &trimmed[nl + 1..];
                    continue;
                }
                None => return rest,
            }
        } else if trimmed.starts_with(b"/*") {
            let end = find_block_comment_end(trimmed, 0);
            if end >= trimmed.len() && !trimmed[..end].ends_with(b"*/") {
                return rest; // unterminated block comment
            }
            rest = &trimmed[end..];
            continue;
        } else {
            return trimmed;
        }
    }
}

/// Returns the first real SQL keyword of `stmt`, uppercased.
pub fn statement_keyword(stmt: &[u8]) -> String {
    let rest = strip_leading_comments(stmt).trim_ascii_start();
    let end = rest
        .iter()
        .position(|b| !b.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).to_uppercase()
}

/// Returns the index of the `)` matching the `(` at `open_paren_pos`.
/// Quote-aware: skips over quoted strings so parens inside string/ident
/// literals don't confuse depth tracking.
pub fn find_matching_paren(data: &[u8], open_paren_pos: usize) -> usize {
    debug_assert_eq!(data[open_paren_pos], b'(');
    let len = data.len();
    let mut depth: i32 = 0;
    let mut i = open_paren_pos;
    loop {
        if i >= len {
            panic!("unbalanced parentheses");
        }
        let Some(j) = find_nearest(data, i, &[b"'\"`", b"()"]) else {
            panic!("unbalanced parentheses");
        };
        match data[j] {
            b'\'' | b'"' | b'`' => i = skip_quoted(data, j),
            b'(' => {
                depth += 1;
                i = j + 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
                i = j + 1;
            }
            _ => unreachable!(),
        }
    }
}

/// Splits `data` on the single byte `sep` at paren-depth 0, quote-aware.
/// Used both for CREATE TABLE column/key definitions and for the
/// individual value tokens inside one INSERT row tuple.
pub fn split_toplevel(data: &[u8], sep: u8) -> Vec<&[u8]> {
    let len = data.len();
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0usize;
    let mut last_cut = 0usize;
    loop {
        if i >= len {
            break;
        }
        let Some(j) = find_nearest(data, i, &[b"'\"`", b"()", &[sep]]) else {
            break;
        };
        match data[j] {
            b'\'' | b'"' | b'`' => i = skip_quoted(data, j),
            b'(' => {
                depth += 1;
                i = j + 1;
            }
            b')' => {
                depth -= 1;
                i = j + 1;
            }
            b if b == sep => {
                if depth == 0 {
                    parts.push(&data[last_cut..j]);
                    last_cut = j + 1;
                }
                i = j + 1;
            }
            _ => unreachable!(),
        }
    }
    parts.push(&data[last_cut..]);
    parts
}

/// Yields the inner content of each top-level `(...)` group in `data`,
/// e.g. pulling individual row tuples out of an
/// `INSERT ... VALUES (a,b),(c,d), ...` clause.
pub struct ParenGroups<'a> {
    data: &'a [u8],
    pos: usize,
}

pub fn iter_paren_groups(data: &[u8], start: usize) -> ParenGroups<'_> {
    ParenGroups { data, pos: start }
}

impl<'a> Iterator for ParenGroups<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let data = self.data;
        let len = data.len();
        loop {
            if self.pos >= len {
                return None;
            }
            let Some(j) = find_nearest(data, self.pos, &[b"'\"`", b"("]) else {
                return None;
            };
            match data[j] {
                b'\'' | b'"' | b'`' => self.pos = skip_quoted(data, j),
                b'(' => {
                    let close = find_matching_paren(data, j);
                    self.pos = close + 1;
                    return Some(&data[j + 1..close]);
                }
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_statements() {
        let data = b"CREATE TABLE t (a int);\nINSERT INTO t VALUES (1);\n";
        let stmts: Vec<&[u8]> = iter_statements(data).collect();
        assert_eq!(stmts, vec![
            &b"CREATE TABLE t (a int)"[..],
            &b"INSERT INTO t VALUES (1)"[..],
        ]);
    }

    #[test]
    fn ignores_semicolons_inside_strings_and_comments() {
        let data = b"-- a;b\nINSERT INTO t VALUES ('a;b', \"c;d\", `e;f`);\n";
        let stmts: Vec<&[u8]> = iter_statements(data).collect();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].starts_with(b"-- a;b\nINSERT"));
    }

    #[test]
    fn skips_block_and_hash_comments() {
        // A comment attaches to the *following* statement chunk (same as
        // mysqldump's "-- Table structure..." comments preceding a DROP
        // TABLE): there's no unquoted ';' between "# e;f" and "SELECT 2",
        // so they're one chunk. statement_keyword()/strip_leading_comments()
        // are what recover "SELECT" as the real keyword.
        let data = b"/* c;d */ SELECT 1; # e;f\nSELECT 2;";
        let stmts: Vec<&[u8]> = iter_statements(data).collect();
        assert_eq!(
            stmts,
            vec![&b"/* c;d */ SELECT 1"[..], &b"# e;f\nSELECT 2"[..]]
        );
        assert_eq!(statement_keyword(stmts[1]), "SELECT");
    }

    #[test]
    fn keyword_skips_decorative_comments() {
        let stmt = b"--\n-- Table structure for table `x`\n--\n\nDROP TABLE IF EXISTS `x`";
        assert_eq!(statement_keyword(stmt), "DROP");
    }

    #[test]
    fn matching_paren_is_quote_aware() {
        let data = b"(a, 'b)c', (nested), `d)e`)";
        assert_eq!(find_matching_paren(data, 0), data.len() - 1);
    }

    #[test]
    fn split_toplevel_respects_depth_and_quotes() {
        let data = b"1,'a,b',(2,3),`c,d`,4";
        let parts = split_toplevel(data, b',');
        assert_eq!(
            parts,
            vec![
                &b"1"[..],
                &b"'a,b'"[..],
                &b"(2,3)"[..],
                &b"`c,d`"[..],
                &b"4"[..],
            ]
        );
    }

    #[test]
    fn paren_groups_pulls_out_row_tuples() {
        let data = b"(1,2),(3,'x,y'),(5,6)";
        let groups: Vec<&[u8]> = iter_paren_groups(data, 0).collect();
        assert_eq!(groups, vec![&b"1,2"[..], &b"3,'x,y'"[..], &b"5,6"[..]]);
    }
}
