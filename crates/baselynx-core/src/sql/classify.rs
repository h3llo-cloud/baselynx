//! What a statement does, from its words outside strings, comments and quoted names.
//! When unsure, a statement counts as a write.

/// A word of the statement and how deep in parentheses it is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    upper: String,
    depth: usize,
    /// A double-quoted identifier: never a keyword.
    quoted: bool,
    /// Byte position of the word in the statement.
    at: usize,
}

/// Words and quoted identifiers, skipping comments, string literals and dollar quotes.
fn words(sql: &str) -> Vec<Word> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut level = 1;
                i += 2;
                while i < b.len() && level > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        level += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        level -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'\'' => {
                let escapes = i > 0 && matches!(b[i - 1], b'E' | b'e');
                i += 1;
                while i < b.len() {
                    if escapes && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut name = String::new();
                while i < b.len() {
                    if b[i] == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            name.push('"');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    let ch = sql[i..].chars().next().unwrap_or('?');
                    name.push(ch);
                    i += ch.len_utf8();
                }
                // A quoted name keeps its case; `upper` holds it as written.
                out.push(Word {
                    upper: name,
                    depth,
                    quoted: true,
                    at: start,
                });
                i += 1;
            }
            b'$' => {
                // A dollar quote `$tag$ … $tag$`; `$1` is a parameter.
                let rest = &sql[i + 1..];
                let tag_len = rest
                    .char_indices()
                    .find(|(_, ch)| !(ch.is_alphanumeric() || *ch == '_'))
                    .filter(|(_, ch)| *ch == '$')
                    .map(|(n, _)| n)
                    .filter(|n| *n == 0 || !rest.starts_with(|ch: char| ch.is_ascii_digit()));
                match tag_len {
                    Some(n) => {
                        let tag = &sql[i..i + n + 2];
                        i += n + 2;
                        match sql[i..].find(tag) {
                            Some(end) => i += end + tag.len(),
                            None => i = b.len(),
                        }
                    }
                    None => i += 1,
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                out.push(Word {
                    upper: sql[start..i].to_ascii_uppercase(),
                    depth,
                    quoted: false,
                    at: start,
                });
            }
            _ => i += 1,
        }
    }
    out
}

fn keyword(w: &Word, k: &str) -> bool {
    !w.quoted && w.upper == k
}

const WRITE_VERBS: [&str; 4] = ["INSERT", "UPDATE", "DELETE", "MERGE"];

/// True unless the statement only reads: `SELECT`, `SHOW`, `EXPLAIN` (without `ANALYZE`
/// of a write), `VALUES`, `TABLE`, or `WITH` whose body writes nothing.
pub fn is_write(sql: &str) -> bool {
    let w = words(sql);
    let Some(first) = w.first() else { return false };
    if first.quoted {
        return true;
    }
    let any_write_verb = || w.iter().any(|x| WRITE_VERBS.iter().any(|v| keyword(x, v)));
    match first.upper.as_str() {
        // `SELECT … INTO new_table` creates a table.
        "SELECT" => w.iter().any(|x| x.depth == 0 && keyword(x, "INTO")),
        // MySQL's DESCRIBE / DESC show a table's columns.
        "SHOW" | "VALUES" | "TABLE" | "DESCRIBE" | "DESC" => false,
        "WITH" => any_write_verb(),
        // EXPLAIN ANALYZE runs the statement.
        "EXPLAIN" => w.iter().any(|x| keyword(x, "ANALYZE") || keyword(x, "ANALYSE")) && any_write_verb(),
        _ => true,
    }
}

/// Statements that manage transactions: when a script has one, baselynx does not open its own.
pub fn is_transaction_control(sql: &str) -> bool {
    let w = words(sql);
    let Some(first) = w.first().filter(|f| !f.quoted) else {
        return false;
    };
    match first.upper.as_str() {
        "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "ABORT" | "SAVEPOINT" | "RELEASE" => true,
        "START" => w.get(1).is_some_and(|x| keyword(x, "TRANSACTION")),
        "PREPARE" => w.get(1).is_some_and(|x| keyword(x, "TRANSACTION")),
        _ => false,
    }
}

