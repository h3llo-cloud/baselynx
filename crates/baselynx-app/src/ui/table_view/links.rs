//! A table tab's view of one row and its relations: the record panel,
//! following foreign keys, the peek card and "Referenced by" (3.3).

use baselynx_core::catalog::ForeignKey;
use baselynx_core::table::key_filter;
use gpui_kit::component::Icon;

use super::*;
use crate::edit_model::Shown;
use crate::ui::record::{ChildRef, RecordField, RecordPanel};

/// The referenced row shown by Space on a foreign-key cell.
pub(super) struct Peek {
    pub title: SharedString,
    pub target: (String, String, String),
    pub fields: Vec<(String, Option<String>)>,
    pub error: Option<SharedString>,
    pub loading: bool,
    pub _task: Option<Task<()>>,
}

/// Counts of child rows for the record panel, for one row key.
pub(super) struct ChildCounts {
    pub key: Vec<Option<String>>,
    pub counts: Vec<Option<String>>,
    pub _task: Option<Task<()>>,
}

const CHILD_CAP: u64 = 1000;

/// The column glance (4.6): what `pg_stats` says about a column.
pub(super) struct Glance {
    pub column: String,
    pub at: Point<Pixels>,
    /// `None` while loading; `Ok(None)` when the table was never analysed.
    pub stats: Option<std::result::Result<Option<baselynx_core::catalog::ColumnStats>, String>>,
    pub _task: Task<()>,
}

impl TableView {
    // ------------------------------------------------------------------ values of a row

    /// The value a cell shows now, pending edits included.
    fn cell_value(&self, row: usize, name: &str, cx: &App) -> Option<String> {
        let d = self.grid.read(cx).delegate();
        let col = d.columns.iter().position(|c| c.name == name)?;
        match d.model.shown(row, col) {
            Shown::Value(s) => Some(s.to_string()),
            _ => None,
        }
    }

    fn values(&self, row: usize, names: &[String], cx: &App) -> Vec<Option<String>> {
        names.iter().map(|n| self.cell_value(row, n, cx)).collect()
    }

    // ------------------------------------------------------------------ following keys

    /// F4 or Alt+click on a foreign-key cell: open the referenced row.
    pub(super) fn follow_key(&mut self, row: usize, data_col: usize, cx: &mut Context<Self>) {
        let Some(rel) = self.rel.clone() else { return };
        let Some(name) = self
            .grid
            .read(cx)
            .delegate()
            .columns
            .get(data_col)
            .map(|c| c.name.clone())
        else {
            return;
        };
        let Some(fk) = rel.foreign_key_of(&name).cloned() else {
            return;
        };
        let values = self.values(row, &fk.from_cols, cx);
        let Some(filter) = key_filter(&fk.to_cols, &values, self.conn.dialect()) else {
            return;
        };
        self.peek = None;
        cx.emit(TabEvent::OpenTable {
            schema: fk.to_schema,
            table: fk.to_table,
            filter: Some(filter),
        });
    }

    pub(super) fn on_follow_key(&mut self, _: &FollowKey, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((r, c)) = self.selected(cx) {
            self.follow_key(r, c, cx);
        }
    }

