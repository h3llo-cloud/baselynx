//! Completion for the SQL editor from catalogs already loaded. It never
//! queries the server while typing, except to fetch one table's columns the first time
//! they are needed; those are cached for the session.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use baselynx_core::catalog::{self, RelationRef};
use baselynx_core::session::Session;
use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit,
};

use crate::completion::{self, SQL_WORDS, TableRef, Want, ident_sql};

/// `(schema, table)` → `(column, type)`.
pub type ColumnCache = Arc<Mutex<HashMap<(String, String), Arc<Vec<(String, String)>>>>>;

const MAX_ITEMS: usize = 50;

/// What completion draws on; the console updates it when its connection changes.
#[derive(Default)]
pub struct Source {
    pub relations: Arc<Vec<RelationRef>>,
    /// The connection's catalog session, for the first fetch of a table's columns.
    pub catalog: Option<Arc<Session>>,
    pub columns: ColumnCache,
}

pub struct SqlCompletion {
    pub source: Rc<RefCell<Source>>,
}

/// The relation a statement means by `t`: exact schema when given, else `public` first.
fn resolve<'a>(relations: &'a [RelationRef], t: &TableRef) -> Option<&'a RelationRef> {
    let mut candidates = relations.iter().filter(|r| r.name == t.name);
    match &t.schema {
        Some(s) => candidates.find(|r| &r.schema == s),
        None => candidates.min_by_key(|r| r.schema != "public"),
    }
}

async fn columns_of(
    rel: &RelationRef,
    catalog: &Option<Arc<Session>>,
    cache: &ColumnCache,
) -> Arc<Vec<(String, String)>> {
    let key = (rel.schema.clone(), rel.name.clone());
    if let Some(c) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return c;
    }
    let Some(session) = catalog else {
        return Arc::new(Vec::new());
    };
    let cols = Arc::new(
        catalog::list_columns(session, &rel.schema, &rel.name)
            .await
            .unwrap_or_default(),
    );
    if let Ok(mut c) = cache.lock() {
        c.insert(key, cols.clone());
    }
    cols
}

fn matches(prefix: &str, name: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(2);
    }
    let (p, n) = (prefix.to_lowercase(), name.to_lowercase());
    if n.starts_with(&p) {
        Some(0)
    } else if n.contains(&p) {
        Some(1)
    } else {
        None
    }
}

fn item(
    label: String,
    insert: String,
    detail: Option<String>,
    kind: CompletionItemKind,
    range: lsp_types::Range,
    order: usize,
) -> CompletionItem {
    CompletionItem {
        label,
        kind: Some(kind),
        detail,
        sort_text: Some(format!("{order:05}")),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: insert,
        })),
        ..Default::default()
    }
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = rope.to_string();
        let src = self.source.borrow();
        let relations = src.relations.clone();
        let catalog = src.catalog.clone();
        let cache = src.columns.clone();
        drop(src);
        let ctx = completion::context(&text, offset, &|s| relations.iter().any(|r| r.schema == s));
        let range = lsp_types::Range {
            start: rope.offset_to_position(ctx.prefix_start),
            end: rope.offset_to_position(offset),
        };
        cx.background_spawn(async move {
            let prefix = ctx.prefix.clone();
            let mut items = Vec::new();
            match ctx.want {
                Want::Tables { schema } => {
                    let pool: Vec<&RelationRef> = relations
                        .iter()
                        .filter(|r| schema.as_ref().is_none_or(|s| &r.schema == s))
                        .collect();
                    let labels: Vec<(String, u8)> =
                        pool.iter().map(|r| (format!("{}.{}", r.schema, r.name), 0)).collect();
                    let order: Vec<usize> = if prefix.is_empty() {
                        (0..labels.len().min(MAX_ITEMS)).collect()
                    } else {
                        crate::palette_rank::rank(&labels, &prefix, MAX_ITEMS)
                    };
                    for (n, i) in order.into_iter().enumerate() {
                        let r = pool[i];
                        let insert = if schema.is_some() || r.schema == "public" {
                            ident_sql(&r.name)
                        } else {
                            format!("{}.{}", ident_sql(&r.schema), ident_sql(&r.name))
                        };
                        let kind = if r.kind.is_table() {
                            CompletionItemKind::CLASS
                        } else {
                            CompletionItemKind::INTERFACE
                        };
                        items.push(item(
                            labels[i].0.clone(),
                            insert,
                            Some(r.kind.label().into()),
                            kind,
                            range,
                            n,
                        ));
                    }
                }
                Want::ColumnsOf(t) => {
                    if let Some(rel) = resolve(&relations, &t) {
                        let cols = columns_of(rel, &catalog, &cache).await;
                        let mut found: Vec<(u8, usize, &(String, String))> = cols
                            .iter()
                            .enumerate()
                            .filter_map(|(i, c)| matches(&prefix, &c.0).map(|m| (m, i, c)))
                            .collect();
                        found.sort();
                        for (n, (_, _, (name, ty))) in found.into_iter().take(MAX_ITEMS).enumerate() {
                            items.push(item(
                                name.clone(),
                                ident_sql(name),
                                Some(ty.clone()),
                                CompletionItemKind::FIELD,
                                range,
                                n,
                            ));
                        }
                    }
                }
                Want::General { tables } => {
                    // Nothing typed yet: no menu, so plain typing is never interrupted.
                    if prefix.is_empty() {
                        return Ok(CompletionResponse::Array(Vec::new()));
                    }
                    let mut seen = std::collections::HashSet::new();
                    let mut found: Vec<(u8, u8, String, String, Option<String>, CompletionItemKind)> = Vec::new();
                    for t in &tables {
                        let Some(rel) = resolve(&relations, t) else { continue };
                        for (name, ty) in columns_of(rel, &catalog, &cache).await.iter() {
                            if let Some(m) = matches(&prefix, name)
                                && seen.insert(name.clone())
                            {
                                found.push((
                                    m,
                                    0,
                                    name.clone(),
                                    ident_sql(name),
                                    Some(ty.clone()),
                                    CompletionItemKind::FIELD,
                                ));
                            }
                        }
                    }
                    // Keywords keep the case the user is typing in.
                    let lower = prefix.chars().all(|c| !c.is_uppercase());
                    for w in SQL_WORDS {
                        if let Some(m) = matches(&prefix, w).filter(|m| *m == 0) {
                            let shown = if lower { w.to_lowercase() } else { w.to_string() };
                            found.push((m, 1, shown.clone(), shown, None, CompletionItemKind::KEYWORD));
                        }
                    }
                    found.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
                    for (n, (_, _, label, insert, detail, kind)) in found.into_iter().take(MAX_ITEMS).enumerate() {
                        items.push(item(label, insert, detail, kind, range, n));
                    }
                }
            }
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
