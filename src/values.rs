//! Parses INSERT ... VALUES tokens into `mysql::Value`, the same
//! representation a text-protocol query result gives back (verified
//! empirically against a real MariaDB connection — see probe_mysql.rs):
//! every column, regardless of declared type, comes back as either
//! `Value::NULL` or `Value::Bytes(text)`; there's no server-side type
//! conversion in the text protocol. And parameter binding for a prepared
//! statement accepts a plain `Value::Bytes(text)` into a typed column
//! (int, date, decimal, ...) just fine — MySQL casts it same as any
//! string-typed bind parameter.
//!
//! That means dump rows and locally-fetched rows can be compared with
//! plain `==` on `Vec<mysql::Value>` with *no* per-column-type coercion
//! needed at all (earlier draft of this module built a whole int/decimal/
//! date/time coercion table for this — turned out unnecessary once this
//! was checked against a real connection instead of assumed).
//!
//! The one thing that still matters here: unescaping the SQL literal
//! into its raw byte content. That's still zero-copy in the common case
//! (no escape sequences present).

use std::borrow::Cow;

use memchr::{memchr, memchr2};
use mysql::Value;

/// Output of parsing one VALUES token: syntax-level only, no MySQL type
/// involved (there's nothing left to coerce — see module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum RawValue<'a> {
    Null,
    /// Unescaped string/blob content, or a bare numeric literal's ascii
    /// text untouched (no escaping applies to those).
    Bytes(Cow<'a, [u8]>),
}

impl<'a> RawValue<'a> {
    pub fn into_mysql_value(self) -> Value {
        match self {
            RawValue::Null => Value::NULL,
            RawValue::Bytes(b) => Value::Bytes(b.into_owned()),
        }
    }

    /// Same data as `into_mysql_value`, but as `Option<Vec<u8>>` — the
    /// sync engine's internal row representation. `mysql::Value` only
    /// derives `PartialEq`/`PartialOrd` (it has float variants, so no
    /// `Eq`/`Hash`), but every value here is only ever `NULL` or `Bytes`
    /// (verified against a real connection — see module docs), and
    /// `Option<Vec<u8>>` is directly hashable, so rows can be dict/set
    /// keys for the diffing step without a wrapper type.
    pub fn into_cell(self) -> Option<Vec<u8>> {
        match self {
            RawValue::Null => None,
            RawValue::Bytes(b) => Some(b.into_owned()),
        }
    }
}

/// The other direction of `into_cell`, used right before binding a query
/// parameter.
pub fn cell_to_value(cell: Option<Vec<u8>>) -> Value {
    match cell {
        None => Value::NULL,
        Some(b) => Value::Bytes(b),
    }
}

fn find_special(inner: &[u8], quote: u8) -> Option<usize> {
    if quote == b'`' {
        memchr(b'`', inner)
    } else {
        memchr2(quote, b'\\', inner)
    }
}

/// Unescape the content between a pair of quotes (quotes already
/// stripped by the caller). Handles `\`-escaping (not recognized for
/// backtick identifiers, matching MySQL) and doubled-quote-of-the-same-
/// kind escaping (`''`, `""`, `` `` ``). Borrows when there's nothing to
/// unescape (the common case for plain text/numbers).
fn unescape_span<'a>(inner: &'a [u8], quote: u8) -> Cow<'a, [u8]> {
    let Some(first) = find_special(inner, quote) else {
        return Cow::Borrowed(inner);
    };
    let mut out = Vec::with_capacity(inner.len());
    out.extend_from_slice(&inner[..first]);
    let mut i = first;
    let len = inner.len();
    while i < len {
        let b = inner[i];
        if b == b'\\' && quote != b'`' {
            if i + 1 < len {
                out.push(unescape_char(inner[i + 1]));
                i += 2;
            } else {
                i += 1; // trailing lone backslash; defensive, shouldn't happen
            }
            continue;
        }
        if b == quote {
            if i + 1 < len && inner[i + 1] == quote {
                out.push(quote);
                i += 2;
                continue;
            }
            // a lone, undoubled quote-of-the-same-kind inside the span
            // shouldn't happen (the tokenizer's skip_quoted wouldn't have
            // ended the string there) but just pass it through defensively
            out.push(b);
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
    }
    Cow::Owned(out)
}

fn unescape_char(c: u8) -> u8 {
    match c {
        b'0' => 0,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'b' => 0x08,
        b'Z' => 0x1a,
        // MySQL's rule: an unrecognized `\<c>` escape just drops the
        // backslash and keeps `<c>` literally (this also covers `\'`,
        // `\"`, `\\`). `\%`/`\_` (LIKE-pattern escapes) fall through here
        // too; that's fine since LIKE-escaped data doesn't occur in real
        // column content, only in pattern strings.
        other => other,
    }
}

pub fn parse_value_token(raw: &[u8]) -> RawValue<'_> {
    parse_value_token_typed(raw, None)
}

