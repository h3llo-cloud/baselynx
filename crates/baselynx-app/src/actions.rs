//! Every command is one action, reachable from its shortcut, buttons and menus.

use gpui_kit::*;

pub const WORKSPACE: &str = "Workspace";
pub const CONSOLE: &str = "PgConsole";
pub const TABLE: &str = "PgTable";
pub const GRID: &str = "PgGrid";
pub const CELL_EDITOR: &str = "PgCellEditor";
pub const FILTER: &str = "PgFilter";

actions!(
    baselynx,
    [
        OpenPalette,
        NewConsole,
        ToggleSidebar,
        CloseTab,
        NextTab,
        PrevTab,
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateTab9,
        AddConnection,
        OpenConfig,
        EditConfig,
        ShowBackups,
        NewProject,
        RunStatement,
        RunAll,
        StopQuery,
        Refresh,
        AddRow,
        DeleteRow,
        CommitChanges,
        DiscardChanges,
        PrevPage,
        NextPage,
        FocusFilter,
        ClearFilter,
        FilterByValue,
        ExcludeValue,
        SetNull,
        RevertCell,
        BeginEdit,
        CopyCell,
        ExportData,
        CountRows,
        ConfirmEditRight,
        ConfirmEditUp,
        ConfirmEditDown,
        CancelEdit,
        Quit,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        CycleTheme,
        ThemeLight,
        ThemeDark,
        ThemeSystem,
        GridRowStart,
        GridRowEnd,
        GridTop,
        GridBottom,
        SideUp,
        SideDown,
        SideLeft,
        SideRight,
        SideOpen,
        SideMenu,
        SideFilterEscape,
        FocusSidebarFilter,
        ShowHideSidebar,
        ToggleRecord,
        FollowKey,
        TrailBack,
        TrailForward,
        AcceptCompletion,
        FilterAccept,
        FilterComplete,
        FilterPrev,
        FilterNext,
        UndoEdit,
        RedoEdit,
        CopyRowInsert,
        CopyRowJson,
        CopyRowTsv,
        ExtendUp,
        ExtendDown,
        PasteCells,
    ]
);

pub const SIDEBAR: &str = "PgSidebar";

/// A binding whose `secondary-` resolves to Cmd on macOS and Ctrl elsewhere.
pub fn key<A: Action>(keys: &str, action: A, context: Option<&str>, cx: &App) -> KeyBinding {
    let predicate = context.map(|c| std::rc::Rc::new(KeyBindingContextPredicate::parse(c).expect("valid context")));
    KeyBinding::load(keys, Box::new(action), predicate, false, None, cx).expect("valid key binding")
}