/// What a write does to rows, for "1 204 rows updated".
pub fn write_verb(sql: &str) -> &'static str {
    match words(sql).first().map(|w| w.upper.as_str()) {
        Some("UPDATE") => "updated",
        Some("DELETE") => "deleted",
        Some("INSERT") => "inserted",
        Some("MERGE") => "merged",
        _ => "changed",
    }
}

/// An `UPDATE` or `DELETE` without a top-level `WHERE`: the table it would touch whole,
/// as `(schema, name)` with `schema` empty when unqualified.
pub fn missing_where(sql: &str) -> Option<(String, String)> {
    let w = words(sql);
    let first = w.first().filter(|f| !f.quoted)?;
    let target_at = match first.upper.as_str() {
        "UPDATE" => 1,
        "DELETE" => {
            if !w.get(1).is_some_and(|x| keyword(x, "FROM")) {
                return None;
            }
            2
        }
        _ => return None,
    };
    if w.iter().any(|x| x.depth == 0 && keyword(x, "WHERE")) {
        return None;
    }
    let mut ix = target_at;
    if w.get(ix).is_some_and(|x| keyword(x, "ONLY")) {
        ix += 1;
    }
    let name_word = w.get(ix)?;
    let name = |x: &Word| {
        if x.quoted {
            x.upper.clone()
        } else {
            x.upper.to_lowercase()
        }
    };
    // `schema.table`: the next word follows a dot.
    let next = w.get(ix + 1);
    let dotted = next.is_some_and(|n| sql[..n.at].trim_end().ends_with('.'));
    if dotted {
        Some((name(name_word), name(next?)))
    } else {
        Some((String::new(), name(name_word)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes() {
        for read in [
            "select * from t",
            "  -- c\n SELECT 1",
            "show work_mem",
            "values (1)",
            "table t",
            "describe customers",
            "DESC orders",
            "explain select 1",
            "explain update t set a = 1",
            "with x as (select 1) select * from x",
            "select 'update t set a = 1'",
            "select \"update\" from t",
        ] {
            assert!(!is_write(read), "{read}");
        }
        for write in [
            "update t set a = 1",
            "delete from t",
            "insert into t values (1)",
            "with x as (delete from t returning *) select * from x",
            "explain analyze delete from t",
            "create table x (a int)",
            "vacuum t",
            "select * into newt from t",
            "\"weird\"",
            "do $$ begin perform 1; end $$",
        ] {
            assert!(is_write(write), "{write}");
        }
        assert!(!is_write(""));
    }

    #[test]
    fn transaction_control() {
        for s in [
            "begin",
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "commit",
            "end",
            "rollback",
            "abort",
            "start transaction",
            "savepoint a",
            "release a",
        ] {
            assert!(is_transaction_control(s), "{s}");
        }
        for s in ["select 1", "start", "update t set a = 1", "prepare q as select 1"] {
            assert!(!is_transaction_control(s), "{s}");
        }
    }

    #[test]
    fn missing_where_clauses() {
        assert_eq!(missing_where("update t set a = 1"), Some((String::new(), "t".into())));
        assert_eq!(
            missing_where("UPDATE public.Customers SET a = (SELECT 1 WHERE true)"),
            Some(("public".into(), "customers".into()))
        );
        assert_eq!(
            missing_where("delete from only \"My T\""),
            Some((String::new(), "My T".into()))
        );
        assert_eq!(missing_where("update t set a = 1 where id = 2"), None);
        assert_eq!(missing_where("delete from t where current of c"), None);
        assert_eq!(
            missing_where("delete from t -- where id = 1"),
            Some((String::new(), "t".into()))
        );
        assert_eq!(
            missing_where("update t set note = 'where'"),
            Some((String::new(), "t".into()))
        );
        assert_eq!(missing_where("select 1"), None);
        assert_eq!(write_verb("update t set a = 1"), "updated");
        assert_eq!(
            write_verb("merge into t using s on true when matched then delete"),
            "merged"
        );
    }
}
