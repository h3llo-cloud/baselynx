//! Command palette.

use std::rc::Rc;
use std::sync::Arc;

use baselynx_core::catalog::RelKind;
use baselynx_core::model::ConnectionColor;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::palette_rank::rank;
use crate::ui::tokens;

const CONTEXT: &str = "PgPalette";
pub const MAX_RESULTS: usize = 50;
/// The category of history entries: only `>` searches them.
const QUERY: u8 = 4;

actions!(baselynx_palette, [SelectPrevItem, SelectNextItem, CycleTarget]);

pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevItem, Some(CONTEXT)),
        KeyBinding::new("down", SelectNextItem, Some(CONTEXT)),
        // Tab picks which connection a table found in several opens on (3.5).
        KeyBinding::new("tab", CycleTarget, Some("PgPalette > Input")),
    ]);
}

/// One connection that has a table.
#[derive(Clone, Debug)]
pub struct RelTarget {
    pub conn: Uuid,
    pub conn_name: String,
    pub color: ConnectionColor,
    pub key: Vec<String>,
}

#[derive(Clone)]
pub enum PaletteItem {
    Relation {
        conn: Uuid,
        conn_name: String,
        color: ConnectionColor,
        schema: String,
        name: String,
        kind: RelKind,
        /// Primary key columns, for `customers 42`.
        key: Vec<String>,
        /// Every connection with this `schema.table`; the fields above are the chosen one.
        targets: Rc<Vec<RelTarget>>,
    },
    /// `customers 42`: the row of a table with that key.
    Row {
        conn: Uuid,
        conn_name: String,
        color: ConnectionColor,
        schema: String,
        name: String,
        key: Vec<String>,
        values: Vec<String>,
    },
    Tab {
        index: usize,
        title: String,
    },
    Command {
        label: String,
        action: fn() -> Box<dyn Action>,
    },
    Connect {
        conn: Uuid,
        name: String,
    },
    /// A statement from the query history.
    Query {
        conn: Uuid,
        conn_name: String,
        text: String,
    },
    Project {
        id: Uuid,
        name: String,
    },
}

impl PaletteItem {
    fn label(&self) -> String {
        match self {
            PaletteItem::Relation { schema, name, .. } => format!("{schema}.{name}"),
            PaletteItem::Row {
                schema,
                name,
                key,
                values,
                ..
            } => {
                let cond: Vec<String> = key.iter().zip(values).map(|(k, v)| format!("{k} = {v}")).collect();
                format!("{schema}.{name} where {}", cond.join(", "))
            }
            PaletteItem::Tab { title, .. } => title.clone(),
            PaletteItem::Command { label, .. } => label.clone(),
            PaletteItem::Connect { name, .. } => format!("Connect {name}"),
            PaletteItem::Query { text, .. } => crate::format::statement_preview(text, 100),
            PaletteItem::Project { name, .. } => format!("Switch project → {name}"),
        }
    }

    fn category(&self) -> u8 {
        match self {
            PaletteItem::Relation { .. } | PaletteItem::Row { .. } => 0,
            PaletteItem::Tab { .. } => 1,
            PaletteItem::Command { .. } | PaletteItem::Project { .. } => 2,
            PaletteItem::Connect { .. } => 3,
            PaletteItem::Query { .. } => QUERY,
        }
    }
}

pub type PickHandler = Rc<dyn Fn(PaletteItem, bool, &mut Window, &mut App)>;

pub struct Palette {
    input: Entity<InputState>,
    items: Arc<Vec<(String, u8)>>,
    entries: Rc<Vec<PaletteItem>>,
    /// Shown when the query is empty: recent tables then commands.
    default_order: Vec<usize>,
    results: Vec<usize>,
    /// A row lookup shown before the results: `customers 42`.
    extra: Option<PaletteItem>,
    /// Entry → chosen target, after Tab.
    chosen: std::collections::HashMap<usize, usize>,
    selected: usize,
    revision: u64,
    _search: Option<Task<()>>,
    on_pick: PickHandler,
    _subs: Vec<Subscription>,
}