pub fn bind(cx: &mut App) {
    let bindings = [
        key("secondary-k", OpenPalette, None, cx),
        key("secondary-t", NewConsole, None, cx),
        key("secondary-b", ToggleSidebar, None, cx),
        key("secondary-w", CloseTab, None, cx),
        key("ctrl-tab", NextTab, None, cx),
        key("ctrl-shift-tab", PrevTab, None, cx),
        key("secondary-1", ActivateTab1, None, cx),
        key("secondary-2", ActivateTab2, None, cx),
        key("secondary-3", ActivateTab3, None, cx),
        key("secondary-4", ActivateTab4, None, cx),
        key("secondary-5", ActivateTab5, None, cx),
        key("secondary-6", ActivateTab6, None, cx),
        key("secondary-7", ActivateTab7, None, cx),
        key("secondary-8", ActivateTab8, None, cx),
        key("secondary-9", ActivateTab9, None, cx),
        key("secondary-q", Quit, None, cx),
        // The last binding of an action is the one menus show: Ctrl+=, not Ctrl++.
        key("secondary-+", ZoomIn, None, cx),
        key("secondary-=", ZoomIn, None, cx),
        key("secondary--", ZoomOut, None, cx),
        key("secondary-0", ResetZoom, None, cx),
        // Inputs bind some ctrl keys themselves; at their depth, later bindings win.
        key("secondary-+", ZoomIn, Some("Input"), cx),
        key("secondary-=", ZoomIn, Some("Input"), cx),
        key("secondary--", ZoomOut, Some("Input"), cx),
        key("secondary-0", ResetZoom, Some("Input"), cx),
        // Tab switching keeps working with the caret in the SQL editor or the filter box.
        key("secondary-1", ActivateTab1, Some("Input"), cx),
        key("secondary-2", ActivateTab2, Some("Input"), cx),
        key("secondary-3", ActivateTab3, Some("Input"), cx),
        key("secondary-4", ActivateTab4, Some("Input"), cx),
        key("secondary-5", ActivateTab5, Some("Input"), cx),
        key("secondary-6", ActivateTab6, Some("Input"), cx),
        key("secondary-7", ActivateTab7, Some("Input"), cx),
        key("secondary-8", ActivateTab8, Some("Input"), cx),
        key("secondary-9", ActivateTab9, Some("Input"), cx),
        key("ctrl-tab", NextTab, Some("Input"), cx),
        key("ctrl-shift-tab", PrevTab, Some("Input"), cx),
        // record panel and keys (3.3, 3.4)
        key("secondary-i", ToggleRecord, Some(TABLE), cx),
        key("secondary-i", ToggleRecord, Some(CONSOLE), cx),
        key("f4", FollowKey, Some(GRID), cx),
        key("alt-[", TrailBack, None, cx),
        key("alt-]", TrailForward, None, cx),
        // sidebar tree (3.7)
        key("secondary-shift-k", FocusSidebarFilter, None, cx),
        key("up", SideUp, Some(SIDEBAR), cx),
        key("down", SideDown, Some(SIDEBAR), cx),
        key("left", SideLeft, Some(SIDEBAR), cx),
        key("right", SideRight, Some(SIDEBAR), cx),
        key("enter", SideOpen, Some(SIDEBAR), cx),
        key("menu", SideMenu, Some(SIDEBAR), cx),
        key("shift-f10", SideMenu, Some(SIDEBAR), cx),
        // In the filter box, Up/Down walk the results and Enter opens one.
        key("up", SideUp, Some("PgSidebar > Input"), cx),
        key("down", SideDown, Some("PgSidebar > Input"), cx),
        key("enter", SideOpen, Some("PgSidebar > Input"), cx),
        key("escape", SideFilterEscape, Some("PgSidebar > Input"), cx),
        // console
        key("secondary-enter", RunStatement, Some(CONSOLE), cx),
        key("secondary-shift-enter", RunAll, Some(CONSOLE), cx),
        // The editor binds these keys itself; at its depth, later bindings win.
        key("secondary-enter", RunStatement, Some("PgConsole > Input"), cx),
        // Tab takes the highlighted completion; without a menu it indents as before.
        key("tab", AcceptCompletion, Some("PgConsole > Input"), cx),
        key("secondary-shift-enter", RunAll, Some("PgConsole > Input"), cx),
        key("escape", StopQuery, Some(CONSOLE), cx),
        // table tab
        key("f5", Refresh, Some(TABLE), cx),
        key("secondary-r", Refresh, Some(TABLE), cx),
        key("secondary-n", AddRow, Some(TABLE), cx),
        key("secondary-delete", DeleteRow, Some(TABLE), cx),
        key("secondary-s", CommitChanges, Some(TABLE), cx),
        key("alt-left", PrevPage, Some(TABLE), cx),
        key("alt-right", NextPage, Some(TABLE), cx),
        key("secondary-f", FocusFilter, Some(TABLE), cx),
        key("secondary-shift-f", FilterByValue, Some(TABLE), cx),
        key("secondary-shift-e", ExcludeValue, Some(TABLE), cx),
        key("secondary-shift-n", SetNull, Some(TABLE), cx),
        key("escape", ClearFilter, Some(FILTER), cx),
        // In the filter box: suggestions and the filters used before (3.6).
        key("enter", FilterAccept, Some("PgFilter > Input"), cx),
        key("tab", FilterComplete, Some("PgFilter > Input"), cx),
        key("up", FilterPrev, Some("PgFilter > Input"), cx),
        key("down", FilterNext, Some("PgFilter > Input"), cx),
        // grid (console result and table)
        key("enter", BeginEdit, Some(GRID), cx),
        key("f2", BeginEdit, Some(GRID), cx),
        key("secondary-c", CopyCell, Some(GRID), cx),
        // Undo and redo of pending edits (4.7); inputs keep their own.
        key("secondary-z", UndoEdit, Some(GRID), cx),
        key("secondary-y", RedoEdit, Some(GRID), cx),
        key("secondary-shift-z", RedoEdit, Some(GRID), cx),
        // A range of cells in one column, as in a spreadsheet.
        key("shift-up", ExtendUp, Some("PgGrid > DataTable"), cx),
        key("shift-down", ExtendDown, Some("PgGrid > DataTable"), cx),
        key("secondary-v", PasteCells, Some(GRID), cx),
        // The toolkit's Home/End select the gutter column; at its depth, later bindings win.
        key("home", GridRowStart, Some("PgGrid > DataTable"), cx),
        key("end", GridRowEnd, Some("PgGrid > DataTable"), cx),
        key("secondary-home", GridTop, Some("PgGrid > DataTable"), cx),
        key("secondary-end", GridBottom, Some("PgGrid > DataTable"), cx),
        // cell editor
        key("tab", ConfirmEditRight, Some(CELL_EDITOR), cx),
        key("up", ConfirmEditUp, Some(CELL_EDITOR), cx),
        key("down", ConfirmEditDown, Some(CELL_EDITOR), cx),
        key("escape", CancelEdit, Some(CELL_EDITOR), cx),
    ];
    cx.bind_keys(bindings);
}
