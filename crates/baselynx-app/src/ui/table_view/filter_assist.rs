//! The filter box knows the table: column names complete as you type, and
//! Up/Down walk the filters used on this table before.

use std::cell::Cell;

use super::*;
use crate::completion::ident_sql;

const MAX_SUGGESTIONS: usize = 8;

pub(super) struct FilterSuggest {
    pub items: Vec<(String, String)>,
    pub selected: usize,
    /// Byte range of the word being completed.
    pub word: std::ops::Range<usize>,
}

pub(super) struct FilterAssist {
    pub suggest: Option<FilterSuggest>,
    /// Filters used before on this table, oldest first (the last 20).
    pub history: Vec<String>,
    /// Position while walking the history with Up/Down: 0 is the newest.
    pub walking: Option<usize>,
    pub bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl FilterAssist {
    pub fn new() -> Self {
        Self {
            suggest: None,
            history: Vec::new(),
            walking: None,
            bounds: Rc::new(Cell::new(None)),
        }
    }
}

impl TableView {
    pub fn set_filter_history(&mut self, history: Vec<String>) {
        self.assist.history = history;
    }

    /// Typing in the filter box: suggest the columns the word under the caret starts.
    pub(super) fn filter_changed(&mut self, cx: &mut Context<Self>) {
        let input = self.filter_input.read(cx);
        let text = input.value().to_string();
        let caret = input.cursor().min(text.len());
        let start = caret
            - text[..caret]
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .map(char::len_utf8)
                .sum::<usize>();
        let word = text[start..caret].to_lowercase();
        let in_string = text[..start].matches('\'').count() % 2 == 1;
        self.assist.suggest = None;
        if !word.is_empty() && !in_string {
            let d = self.grid.read(cx).delegate();
            let mut items: Vec<(u8, String, String)> = d
                .columns
                .iter()
                .filter_map(|c| {
                    let n = c.name.to_lowercase();
                    let rank = if n.starts_with(&word) {
                        0
                    } else if n.contains(&word) {
                        1
                    } else {
                        return None;
                    };
                    (n != word).then(|| (rank, c.name.clone(), c.type_label.clone()))
                })
                .collect();
            items.sort();
            if !items.is_empty() {
                self.assist.suggest = Some(FilterSuggest {
                    items: items
                        .into_iter()
                        .take(MAX_SUGGESTIONS)
                        .map(|(_, n, t)| (n, t))
                        .collect(),
                    selected: 0,
                    word: start..caret,
                });
            }
        }
        cx.notify();
    }

    fn accept_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(s) = self.assist.suggest.take() else {
            return false;
        };
        let Some((name, _)) = s.items.get(s.selected) else {
            return false;
        };
        let text = self.filter_input.read(cx).value().to_string();
        if s.word.end > text.len() {
            return false;
        }
        let insert = format!("{} ", ident_sql(name));
        let new_text = format!("{}{}{}", &text[..s.word.start], insert, &text[s.word.end..]);
        let caret = s.word.start + insert.len();
        self.filter_input.update(cx, |i, cx| {
            i.set_value(new_text, window, cx);
            i.set_selected_range(caret..caret, cx);
        });
        self.assist.suggest = None;
        cx.notify();
        true
    }

    /// Enter: take the suggestion when one is shown, otherwise apply the filter.
    pub(super) fn filter_enter(&mut self, _: &FilterAccept, window: &mut Window, cx: &mut Context<Self>) {
        if self.accept_suggestion(window, cx) {
            return;
        }
        let text = self.filter_input.read(cx).value().trim().to_string();
        let filter = if text.is_empty() { None } else { Some(text) };
        self.assist.walking = None;
        if let Some(f) = &filter {
            cx.emit(TabEvent::FilterUsed {
                schema: self.schema.clone(),
                table: self.table.clone(),
                filter: f.clone(),
            });
        }
        self.request_nav(Nav::Filter(filter), window, cx);
    }

    pub(super) fn filter_tab(&mut self, _: &FilterComplete, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accept_suggestion(window, cx) {
            cx.propagate();
        }
    }

    /// Up: the previous suggestion, or an older filter when the box is empty or walking.
    pub(super) fn filter_up(&mut self, _: &FilterPrev, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.assist.suggest.as_mut() {
            s.selected = s.selected.checked_sub(1).unwrap_or(s.items.len() - 1);
            cx.notify();
            return;
        }
        let empty = self.filter_input.read(cx).value().trim().is_empty();
        let next = match self.assist.walking {
            Some(p) => p + 1,
            None if empty => 0,
            None => return,
        };
        let n = self.assist.history.len();
        if next < n {
            self.assist.walking = Some(next);
            let text = self.assist.history[n - 1 - next].clone();
            self.set_filter_text(text, window, cx);
        }
    }

    pub(super) fn filter_down(&mut self, _: &FilterNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.assist.suggest.as_mut() {
            s.selected = (s.selected + 1) % s.items.len();
            cx.notify();
            return;
        }
        match self.assist.walking {
            Some(0) => {
                self.assist.walking = None;
                self.set_filter_text(String::new(), window, cx);
            }
            Some(p) => {
                self.assist.walking = Some(p - 1);
                let n = self.assist.history.len();
                let text = self.assist.history[n - p].clone();
                self.set_filter_text(text, window, cx);
            }
            None => {}
        }
    }

    fn set_filter_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let end = text.len();
        self.filter_input.update(cx, |i, cx| {
            i.set_value(text, window, cx);
            i.set_selected_range(end..end, cx);
        });
        self.assist.suggest = None;
        cx.notify();
    }

    /// Escape closes the suggestions first.
    pub(super) fn filter_escape_suggestions(&mut self, cx: &mut Context<Self>) -> bool {
        if self.assist.suggest.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    pub(super) fn render_filter_suggestions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.assist.suggest.as_ref()?;
        let at = self.assist.bounds.get()?;
        let secondary = tokens::secondary_text(cx);
        let rows: Vec<AnyElement> = s
            .items
            .iter()
            .enumerate()
            .map(|(i, (name, ty))| {
                h_flex()
                    .id(("filter-suggest", i))
                    .px_2()
                    .py_0p5()
                    .gap_3()
                    .justify_between()
                    .rounded(cx.theme().radius)
                    .when(i == s.selected, |d| d.bg(cx.theme().list_active))
                    .hover(|d| d.bg(cx.theme().list_hover))
                    .cursor_pointer()
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(name.clone()),
                    )
                    .child(div().text_xs().text_color(secondary).child(ty.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(s) = this.assist.suggest.as_mut() {
                            s.selected = i;
                        }
                        this.accept_suggestion(window, cx);
                        this.filter_input.update(cx, |i, cx| i.focus(window, cx));
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            deferred(
                anchored().position(point(at.left(), at.bottom() + px(2.))).child(
                    v_flex()
                        .min_w(px(220.))
                        .p_1()
                        .text_sm()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded(cx.theme().radius)
                        .shadow_md()
                        .children(rows)
                        .child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_xs()
                                .text_color(secondary)
                                .child("Tab or Enter inserts · Esc closes"),
                        ),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
