//! Quoting for generated SQL.

/// Always double-quotes, doubling inner quotes: `a"b` → `"a""b"`.
pub fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// `schema.name` with both parts quoted.
pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// A string literal as PostgreSQL's `quote_literal` writes it: quotes doubled, and an
/// `E'…'` literal with doubled backslashes when the value contains a backslash.
pub fn quote_literal(value: &str) -> String {
    let has_backslash = value.contains('\\');
    let mut out = String::with_capacity(value.len() + 3);
    if has_backslash {
        out.push('E');
    }
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("Weird Name"), "\"Weird Name\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident("таблица"), "\"таблица\"");
        assert_eq!(quote_ident(""), "\"\"");
        assert_eq!(quote_qualified("demo", "Weird Name"), "\"demo\".\"Weird Name\"");
    }

    #[test]
    fn literals() {
        assert_eq!(quote_literal("abc"), "'abc'");
        assert_eq!(quote_literal("O'Brien"), "'O''Brien'");
        assert_eq!(quote_literal("C:\\temp"), "E'C:\\\\temp'");
        assert_eq!(quote_literal("it's \\ both"), "E'it''s \\\\ both'");
        assert_eq!(quote_literal("ünïcødé ✓"), "'ünïcødé ✓'");
        assert_eq!(quote_literal(""), "''");
    }
}
