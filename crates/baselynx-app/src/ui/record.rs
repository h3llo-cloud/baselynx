//! The record panel: the selected row top to bottom, readable and editable.

use std::collections::HashSet;
use std::rc::Rc;

use baselynx_core::edit::CellValue;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::ui::tokens;
use crate::value_fmt;

/// One column of the shown row.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordField {
    /// Data column index in the grid.
    pub col: usize,
    pub name: String,
    pub type_name: String,
    /// `None` for NULL.
    pub value: Option<String>,
    /// A new row's cell left to its default.
    pub default: bool,
    pub changed: bool,
    pub editable: bool,
    pub key: bool,
    /// `schema.table` this column references.
    pub references: Option<String>,
}

/// A table that references the shown row.
#[derive(Clone, Debug, PartialEq)]
pub struct ChildRef {
    pub label: String,
    /// `None` while counting.
    pub count: Option<String>,
}

pub type EditFn = Rc<dyn Fn(usize, CellValue, &mut Window, &mut App)>;
pub type IndexFn = Rc<dyn Fn(usize, &mut Window, &mut App)>;

pub struct RecordPanel {
    title: SharedString,
    fields: Vec<RecordField>,
    children: Vec<ChildRef>,
    collapsed: HashSet<String>,
    editing: Option<(usize, Entity<TextareaState>, Subscription)>,
    on_edit: Option<EditFn>,
    on_follow: Option<IndexFn>,
    on_open_child: Option<IndexFn>,
    scroll: ScrollHandle,
}

impl RecordPanel {
    pub fn new() -> Self {
        Self {
            title: "".into(),
            fields: Vec::new(),
            children: Vec::new(),
            collapsed: HashSet::new(),
            editing: None,
            on_edit: None,
            on_follow: None,
            on_open_child: None,
            scroll: ScrollHandle::new(),
        }
    }

    pub fn on_edit(mut self, f: EditFn) -> Self {
        self.on_edit = Some(f);
        self
    }

    pub fn on_follow(mut self, f: IndexFn) -> Self {
        self.on_follow = Some(f);
        self
    }

    pub fn on_open_child(mut self, f: IndexFn) -> Self {
        self.on_open_child = Some(f);
        self
    }

    /// Shows another row (or the same one with new values). An open editor stays only when
    /// it is on the same column of the same row.
    pub fn set_record(&mut self, title: SharedString, fields: Vec<RecordField>, cx: &mut Context<Self>) {
        let same_row = title == self.title;
        if !same_row {
            self.editing = None;
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        if self.title != title || self.fields != fields {
            self.title = title;
            self.fields = fields;
            cx.notify();
        }
    }

    pub fn set_children(&mut self, children: Vec<ChildRef>, cx: &mut Context<Self>) {
        if self.children != children {
            self.children = children;
            cx.notify();
        }
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.fields.get(ix) else { return };
        if !f.editable {
            return;
        }
        let col = f.col;
        let text = f
            .value
            .as_deref()
            .map(|v| crate::ui::grid::display_value(v, f.type_name == "boolean").into_owned())
            .unwrap_or_default();
        let state = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 14).default_value(text));
        let sub = cx.subscribe_in(&state, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::Blur = ev {
                this.finish_edit(true, window, cx);
            }
        });
        state.update(cx, |s, cx| s.focus(window, cx));
        self.editing = Some((col, state, sub));
        cx.notify();
    }

    fn finish_edit(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((col, state, _)) = self.editing.take() else {
            return;
        };
        if keep {
            let text = state.read(cx).value().to_string();
            let unchanged = self.fields.iter().find(|f| f.col == col).is_some_and(|f| {
                f.value
                    .as_deref()
                    .map(|v| crate::ui::grid::display_value(v, f.type_name == "boolean"))
                    .as_deref()
                    == Some(text.as_str())
            });
            if !unchanged && let Some(on_edit) = self.on_edit.clone() {
                on_edit(col, CellValue::Text(text), window, cx);
            }
        }
        cx.notify();
    }

    fn render_value(&self, ix: usize, f: &RecordField, cx: &mut Context<Self>) -> AnyElement {
        let secondary = tokens::secondary_text(cx);
        let mono = cx.theme().mono_font_family.clone();
        if let Some((col, state, _)) = &self.editing
            && *col == f.col
        {
            let col = *col;
            return v_flex()
                .gap_1()
                .child(Textarea::new(state).font_family(mono))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("rec-done", ix))
                                .label("Done")
                                .xsmall()
                                .primary()
                                .on_click(cx.listener(|this, _, w, cx| this.finish_edit(true, w, cx))),
                        )
                        .child(
                            Button::new(("rec-null", ix))
                                .label("Set NULL")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.editing = None;
                                    if let Some(on_edit) = this.on_edit.clone() {
                                        on_edit(col, CellValue::Null, w, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("rec-cancel", ix))
                                .label("Cancel")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(|this, _, w, cx| this.finish_edit(false, w, cx))),
                        ),
                )
                .into_any_element();
        }
        let Some(value) = f.value.clone() else {
            return div()
                .italic()
                .text_color(secondary)
                .child(if f.default { "DEFAULT" } else { "NULL" })
                .into_any_element();
        };
        let ty = f.type_name.as_str();
        let mut body = v_flex().gap_0p5();
        if ty == "json" || ty == "jsonb" {
            let collapsed = self.collapsed.contains(&f.name);
            let pretty = value_fmt::pretty_json(&value).unwrap_or_else(|| value.clone());
            let lines = pretty.lines().count();
            let name = f.name.clone();
            body = body
                .child(
                    div()
                        .font_family(mono)
                        .text_sm()
                        .whitespace_normal()
                        .child(if collapsed { value.clone() } else { pretty }),
                )
                .when(lines > 1, |d| {
                    d.child(
                        div()
                            .id(("rec-fold", ix))
                            .text_xs()
                            .text_color(cx.theme().primary)
                            .cursor_pointer()
                            .child(if collapsed { "Expand" } else { "Collapse" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.collapsed.remove(&name) {
                                    this.collapsed.insert(name.clone());
                                }
                                cx.notify();
                            })),
                    )
                });
        } else if ty == "bytea" {
            body = body.child(
                div()
                    .font_family(mono)
                    .text_sm()
                    .child(value_fmt::bytea_summary(&value).unwrap_or(value.clone())),
            );
        } else {
            let shown = crate::ui::grid::display_value(&value, ty == "boolean").into_owned();
            body = body.child(
                div()
                    .font_family(mono)
                    .text_sm()
                    .whitespace_normal()
                    .when(value.is_empty(), |d| d.text_color(secondary))
                    .child(if value.is_empty() { "''".to_string() } else { shown }),
            );
            if ty == "timestamp with time zone"
                && let Some(t) = value_fmt::parse_timestamptz(&value)
            {
                body = body.child(div().text_xs().text_color(secondary).child(format!(
                    "{} · {}",
                    value_fmt::utc_text(t),
                    value_fmt::relative(t, value_fmt::now_epoch())
                )));
            }
        }
        body.into_any_element()
    }
}