    /// Space on a foreign-key cell: a card with the referenced row, without leaving.
    pub(super) fn open_peek(&mut self, row: usize, data_col: usize, cx: &mut Context<Self>) -> bool {
        let (Some(rel), Some(session)) = (self.rel.clone(), self.session.clone()) else {
            return false;
        };
        let Some(name) = self
            .grid
            .read(cx)
            .delegate()
            .columns
            .get(data_col)
            .map(|c| c.name.clone())
        else {
            return false;
        };
        let Some(fk) = rel.foreign_key_of(&name).cloned() else {
            return false;
        };
        let values = self.values(row, &fk.from_cols, cx);
        let Some(filter) = key_filter(&fk.to_cols, &values, self.conn.dialect()) else {
            return false;
        };
        let sql = format!(
            "SELECT * FROM {} WHERE {filter} LIMIT 1",
            self.conn.dialect().qualified(&fk.to_schema, &fk.to_table)
        );
        let title: SharedString = format!("{}.{}", fk.to_schema, fk.to_table).into();
        let task = cx.spawn(async move |this, cx| {
            let got = exec::execute_collect(&session, sql, Some(1)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(p) = this.peek.as_mut() {
                    p.loading = false;
                    match got {
                        Ok(c) => match c.rows.first() {
                            Some(r) => {
                                p.fields = c
                                    .columns
                                    .iter()
                                    .zip(r)
                                    .take(8)
                                    .map(|(m, v)| {
                                        let boolean = m.is_boolean();
                                        let v = v
                                            .as_deref()
                                            .map(|s| crate::ui::grid::display_value(s, boolean).into_owned());
                                        (m.name.clone(), v)
                                    })
                                    .collect()
                            }
                            None => p.error = Some("The referenced row does not exist.".into()),
                        },
                        Err(e) => p.error = Some(e.to_string().into()),
                    }
                    cx.notify();
                }
            });
        });
        self.peek = Some(Peek {
            title,
            target: (
                fk.to_schema.clone(),
                fk.to_table.clone(),
                key_filter(&fk.to_cols, &values, self.conn.dialect()).unwrap_or_default(),
            ),
            fields: Vec::new(),
            error: None,
            loading: true,
            _task: Some(task),
        });
        cx.notify();
        true
    }

    pub(super) fn render_peek(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.peek.as_ref()?;
        let secondary = tokens::secondary_text(cx);
        let (schema, table, filter) = p.target.clone();
        Some(
            v_flex()
                .id("peek-card")
                .occlude()
                .absolute()
                .top(tokens::ui(44., cx))
                .right_4()
                .w(tokens::ui(380., cx))
                .p_3()
                .gap_1()
                .rounded(cx.theme().radius_lg)
                .bg(cx.theme().popover)
                .border_1()
                .border_color(cx.theme().border)
                .shadow_lg()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(p.title.clone()),
                        )
                        .child(
                            Button::new("peek-open")
                                .icon(gpui_kit::assets::IconName::ArrowUpRight)
                                .label("Open")
                                .xsmall()
                                .ghost()
                                .tooltip_with_action("Open the referenced row", &FollowKey, Some(GRID))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.peek = None;
                                    cx.emit(TabEvent::OpenTable {
                                        schema: schema.clone(),
                                        table: table.clone(),
                                        filter: Some(filter.clone()),
                                    });
                                })),
                        ),
                )
                .when(p.loading, |d| {
                    d.child(div().text_sm().text_color(secondary).child("Loading…"))
                })
                .when_some(p.error.clone(), |d, e| {
                    d.child(div().text_sm().text_color(tokens::error_text(cx)).child(e))
                })
                .children(p.fields.iter().map(|(name, value)| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .w(tokens::ui(120., cx))
                                .flex_shrink_0()
                                .truncate()
                                .text_color(secondary)
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(cx.theme().mono_font_family.clone())
                                .when(value.is_none(), |d| d.italic().text_color(secondary))
                                .child(value.clone().unwrap_or_else(|| "NULL".into())),
                        )
                }))
                .child(
                    div()
                        .pt_1()
                        .text_xs()
                        .text_color(secondary)
                        .child("Esc closes · F4 opens"),
                )
                .into_any_element(),
        )
    }

    // ------------------------------------------------------------------ column glance

    pub(super) fn open_glance(&mut self, column: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else { return };
        let fut = catalog::column_stats(&session, &self.schema, &self.table, column);
        let task = cx.spawn(async move |this, cx| {
            let got = fut.await.map_err(|e| e.to_string());
            let _ = this.update(cx, |this, cx| {
                if let Some(g) = this.glance.as_mut() {
                    g.stats = Some(got);
                }
                cx.notify();
            });
        });
        self.glance = Some(Glance {
            column: column.to_string(),
            at: window.mouse_position(),
            stats: None,
            _task: task,
        });
        // Esc closes the card; the grid keeps the keyboard.
        self.focus(window, cx);
        cx.notify();
    }

    /// Null share, distinct count and the most common values as bars; a value filters.
    pub(super) fn render_glance(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = self.glance.as_ref()?;
        let secondary = tokens::secondary_text(cx);
        let column = g.column.clone();
        let mut body = v_flex().gap_1();
        match &g.stats {
            None => body = body.child(div().text_sm().text_color(secondary).child("Reading pg_stats…")),
            Some(Err(e)) => body = body.child(div().text_sm().text_color(tokens::error_text(cx)).child(e.clone())),
            Some(Ok(None)) => {
                body =
                    body.child(div().text_sm().text_color(secondary).child(
                        "No statistics: the table has not been analysed. ANALYZE would read it; baselynx does not.",
                    ))
            }
            Some(Ok(Some(st))) => {
                body = body.child(
                    h_flex()
                        .gap_3()
                        .text_sm()
                        .child(format!("NULL {:.0} %", st.null_frac * 100.0))
                        .when_some(st.distinct, |d, n| d.child(format!("≈ {} distinct", format::count(n)))),
                );
                let max = st.common.iter().map(|c| c.1).fold(0.0f32, f32::max).max(0.0001);
                let boolean = self
                    .grid
                    .read(cx)
                    .delegate()
                    .columns
                    .iter()
                    .any(|c| c.name == column && c.boolean);
                for (i, (value, freq)) in st.common.iter().take(10).enumerate() {
                    let (value, freq) = (value.clone(), *freq);
                    let col = column.clone();
                    let label = crate::ui::grid::display_value(&value, boolean).into_owned();
                    body = body.child(
                        h_flex()
                            .id(("glance-value", i))
                            .gap_2()
                            .text_sm()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .hover(|d| d.bg(cx.theme().muted))
                            .child(
                                div()
                                    .w(tokens::ui(150., cx))
                                    .truncate()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(label),
                            )
                            .child(
                                div().flex_1().h(px(8.)).rounded_full().bg(cx.theme().muted).child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(cx.theme().primary.opacity(0.7))
                                        .w(relative(freq / max)),
                                ),
                            )
                            .child(
                                div()
                                    .w(tokens::ui(48., cx))
                                    .text_right()
                                    .text_xs()
                                    .text_color(secondary)
                                    .child(format!("{:.1} %", freq * 100.0)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.glance = None;
                                let cond = table::quick_filter(
                                    &col,
                                    &CellValue::Text(value.clone()),
                                    false,
                                    this.conn.dialect(),
                                );
                                let filter = table::append_filter(this.filter.as_deref(), &cond);
                                this.filter_input
                                    .update(cx, |i, cx| i.set_value(filter.clone(), window, cx));
                                this.request_nav(Nav::Filter(Some(filter)), window, cx);
                            })),
                    );
                }
            }
        }
        Some(
            deferred(
                anchored().position(g.at).snap_to_window_with_margin(px(8.)).child(
                    v_flex()
                        .id("glance")
                        .occlude()
                        .w(tokens::ui(380., cx))
                        .p_3()
                        .gap_2()
                        .rounded(cx.theme().radius_lg)
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.glance = None;
                            cx.notify();
                        }))
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" {
                                this.glance = None;
                                this.focus(window, cx);
                                cx.notify();
                            }
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(gpui_kit::assets::IconName::ChartNoAxesColumn)
                                        .xsmall()
                                        .text_color(secondary),
                                )
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(g.column.clone()))
                                .child(div().text_xs().text_color(secondary).child("from pg_stats")),
                        )
                        .child(body),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    // ------------------------------------------------------------------ record panel

    pub(super) fn toggle_record(&mut self, _: &ToggleRecord, window: &mut Window, cx: &mut Context<Self>) {
        if self.record.take().is_some() {
            self.focus(window, cx);
            cx.notify();
            return;
        }
        let view = cx.entity().downgrade();
        let (v1, v2, v3) = (view.clone(), view.clone(), view);
        let panel = cx.new(|_| {
            RecordPanel::new()
                .on_edit(Rc::new(move |col, value, window, cx| {
                    let _ = v1.update(cx, |this, cx| this.edit_from_record(col, value, window, cx));
                }))
                .on_follow(Rc::new(move |col, _, cx| {
                    let _ = v2.update(cx, |this, cx| {
                        if let Some((r, _)) = this.selected(cx) {
                            this.follow_key(r, col, cx);
                        }
                    });
                }))
                .on_open_child(Rc::new(move |i, _, cx| {
                    let _ = v3.update(cx, |this, cx| this.open_child(i, cx));
                }))
        });
        self.record = Some(panel);
        self.sync_record(cx);
        cx.notify();
    }

    fn edit_from_record(&mut self, col: usize, value: CellValue, _: &mut Window, cx: &mut Context<Self>) {
        let Some((row, _)) = self.selected(cx) else { return };
        let value = match value {
            CellValue::Text(t) if self.grid.read(cx).delegate().is_boolean(col) => {
                CellValue::Text(crate::ui::grid::normalize_bool(&t))
            }
            v => v,
        };
        self.grid.update(cx, |g, cx| {
            apply_edit(g.delegate_mut(), row, col, value);
            g.refresh(cx);
        });
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    /// "Referenced by": opens the child table filtered to rows pointing at this one.
    fn open_child(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(rel) = self.rel.clone() else { return };
        let Some(fk) = rel.referenced_by.get(i).cloned() else {
            return;
        };
        let Some((row, _)) = self.selected(cx) else { return };
        let values = self.values(row, &fk.to_cols, cx);
        let Some(filter) = key_filter(&fk.from_cols, &values, self.conn.dialect()) else {
            return;
        };
        cx.emit(TabEvent::OpenTable {
            schema: fk.from_schema,
            table: fk.from_table,
            filter: Some(filter),
        });
    }

    /// Puts the selected row into the record panel; called on every render, so it follows
    /// the selection, edits and reloads. Only changes reach the panel.
    pub(super) fn sync_record(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.record.clone() else { return };
        let rel = self.rel.clone();
        let d = self.grid.read(cx).delegate();
        let Some((row, _)) = self.selected(cx) else {
            panel.update(cx, |p, cx| {
                p.set_record("No row selected".into(), Vec::new(), cx);
                p.set_children(Vec::new(), cx);
            });
            return;
        };
        let title: SharedString = if d.model.is_new(row) {
            "New row".into()
        } else {
            format!(
                "Row {}",
                format::count(self.page * self.page_size as u64 + row as u64 + 1)
            )
            .into()
        };
        let fields: Vec<RecordField> = d
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let info = rel.as_ref().and_then(|r| r.column(&c.name));
                let (value, default) = match d.model.shown(row, i) {
                    Shown::Value(s) => (Some(s.to_string()), false),
                    Shown::Null => (None, false),
                    Shown::Default => (None, true),
                };
                RecordField {
                    col: i,
                    name: c.name.clone(),
                    type_name: info
                        .map(|i| i.type_name.clone())
                        .unwrap_or_else(|| c.type_label.clone()),
                    value,
                    default,
                    changed: d.model.is_cell_changed(row, i),
                    editable: d.can_edit(row, i),
                    key: c.key,
                    references: rel
                        .as_ref()
                        .and_then(|r| r.foreign_key_of(&c.name))
                        .map(|f| format!("{}.{}", f.to_schema, f.to_table)),
                }
            })
            .collect();
        let is_new = d.model.is_new(row);
        panel.update(cx, |p, cx| p.set_record(title, fields, cx));

        // Child counts, for a loaded row with incoming keys.
        let Some(rel) = rel.filter(|r| !r.referenced_by.is_empty() && !is_new) else {
            panel.update(cx, |p, cx| p.set_children(Vec::new(), cx));
            return;
        };
        let key: Vec<Option<String>> = rel
            .referenced_by
            .iter()
            .flat_map(|f| f.to_cols.iter())
            .map(|c| self.cell_value(row, c, cx))
            .collect();
        if self.child_counts.as_ref().is_none_or(|c| c.key != key) {
            self.load_child_counts(rel.referenced_by.clone(), row, key, cx);
        }
        let counts = self.child_counts.as_ref().map(|c| c.counts.clone()).unwrap_or_default();
        let children = rel
            .referenced_by
            .iter()
            .enumerate()
            .map(|(i, f)| ChildRef {
                label: format!("{}.{} ({})", f.from_schema, f.from_table, f.from_cols.join(", ")),
                count: counts.get(i).cloned().flatten(),
            })
            .collect();
        panel.update(cx, |p, cx| p.set_children(children, cx));
    }

    /// Counts child rows after the selection settles, capped so a huge child table stays
    /// cheap: `1 000+`.
    fn load_child_counts(
        &mut self,
        keys: Vec<ForeignKey>,
        row: usize,
        key: Vec<Option<String>>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else { return };
        let queries: Vec<Option<String>> = keys
            .iter()
            .map(|f| {
                let values = self.values(row, &f.to_cols, cx);
                key_filter(&f.from_cols, &values, self.conn.dialect()).map(|filter| {
                    format!(
                        "SELECT count(*) FROM (SELECT 1 FROM {} WHERE {filter} LIMIT {}) s",
                        self.conn.dialect().qualified(&f.from_schema, &f.from_table),
                        CHILD_CAP + 1
                    )
                })
            })
            .collect();
        let n = queries.len();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let mut counts = Vec::with_capacity(n);
            for q in queries {
                let c = match q {
                    None => Some("0".to_string()),
                    Some(sql) => match exec::execute_collect(&session, sql, None).await {
                        Ok(got) => got
                            .rows
                            .first()
                            .and_then(|r| r.first().cloned().flatten())
                            .and_then(|v| v.parse::<u64>().ok())
                            .map(|v| {
                                if v > CHILD_CAP {
                                    format!("{}+", format::count(CHILD_CAP))
                                } else {
                                    format::count(v)
                                }
                            }),
                        Err(_) => Some("?".into()),
                    },
                };
                counts.push(c);
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(c) = this.child_counts.as_mut() {
                    c.counts = counts;
                    c._task = None;
                }
                cx.notify();
            });
        });
        self.child_counts = Some(ChildCounts {
            key,
            counts: vec![None; n],
            _task: Some(task),
        });
    }

    pub fn set_record_width(&mut self, w: f32, cx: &mut Context<Self>) {
        if (self.record_width - w).abs() > 0.5 {
            self.record_width = w;
            cx.notify();
        }
    }
}
