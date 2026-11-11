//! The grid shared by the console result and the table view.
//!
//! Grid column 0 is the gutter (row number and change marker); data column `i` is grid
//! column `i + 1`.

use std::rc::Rc;

use baselynx_core::catalog::RelationInfo;
use baselynx_core::edit::CellValue;
use baselynx_core::exec::{ColumnMeta, Row};
use baselynx_core::model::SortDir;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::actions::*;
use crate::edit_model::{EditModel, RowState, Shown};
use crate::format;
use crate::ui::tokens;

/// Characters shown in a cell; the editor and the preview strip show the full value.
pub const CELL_CHARS: usize = 200;
const NUMERIC_OIDS: [u32; 8] = [20, 21, 23, 26, 700, 701, 790, 1700];
const BOOL_OID: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    Console,
    Table,
}

#[derive(Debug, Clone)]
pub struct GridColumn {
    pub name: String,
    pub type_label: String,
    pub numeric: bool,
    pub key: bool,
    pub writable: bool,
    pub has_default: bool,
    /// `boolean`: shown as `true` / `false`.
    pub boolean: bool,
    /// Part of a foreign key: shows an arrow and follows with F4 or Alt+click.
    pub fk: bool,
    /// Width at 100 % zoom.
    pub width: f32,
}

pub struct CellEditor {
    pub row: usize,
    pub col: usize,
    pub input: Entity<InputState>,
}

pub type SortHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// Right click on a column header: the data column.
pub type HeaderMenuHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;
/// A click with Shift, Ctrl or Alt on a cell: row, data column, modifiers. Returns true
/// when the view took it.
pub type CellMouseHandler = Rc<dyn Fn(usize, usize, Modifiers, &mut Window, &mut App) -> bool>;

/// Several cells of one column, selected together to change them at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSelection {
    /// Data column.
    pub col: usize,
    /// Where a Shift range starts.
    pub anchor: usize,
    /// The row Shift+Up/Down moves.
    pub active: usize,
    pub rows: std::collections::BTreeSet<usize>,
    /// Cells picked before the current range began (Ctrl+click); a range adds to them.
    pub base: std::collections::BTreeSet<usize>,
}

impl ColumnSelection {
    pub fn new(col: usize, row: usize) -> Self {
        Self {
            col,
            anchor: row,
            active: row,
            rows: [row].into(),
            base: Default::default(),
        }
    }

    /// Selects the range from the anchor to `row`, keeping the cells picked before.
    pub fn range_to(&mut self, row: usize) {
        self.active = row;
        self.rows = self.base.clone();
        self.rows.extend(self.anchor.min(row)..=self.anchor.max(row));
    }

    /// Adds or removes one cell; the next range starts there.
    pub fn toggle(&mut self, row: usize) {
        if !self.rows.insert(row) && self.rows.len() > 1 {
            self.rows.remove(&row);
        }
        self.anchor = row;
        self.active = row;
        self.base = self.rows.clone();
        self.base.remove(&row);
    }

    pub fn cells(&self) -> Vec<(usize, usize)> {
        self.rows.iter().map(|&r| (r, self.col)).collect()
    }
}

#[cfg(test)]
mod selection_tests {
    use super::ColumnSelection;

    #[test]
    fn ranges_add_to_picked_cells() {
        let mut s = ColumnSelection::new(2, 1);
        s.range_to(3);
        assert_eq!(s.rows.iter().copied().collect::<Vec<_>>(), [1, 2, 3]);
        s.toggle(7);
        s.range_to(9);
        assert_eq!(s.rows.iter().copied().collect::<Vec<_>>(), [1, 2, 3, 7, 8, 9]);
        s.range_to(6);
        assert_eq!(s.rows.iter().copied().collect::<Vec<_>>(), [1, 2, 3, 6, 7]);
        s.toggle(2);
        assert_eq!(s.rows.iter().copied().collect::<Vec<_>>(), [1, 3, 6, 7]);
        // The last cell cannot be removed: a selection always has its cursor.
        let mut one = ColumnSelection::new(0, 4);
        one.toggle(4);
        assert_eq!(one.rows.len(), 1);
    }
}

