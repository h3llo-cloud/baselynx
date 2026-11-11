//! Splits a SQL script into statements on top-level semicolons.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    /// Byte range of `text` in the input: from the first code byte to the end of the last
    /// code token, so surrounding whitespace and comments are excluded.
    pub range: Range<usize>,
}

/// Splits on semicolons outside strings, quoted identifiers, dollar quotes and comments.
/// Statements with no code (empty or only comments) are dropped.
pub fn split(sql: &str) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut scan = Scanner::new(sql);
    let mut first: Option<usize> = None;
    let mut last_end = 0usize;
    while let Some(tok) = scan.next_token() {
        match tok {
            Token::Semicolon => {
                if let Some(start) = first.take() {
                    out.push(Statement {
                        text: sql[start..last_end].to_string(),
                        range: start..last_end,
                    });
                }
            }
            Token::Code(r) => {
                first.get_or_insert(r.start);
                last_end = r.end;
            }
            Token::Trivia => {}
        }
    }
    if let Some(start) = first {
        out.push(Statement {
            text: sql[start..last_end].to_string(),
            range: start..last_end,
        });
    }
    out
}

/// True when the text has a semicolon outside strings, identifiers and comments.
pub fn has_top_level_semicolon(sql: &str) -> bool {
    let mut scan = Scanner::new(sql);
    while let Some(tok) = scan.next_token() {
        if tok == Token::Semicolon {
            return true;
        }
    }
    false
}

/// The statement whose range contains `offset`, or the nearest one before it (the caret
/// is in trailing whitespace), or the first one when the caret is before all statements.
pub fn statement_at(sql: &str, offset: usize) -> Option<Statement> {
    let stmts = split(sql);
    let idx = stmts.iter().rposition(|s| s.range.start <= offset).unwrap_or(0);
    stmts.into_iter().nth(idx)
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Semicolon,
    Code(Range<usize>),
    Trivia,
}

