//! Parses CREATE TABLE statements into structured form.

use memchr::memchr;

use crate::sqlstream;
use crate::values;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub r#type: String, // base type keyword, lowercased: int, varchar, decimal, ...
    pub nullable: bool,
    /// Declared width for a `bit` column (the `n` in `BIT(n)`, defaulting
    /// to 1 when unparenthesized); `None` for every other type. Needed to
    /// decode that column's dump-row bit-literal values (`b'1010'`) into
    /// the same byte shape a live `SELECT` of the column returns — see
    /// values.rs's `parse_value_token_typed`.
    pub bit_width: Option<u32>,
    /// The column's `DEFAULT` clause, if any — needed so a dump row using
    /// an explicit column list (`INSERT INTO t (a,b) VALUES ...`) can fill
    /// in an omitted column the same way a real `INSERT` would (its
    /// declared default), instead of treating "not in the list" as NULL.
    pub default: ColumnDefault,
}

/// A column's parsed `DEFAULT` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnDefault {
    /// No `DEFAULT` clause in the `CREATE TABLE` text.
    None,
    /// A literal default (`DEFAULT NULL`, `DEFAULT '0'`, `DEFAULT b'1'`,
    /// ...), pre-parsed into the same `Cell` shape a dump row value would
    /// produce. `None` here means SQL NULL.
    Literal(Option<Vec<u8>>),
    /// A non-literal default expression (`DEFAULT CURRENT_TIMESTAMP`,
    /// `DEFAULT (expr)`, ...). Its actual value at INSERT time can't be
    /// determined statically from the dump text, so callers that need a
    /// fallback value can't resolve this one.
    Expression,
}