impl Palette {
    pub fn new(
        entries: Vec<PaletteItem>,
        default_order: Vec<usize>,
        on_pick: PickHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Go to a table or run a command…"));
        let items: Vec<(String, u8)> = entries.iter().map(|e| (e.label(), e.category())).collect();
        let subs = vec![
            cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let q = input.read(cx).value().to_string();
                    this.search(q, cx);
                }
                InputEvent::PressEnter { secondary, .. } => this.confirm(*secondary, window, cx),
                _ => {}
            }),
        ];
        let results = default_order.iter().copied().take(MAX_RESULTS).collect();
        Self {
            input,
            items: Arc::new(items),
            entries: Rc::new(entries),
            default_order,
            results,
            extra: None,
            chosen: Default::default(),
            selected: 0,
            revision: 0,
            _search: None,
            on_pick,
            _subs: subs,
        }
    }

    pub fn open(
        entries: Vec<PaletteItem>,
        default_order: Vec<usize>,
        on_pick: PickHandler,
        window: &mut Window,
        cx: &mut App,
    ) {
        let palette = cx.new(|cx| Palette::new(entries, default_order, on_pick, window, cx));
        let input = palette.read(cx).input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            // Enter is handled by the palette itself, so the dialog's Confirm must not close it.
            dialog
                .close_button(false)
                .w(tokens::ui(560., cx))
                .margin_top(tokens::ui(96., cx))
                .p_0()
                .on_ok(|_, _, _| false)
                .child(palette.clone())
        });
        input.update(cx, |i, cx| i.focus(window, cx));
    }

    fn search(&mut self, query: String, cx: &mut Context<Self>) {
        self.revision += 1;
        let revision = self.revision;
        if query.trim().is_empty() {
            self.results = self.default_order.iter().copied().take(MAX_RESULTS).collect();
            self.extra = None;
            self.selected = 0;
            self._search = None;
            cx.notify();
            return;
        }
        let items = self.items.clone();
        let entries = self.entries.clone();
        // `table value…`: the table part is ranked on its own; its best relation with a
        // key of that many columns becomes the first result.
        let row_query = query.trim().split_once(char::is_whitespace).map(|(t, rest)| {
            (
                t.to_string(),
                rest.split_whitespace().map(String::from).collect::<Vec<_>>(),
            )
        });
        let ranked = cx.background_spawn(async move {
            // `>select …` searches the query history; plain text everything else.
            if let Some(q) = query.trim_start().strip_prefix('>') {
                let pool: Vec<usize> = (0..items.len()).filter(|&i| items[i].1 == QUERY).collect();
                let results = if q.trim().is_empty() {
                    pool.iter().copied().take(MAX_RESULTS).collect()
                } else {
                    let labels: Vec<(String, u8)> = pool.iter().map(|&i| items[i].clone()).collect();
                    rank(&labels, q, MAX_RESULTS).into_iter().map(|j| pool[j]).collect()
                };
                return (results, None);
            }
            let results: Vec<usize> = rank(&items, &query, MAX_RESULTS * 2)
                .into_iter()
                .filter(|&i| items[i].1 != QUERY)
                .take(MAX_RESULTS)
                .collect();
            let table = row_query.and_then(|(t, values)| {
                rank(&items, &t, 20)
                    .into_iter()
                    .find(|&i| matches!(&items[i], (_, 0)))
                    .map(|i| (i, values))
            });
            (results, table)
        });
        self._search = Some(cx.spawn(async move |this, cx| {
            let (results, table) = ranked.await;
            let _ = &entries;
            let extra = table.and_then(|(i, values)| match &this.read_with(cx, |p, _| p.with_target(i)).ok()? {
                PaletteItem::Relation {
                    conn,
                    conn_name,
                    color,
                    schema,
                    name,
                    key,
                    ..
                } if !key.is_empty() && key.len() == values.len() => Some(PaletteItem::Row {
                    conn: *conn,
                    conn_name: conn_name.clone(),
                    color: *color,
                    schema: schema.clone(),
                    name: name.clone(),
                    key: key.clone(),
                    values,
                }),
                _ => None,
            });
            let _ = this.update(cx, |this, cx| {
                if this.revision == revision {
                    this.results = results;
                    this.extra = extra;
                    this.selected = 0;
                    cx.notify();
                }
            });
        }));
    }

    /// The item at a position of the shown list: the row lookup first, then the results.
    fn item_at(&self, pos: usize) -> Option<PaletteItem> {
        let ix = self.entry_at(pos);
        match (&self.extra, pos, ix) {
            (Some(e), 0, _) => Some(e.clone()),
            (_, _, Some(i)) => Some(self.with_target(i)),
            _ => None,
        }
    }

    fn entry_at(&self, pos: usize) -> Option<usize> {
        match (&self.extra, pos) {
            (Some(_), 0) => None,
            (Some(_), p) => self.results.get(p - 1).copied(),
            (None, p) => self.results.get(p).copied(),
        }
    }

    /// Entry `i` with the connection chosen by Tab.
    fn with_target(&self, i: usize) -> PaletteItem {
        let mut item = self.entries[i].clone();
        if let (
            PaletteItem::Relation {
                conn,
                conn_name,
                color,
                key,
                targets,
                ..
            },
            Some(&t),
        ) = (&mut item, self.chosen.get(&i))
            && let Some(target) = targets.get(t).cloned()
        {
            *conn = target.conn;
            *conn_name = target.conn_name;
            *color = target.color;
            *key = target.key;
        }
        item
    }

    fn cycle_target(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.entry_at(self.selected) else { return };
        if let PaletteItem::Relation { targets, conn, .. } = &self.entries[i]
            && targets.len() > 1
        {
            let current = self
                .chosen
                .get(&i)
                .copied()
                .unwrap_or_else(|| targets.iter().position(|t| t.conn == *conn).unwrap_or(0));
            self.chosen.insert(i, (current + 1) % targets.len());
            cx.notify();
        }
    }

    fn shown_len(&self) -> usize {
        self.results.len() + usize::from(self.extra.is_some())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.item_at(self.selected) else {
            return;
        };
        window.close_dialog(cx);
        let on_pick = self.on_pick.clone();
        // After the palette has closed, so a dialog opened by the pick is not closed with it.
        window.defer(cx, move |window, cx| on_pick(item, secondary, window, cx));
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.shown_len() == 0 {
            return;
        }
        let n = self.shown_len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
        cx.notify();
    }
}

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let shown: Vec<(usize, PaletteItem)> = (0..self.shown_len())
            .filter_map(|pos| self.item_at(pos).map(|i| (pos, i)))
            .collect();
        let rows: Vec<AnyElement> = shown
            .iter()
            .map(|(pos, item)| {
                let (pos, ix) = (*pos, *pos);
                let right: AnyElement = match item {
                    PaletteItem::Row { conn_name, color, .. } => h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(tokens::secondary_text(cx))
                        .child("row")
                        .when_some(tokens::connection(*color, cx), |d, c| {
                            d.child(div().size_2().rounded_full().bg(c))
                        })
                        .child(conn_name.clone())
                        .into_any_element(),
                    PaletteItem::Relation {
                        conn,
                        conn_name,
                        kind,
                        targets,
                        ..
                    } => h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(tokens::secondary_text(cx))
                        .when(!kind.is_table(), |d| d.child(kind.label()))
                        // One dot per connection with this table; the chosen one is ringed.
                        .children(targets.iter().map(|t| {
                            let chosen = t.conn == *conn;
                            div()
                                .size_2()
                                .rounded_full()
                                .map(|d| match tokens::connection(t.color, cx) {
                                    Some(c) => d.bg(c),
                                    None => d.border_1().border_color(tokens::secondary_text(cx)),
                                })
                                .when(chosen && targets.len() > 1, |d| {
                                    d.border_1().border_color(cx.theme().foreground)
                                })
                        }))
                        .child(conn_name.clone())
                        .when(targets.len() > 1, |d| d.child("· Tab"))
                        .into_any_element(),
                    PaletteItem::Command { action, .. } => Kbd::binding_for_action(action().as_ref(), None, window)
                        .map(|k| k.into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                    PaletteItem::Query { conn_name, .. } => div()
                        .text_xs()
                        .text_color(tokens::secondary_text(cx))
                        .child(format!("recent query · {conn_name}"))
                        .into_any_element(),
                    PaletteItem::Tab { .. } => div()
                        .text_xs()
                        .text_color(tokens::secondary_text(cx))
                        .child("open tab")
                        .into_any_element(),
                    _ => div().into_any_element(),
                };
                let on_pick = self.on_pick.clone();
                let item_clone = item.clone();
                h_flex()
                    .id(("palette-item", ix))
                    .px_3()
                    .h_8()
                    .gap_2()
                    .justify_between()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .when(pos == selected, |d| d.bg(cx.theme().list_active))
                    .hover(|d| d.bg(cx.theme().list_hover))
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let (on_pick, item) = (on_pick.clone(), item_clone.clone());
                        window.defer(cx, move |window, cx| on_pick(item, false, window, cx));
                    })
                    .child(div().truncate().child(item.label()))
                    .child(right)
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &SelectPrevItem, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectNextItem, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &CycleTarget, _, cx| this.cycle_target(cx)))
            .w_full()
            .child(div().p_2().child(Input::new(&self.input).appearance(false)))
            .child(
                v_flex()
                    .id("palette-results")
                    .max_h_96()
                    .overflow_y_scroll()
                    .px_1()
                    .pb_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .children(rows)
                    .when(empty, |d| {
                        d.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(tokens::secondary_text(cx))
                                .child("Nothing matches."),
                        )
                    }),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(tokens::secondary_text(cx))
                    .child(
                        "Enter opens · Ctrl+Enter opens a console with SELECT · > searches recent queries · Esc closes",
                    ),
            )
    }
}