struct Scanner<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.pos + ahead).copied()
    }

    fn prev_is_ident_char(&self, at: usize) -> bool {
        // Look at the character before byte `at`. Non-ASCII letters count as identifier chars.
        self.src[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    }

    fn next_token(&mut self) -> Option<Token> {
        let b = self.peek(0)?;
        let start = self.pos;
        match b {
            b';' => {
                self.pos += 1;
                Some(Token::Semicolon)
            }
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {
                self.pos += 1;
                Some(Token::Trivia)
            }
            b'-' if self.peek(1) == Some(b'-') => {
                while let Some(c) = self.peek(0) {
                    if c == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
                Some(Token::Trivia)
            }
            b'/' if self.peek(1) == Some(b'*') => {
                self.pos += 2;
                let mut depth = 1usize;
                while depth > 0 {
                    match (self.peek(0), self.peek(1)) {
                        (None, _) => break,
                        (Some(b'/'), Some(b'*')) => {
                            depth += 1;
                            self.pos += 2;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            depth -= 1;
                            self.pos += 2;
                        }
                        _ => self.pos += 1,
                    }
                }
                Some(Token::Trivia)
            }
            b'\'' => {
                let escapes =
                    start > 0 && matches!(self.bytes[start - 1], b'E' | b'e') && !self.prev_is_ident_char(start - 1);
                self.pos += 1;
                self.skip_quoted(b'\'', escapes);
                Some(Token::Code(start..self.pos))
            }
            b'"' => {
                self.pos += 1;
                self.skip_quoted(b'"', false);
                Some(Token::Code(start..self.pos))
            }
            b'$' if !self.prev_is_ident_char(start) => {
                if let Some(tag_len) = self.dollar_tag_len() {
                    let tag = &self.src[start..start + tag_len];
                    self.pos += tag_len;
                    match self.src[self.pos..].find(tag) {
                        Some(i) => self.pos += i + tag_len,
                        None => self.pos = self.bytes.len(),
                    }
                } else {
                    self.pos += 1;
                }
                Some(Token::Code(start..self.pos))
            }
            _ => {
                // One UTF-8 character of ordinary code.
                let ch_len = self.src[start..].chars().next().map_or(1, char::len_utf8);
                self.pos += ch_len;
                Some(Token::Code(start..self.pos))
            }
        }
    }

    /// Length of `$tag$` at the current position, or `None` when it is not a dollar quote
    /// (for example `$1`, a positional parameter).
    fn dollar_tag_len(&self) -> Option<usize> {
        let rest = &self.src[self.pos + 1..];
        for (i, ch) in rest.char_indices() {
            if ch == '$' {
                // `$` + tag (i bytes) + `$`
                return Some(i + 2);
            }
            let ok = if i == 0 {
                ch.is_alphabetic() || ch == '_'
            } else {
                ch.is_alphanumeric() || ch == '_'
            };
            if !ok {
                return None;
            }
        }
        None
    }

    /// Skips to just past the closing quote. A doubled quote is an escaped quote. With
    /// `escapes`, a backslash escapes the next character. Unterminated runs to the end.
    fn skip_quoted(&mut self, quote: u8, escapes: bool) {
        while let Some(c) = self.peek(0) {
            if escapes && c == b'\\' {
                self.pos += if self.peek(1).is_some() { 2 } else { 1 };
                continue;
            }
            if c == quote {
                if self.peek(1) == Some(quote) {
                    self.pos += 2;
                    continue;
                }
                self.pos += 1;
                return;
            }
            self.pos += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<String> {
        split(sql).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn basic() {
        assert_eq!(texts("select 1; select 2;"), vec!["select 1", "select 2"]);
        assert_eq!(texts("select 1"), vec!["select 1"]);
        assert_eq!(texts("  ;;  ; "), Vec::<String>::new());
    }

    #[test]
    fn single_quotes() {
        assert_eq!(texts("select 'a;b'; select 2"), vec!["select 'a;b'", "select 2"]);
        assert_eq!(texts("select 'it''s; fine'; x"), vec!["select 'it''s; fine'", "x"]);
        // Without E, a backslash is literal: the string ends at the second quote.
        assert_eq!(texts(r"select 'a\'; select 2"), vec![r"select 'a\'", "select 2"]);
    }

    #[test]
    fn escape_strings() {
        assert_eq!(
            texts(r"select E'a\';b'; select 2"),
            vec![r"select E'a\';b'", "select 2"]
        );
        assert_eq!(texts(r"select e'x\\'; y"), vec![r"select e'x\\'", "y"]);
        // `name'...'` where E is the end of an identifier is not an escape string.
        assert_eq!(texts(r"select typE'a\'; z"), vec![r"select typE'a\'", "z"]);
    }

    #[test]
    fn double_quotes() {
        assert_eq!(
            texts(r#"select "a;b"; select "x""y;" from t"#),
            vec![r#"select "a;b""#, r#"select "x""y;" from t"#]
        );
    }

    #[test]
    fn dollar_quotes() {
        assert_eq!(texts("select $$a;b$$; select 2"), vec!["select $$a;b$$", "select 2"]);
        assert_eq!(
            texts("select $tag$ a; $$ ; $tag$; x"),
            vec!["select $tag$ a; $$ ; $tag$", "x"]
        );
    }

    #[test]
    fn do_block_and_function_body() {
        let sql = "DO $$ BEGIN PERFORM 1; PERFORM 2; END $$;\nselect 3;";
        assert_eq!(texts(sql), vec!["DO $$ BEGIN PERFORM 1; PERFORM 2; END $$", "select 3"]);
        let f = "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $body$\nBEGIN\n  RETURN 1;\nEND;\n$body$;\nSELECT f();";
        let got = texts(f);
        assert_eq!(got.len(), 2);
        assert!(got[0].ends_with("$body$"));
        assert_eq!(got[1], "SELECT f()");
    }

    #[test]
    fn positional_parameters_are_not_dollar_quotes() {
        assert_eq!(texts("select $1; select $2 + $3;"), vec!["select $1", "select $2 + $3"]);
        // `$` inside an identifier
        assert_eq!(texts("select a$b; c"), vec!["select a$b", "c"]);
    }

    #[test]
    fn comments() {
        assert_eq!(texts("select 1 -- a; b\n; select 2"), vec!["select 1", "select 2"]);
        assert_eq!(
            texts("/* a; /* nested; */ still; */ select 1; x"),
            vec!["select 1", "x"]
        );
        assert_eq!(texts("-- only a comment;\n"), Vec::<String>::new());
        assert_eq!(texts("/* only */ ; select 1"), vec!["select 1"]);
    }

    #[test]
    fn unterminated() {
        assert_eq!(
            texts("select 1; select 'abc; def"),
            vec!["select 1", "select 'abc; def"]
        );
        assert_eq!(texts("select $$ abc; def"), vec!["select $$ abc; def"]);
        assert_eq!(texts("select 1 /* open; comment"), vec!["select 1"]);
        assert_eq!(texts("select \"open; ident"), vec!["select \"open; ident"]);
    }

    #[test]
    fn multibyte() {
        let sql = "select 'привет; мир'; select 'ü' as \"größe\"; select $ü$ x;y $ü$";
        assert_eq!(
            texts(sql),
            vec!["select 'привет; мир'", "select 'ü' as \"größe\"", "select $ü$ x;y $ü$"]
        );
        for s in split(sql) {
            assert_eq!(&sql[s.range.clone()], s.text);
        }
    }

    #[test]
    fn ranges_and_statement_at() {
        let sql = "select 1;\n\n  select 2;  \n";
        let s = split(sql);
        assert_eq!(&sql[s[0].range.clone()], "select 1");
        assert_eq!(&sql[s[1].range.clone()], "select 2");
        assert_eq!(statement_at(sql, 0).unwrap().text, "select 1");
        assert_eq!(statement_at(sql, 3).unwrap().text, "select 1");
        // right after the semicolon, still in statement 1's trailing whitespace
        assert_eq!(statement_at(sql, 9).unwrap().text, "select 1");
        assert_eq!(statement_at(sql, 13).unwrap().text, "select 2");
        assert_eq!(statement_at(sql, sql.len()).unwrap().text, "select 2");
        assert_eq!(statement_at("  \n select 9", 0).unwrap().text, "select 9");
        assert!(statement_at("  ", 1).is_none());
    }

    #[test]
    fn top_level_semicolon() {
        assert!(!has_top_level_semicolon("a = 'x;y' and \"b;\" > 1 -- ;"));
        assert!(has_top_level_semicolon("a = 1; drop table t"));
        assert!(has_top_level_semicolon("a = 1;"));
    }
}
