//! What changed between two loads of the same table page. Pure, no UI.

use std::collections::{HashMap, HashSet};

use baselynx_core::exec::Row;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageDiff {
    /// (row of the new page, column of the new page) → the value before.
    pub changed: HashMap<(usize, usize), Option<String>>,
    /// Rows of the new page whose key was not on the old one.
    pub new_rows: HashSet<usize>,
    /// Keys of the old page that are not on the new one.
    pub gone: usize,
}

impl PageDiff {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.new_rows.is_empty() && self.gone == 0
    }

    /// The first change in reading order, for "jump to the first change".
    pub fn first(&self) -> Option<(usize, usize)> {
        let cell = self.changed.keys().min_by_key(|(r, c)| (*r, *c)).copied();
        let row = self.new_rows.iter().min().map(|r| (*r, 0));
        match (cell, row) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Compares two pages by row key. `None` when a key column is missing from either page.
pub fn diff(
    old_cols: &[String],
    old_rows: &[Row],
    new_cols: &[String],
    new_rows: &[Row],
    key: &[String],
) -> Option<PageDiff> {
    if key.is_empty() {
        return None;
    }
    let pos = |cols: &[String], k: &String| cols.iter().position(|c| c == k);
    let old_key: Vec<usize> = key.iter().map(|k| pos(old_cols, k)).collect::<Option<_>>()?;
    let new_key: Vec<usize> = key.iter().map(|k| pos(new_cols, k)).collect::<Option<_>>()?;
    let key_of = |row: &Row, idx: &[usize]| -> Vec<Option<String>> {
        idx.iter().map(|&i| row.get(i).cloned().flatten()).collect()
    };
    let old_by_key: HashMap<Vec<Option<String>>, usize> = old_rows
        .iter()
        .enumerate()
        .map(|(i, r)| (key_of(r, &old_key), i))
        .collect();
    // New column i is old column `map[i]`, when the column still exists.
    let map: Vec<Option<usize>> = new_cols.iter().map(|c| pos(old_cols, c)).collect();
    let mut out = PageDiff::default();
    let mut seen = HashSet::new();
    for (ri, row) in new_rows.iter().enumerate() {
        let k = key_of(row, &new_key);
        match old_by_key.get(&k) {
            Some(&oi) => {
                seen.insert(oi);
                let old = &old_rows[oi];
                for (ci, m) in map.iter().enumerate() {
                    let Some(oc) = m else { continue };
                    let before = old.get(*oc).cloned().flatten();
                    let now = row.get(ci).cloned().flatten();
                    if before != now {
                        out.changed.insert((ri, ci), before);
                    }
                }
            }
            None => {
                out.new_rows.insert(ri);
            }
        }
    }
    out.gone = old_rows.len() - seen.len();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    fn cols(c: &[&str]) -> Vec<String> {
        c.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn changed_new_and_gone() {
        let c = cols(&["id", "name", "n"]);
        let old = vec![
            vec![s("1"), s("a"), s("10")],
            vec![s("2"), s("b"), None],
            vec![s("3"), s("c"), s("30")],
        ];
        let new = vec![
            vec![s("1"), s("a"), s("11")],
            vec![s("2"), s("b"), s("0")],
            vec![s("4"), s("d"), s("40")],
        ];
        let d = diff(&c, &old, &c, &new, &cols(&["id"])).unwrap();
        assert_eq!(d.changed.len(), 2);
        assert_eq!(d.changed.get(&(0, 2)), Some(&s("10")));
        assert_eq!(d.changed.get(&(1, 2)), Some(&None), "NULL before");
        assert_eq!(d.new_rows, HashSet::from([2]));
        assert_eq!(d.gone, 1);
        assert_eq!(d.first(), Some((0, 2)));
    }

    #[test]
    fn same_page_is_empty_and_order_does_not_matter() {
        let c = cols(&["id", "v"]);
        let old = vec![vec![s("1"), s("x")], vec![s("2"), s("y")]];
        let new = vec![vec![s("2"), s("y")], vec![s("1"), s("x")]];
        assert!(diff(&c, &old, &c, &new, &cols(&["id"])).unwrap().is_empty());
    }

    #[test]
    fn composite_keys_and_missing_key() {
        let c = cols(&["r", "k", "v"]);
        let old = vec![vec![s("eu"), s("1"), s("a")]];
        let new = vec![vec![s("eu"), s("1"), s("b")], vec![s("us"), s("1"), s("a")]];
        let d = diff(&c, &old, &c, &new, &cols(&["r", "k"])).unwrap();
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.new_rows.len(), 1);
        assert!(diff(&c, &old, &c, &new, &cols(&["nope"])).is_none());
        assert!(diff(&c, &old, &c, &new, &[]).is_none());
    }
}