/// The grid with a horizontal scrollbar that is always shown: the table's own one only
/// appears while scrolling, so a wide table did not look scrollable.
pub fn data_table(state: &Entity<TableState<GridDelegate>>, cx: &App) -> AnyElement {
    use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
    let g = state.read(cx);
    // The bar starts after the columns that stay put: the gutter and pinned key columns.
    let fixed: Pixels = (0..=g.delegate().pinned())
        .filter_map(|i| g.col_width(i))
        .fold(px(0.), |a, b| a + b);
    let handle = g.horizontal_scroll_handle.clone();
    div()
        .relative()
        .size_full()
        .child(
            gpui_kit::component::table::DataTable::new(state)
                .with_size(gpui_kit::component::Size::Size(tokens::ui(28., cx)))
                .header_row_height(tokens::ui(32., cx))
                .bordered(false)
                .scrollbar_visible(true, false),
        )
        .child(
            div()
                .absolute()
                .left(fixed)
                .right_0()
                .bottom_0()
                .h(Scrollbar::width())
                .child(
                    Scrollbar::horizontal(&handle)
                        .mode(ScrollbarMode::Always)
                        .viewport_from_layout(),
                ),
        )
        .into_any_element()
}

pub struct GridDelegate {
    pub mode: GridMode,
    pub columns: Vec<GridColumn>,
    pub model: EditModel,
    /// Absolute number of the first row, for the gutter.
    pub row_offset: u64,
    pub editable: bool,
    /// (data column, direction)
    pub sort: Option<(usize, SortDir)>,
    pub editing: Option<CellEditor>,
    /// Data column at each display position (grid column `i + 1`); hidden columns are left
    /// out and row-key columns come first when pinned.
    order: Vec<usize>,
    /// Leading display columns that stay put while scrolling sideways.
    pinned: usize,
    pub pin_keys: bool,
    pub hidden: std::collections::HashSet<String>,
    on_sort: Option<SortHandler>,
    on_header_menu: Option<HeaderMenuHandler>,
    on_cell_mouse: Option<CellMouseHandler>,
    /// Cells of one column selected together (Shift or Ctrl with clicks, Shift+Up/Down).
    pub multi: Option<ColumnSelection>,
    /// What the last reload changed; cleared by the next one.
    pub diff: Option<crate::page_diff::PageDiff>,
    /// The diff comes from a commit: tinted green, briefly (4.13).
    pub diff_committed: bool,
}

impl GridDelegate {
    pub fn new(mode: GridMode) -> Self {
        Self {
            mode,
            columns: Vec::new(),
            model: EditModel::default(),
            row_offset: 0,
            editable: false,
            sort: None,
            editing: None,
            order: Vec::new(),
            pinned: 0,
            pin_keys: true,
            hidden: Default::default(),
            on_sort: None,
            on_header_menu: None,
            on_cell_mouse: None,
            multi: None,
            diff: None,
            diff_committed: false,
        }
    }

    fn diff_color(&self, cx: &App) -> Hsla {
        if self.diff_committed {
            tokens::added(cx)
        } else {
            cx.theme().primary
        }
    }

    pub fn on_cell_mouse(&mut self, handler: CellMouseHandler) {
        self.on_cell_mouse = Some(handler);
    }

    /// Pinned leading columns, for the scrollbar's start.
    pub fn pinned(&self) -> usize {
        self.pinned
    }

    pub fn on_sort(&mut self, handler: SortHandler) {
        self.on_sort = Some(handler);
    }

    pub fn on_header_menu(&mut self, handler: HeaderMenuHandler) {
        self.on_header_menu = Some(handler);
    }

