//! Command palette ranking. Pure: no UI.

/// Lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    Exact,
    Prefix,
    WordStart,
    Substring,
    Subsequence,
}

/// How `label` matches `query` (both compared case-insensitively). For relations the
/// label is `schema.name`; a query without a dot also matches the bare name, so the
/// user may omit `public.` (and any schema).
pub fn match_kind(label: &str, query: &str) -> Option<MatchKind> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(MatchKind::Substring);
    }
    let l = label.to_lowercase();
    let mut best = kind_for(&l, &q);
    if !q.contains('.')
        && let Some((_, bare)) = l.split_once('.')
    {
        best = min_opt(best, kind_for(bare, &q));
    }
    best
}

fn min_opt(a: Option<MatchKind>, b: Option<MatchKind>) -> Option<MatchKind> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn kind_for(l: &str, q: &str) -> Option<MatchKind> {
    if l == q {
        return Some(MatchKind::Exact);
    }
    if l.starts_with(q) {
        return Some(MatchKind::Prefix);
    }
    let mut found_substring = false;
    for (i, _) in l.match_indices(q) {
        found_substring = true;
        let prev = l[..i].chars().next_back();
        if matches!(prev, Some('_' | '.' | ' ' | '-')) {
            return Some(MatchKind::WordStart);
        }
    }
    if found_substring {
        return Some(MatchKind::Substring);
    }
    let mut chars = l.chars();
    if q.chars()
        .filter(|c| !c.is_whitespace())
        .all(|qc| chars.any(|lc| lc == qc))
    {
        return Some(MatchKind::Subsequence);
    }
    None
}

/// Indexes of `items` that match, best first: by match kind, then category (tables,
/// tabs, commands, connections), then shorter label, then alphabetically. At most `limit`.
pub fn rank(items: &[(String, u8)], query: &str, limit: usize) -> Vec<usize> {
    let mut scored: Vec<(MatchKind, u8, usize, bool, &str, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (label, category))| {
            // Ties go to the shorter name: for `schema.name` that is the name; `public` first.
            let (schema, name) = label.split_once('.').unwrap_or(("", label.as_str()));
            match_kind(label, query).map(|k| {
                (
                    k,
                    *category,
                    name.chars().count(),
                    schema != "public",
                    label.as_str(),
                    i,
                )
            })
        })
        .collect();
    scored.sort();
    scored.into_iter().take(limit).map(|(.., i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(match_kind("public.customers", "customers"), Some(MatchKind::Exact));
        assert_eq!(
            match_kind("public.customers", "public.customers"),
            Some(MatchKind::Exact)
        );
        assert_eq!(match_kind("public.customers", "cust"), Some(MatchKind::Prefix));
        assert_eq!(match_kind("demo.user_events", "events"), Some(MatchKind::WordStart));
        assert_eq!(match_kind("demo.userevents", "events"), Some(MatchKind::Substring));
        assert_eq!(match_kind("demo.user_events", "usrevt"), Some(MatchKind::Subsequence));
        assert_eq!(match_kind("demo.user_events", "xyz"), None);
        assert_eq!(match_kind("Demo.Customers", "CUST"), Some(MatchKind::Prefix));
    }

    #[test]
    fn categories_break_ties() {
        let items = vec![("refresh".to_string(), 2), ("public.refresh".to_string(), 0)];
        // both exact; the table comes first
        assert_eq!(rank(&items, "refresh", 10), vec![1, 0]);
    }

    #[test]
    fn ranking_order() {
        let labels: Vec<(String, u8)> = [
            "demo.customer_events",
            "public.customers",
            "demo.customers_v",
            "demo.recustomers",
            "public.c_u_s_t",
            "billing.customers",
        ]
        .iter()
        .map(|l| (l.to_string(), 0))
        .collect();
        let label = |i: usize| labels[i].0.as_str();
        let got: Vec<&str> = rank(&labels, "customers", 50).into_iter().map(label).collect();
        // exact matches first (shorter label first), then prefix, then substring
        assert_eq!(
            got,
            vec![
                "public.customers",
                "billing.customers",
                "demo.customers_v",
                "demo.recustomers",
                "demo.customer_events"
            ]
        );
        let got: Vec<&str> = rank(&labels, "cust", 3).into_iter().map(label).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], "public.customers");
    }

    #[test]
    fn empty_query_matches_everything() {
        let labels = vec![("b".to_string(), 0), ("a".to_string(), 1)];
        assert_eq!(rank(&labels, "", 50).len(), 2);
    }

    #[test]
    fn ranks_fifty_thousand_quickly() {
        let labels: Vec<(String, u8)> = (0..50_000)
            .map(|i| (format!("schema_{}.table_{i}", i % 40), 0))
            .collect();
        let started = std::time::Instant::now();
        let got = rank(&labels, "tbl123", 50);
        eprintln!("ranked 50 000 labels in {:?}", started.elapsed());
        assert!(!got.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }
}