impl ColumnDefault {
    /// Best-effort fallback value for a dump row that omits this column.
    /// `Expression` defaults can't be resolved statically, so they (like
    /// `None`) fall back to NULL rather than guessing.
    pub fn resolve(&self) -> Option<Vec<u8>> {
        match self {
            ColumnDefault::Literal(v) => v.clone(),
            ColumnDefault::None | ColumnDefault::Expression => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    pub unique_keys: Vec<Vec<String>>,
    /// Exact CREATE TABLE statement text, executed verbatim when a table
    /// needs to be created or rebuilt (never reconstructed from parts).
    pub raw_statement: Vec<u8>,
}

impl TableSchema {
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }

    /// Best available row-identity key: PK, else first non-nullable
    /// unique key, else None (caller falls back to full-row multiset diff).
    pub fn key_columns(&self) -> Option<&[String]> {
        if !self.primary_key.is_empty() {
            return Some(&self.primary_key);
        }
        for uk in &self.unique_keys {
            let all_not_null = uk.iter().all(|c| {
                self.columns
                    .iter()
                    .find(|col| &col.name == c)
                    .map(|col| !col.nullable)
                    .unwrap_or(false)
            });
            if all_not_null {
                return Some(uk);
            }
        }
        None
    }

    /// Comparable signature of the parts of a schema that affect row
    /// identity/shape (column names+types+nullability in order, plus PK).
    /// Index/engine/charset differences are intentionally ignored.
    pub fn signature(&self) -> (Vec<(String, String, bool)>, Vec<String>) {
        (
            self.columns
                .iter()
                .map(|c| (c.name.clone(), c.r#type.clone(), c.nullable))
                .collect(),
            self.primary_key.clone(),
        )
    }
}

fn backtick_name(item: &[u8]) -> Option<(&str, &[u8])> {
    // item starts with a `` ` `` (already checked by caller); returns the
    // decoded name and the remainder of `item` after the closing backtick.
    let end = 1 + memchr(b'`', &item[1..])?;
    let name = std::str::from_utf8(&item[1..end]).ok()?;
    Some((name, &item[end + 1..]))
}

fn extract_key_cols(def: &[u8]) -> Vec<String> {
    let Some(open) = memchr(b'(', def) else {
        return Vec::new();
    };
    let close = sqlstream::find_matching_paren(def, open);
    let inner = &def[open + 1..close];
    sqlstream::split_toplevel(inner, b',')
        .into_iter()
        .filter_map(|part| {
            let part = part.trim_ascii();
            if part.first() == Some(&b'`') {
                backtick_name(part).map(|(name, _)| name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Parses the `n` out of a `bit` column's `(n)` width suffix (the part of
/// the column def right after the `bit` type keyword). A bare `bit` with
/// no parenthesized width is `BIT(1)`.
fn parse_bit_width(after_type: &[u8]) -> u32 {
    let after = after_type.trim_ascii_start();
    if after.first() == Some(&b'(') {
        if let Some(close) = memchr(b')', after) {
            if let Ok(n) = std::str::from_utf8(&after[1..close]).unwrap_or("").trim().parse() {
                return n;
            }
        }
    }
    1
}

/// Finds a case-insensitive, word-bounded, quote-aware occurrence of
/// `word` in `data` — used to locate the `DEFAULT` keyword without
/// matching it inside a quoted string (e.g. a `COMMENT '...'` clause that
/// happens to contain the word "default").
fn find_word_ci(data: &[u8], word: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\'' | b'"' | b'`' => i = sqlstream::skip_quoted(data, i),
            _ => {
                let boundary_before = i == 0 || !data[i - 1].is_ascii_alphanumeric();
                let matches = boundary_before
                    && i + word.len() <= data.len()
                    && data[i..i + word.len()].eq_ignore_ascii_case(word)
                    && data.get(i + word.len()).is_none_or(|b| !b.is_ascii_alphanumeric());
                if matches {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// Parses a column definition's `DEFAULT` clause. `rest` is everything
/// after the column's backtick-quoted name (so it also covers `NOT NULL`,
/// `COMMENT`, etc., which `find_word_ci` must not be confused by).
fn extract_default(rest: &[u8], bit_width: Option<u32>) -> ColumnDefault {
    let Some(pos) = find_word_ci(rest, b"DEFAULT") else {
        return ColumnDefault::None;
    };
    let after = rest[pos + b"DEFAULT".len()..].trim_ascii_start();
    parse_default_value(after, bit_width)
}

fn parse_default_value(after: &[u8], bit_width: Option<u32>) -> ColumnDefault {
    let word_end = after
        .iter()
        .position(|b| !b.is_ascii_alphanumeric())
        .unwrap_or(after.len());
    if after[..word_end].eq_ignore_ascii_case(b"NULL") {
        return ColumnDefault::Literal(None);
    }
    let literal_end = match after.first() {
        Some(b'\'') | Some(b'"') => Some(sqlstream::skip_quoted(after, 0)),
        Some(b'b') | Some(b'B') if after.get(1) == Some(&b'\'') => {
            Some(sqlstream::skip_quoted(after, 1))
        }
        Some(b'0') if after.get(1).is_some_and(|&c| c == b'x' || c == b'X') => Some(
            after
                .iter()
                .position(|b| !b.is_ascii_hexdigit())
                .unwrap_or(after.len()),
        ),
        Some(b'0'..=b'9' | b'-' | b'+' | b'.') => Some(
            after
                .iter()
                .position(|b| !matches!(b, b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E'))
                .unwrap_or(after.len()),
        ),
        _ => None, // CURRENT_TIMESTAMP, NOW(), (expr), ...: unresolvable statically
    };
    match literal_end {
        Some(end) => {
            ColumnDefault::Literal(values::parse_value_token_typed(&after[..end], bit_width).into_cell())
        }
        None => ColumnDefault::Expression,
    }
}

fn starts_with_ci(item: &[u8], word: &[u8]) -> bool {
    item.len() >= word.len() && item[..word.len()].eq_ignore_ascii_case(word)
}

/// Parses a single `CREATE TABLE ...` statement (as yielded by
/// `sqlstream::iter_statements`) into a `TableSchema`.
pub fn parse_create_table(stmt: &[u8]) -> TableSchema {
    let stmt = sqlstream::strip_leading_comments(stmt);
    assert!(
        starts_with_ci(stmt, b"CREATE TABLE"),
        "not a CREATE TABLE statement: {:?}",
        &stmt[..stmt.len().min(80)]
    );
    let after_kw = &stmt[b"CREATE TABLE".len()..];
    let name_rel = memchr(b'`', after_kw).expect("CREATE TABLE missing table name");
    let (table_name, _) = backtick_name(&after_kw[name_rel..]).expect("malformed table name");
    let table_name = table_name.to_string();

    let open = name_rel
        + after_kw[name_rel..]
            .iter()
            .position(|&b| b == b'(')
            .expect("CREATE TABLE missing column list");
    let close = sqlstream::find_matching_paren(after_kw, open);
    let inner = &after_kw[open + 1..close];

    let mut columns = Vec::new();
    let mut primary_key = Vec::new();
    let mut unique_keys = Vec::new();

    for item in sqlstream::split_toplevel(inner, b',') {
        let item = item.trim_ascii();
        if item.is_empty() {
            continue;
        }
        if item.first() == Some(&b'`') {
            let Some((col_name, rest)) = backtick_name(item) else {
                continue;
            };
            let rest = rest.trim_ascii_start();
            let type_end = rest
                .iter()
                .position(|b| !b.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            let col_type = String::from_utf8_lossy(&rest[..type_end]).to_lowercase();
            let bit_width = (col_type == "bit").then(|| parse_bit_width(&rest[type_end..]));
            let nullable = !contains_ci(item, b"NOT NULL");
            let default = extract_default(rest, bit_width);
            columns.push(Column {
                name: col_name.to_string(),
                r#type: col_type,
                nullable,
                bit_width,
                default,
            });
        } else if starts_with_ci(item, b"PRIMARY KEY") {
            primary_key = extract_key_cols(item);
        } else if starts_with_ci(item, b"UNIQUE KEY") || starts_with_ci(item, b"UNIQUE INDEX") {
            unique_keys.push(extract_key_cols(item));
        }
        // plain KEY/INDEX, CONSTRAINT, FOREIGN KEY: irrelevant to row identity/diffing
    }

    TableSchema {
        name: table_name,
        columns,
        primary_key,
        unique_keys,
        raw_statement: stmt.to_vec(),
    }
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_columns_and_primary_key() {
        let stmt = b"CREATE TABLE `_building` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `building_id` int NOT NULL DEFAULT '0',\n  PRIMARY KEY (`id`),\n  KEY `IDX_building_id` (`building_id`)\n) ENGINE=InnoDB AUTO_INCREMENT=2208668 DEFAULT CHARSET=utf8mb3";
        let schema = parse_create_table(stmt);
        assert_eq!(schema.name, "_building");
        assert_eq!(schema.column_names(), vec!["id", "building_id"]);
        assert_eq!(schema.primary_key, vec!["id".to_string()]);
        assert_eq!(schema.key_columns(), Some(&["id".to_string()][..]));
        assert!(!schema.columns[0].nullable);
        assert_eq!(
            schema.columns[1].default,
            ColumnDefault::Literal(Some(b"0".to_vec()))
        );
    }

    #[test]
    fn bit_column_width_defaults_to_one_without_parens() {
        let stmt = b"CREATE TABLE `t` (`flag` bit NOT NULL, `flags` bit(9) NOT NULL)";
        let schema = parse_create_table(stmt);
        assert_eq!(schema.columns[0].bit_width, Some(1));
        assert_eq!(schema.columns[1].bit_width, Some(9));
        assert_eq!(schema.columns[0].r#type, "bit");
        // non-bit columns don't get a width at all
        let other = parse_create_table(b"CREATE TABLE `t` (`id` int NOT NULL)");
        assert_eq!(other.columns[0].bit_width, None);
    }

    #[test]
    fn default_clause_variants() {
        let stmt = b"CREATE TABLE `t` (\n\
            `a` int DEFAULT NULL,\n\
            `b` int NOT NULL DEFAULT '0',\n\
            `c` varchar(10) NOT NULL DEFAULT 'x',\n\
            `d` bit(4) NOT NULL DEFAULT b'1010',\n\
            `e` timestamp NULL DEFAULT CURRENT_TIMESTAMP,\n\
            `f` int NOT NULL\n\
        )";
        let schema = parse_create_table(stmt);
        assert_eq!(schema.columns[0].default, ColumnDefault::Literal(None));
        assert_eq!(
            schema.columns[1].default,
            ColumnDefault::Literal(Some(b"0".to_vec()))
        );
        assert_eq!(
            schema.columns[2].default,
            ColumnDefault::Literal(Some(b"x".to_vec()))
        );
        assert_eq!(
            schema.columns[3].default,
            ColumnDefault::Literal(Some(vec![0x0A]))
        );
        assert_eq!(schema.columns[4].default, ColumnDefault::Expression);
        assert_eq!(schema.columns[5].default, ColumnDefault::None);
    }

    #[test]
    fn default_word_inside_comment_is_not_mistaken_for_default_clause() {
        let stmt =
            b"CREATE TABLE `t` (`a` int NOT NULL COMMENT 'uses the default value elsewhere')";
        let schema = parse_create_table(stmt);
        assert_eq!(schema.columns[0].default, ColumnDefault::None);
    }

    #[test]
    fn table_with_no_primary_or_unique_key_has_no_key_columns() {
        let stmt = b"CREATE TABLE `system` (\n  `name` varchar(255) DEFAULT NULL,\n  `value` text,\n  `created_at` timestamp NULL DEFAULT NULL,\n  `updated_at` timestamp NULL DEFAULT NULL,\n  KEY `IDX_name` (`name`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb3";
        let schema = parse_create_table(stmt);
        assert_eq!(schema.name, "system");
        assert!(schema.primary_key.is_empty());
        assert_eq!(schema.key_columns(), None);
        assert!(schema.columns[0].nullable);
    }

    #[test]
    fn unique_key_used_only_when_all_columns_non_nullable() {
        let stmt = b"CREATE TABLE `t` (\n  `a` int NOT NULL,\n  `b` int DEFAULT NULL,\n  UNIQUE KEY `uk_a` (`a`),\n  UNIQUE KEY `uk_ab` (`a`,`b`)\n) ENGINE=InnoDB";
        let schema = parse_create_table(stmt);
        assert!(schema.primary_key.is_empty());
        // uk_a (a only, non-nullable) should win over uk_ab (b is nullable)
        assert_eq!(schema.key_columns(), Some(&["a".to_string()][..]));
    }

    #[test]
    fn signature_ignores_index_and_engine_but_catches_column_changes() {
        let a = parse_create_table(
            b"CREATE TABLE `t` (`id` int NOT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB AUTO_INCREMENT=5",
        );
        let b = parse_create_table(
            b"CREATE TABLE `t` (`id` int NOT NULL, KEY `ix` (`id`), PRIMARY KEY (`id`)) ENGINE=MyISAM",
        );
        assert_eq!(a.signature(), b.signature());

        let c = parse_create_table(b"CREATE TABLE `t` (`id` bigint NOT NULL, PRIMARY KEY (`id`))");
        assert_ne!(a.signature(), c.signature());
    }
}