    /// Display order: pinned key columns first, then the rest in table order, without
    /// hidden ones.
    pub fn rebuild_order(&mut self) {
        let visible = |c: &GridColumn| !self.hidden.contains(&c.name);
        let keys: Vec<usize> = if self.pin_keys {
            (0..self.columns.len())
                .filter(|&i| self.columns[i].key && visible(&self.columns[i]))
                .collect()
        } else {
            Vec::new()
        };
        let rest = (0..self.columns.len()).filter(|i| !keys.contains(i) && visible(&self.columns[*i]));
        self.pinned = keys.len();
        self.order = keys.iter().copied().chain(rest).collect();
    }

    /// The data column shown at grid column `grid_col` (0 is the gutter).
    pub fn data_col(&self, grid_col: usize) -> Option<usize> {
        grid_col.checked_sub(1).and_then(|i| self.order.get(i).copied())
    }

    /// The grid column showing data column `data_col`, if it is visible.
    pub fn grid_col(&self, data_col: usize) -> Option<usize> {
        self.order.iter().position(|&c| c == data_col).map(|p| p + 1)
    }

    /// Visible data columns.
    pub fn display_count(&self) -> usize {
        self.order.len()
    }

    /// Data columns in display order.
    pub fn display_order(&self) -> &[usize] {
        &self.order
    }

    /// Replaces columns and rows. Widths are kept when the columns are the same.
    pub fn set_data(&mut self, metas: &[ColumnMeta], rows: Vec<Row>, rel: Option<&RelationInfo>, row_offset: u64) {
        let same = metas.len() == self.columns.len() && metas.iter().zip(&self.columns).all(|(m, c)| m.name == c.name);
        if !same {
            self.columns = metas
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let info = rel.and_then(|r| r.column(&m.name));
                    let type_label = info
                        .map(|c| format::short_type(&c.type_name))
                        .or_else(|| m.type_name.clone())
                        .unwrap_or_default();
                    GridColumn {
                        name: m.name.clone(),
                        numeric: m.type_oid.is_some_and(|o| NUMERIC_OIDS.contains(&o)),
                        key: rel.is_some_and(|r| r.is_key_column(&m.name)),
                        writable: info.is_some_and(|c| c.writable()),
                        has_default: info.is_some_and(|c| c.has_default),
                        boolean: m.type_oid == Some(BOOL_OID),
                        fk: rel.is_some_and(|r| r.foreign_key_of(&m.name).is_some()),
                        width: measure_width(&m.name, &type_label, &rows, i),
                        type_label,
                    }
                })
                .collect();
        }
        self.model.reset(rows, metas.len());
        self.row_offset = row_offset;
        self.editing = None;
        self.multi = None;
        self.rebuild_order();
    }

    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// The value of a cell as text, `None` for NULL or DEFAULT.
    pub fn text(&self, row: usize, data_col: usize) -> Option<String> {
        match self.model.shown(row, data_col) {
            Shown::Value(s) => Some(s.to_string()),
            _ => None,
        }
    }

    /// The value as the grid shows it (booleans as words); what Copy and the editor use.
    pub fn shown_text(&self, row: usize, data_col: usize) -> Option<String> {
        let boolean = self.columns.get(data_col).is_some_and(|c| c.boolean);
        self.text(row, data_col)
            .map(|t| display_value(&t, boolean).into_owned())
    }

    pub fn is_boolean(&self, data_col: usize) -> bool {
        self.columns.get(data_col).is_some_and(|c| c.boolean)
    }

    /// The row as cells for "Copy row as", in table order, pending edits included.
    pub fn row_cells(&self, row: usize) -> Vec<crate::row_text::Cell<'_>> {
        self.columns
            .iter()
            .enumerate()
            .map(|(i, c)| crate::row_text::Cell {
                name: &c.name,
                numeric: c.numeric,
                boolean: c.boolean,
                value: match self.model.shown(row, i) {
                    Shown::Value(s) => Some(s),
                    _ => None,
                },
            })
            .collect()
    }

    pub fn can_edit(&self, row: usize, data_col: usize) -> bool {
        self.mode == GridMode::Table
            && self.editable
            && !self.model.is_deleted(row)
            && self.columns.get(data_col).is_some_and(|c| c.writable)
    }
}

