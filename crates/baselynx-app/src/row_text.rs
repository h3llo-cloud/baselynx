//! "Copy row as": one row as an INSERT, a JSON object or tab-separated text.
//! Pure, no UI.

use baselynx_core::sql::{quote_ident, quote_literal, quote_qualified};

/// A column of the row: name, whether it holds numbers or booleans, and its value.
pub struct Cell<'a> {
    pub name: &'a str,
    pub numeric: bool,
    pub boolean: bool,
    pub value: Option<&'a str>,
}

pub fn insert(schema: &str, table: &str, cells: &[Cell]) -> String {
    let names: Vec<String> = cells.iter().map(|c| quote_ident(c.name)).collect();
    let values: Vec<String> = cells
        .iter()
        .map(|c| match c.value {
            None => "NULL".into(),
            Some(v) => quote_literal(v),
        })
        .collect();
    format!(
        "INSERT INTO {} ({}) VALUES ({});",
        quote_qualified(schema, table),
        names.join(", "),
        values.join(", ")
    )
}

/// Numbers and booleans stay JSON numbers and booleans when they read as such.
pub fn json(cells: &[Cell]) -> String {
    let mut map = serde_json::Map::new();
    for c in cells {
        let v = match c.value {
            None => serde_json::Value::Null,
            Some(v) if c.boolean && (v == "t" || v == "f") => serde_json::Value::Bool(v == "t"),
            Some(v) if c.numeric => serde_json::from_str::<serde_json::Number>(v)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|_| serde_json::Value::String(v.to_string())),
            Some(v) => serde_json::Value::String(v.to_string()),
        };
        map.insert(c.name.to_string(), v);
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap_or_default()
}

/// Header line and value line; NULL is empty, tabs and line breaks inside values become
/// spaces so the result pastes into a spreadsheet as one row.
pub fn tsv(cells: &[Cell]) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let names: Vec<String> = cells.iter().map(|c| clean(c.name)).collect();
    let values: Vec<String> = cells.iter().map(|c| c.value.map(clean).unwrap_or_default()).collect();
    format!("{}\n{}", names.join("\t"), values.join("\t"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells() -> Vec<Cell<'static>> {
        vec![
            Cell {
                name: "id",
                numeric: true,
                boolean: false,
                value: Some("42"),
            },
            Cell {
                name: "name",
                numeric: false,
                boolean: false,
                value: Some("O'Brien\tjr"),
            },
            Cell {
                name: "active",
                numeric: false,
                boolean: true,
                value: Some("t"),
            },
            Cell {
                name: "note",
                numeric: false,
                boolean: false,
                value: None,
            },
        ]
    }

    #[test]
    fn formats() {
        assert_eq!(
            insert("demo", "customers", &cells()),
            "INSERT INTO \"demo\".\"customers\" (\"id\", \"name\", \"active\", \"note\") VALUES ('42', 'O''Brien\tjr', 't', NULL);"
        );
        let j: serde_json::Value = serde_json::from_str(&json(&cells())).unwrap();
        assert_eq!(j["id"], 42);
        assert_eq!(j["active"], true);
        assert_eq!(j["name"], "O'Brien\tjr");
        assert!(j["note"].is_null());
        assert_eq!(tsv(&cells()), "id\tname\tactive\tnote\n42\tO'Brien jr\tt\t");
    }
}
