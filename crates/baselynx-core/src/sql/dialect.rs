//! What differs in generated SQL between engines: quoting, ordering, parameters.

use crate::model::SortDir;

/// The SQL dialect of an engine. Postgres is the default; the free functions in
/// [`crate::sql`] are its shortcuts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Dialect {
    #[default]
    Postgres,
    MySql,
    Sqlite,
}

impl Dialect {
    /// Always quotes: `"a""b"` (Postgres, SQLite) or `` `a``b` `` (MySQL).
    pub fn quote_ident(self, ident: &str) -> String {
        let q = match self {
            Dialect::MySql => '`',
            Dialect::Postgres | Dialect::Sqlite => '"',
        };
        let mut out = String::with_capacity(ident.len() + 2);
        out.push(q);
        for ch in ident.chars() {
            if ch == q {
                out.push(q);
            }
            out.push(ch);
        }
        out.push(q);
        out
    }

    /// `schema.name`, both quoted.
    pub fn qualified(self, schema: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
    }

    /// A string literal. Postgres uses `E'…'` when there is a backslash; MySQL escapes
    /// backslashes (its default `sql_mode` reads them as escapes); SQLite only doubles quotes.
    pub fn quote_literal(self, value: &str) -> String {
        match self {
            Dialect::Postgres => super::quote::quote_literal(value),
            Dialect::MySql => {
                let mut out = String::with_capacity(value.len() + 2);
                out.push('\'');
                for ch in value.chars() {
                    match ch {
                        '\'' => out.push_str("''"),
                        '\\' => out.push_str("\\\\"),
                        '\0' => out.push_str("\\0"),
                        c => out.push(c),
                    }
                }
                out.push('\'');
                out
            }
            Dialect::Sqlite => {
                let mut out = String::with_capacity(value.len() + 2);
                out.push('\'');
                for ch in value.chars() {
                    if ch == '\'' {
                        out.push('\'');
                    }
                    out.push(ch);
                }
                out.push('\'');
                out
            }
        }
    }

    /// One `ORDER BY` item that puts NULLs last in both directions. MySQL has no
    /// `NULLS LAST`; `col IS NULL` sorts the NULLs after the rest.
    pub fn order_nulls_last(self, column: &str, dir: SortDir) -> String {
        let col = self.quote_ident(column);
        let dir = match dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        match self {
            Dialect::MySql => format!("{col} IS NULL, {col} {dir}"),
            Dialect::Postgres | Dialect::Sqlite => format!("{col} {dir} NULLS LAST"),
        }
    }

    /// The text of the `n`-th (1-based) statement parameter.
    pub fn placeholder(self, n: usize) -> String {
        match self {
            Dialect::Postgres => format!("${n}"),
            Dialect::MySql | Dialect::Sqlite => "?".into(),
        }
    }

    /// Statements that only open or close a transaction, as the engine spells them.
    pub fn begin(self) -> &'static str {
        match self {
            Dialect::MySql => "START TRANSACTION",
            Dialect::Postgres | Dialect::Sqlite => "BEGIN",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_per_engine() {
        assert_eq!(Dialect::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect::Sqlite.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect::MySql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Dialect::MySql.qualified("shop", "order items"), "`shop`.`order items`");
        assert_eq!(Dialect::Postgres.quote_literal("C:\\x"), "E'C:\\\\x'");
        assert_eq!(Dialect::MySql.quote_literal("O'Brien \\ "), "'O''Brien \\\\ '");
        assert_eq!(Dialect::Sqlite.quote_literal("O'Brien \\ "), "'O''Brien \\ '");
    }

    #[test]
    fn nulls_last() {
        assert_eq!(
            Dialect::Postgres.order_nulls_last("a", SortDir::Desc),
            "\"a\" DESC NULLS LAST"
        );
        assert_eq!(
            Dialect::MySql.order_nulls_last("a", SortDir::Asc),
            "`a` IS NULL, `a` ASC"
        );
        assert_eq!(Dialect::Sqlite.placeholder(3), "?");
        assert_eq!(Dialect::Postgres.placeholder(3), "$3");
    }
}