/// Same as `parse_value_token`, but when `bit_width` is `Some(n)` (the
/// token belongs to a `BIT(n)` column) also recognizes MySQL's bit-literal
/// syntax (`b'1010'` / `B'1010'`), decoding it into the same big-endian,
/// `ceil(n/8)`-byte representation a text-protocol `SELECT` of that column
/// would return. Without this, a bit literal falls through to the
/// catch-all branch below and gets stored as its literal source text
/// (e.g. the 7 bytes `b'1010'`) instead of the actual bit value.
pub fn parse_value_token_typed(raw: &[u8], bit_width: Option<u32>) -> RawValue<'_> {
    let raw = raw.trim_ascii();
    if raw.eq_ignore_ascii_case(b"NULL") {
        return RawValue::Null;
    }
    if let Some(width) = bit_width {
        if let Some(bits) = strip_bit_literal(raw) {
            return RawValue::Bytes(Cow::Owned(encode_bit_value(bits, width)));
        }
    }
    match raw.first() {
        Some(b'\'') => RawValue::Bytes(unescape_span(&raw[1..raw.len() - 1], b'\'')),
        Some(b'"') => RawValue::Bytes(unescape_span(&raw[1..raw.len() - 1], b'"')),
        Some(b'0') if raw.len() > 2 && (raw[1] == b'x' || raw[1] == b'X') => {
            let hex = &raw[2..];
            let bytes = if hex.len() % 2 == 1 {
                let mut padded = Vec::with_capacity(hex.len() + 1);
                padded.push(b'0');
                padded.extend_from_slice(hex);
                hex_decode(&padded)
            } else {
                hex_decode(hex)
            };
            RawValue::Bytes(Cow::Owned(bytes))
        }
        _ => RawValue::Bytes(Cow::Borrowed(raw)),
    }
}

/// Recognizes `b'...'` / `B'...'` (MySQL bit-literal syntax) and returns
/// the span of `0`/`1` characters between the quotes.
fn strip_bit_literal(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() < 3 || (raw[0] != b'b' && raw[0] != b'B') || raw[1] != b'\'' {
        return None;
    }
    raw.strip_prefix(b"b'")
        .or_else(|| raw.strip_prefix(b"B'"))
        .and_then(|rest| rest.strip_suffix(b"'"))
}

/// Encodes the value of a bit-literal (`bits`: ascii `0`/`1` characters,
/// MSB first) the way MySQL returns a `BIT(width)` column over the text
/// protocol: big-endian, `ceil(width/8)` bytes, zero-padded on the left.
fn encode_bit_value(bits: &[u8], width: u32) -> Vec<u8> {
    let mut value: u64 = 0;
    for &b in bits {
        value = (value << 1) | u64::from(b == b'1');
    }
    let nbytes = (width as usize).div_ceil(8).max(1);
    let mut out = vec![0u8; nbytes];
    for (i, byte) in out.iter_mut().rev().enumerate() {
        *byte = (value >> (i * 8)) as u8;
    }
    out
}

fn hex_decode(hex: &[u8]) -> Vec<u8> {
    fn nibble(b: u8) -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => 0,
        }
    }
    hex.chunks_exact(2)
        .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_case_insensitive() {
        assert_eq!(parse_value_token(b"NULL"), RawValue::Null);
        assert_eq!(parse_value_token(b"null"), RawValue::Null);
    }

    #[test]
    fn plain_string_borrows_zero_copy() {
        match parse_value_token(b"'Council'") {
            RawValue::Bytes(Cow::Borrowed(b)) => assert_eq!(b, b"Council"),
            other => panic!("expected a borrowed span, got {other:?}"),
        }
    }

    #[test]
    fn backslash_escaped_quote_from_real_dump_style_data() {
        // real style: "...client's reference..." dumped as "client\'s reference"
        let raw = br"'client\'s reference'";
        match parse_value_token(raw) {
            RawValue::Bytes(b) => assert_eq!(&*b, b"client's reference"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn doubled_quote_escape() {
        match parse_value_token(b"'it''s here'") {
            RawValue::Bytes(b) => assert_eq!(&*b, b"it's here"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backslash_n_is_a_real_newline_not_null() {
        let raw = br"'line1\nline2'";
        match parse_value_token(raw) {
            RawValue::Bytes(b) => assert_eq!(&*b, b"line1\nline2"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hex_literal_decodes_to_bytes() {
        match parse_value_token(b"0x48656c6c6f") {
            RawValue::Bytes(b) => assert_eq!(&*b, b"Hello"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bare_number_is_untouched_text() {
        match parse_value_token(b"-36.848461") {
            RawValue::Bytes(Cow::Borrowed(b)) => assert_eq!(b, b"-36.848461"),
            other => panic!("expected a borrowed span, got {other:?}"),
        }
    }

    #[test]
    fn bit_literal_decodes_to_binary_value_not_source_text() {
        // BIT(4) column storing b'1010' (10) should come back over the
        // text protocol as the single byte 0x0A, not the literal text.
        match parse_value_token_typed(b"b'1010'", Some(4)) {
            RawValue::Bytes(b) => assert_eq!(&*b, &[0x0Au8][..]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bit_literal_uppercase_prefix_and_wider_column() {
        // BIT(16) storing 0b1010 (10) pads to 2 bytes: 0x00 0x0A.
        match parse_value_token_typed(b"B'1010'", Some(16)) {
            RawValue::Bytes(b) => assert_eq!(&*b, &[0x00u8, 0x0A][..]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bit_literal_without_bit_width_hint_falls_back_to_source_text() {
        // no column-type context (bit_width: None): can't safely special-
        // case this, so it's treated like any other non-quoted token.
        match parse_value_token(b"b'1010'") {
            RawValue::Bytes(Cow::Borrowed(b)) => assert_eq!(b, b"b'1010'"),
            other => panic!("expected a borrowed span, got {other:?}"),
        }
    }

    #[test]
    fn into_mysql_value_matches_text_protocol_shape() {
        assert_eq!(RawValue::Null.into_mysql_value(), Value::NULL);
        assert_eq!(
            parse_value_token(b"'2025-11-10 14:36:37'").into_mysql_value(),
            Value::Bytes(b"2025-11-10 14:36:37".to_vec())
        );
    }
}
