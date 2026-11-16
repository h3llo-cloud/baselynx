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

/// Four console tabs on a SQLite file (an empty file is an empty database, so no server).
fn console_project(store: &Store, dir: &std::path::Path) -> Vec<Uuid> {
    let path = dir.join("tabs.db");
    std::fs::write(&path, b"").unwrap();
    let mut conn = ConnectionConfig::new("notes");
    conn.engine = baselynx_core::model::Engine::Sqlite;
    conn.database = path.to_string_lossy().into_owned();
    let mut p = Project::new("Tabs test");
    let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
    for (i, id) in ids.iter().enumerate() {
        p.tabs.push(TabState::Console {
            id: *id,
            connection: conn.id,
            title: format!("console {}", i + 1),
            text: format!("select {};", i + 1),
        });
    }
    p.active_tab = Some(ids[0]);
    p.connections.push(conn);
    store.save_project(&p).unwrap();
    ids
}

fn tab_ids(ws: &Entity<Workspace>, cx: &App) -> Vec<Uuid> {
    ws.read(cx).tabs.iter().map(|t| t.id(cx)).collect()
}

#[gpui_kit::test]
fn tab_menu_closes_others_and_reopens(cx: &mut TestAppContext) {
    init(cx);
    // The SQLite connection connects on the core runtime, outside the test scheduler.
    cx.executor().allow_parking();
    let (dir, store) = temp_store();
    let ids = console_project(&store, dir.path());
    let (w, ws) = open(cx, store, Arc::new(MemorySecretStore::default()));
    wait_until(cx, w, "the four tabs", |cx| ws.read(cx).tabs.len() == 4);

    // Right-click the second tab (on its close button: tabs themselves are not registered for
    // tests; the click reaches the tab all the same). Item 1 is "Close others".
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.right_click(("close-tab", 1usize), cx);
    });
    cx.run_until_parked();
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(1usize, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(tab_ids(&ws, cx), vec![ids[1]], "only the clicked tab stays");
        assert_eq!(ws.read(cx).active, Some(0), "and it is active");
    });

    // Ctrl+Shift+T brings the closed tabs back, the last closed first, with their text.
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-t", cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let ws_ = ws.read(cx);
        assert_eq!(tab_ids(&ws, cx), vec![ids[1], ids[3]]);
        assert_eq!(ws_.active, Some(1), "the reopened tab is active");
        match ws_.tabs[1].state(cx) {
            TabState::Console { text, .. } => assert_eq!(text, "select 4;"),
            other => panic!("a console comes back, not {other:?}"),
        }
    });

    // Middle-click closes a tab.
    frame(cx, w, |window, cx| {
        window.render_frame(cx);
        window.click_with_options(
            ("close-tab", 0usize),
            gpui_kit::test::ClickOptions::new().with_button(gpui_kit::MouseButton::Middle),
            cx,
        );
    });
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(tab_ids(&ws, cx), vec![ids[3]]));
}

// ---------------------------------------------------------------- embedded in another application

mod embedded {
    use super::*;
    use crate::embed::{DbWorkspace, EmbedConnection, EmbedEvent, EmbedOptions};
    use gpui_kit::{
        Context, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, div,
    };

    /// A host window: an area of its own that can hold the focus, beside the embedded workspace.
    struct Host {
        focus: FocusHandle,
        ws: Entity<DbWorkspace>,
    }