/// Starts from the header and the first 50 rows, between 60 and 400 px.
fn measure_width(name: &str, type_label: &str, rows: &[Row], col: usize) -> f32 {
    let header = name.chars().count() + type_label.chars().count() + 4;
    let cells = rows
        .iter()
        .take(50)
        .map(|r| {
            r.get(col)
                .and_then(|v| v.as_deref())
                .map_or(4, |s| s.lines().next().unwrap_or("").chars().count().min(60))
        })
        .max()
        .unwrap_or(0);
    // About 8 px per character of 13 px monospace, plus cell padding.
    (header.max(cells) as f32 * 8.0 + 24.0).clamp(60.0, 400.0)
}

fn one_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    let multi = s.contains('\n');
    let mut out: String = first.chars().take(CELL_CHARS).collect();
    if multi || first.chars().count() > CELL_CHARS {
        out.push('…');
    }
    out
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.order.len() + 1
    }

    fn rows_count(&self, _: &App) -> usize {
        self.model.row_count()
    }

    fn column(&self, col_ix: usize, cx: &App) -> Column {
        if col_ix == 0 {
            return Column::new("__gutter", "")
                .p_0()
                .width(tokens::ui(64., cx))
                .fixed_left()
                .resizable(false)
                .movable(false)
                .selectable(false)
                .text_right();
        }
        let pinned = col_ix <= self.pinned;
        let Some(c) = self.data_col(col_ix).and_then(|d| self.columns.get(d)) else {
            return Column::new(SharedString::from(format!("__missing{col_ix}")), "");
        };
        Column::new(
            SharedString::from(format!("{col_ix}:{}", c.name)),
            SharedString::from(c.name.clone()),
        )
        .p_0()
        .width(tokens::ui(c.width, cx))
        .min_width(tokens::ui(60., cx))
        .max_width(tokens::ui(1200., cx))
        .movable(false)
        .when(pinned, |c| c.fixed_left())
        .when(c.numeric, |c| c.text_right())
    }

    /// Double-click on a column edge: fit the widest of the header and loaded cells.
    fn fit_column(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Option<Pixels> {
        let data = self.data_col(col_ix)?;
        let c = self.columns.get(data)?;
        let cells = (0..self.model.row_count().min(2000))
            .map(|r| match self.model.shown(r, data) {
                Shown::Value(s) => one_line(&display_value(s, c.boolean)).chars().count(),
                _ => 7,
            })
            .max()
            .unwrap_or(0);
        let header = c.name.chars().count() + c.type_label.chars().count() + 5;
        // A monospace advance is about 0.6 em; the table adds the cell padding.
        let em = tokens::BASE_FONT * tokens::zoom(cx);
        Some(px(header.max(cells) as f32 * em * 0.6 + 8.0))
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        if col_ix == 0 {
            return div().id(("th", col_ix)).into_any_element();
        }
        let Some(data_col) = self.data_col(col_ix) else {
            return div().id(("th", col_ix)).into_any_element();
        };
        let c = &self.columns[data_col];
        let sorted = self.sort.filter(|(i, _)| *i == data_col).map(|(_, d)| d);
        let on_sort = self.on_sort.clone();
        let on_menu = self.on_header_menu.clone();
        let can_sort = on_sort.is_some();
        h_flex()
            .id(("th", col_ix))
            .group("pg-th")
            .size_full()
            .px_2()
            .gap_1()
            .overflow_hidden()
            .when_some(on_menu, |this, handler| {
                this.on_mouse_down(MouseButton::Right, move |_, window, cx| {
                    cx.stop_propagation();
                    handler(data_col, window, cx)
                })
            })
            .when(on_sort.is_some(), |this| this.cursor_pointer())
            .when_some(on_sort, |this, handler| {
                this.on_click(move |_, window, cx| handler(data_col, window, cx))
            })
            .when(c.key, |this| {
                this.child(
                    Icon::new(gpui_kit::assets::IconName::KeyRound)
                        .xsmall()
                        .text_color(tokens::secondary_text(cx)),
                )
            })
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_sm()
                    .truncate()
                    .child(c.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(tokens::secondary_text(cx))
                    .whitespace_nowrap()
                    .child(c.type_label.clone()),
            )
            .when_some(sorted, |this, dir| {
                this.child(
                    Icon::new(match dir {
                        SortDir::Asc => IconName::SortAscending,
                        SortDir::Desc => IconName::SortDescending,
                    })
                    .xsmall()
                    .text_color(cx.theme().primary),
                )
            })
            // A faint arrow on hover says the header sorts (4.5).
            .when(sorted.is_none() && can_sort, |this| {
                this.child(
                    div()
                        .opacity(0.)
                        .group_hover("pg-th", |s| s.opacity(0.5))
                        .child(Icon::new(IconName::SortAscending).xsmall()),
                )
            })
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.model.row_state(row_ix);
        if col_ix == 0 {
            let arrived = self.diff.as_ref().is_some_and(|d| d.new_rows.contains(&row_ix));
            let (marker, color) = match state {
                RowState::Changed => ("●", Some(tokens::changed(cx))),
                RowState::New => ("+", Some(tokens::added(cx))),
                RowState::Deleted => ("−", Some(tokens::removed(cx))),
                // A row that is new on the page since the last reload.
                RowState::Loaded if arrived => ("▎", Some(self.diff_color(cx))),
                RowState::Loaded => ("", None),
            };
            let number = if self.model.is_new(row_ix) {
                String::new()
            } else {
                format::count(self.row_offset + row_ix as u64 + 1)
            };
            return h_flex()
                .size_full()
                .px_2()
                .gap_1()
                .justify_between()
                .text_xs()
                .child(div().when_some(color, |d, c| d.text_color(c)).child(marker))
                .child(div().text_color(tokens::secondary_text(cx)).child(number))
                .into_any_element();
        }
        let Some(data_col) = self.data_col(col_ix) else {
            return div().into_any_element();
        };
        if let Some(ed) = self.editing.as_ref().filter(|e| e.row == row_ix && e.col == data_col) {
            return div()
                .key_context(CELL_EDITOR)
                .size_full()
                .child(Input::new(&ed.input).xsmall().appearance(false))
                .into_any_element();
        }
        let numeric = self.columns.get(data_col).is_some_and(|c| c.numeric);
        let fk = self.columns.get(data_col).is_some_and(|c| c.fk)
            && matches!(self.model.shown(row_ix, data_col), Shown::Value(_))
            && !self.model.is_new(row_ix);
        let cell_mouse = self.on_cell_mouse.clone();
        let in_multi = self
            .multi
            .as_ref()
            .is_some_and(|m| m.col == data_col && m.rows.contains(&row_ix) && m.rows.len() > 1);
        let changed = self.model.is_cell_changed(row_ix, data_col);
        let was = self
            .diff
            .as_ref()
            .and_then(|d| d.changed.get(&(row_ix, data_col)))
            .cloned();
        let diff_color = self.diff_color(cx);
        let boolean = self.columns.get(data_col).is_some_and(|c| c.boolean);
        let mut empty = false;
        let (mut lead, mut trail) = (false, false);
        let (text, placeholder) = match self.model.shown(row_ix, data_col) {
            Shown::Value("") => {
                empty = true;
                ("''".to_string(), false)
            }
            Shown::Value(s) => {
                lead = s.starts_with(char::is_whitespace);
                trail = s.ends_with(char::is_whitespace);
                (one_line(&display_value(s, boolean)), false)
            }
            Shown::Null => ("NULL".to_string(), true),
            Shown::Default => ("DEFAULT".to_string(), true),
        };
        let edge_dot = || {
            div()
                .flex_shrink_0()
                .size_1()
                .rounded_full()
                .bg(tokens::secondary_text(cx).opacity(0.7))
        };
        div()
            .id(ElementId::NamedInteger(
                "cell".into(),
                ((row_ix as u64) << 16) | data_col as u64,
            ))
            .size_full()
            .px_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .font_family(cx.theme().mono_font_family.clone())
            .when(numeric && !placeholder, |d| d.justify_end())
            .when(placeholder, |d| d.italic().text_color(tokens::secondary_text(cx)))
            .when(empty, |d| d.text_color(tokens::secondary_text(cx).opacity(0.7)))
            .when(was.is_some() && !changed, |d| d.bg(diff_color.opacity(0.16)))
            .when(changed, |d| {
                d.bg(tokens::changed_fill(cx))
                    .border_l_2()
                    .border_color(tokens::changed(cx))
            })
            .when(state == RowState::New, |d| d.bg(tokens::added_fill(cx)))
            .when(state == RowState::Deleted, |d| {
                d.bg(tokens::removed_fill(cx))
                    .line_through()
                    .text_color(tokens::secondary_text(cx))
            })
            .when(in_multi, |d| {
                d.bg(cx
                    .theme()
                    .primary
                    .opacity(if cx.theme().is_dark() { 0.28 } else { 0.18 }))
            })
            .when(lead, |d| d.child(edge_dot()))
            .child(div().truncate().child(text))
            .when(trail, |d| d.child(edge_dot()))
            // A foreign key: a faint arrow, Alt+click opens the referenced row (3.3).
            .when(fk, |d| {
                d.gap_1().child(
                    Icon::new(gpui_kit::assets::IconName::ArrowUpRight)
                        .xsmall()
                        .flex_shrink_0()
                        .text_color(tokens::secondary_text(cx).opacity(0.6)),
                )
            })
            // The value before the reload, on hover.
            .when_some(was, |d, before| {
                let tip: SharedString = format!("was: {}", before.unwrap_or_else(|| "NULL".into())).into();
                d.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
            })
            .when_some(cell_mouse, |d, handler| {
                d.on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                    let m = ev.modifiers;
                    if (m.shift || m.secondary(cx) || m.alt) && handler(row_ix, data_col, m, window, cx) {
                        cx.stop_propagation();
                    }
                })
            })
            .into_any_element()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.data_col(col_ix)
            .and_then(|d| self.text(row_ix, d))
            .unwrap_or_default()
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let d = self.data_col(col_ix)?;
        Some(self.shown_text(row_ix, d).unwrap_or_else(|| "NULL".into()).into())
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        cell_menu(self, row_ix, menu)
    }

    fn render_empty(&mut self, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        // Empty states are drawn by the owning view.
        div()
    }

    fn move_column(&mut self, _: usize, _: usize, _: &mut Window, _: &mut Context<TableState<Self>>) {}
}

