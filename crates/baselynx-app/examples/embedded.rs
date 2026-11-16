//! The embedded workspace (`baselynx_app::embed`) in a window of its own, on the test database
//! of baselynx's tests (an embedded PostgreSQL on port 54329, or `BASELYNX_TEST_DSN`):
//!
//!     cargo run -p baselynx --example embedded
//!
//! It shows what another application gets: the tables and consoles of the connections it gives,
//! without baselynx's menus, connection form, config file or backups.

#[path = "../../baselynx-core/tests/common/mod.rs"]
mod common;

use baselynx_app::embed::{self, DbWorkspace, EmbedConnection, EmbedOptions};
use gpui_kit::*;

fn main() {
    let db = common::db();
    let mut config = db.config.clone();
    config.name = "test database".into();
    let connection = EmbedConnection {
        config,
        password: db.password.clone(),
    };
    let root = std::env::temp_dir().join("baselynx-embedded-example");
    gpui_kit::application()
        .with_assets(baselynx_app::assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            embed::init(cx);
            baselynx_app::ui::tokens::install(baselynx_app::ui::tokens::ThemePref::System, 1.0, cx);
            let options = EmbedOptions {
                store_root: root.clone(),
                review_all_writes: false,
                read_only: false,
            };
            let window = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(800.)), cx)),
                ..Default::default()
            };
            gpui_kit::open_window(window, cx, |window, cx| {
                cx.new(|cx| DbWorkspace::new(vec![connection.clone()], options, window, cx))
            })
            .expect("open the window");
            cx.activate(true);
        });
}