    impl Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().id("host-area").track_focus(&self.focus).h_8())
                .child(div().size_full().child(self.ws.clone()))
        }
    }

    fn init_embed(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::embed::init(cx);
            crate::ui::tokens::install(crate::ui::tokens::ThemePref::Light, 1.0, cx);
        });
    }

    fn test_connection() -> (EmbedConnection, Option<String>) {
        let db = common::db();
        let mut config = db.config.clone();
        config.id = Uuid::new_v4();
        config.name = "embedded".into();
        config.save_password = true;
        (
            EmbedConnection {
                config,
                password: db.password.clone(),
            },
            db.password.clone(),
        )
    }

    fn open_host(
        cx: &mut TestAppContext,
        conns: Vec<EmbedConnection>,
        options: EmbedOptions,
    ) -> (AnyWindowHandle, Entity<Host>, Entity<DbWorkspace>) {
        let mut ws_out = None;
        let window = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let ws = cx.new(|cx| DbWorkspace::new(conns, options, window, cx));
                ws_out = Some(ws.clone());
                cx.new(|cx| Host {
                    focus: cx.focus_handle(),
                    ws,
                })
            })
            .expect("open the window")
        });
        let (w, host) = window;
        (w, host, ws_out.unwrap())
    }

    fn inner(ws: &Entity<DbWorkspace>, cx: &App) -> Entity<Workspace> {
        ws.read(cx).workspace().clone()
    }

    fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[gpui_kit::test]
    fn the_embedded_workspace_shows_its_connections_and_nothing_else(cx: &mut TestAppContext) {
        init_embed(cx);
        let root = tempfile::tempdir().unwrap();
        let (conn, password) = test_connection();
        let id = conn.config.id;
        let options = EmbedOptions {
            store_root: root.path().to_path_buf(),
            review_all_writes: false,
            read_only: false,
        };
        let (w, _host, ws) = open_host(cx, vec![conn], options);
        wait_until(cx, w, "the connection", |cx| inner(&ws, cx).read(cx).is_connected(id));
        frame(cx, w, |window, cx| {
            window.render_frame(cx);
            for absent in [
                "menu-file",
                "add-connection",
                "open-config",
                "first-connection",
                "first-import",
            ] {
                assert!(window.try_find(absent).is_none(), "{absent} is shown");
            }
        });
        // The host's "Go to" finds tables, and opens one.
        let items = cx.update(|cx| ws.read(cx).palette_items("customers", cx));
        assert!(items.iter().any(|i| i.label == "demo.customers"), "{items:?}");
        frame(cx, w, |window, cx| {
            ws.update(cx, |d, cx| d.open_table(id, "demo", "customers", window, cx))
        });
        wait_until(cx, w, "the table tab", |cx| !inner(&ws, cx).read(cx).tabs.is_empty());
        // Tabs are kept in the host's folder; no password ever is.
        cx.update(|cx| inner(&ws, cx).update(cx, |w, cx| w.save_now(cx)));
        cx.run_until_parked();
        let files = files_under(root.path());
        assert!(!files.is_empty(), "nothing was kept");
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            for f in files {
                let text = std::fs::read_to_string(&f).unwrap_or_default();
                assert!(!text.contains(&pw), "the password is in {f:?}");
            }
        }
    }

    #[gpui_kit::test]
    fn read_only_and_review_reach_every_connection(cx: &mut TestAppContext) {
        init_embed(cx);
        let root = tempfile::tempdir().unwrap();
        let (conn, _) = test_connection();
        let options = EmbedOptions {
            store_root: root.path().to_path_buf(),
            review_all_writes: true,
            read_only: true,
        };
        let (_w, _host, ws) = open_host(cx, vec![conn], options);
        cx.run_until_parked();
        let c = cx.update(|cx| inner(&ws, cx).read(cx).project.connections[0].clone());
        assert!(c.read_only);
        assert!(!c.save_password);
        assert_eq!(c.color, baselynx_core::model::ConnectionColor::Red);
    }

    #[gpui_kit::test]
    fn its_keys_act_only_while_it_has_the_focus(cx: &mut TestAppContext) {
        init_embed(cx);
        let root = tempfile::tempdir().unwrap();
        let (conn, _) = test_connection();
        let id = conn.config.id;
        let options = EmbedOptions {
            store_root: root.path().to_path_buf(),
            review_all_writes: false,
            read_only: false,
        };
        let (w, host, ws) = open_host(cx, vec![conn], options);
        wait_until(cx, w, "the connection", |cx| inner(&ws, cx).read(cx).is_connected(id));
        // The host area has the focus: Ctrl+T does nothing here.
        frame(cx, w, |window, cx| {
            let f = host.read(cx).focus.clone();
            f.focus(window, cx);
        });
        cx.simulate_keystrokes(w, "ctrl-t");
        cx.run_until_parked();
        assert!(cx.update(|cx| inner(&ws, cx).read(cx).tabs.is_empty()));
        // The workspace has it: a console opens.
        frame(cx, w, |window, cx| {
            inner(&ws, cx).update(cx, |w, cx| w.focus_main(window, cx))
        });
        cx.simulate_keystrokes(w, "ctrl-t");
        wait_until(cx, w, "a console", |cx| inner(&ws, cx).read(cx).tabs.len() == 1);
        // The window-wide shortcuts stay with the host: Ctrl+W does not close the tab.
        frame(cx, w, |window, cx| {
            inner(&ws, cx).update(cx, |w, cx| w.focus_main(window, cx))
        });
        cx.simulate_keystrokes(w, "ctrl-w");
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| inner(&ws, cx).read(cx).tabs.len()), 1);
    }

    #[gpui_kit::test]
    fn a_lost_connection_is_reported_and_set_connection_restores_it(cx: &mut TestAppContext) {
        init_embed(cx);
        let root = tempfile::tempdir().unwrap();
        let (good, _) = test_connection();
        let id = good.config.id;
        // First the address is wrong (nothing listens there).
        let mut bad = good.clone();
        bad.config.host = "127.0.0.1".into();
        bad.config.port = 1;
        let options = EmbedOptions {
            store_root: root.path().to_path_buf(),
            review_all_writes: false,
            read_only: false,
        };
        let (w, _host, ws) = open_host(cx, vec![bad], options);
        let events: Entity<Vec<EmbedEvent>> = cx.update(|cx| cx.new(|_| Vec::new()));
        let sink = events.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&ws, move |_, ev: &EmbedEvent, cx| {
                sink.update(cx, |v, _| v.push(ev.clone()))
            })
        });
        wait_until(cx, w, "the loss", |cx| !events.read(cx).is_empty());
        let first = cx.update(|cx| events.read(cx)[0].clone());
        assert!(
            matches!(&first, EmbedEvent::ConnectionLost { id: x, .. } if *x == id),
            "{first:?}"
        );
        // The host repaired the route: the same connection, the right address.
        frame(cx, w, |window, cx| {
            ws.update(cx, |ws, cx| ws.set_connection(id, good.clone(), window, cx))
        });
        wait_until(cx, w, "the connection", |cx| inner(&ws, cx).read(cx).is_connected(id));
    }
}