/// The items of a cell's context menu. Each item is an action handled by the owning view.
pub fn cell_menu(d: &GridDelegate, row_ix: usize, menu: PopupMenu) -> PopupMenu {
    let copy = menu.menu("Copy value", Box::new(CopyCell));
    let _ = row_ix;
    // "Copy row as" (4.8); an INSERT needs the table, so results of a console have none.
    let copy = if d.mode == GridMode::Table {
        copy.menu("Copy row as INSERT", Box::new(CopyRowInsert))
    } else {
        copy
    };
    let copy = copy
        .menu("Copy row as JSON", Box::new(CopyRowJson))
        .menu("Copy row as tab-separated", Box::new(CopyRowTsv));
    if d.mode == GridMode::Console {
        return copy;
    }
    let is_new = d.model.is_new(row_ix);
    let deleted = d.model.is_deleted(row_ix);
    let edit = d.editable && !deleted;
    let delete_label = if is_new {
        "Remove new row"
    } else if deleted {
        "Restore row"
    } else {
        "Delete row"
    };
    copy.separator()
        .menu_with_disabled("Filter by this value", Box::new(FilterByValue), is_new)
        .menu_with_disabled("Exclude this value", Box::new(ExcludeValue), is_new)
        .separator()
        .menu_with_disabled("Set NULL", Box::new(SetNull), !edit)
        .menu_with_disabled("Revert cell", Box::new(RevertCell), !edit)
        .separator()
        .menu_with_disabled("Add row", Box::new(AddRow), !d.editable)
        .menu_with_disabled(delete_label, Box::new(DeleteRow), !d.editable)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jump {
    RowStart,
    RowEnd,
    Top,
    Bottom,
}

/// Home / End move within the row (data columns only); Ctrl+Home / Ctrl+End to the first
/// and last row, keeping the column.
pub fn jump(g: &mut TableState<GridDelegate>, to: Jump, cx: &mut Context<TableState<GridDelegate>>) {
    let rows = g.delegate().model.row_count();
    let cols = g.delegate().display_count();
    if rows == 0 || cols == 0 {
        return;
    }
    let (row, col) = g.selected_cell().unwrap_or((0, 1));
    let col = col.max(1);
    let (r, c) = match to {
        Jump::RowStart => (row, 1),
        Jump::RowEnd => (row, cols),
        Jump::Top => (0, col),
        Jump::Bottom => (rows - 1, col),
    };
    g.set_selected_cell(r.min(rows - 1), c.min(cols), cx);
    g.scroll_to_row(r.min(rows - 1), cx);
    g.scroll_to_col(c.min(cols), cx);
}

/// Normalises a typed boolean to PostgreSQL's output form, so `true` over a loaded `t`
/// is not a change. Other text is returned as typed.
pub fn normalize_bool(text: &str) -> String {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "on" | "1" => "t".into(),
        "false" | "f" | "no" | "n" | "off" | "0" => "f".into(),
        _ => text.to_string(),
    }
}

