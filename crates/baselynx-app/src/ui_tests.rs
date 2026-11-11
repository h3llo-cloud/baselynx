//! Window tests with `#[gpui_kit::test]`.
//!
//! Tests that need data use the core test server (see the README) through the core's own
//! harness, so they share its database copy rules.

use std::sync::Arc;
use std::time::{Duration, Instant};

use baselynx_core::model::{ConnectionConfig, Project, TabState};
use baselynx_core::store::{MemorySecretStore, SecretStore, Store};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, App, AppContext as _, Entity, TestAppContext, Window, WindowOptions};
use uuid::Uuid;

use crate::ui::workspace::{TabView, Workspace};

#[path = "../../baselynx-core/tests/common/mod.rs"]
mod common;

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::actions::bind(cx);
        crate::ui::palette::bind(cx);
        crate::ui::workspace::menu_paste::bind(cx);
        crate::ui::tokens::install(crate::ui::tokens::ThemePref::Light, 1.0, cx);
    });
}

fn open(cx: &mut TestAppContext, store: Store, secrets: Arc<dyn SecretStore>) -> (AnyWindowHandle, Entity<Workspace>) {
    cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Workspace::new(store, secrets, None, window, cx))
        })
        .expect("open the window")
    })
}

fn frame(cx: &mut TestAppContext, w: AnyWindowHandle, f: impl FnOnce(&mut Window, &mut App)) {
    cx.update_window(w, |_, window, cx| f(window, cx))
        .expect("window is open");
}

/// Waits in real time: sessions run on the core's tokio runtime, outside the test clock.
fn wait_until(cx: &mut TestAppContext, w: AnyWindowHandle, what: &str, mut ready: impl FnMut(&mut App) -> bool) {
    let started = Instant::now();
    loop {
        cx.run_until_parked();
        frame(cx, w, |window, cx| window.render_frame(cx));
        if cx.update(|cx| ready(cx)) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path()).unwrap();
    (dir, store)
}

#[gpui_kit::test]
fn shell_renders(cx: &mut TestAppContext) {
    init(cx);
    let (_dir, store) = temp_store();
    let (w, _) = open(cx, store, Arc::new(MemorySecretStore::default()));
    cx.run_until_parked();
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.find("first-connection");
        window.find("open-config");
        window.find("menu-file");
    });
}

#[gpui_kit::test]
fn view_menu_toggles_the_sidebar(cx: &mut TestAppContext) {
    init(cx);
    let (_dir, store) = temp_store();
    let (w, _) = open(cx, store, Arc::new(MemorySecretStore::default()));
    cx.run_until_parked();
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.click("menu-view", cx);
    });
    cx.run_until_parked();
    // Item 3 of the View menu is "Hide sidebar".
    frame(cx, w, |window, cx| window.within("popup-menu").click(3usize, cx));
    cx.run_until_parked();
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("add-connection").is_none(), "the sidebar is hidden");
    });
}

/// A project with the test database and one table tab on `demo.customers`.
fn table_project(store: &Store, secrets: &MemorySecretStore) -> (Uuid, Uuid) {
    let db = common::db();
    let mut conn: ConnectionConfig = db.config.clone();
    conn.id = Uuid::new_v4();
    conn.name = "local".into();
    conn.save_password = true;
    if let Some(pw) = &db.password {
        secrets.set(conn.id, pw).unwrap();
    }
    let mut p = Project::new("UI test");
    let tab = Uuid::new_v4();
    p.tabs.push(TabState::Table {
        id: tab,
        connection: conn.id,
        schema: "demo".into(),
        table: "customers".into(),
        filter: None,
        sort: None,
        page_size: 100,
        refresh_secs: None,
    });
    p.active_tab = Some(tab);
    p.connections.push(conn.clone());
    store.save_project(&p).unwrap();
    (conn.id, tab)
}

fn table_of(ws: &Entity<Workspace>, cx: &App) -> Entity<crate::ui::table_view::TableView> {
    match ws.read(cx).tabs.first() {
        Some(TabView::Table(t)) => t.clone(),
        _ => panic!("the first tab is a table"),
    }
}

#[gpui_kit::test]
fn table_tab_edits_and_pages(cx: &mut TestAppContext) {
    init(cx);
    cx.executor().allow_parking();
    let (_dir, store) = temp_store();
    let secrets = MemorySecretStore::default();
    table_project(&store, &secrets);
    let (w, ws) = open(cx, store, Arc::new(secrets));
    wait_until(cx, w, "the first page", |cx| {
        table_of(&ws, cx).read(cx).loaded_rows(cx) == 100
    });

    // Editing a cell through the menu bar adds one pending change (D1).
    let table = cx.update(|cx| table_of(&ws, cx));
    cx.update(|cx| table.update(cx, |t, cx| t.select_cell(0, 1, cx)));
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.click("menu-edit", cx);
    });
    cx.run_until_parked();
    // Edit menu: 0 Undo, 1 Redo, 2 separator, 3 Copy value, 4 Edit cell, 5 Set NULL.
    frame(cx, w, |window, cx| window.within("popup-menu").click(5usize, cx));
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| table.read(cx).pending(cx)), 1, "Set NULL from the menu");

    // Deleting a row adds one change; restoring removes it.
    cx.update(|cx| table.update(cx, |t, cx| t.select_cell(1, 1, cx)));
    frame(cx, w, |window, cx| {
        window.dispatch_action(Box::new(crate::actions::DeleteRow), cx)
    });
    assert_eq!(cx.update(|cx| table.read(cx).pending(cx)), 2);
    frame(cx, w, |window, cx| {
        window.dispatch_action(Box::new(crate::actions::DeleteRow), cx)
    });
    assert_eq!(cx.update(|cx| table.read(cx).pending(cx)), 1);

    // Discard, then the pager moves to page 2.
    cx.update(|cx| table.update(cx, |t, cx| t.discard_all(cx)));
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.click("next", cx);
    });
    wait_until(cx, w, "page 2", |cx| {
        table.read(cx).page_number() == 1 && table.read(cx).loaded_rows(cx) == 100
    });
}
