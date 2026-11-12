//! Where the caret is in a statement, for completion. Pure, no UI and no
//! server: tables and columns come from catalogs already loaded.

use baselynx_core::sql;

/// A table named in the statement, with its alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// After `FROM`, `JOIN`, `UPDATE`, `INTO`, `TABLE`, or `schema.`: tables.
    Tables { schema: Option<String> },
    /// After `alias.` or `table.`: that table's columns.
    ColumnsOf(TableRef),
    /// Anywhere else: the statement's columns, keywords and functions.
    General { tables: Vec<TableRef> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// The word being typed, up to the caret.
    pub prefix: String,
    /// Byte offset where `prefix` starts; a completion replaces `prefix_start..caret`.
    pub prefix_start: usize,
    pub want: Want,
}

/// Keywords that end a table reference, so they are never taken for an alias.
const CLAUSE_WORDS: [&str; 34] = [
    "WHERE",
    "ON",
    "USING",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "GROUP",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "SET",
    "VALUES",
    "RETURNING",
    "AS",
    "NATURAL",
    "LATERAL",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "HAVING",
    "WINDOW",
    "FOR",
    "SELECT",
    "DEFAULT",
    "FROM",
    "AND",
    "OR",
    "NOT",
    "WITH",
    "FETCH",
];

const TABLE_WORDS: [&str; 5] = ["FROM", "JOIN", "UPDATE", "INTO", "TABLE"];

/// Keywords and functions offered where no table or column fits.
pub const SQL_WORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "ILIKE",
    "ORDER BY",
    "GROUP BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "AS",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "ON",
    "USING",
    "UNION",
    "UNION ALL",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "WITH",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "NULLS LAST",
    "TRUE",
    "FALSE",
    "EXPLAIN",
    "ANALYZE",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "coalesce",
    "nullif",
    "now",
    "current_date",
    "date_trunc",
    "extract",
    "lower",
    "upper",
    "length",
    "substring",
    "trim",
    "concat",
    "string_agg",
    "array_agg",
    "jsonb_agg",
    "jsonb_build_object",
    "to_char",
    "to_jsonb",
    "row_number",
    "rank",
    "generate_series",
    "greatest",
    "least",
    "round",
];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The byte range of the statement around `cursor`, from the end of the previous
/// top-level `;` to the next one.
fn statement_range(text: &str, cursor: usize) -> std::ops::Range<usize> {
    let before = &text[..cursor];
    let start = match sql::split(before).last() {
        // A `;` after the last statement means the caret is in a new, empty one.
        Some(s) if sql::has_top_level_semicolon(&before[s.range.end..]) => cursor,
        Some(s) => s.range.start,
        None => cursor,
    };
    let end = sql::split(&text[start..])
        .first()
        .map(|s| start + s.range.end)
        .unwrap_or(cursor)
        .max(cursor);
    start..end
}

/// Identifiers, quoted identifiers and the punctuation that matters, with positions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String, bool),
    Dot,
    Comma,
    Open,
    Close,
    Other,
}

fn tokens(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '\'' => {
                // A string literal: skip it.
                while let Some((_, d)) = it.next() {
                    if d == '\'' {
                        if it.peek().map(|(_, n)| *n) == Some('\'') {
                            it.next();
                            continue;
                        }
                        break;
                    }
                }
                out.push(Tok::Other);
            }
            '"' => {
                let mut name = String::new();
                while let Some((_, d)) = it.next() {
                    if d == '"' {
                        if it.peek().map(|(_, n)| *n) == Some('"') {
                            it.next();
                            name.push('"');
                            continue;
                        }
                        break;
                    }
                    name.push(d);
                }
                out.push(Tok::Word(name, true));
            }
            '-' if it.peek().map(|(_, n)| *n) == Some('-') => {
                for (_, d) in it.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
            }
            '.' => out.push(Tok::Dot),
            ',' => out.push(Tok::Comma),
            '(' => out.push(Tok::Open),
            ')' => out.push(Tok::Close),
            c if is_ident_char(c) && !c.is_ascii_digit() => {
                let mut end = i + c.len_utf8();
                while let Some(&(j, d)) = it.peek() {
                    if !is_ident_char(d) {
                        break;
                    }
                    end = j + d.len_utf8();
                    it.next();
                }
                out.push(Tok::Word(s[i..end].to_string(), false));
            }
            c if c.is_whitespace() => {}
            _ => out.push(Tok::Other),
        }
    }
    out
}