/// How a value is drawn (D12): booleans as words, an empty string as a dimmed `''`.
pub fn display_value(raw: &str, boolean: bool) -> std::borrow::Cow<'_, str> {
    if boolean {
        match raw {
            "t" => return "true".into(),
            "f" => return "false".into(),
            _ => {}
        }
    }
    raw.into()
}

/// Sets the value of the selected cell from the editor's text. Returns false when the
/// cell cannot be edited.
/// A pasted line as a cell value. A copied NULL arrives as an empty line, so an empty line is
/// NULL, except in text columns, where it is the empty string.
pub fn pasted_value(delegate: &GridDelegate, data_col: usize, line: &str) -> CellValue {
    let Some(col) = delegate.columns.get(data_col) else {
        return CellValue::Text(line.to_string());
    };
    if line.is_empty() && !is_text_type(&col.type_label) {
        return CellValue::Null;
    }
    if col.boolean {
        return CellValue::Text(normalize_bool(line));
    }
    CellValue::Text(line.to_string())
}

fn is_text_type(label: &str) -> bool {
    let base = label.split('(').next().unwrap_or(label).trim();
    matches!(
        base,
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "citext" | "name"
    )
}

pub fn apply_edit(delegate: &mut GridDelegate, row: usize, data_col: usize, value: CellValue) -> bool {
    if !delegate.can_edit(row, data_col) {
        return false;
    }
    delegate.model.set(row, data_col, value);
    true
}

#[cfg(test)]
mod tests {
    use super::{display_value, is_text_type, normalize_bool};

    #[test]
    fn text_types_keep_empty_strings() {
        assert!(is_text_type("varchar(40)"));
        assert!(is_text_type("text"));
        assert!(!is_text_type("numeric(12,2)"));
        assert!(!is_text_type("int4"));
    }

    #[test]
    fn booleans_and_empty_values() {
        assert_eq!(display_value("t", true), "true");
        assert_eq!(display_value("f", true), "false");
        assert_eq!(display_value("t", false), "t");
        assert_eq!(normalize_bool("TRUE"), "t");
        assert_eq!(normalize_bool(" no "), "f");
        assert_eq!(normalize_bool("maybe"), "maybe");
    }
}
