//! Parses CREATE TABLE statements into structured form.

use memchr::memchr;

use crate::sqlstream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub r#type: String, // base type keyword, lowercased: int, varchar, decimal, ...
    pub nullable: bool,
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
            let nullable = !contains_ci(item, b"NOT NULL");
            columns.push(Column {
                name: col_name.to_string(),
                r#type: col_type,
                nullable,
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