impl Render for RecordPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let secondary = tokens::secondary_text(cx);
        let fields = self.fields.clone();
        let rows: Vec<AnyElement> = fields
            .iter()
            .enumerate()
            .map(|(ix, f)| {
                let editable = f.editable && self.editing.as_ref().is_none_or(|(c, ..)| *c != f.col);
                let follow = self.on_follow.clone();
                let col = f.col;
                v_flex()
                    .id(("rec-field", ix))
                    .px_3()
                    .py_1p5()
                    .gap_0p5()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.5))
                    .when(f.changed, |d| {
                        d.bg(tokens::changed_fill(cx))
                            .border_l_2()
                            .border_color(tokens::changed(cx))
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .when(f.key, |d| {
                                d.child(
                                    Icon::new(gpui_kit::assets::IconName::KeyRound)
                                        .xsmall()
                                        .text_color(secondary),
                                )
                            })
                            .child(div().font_weight(FontWeight::MEDIUM).child(f.name.clone()))
                            .child(div().text_color(secondary).child(f.type_name.clone()))
                            .child(div().flex_1())
                            .when_some(f.references.clone().filter(|_| f.value.is_some()), |d, target| {
                                d.child(
                                    div()
                                        .id(("rec-follow", ix))
                                        .flex()
                                        .gap_0p5()
                                        .items_center()
                                        .text_color(cx.theme().primary)
                                        .cursor_pointer()
                                        .child(target)
                                        .child(Icon::new(gpui_kit::assets::IconName::ArrowUpRight).xsmall())
                                        .on_click(move |_, w, cx| {
                                            if let Some(f) = follow.clone() {
                                                f(col, w, cx)
                                            }
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id(("rec-value", ix))
                            .when(editable, |d| {
                                d.cursor_text()
                                    .rounded(cx.theme().radius)
                                    .hover(|d| d.bg(cx.theme().muted))
                                    .on_click(cx.listener(move |this, _, w, cx| this.start_edit(ix, w, cx)))
                            })
                            .child(self.render_value(ix, f, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        let children: Vec<AnyElement> = self
            .children
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let open = self.on_open_child.clone();
                h_flex()
                    .id(("rec-child", i))
                    .px_3()
                    .py_1()
                    .gap_2()
                    .text_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(cx.theme().muted))
                    .child(
                        Icon::new(gpui_kit::assets::IconName::Table)
                            .xsmall()
                            .text_color(secondary),
                    )
                    .child(div().flex_1().truncate().child(c.label.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(secondary)
                            .child(c.count.clone().unwrap_or_else(|| "…".into())),
                    )
                    .on_click(move |_, w, cx| {
                        if let Some(f) = open.clone() {
                            f(i, w, cx)
                        }
                    })
                    .into_any_element()
            })
            .collect();
        let has_children = !children.is_empty();
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .h_9()
                    .px_3()
                    .flex_shrink_0()
                    .gap_2()
                    .bg(tokens::raised(cx))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(Icon::new(IconName::PanelRight).xsmall().text_color(secondary))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.title.clone()),
                    ),
            )
            .child(
                div()
                    .id("record-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .when(self.fields.is_empty(), |d| {
                        d.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(secondary)
                                .child("Select a cell to see its row here."),
                        )
                    })
                    .children(rows)
                    .when(has_children, |d| {
                        d.child(
                            div()
                                .px_3()
                                .pt_3()
                                .pb_1()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(secondary)
                                .child("REFERENCED BY"),
                        )
                        .children(children)
                    }),
            )
    }
}