fn upper(t: &Tok) -> Option<String> {
    match t {
        Tok::Word(w, false) => Some(w.to_ascii_uppercase()),
        _ => None,
    }
}

/// An unquoted identifier folds to lower case, as PostgreSQL does.
fn ident(t: &Tok) -> Option<String> {
    match t {
        Tok::Word(w, true) => Some(w.clone()),
        Tok::Word(w, false) => Some(w.to_lowercase()),
        _ => None,
    }
}

fn is_clause(t: &Tok) -> bool {
    upper(t).is_some_and(|u| CLAUSE_WORDS.contains(&u.as_str()) || TABLE_WORDS.contains(&u.as_str()))
}

/// Tables named after `FROM`, `JOIN`, `UPDATE`, `INTO`, with their aliases.
pub fn table_refs(statement: &str) -> Vec<TableRef> {
    let t = tokens(statement);
    let mut out = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let Some(u) = upper(&t[i]) else {
            i += 1;
            continue;
        };
        if !matches!(u.as_str(), "FROM" | "JOIN" | "UPDATE" | "INTO") {
            i += 1;
            continue;
        }
        let in_from = u == "FROM";
        i += 1;
        loop {
            if t.get(i).and_then(upper).as_deref() == Some("ONLY") {
                i += 1;
            }
            let Some(first) = t.get(i).filter(|x| !is_clause(x)).and_then(ident) else {
                break;
            };
            let (schema, name) = if t.get(i + 1) == Some(&Tok::Dot) {
                match t.get(i + 2).and_then(ident) {
                    Some(n) => {
                        i += 3;
                        (Some(first), n)
                    }
                    None => break,
                }
            } else {
                i += 1;
                (None, first)
            };
            if t.get(i).and_then(upper).as_deref() == Some("AS") {
                i += 1;
            }
            let alias = match t.get(i) {
                Some(x) if !is_clause(x) && ident(x).is_some() => {
                    i += 1;
                    ident(x)
                }
                _ => None,
            };
            out.push(TableRef { schema, name, alias });
            if in_from && t.get(i) == Some(&Tok::Comma) {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// What fits at `cursor`. `is_schema` says whether a name is a schema of the connection.
pub fn context(text: &str, cursor: usize, is_schema: &dyn Fn(&str) -> bool) -> Context {
    let cursor = cursor.min(text.len());
    let range = statement_range(text, cursor);
    let statement = &text[range.clone()];
    let before = &text[range.start..cursor];
    let prefix_len: usize = before
        .chars()
        .rev()
        .take_while(|c| is_ident_char(*c))
        .map(char::len_utf8)
        .sum();
    let prefix_start = cursor - prefix_len;
    let prefix = text[prefix_start..cursor].to_string();
    let head = &text[range.start..prefix_start];
    let toks = tokens(head);
    let tables = table_refs(statement);

    // `qualifier.` right before the word.
    if head.ends_with('.') {
        let q = toks.iter().rev().nth(1).and_then(ident);
        if let Some(q) = q {
            if is_schema(&q) {
                return Context {
                    prefix,
                    prefix_start,
                    want: Want::Tables { schema: Some(q) },
                };
            }
            let table = tables
                .iter()
                .find(|t| t.alias.as_deref() == Some(q.as_str()))
                .or_else(|| tables.iter().find(|t| t.name == q))
                .cloned()
                .unwrap_or(TableRef {
                    schema: None,
                    name: q,
                    alias: None,
                });
            return Context {
                prefix,
                prefix_start,
                want: Want::ColumnsOf(table),
            };
        }
    }

    // Right after a table keyword, or after a comma in a FROM list.
    let last = toks.last();
    let after_table_word = last.and_then(upper).is_some_and(|u| TABLE_WORDS.contains(&u.as_str()));
    let in_from_list = last == Some(&Tok::Comma)
        && toks
            .iter()
            .rev()
            .filter_map(|t| upper(t).filter(|_| is_clause(t)))
            .next()
            .as_deref()
            == Some("FROM");
    if after_table_word || in_from_list {
        return Context {
            prefix,
            prefix_start,
            want: Want::Tables { schema: None },
        };
    }
    Context {
        prefix,
        prefix_start,
        want: Want::General { tables },
    }
}

/// An identifier as it must be written: bare when it is lower case and plain, quoted
/// otherwise.
pub fn ident_sql(name: &str, d: sql::Dialect) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { name.to_string() } else { d.quote_ident(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(text_with_caret: &str) -> Context {
        let cursor = text_with_caret.find('|').unwrap();
        let text = text_with_caret.replace('|', "");
        context(&text, cursor, &|s| s == "demo" || s == "public")
    }

    fn t(schema: Option<&str>, name: &str, alias: Option<&str>) -> TableRef {
        TableRef {
            schema: schema.map(String::from),
            name: name.into(),
            alias: alias.map(String::from),
        }
    }

    #[test]
    fn tables_after_from_and_join() {
        let c = ctx("select * from cu|");
        assert_eq!(c.prefix, "cu");
        assert_eq!(c.want, Want::Tables { schema: None });
        assert_eq!(c.prefix_start, 14);
        assert_eq!(ctx("select * from a join |").want, Want::Tables { schema: None });
        assert_eq!(ctx("select * from a, b|").want, Want::Tables { schema: None });
        assert_eq!(ctx("insert into |").want, Want::Tables { schema: None });
        assert_eq!(
            ctx("select * from demo.cu|").want,
            Want::Tables {
                schema: Some("demo".into())
            }
        );
    }

    #[test]
    fn columns_after_alias_dot() {
        let c = ctx("select c.| from demo.customers c");
        assert_eq!(c.prefix, "");
        assert_eq!(c.want, Want::ColumnsOf(t(Some("demo"), "customers", Some("c"))));
        let c = ctx("select x.na| from demo.customers as x join demo.events e on e.customer_id = x.id");
        assert_eq!(c.prefix, "na");
        assert_eq!(c.want, Want::ColumnsOf(t(Some("demo"), "customers", Some("x"))));
        // A table name used as the qualifier.
        assert_eq!(
            ctx("select customers.| from customers").want,
            Want::ColumnsOf(t(None, "customers", None))
        );
    }

    #[test]
    fn general_has_the_statement_tables() {
        let c = ctx("select na| from demo.customers where id = 1");
        assert_eq!(
            c.want,
            Want::General {
                tables: vec![t(Some("demo"), "customers", None)]
            }
        );
        let c = ctx("update demo.customers set ba|");
        assert_eq!(
            c.want,
            Want::General {
                tables: vec![t(Some("demo"), "customers", None)]
            }
        );
        // Only the statement at the caret counts.
        let c = ctx("select * from a;\nselect x| from b");
        assert_eq!(
            c.want,
            Want::General {
                tables: vec![t(None, "b", None)]
            }
        );
        let c = ctx("select 1;\n|");
        assert_eq!(c.want, Want::General { tables: vec![] });
    }

    #[test]
    fn refs_skip_strings_and_keywords() {
        assert_eq!(
            table_refs("select * from \"My T\" m, public.b where x = 'from z'"),
            vec![t(None, "My T", Some("m")), t(Some("public"), "b", None)]
        );
        assert_eq!(table_refs("delete from only t where a = 1"), vec![t(None, "t", None)]);
        assert_eq!(ident_sql("customers", sql::Dialect::Postgres), "customers");
        assert_eq!(ident_sql("Weird Name", sql::Dialect::Postgres), "\"Weird Name\"");
        assert_eq!(ident_sql("Weird Name", sql::Dialect::MySql), "`Weird Name`");
    }
}
