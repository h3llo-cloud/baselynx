//! baselynx: a fast desktop client for PostgreSQL.

// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod app_state;
mod assets;
mod completion;
mod edit_model;
mod format;
mod page_diff;
mod palette_rank;
mod row_text;
mod ui;
#[cfg(test)]
mod ui_tests;
mod value_fmt;

use std::sync::Arc;

use baselynx_core::APP_NAME;
use baselynx_core::store::{KeyringStore, Store};
use gpui_kit::*;

use crate::ui::workspace::Workspace;

/// The config file to open at start: `BASELYNX_CONFIG_FILE`, else `baselynx.yaml` beside the
/// binary. A scratch `BASELYNX_CONFIG_DIR` (tests, trying things out) turns the second off, so
/// such a run never opens the real databases.
fn startup_config() -> Option<std::path::PathBuf> {
    if let Some(f) = std::env::var_os("BASELYNX_CONFIG_FILE") {
        return Some(f.into());
    }
    if std::env::var_os("BASELYNX_CONFIG_DIR").is_some() {
        return None;
    }
    baselynx_core::config::find_beside(&std::env::current_exe().ok()?)
}

fn main() {
    // Start the core runtime before the first window so connecting never waits on it.
    baselynx_core::runtime::runtime();
    gpui_kit::application().with_assets(assets::AppAssets).run(|cx| {
        gpui_kit::init(cx);
        actions::bind(cx);
        ui::palette::bind(cx);
        ui::workspace::menu_paste::bind(cx);
        cx.on_action(|_: &actions::Quit, cx: &mut App| cx.quit());

        let store = match Store::open_default() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{APP_NAME}: {e}");
                cx.quit();
                return;
            }
        };
        let state = store.load_state();
        ui::tokens::install(
            ui::tokens::ThemePref::from_saved(state.theme.as_deref()),
            state.zoom.unwrap_or(1.0),
            cx,
        );
        // One title row: the toolkit's title bar draws the window buttons beside baselynx's own
        // controls.
        let base = gpui_kit::component::TitleBar::window_options();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some(APP_NAME.into()),
                ..gpui_kit::component::TitleBar::title_bar_options()
            }),
            app_id: Some(APP_NAME.into()),
            window_min_size: Some(size(px(720.), px(420.))),
            ..base
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title(APP_NAME);
            cx.new(|cx| Workspace::new(store, Arc::new(KeyringStore), startup_config(), window, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
