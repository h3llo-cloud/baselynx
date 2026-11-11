//! Export: the options dialog, the save prompt and the background job.

use std::path::PathBuf;

use baselynx_core::export::{self, ExportFormat, ExportSource};
use baselynx_core::{CancellationToken, CoreError, watch};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::notification::Notification;
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_state::ExportJob;
use crate::format;
use crate::ui::tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportScope {
    CurrentPage,
    AllRows,
}

/// Options chosen in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportChoice {
    pub format: ExportFormat,
    pub scope: ExportScope,
}

pub struct ExportOptions {
    pub format: ExportFormat,
    pub scope: ExportScope,
    /// Table tabs offer the scope choice; consoles export what they loaded.
    pub offer_scope: bool,
    pub notes: Vec<SharedString>,
}

impl Render for ExportOptions {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let format_button = |id: &'static str, label: &'static str, f: ExportFormat, cx: &mut Context<Self>| {
            Button::new(id)
                .label(label)
                .when(self.format == f, |b| b.primary())
                .when(self.format != f, |b| b.outline())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.format = f;
                    cx.notify();
                }))
        };
        let scope_button = |id: &'static str, label: &'static str, s: ExportScope, cx: &mut Context<Self>| {
            Button::new(id)
                .label(label)
                .when(self.scope == s, |b| b.primary())
                .when(self.scope != s, |b| b.outline())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.scope = s;
                    cx.notify();
                }))
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w_20().child("Format"))
                    .child(format_button("fmt-csv", "CSV", ExportFormat::Csv, cx))
                    .child(format_button("fmt-xlsx", "Excel", ExportFormat::Xlsx, cx)),
            )
            .when(self.offer_scope, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(div().w_20().child("Rows"))
                        .child(scope_button("scope-page", "Current page", ExportScope::CurrentPage, cx))
                        .child(scope_button(
                            "scope-all",
                            "All rows matching the filter",
                            ExportScope::AllRows,
                            cx,
                        )),
                )
            })
            .children(
                self.notes
                    .iter()
                    .map(|n| div().text_sm().text_color(tokens::secondary_text(cx)).child(n.clone())),
            )
    }
}

/// Opens the options dialog; `on_choose` runs when the user presses Export.
pub fn open_options(
    offer_scope: bool,
    notes: Vec<SharedString>,
    window: &mut Window,
    cx: &mut App,
    on_choose: impl Fn(ExportChoice, &mut Window, &mut App) + 'static,
) {
    let options = cx.new(|_| ExportOptions {
        format: ExportFormat::Csv,
        scope: ExportScope::CurrentPage,
        offer_scope,
        notes,
    });
    let on_choose = std::rc::Rc::new(on_choose);
    window.open_dialog(cx, move |dialog, _, cx| {
        let options = options.clone();
        let on_choose = on_choose.clone();
        dialog
            .title("Export")
            .w(tokens::ui(520., cx))
            .child(options.clone())
            .on_ok(move |_, window, cx| {
                let o = options.read(cx);
                let choice = ExportChoice {
                    format: o.format,
                    scope: o.scope,
                };
                let on_choose = on_choose.clone();
                window.defer(cx, move |window, cx| on_choose(choice, window, cx));
                true
            })
            .footer(
                DialogFooter::new()
                    .gap_2()
                    .child(DialogClose::new().child(Button::new("export-cancel").label("Cancel").outline()))
                    .child(DialogAction::new().child(Button::new("export-go").primary().label("Export…"))),
            )
    });
}

/// Asks for a path, then runs the export in the background. The job is handed to
/// `store` so the owning tab can show progress; `finished` runs at the end.
#[allow(clippy::too_many_arguments)]
pub fn run<V: 'static>(
    view: WeakEntity<V>,
    source: ExportSource,
    format: ExportFormat,
    base_name: String,
    window: &mut Window,
    cx: &mut App,
    store: fn(&mut V, Option<ExportJob>),
    on_progress: fn(&mut V, u64),
) {
    let file_name = format!("{}.{}", sanitize_file_name(&base_name), format.extension());
    let dir = default_dir();
    let prompt = cx.prompt_for_new_path(&dir, Some(&file_name));
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let path: PathBuf = match prompt.await {
            Ok(Ok(Some(p))) => p,
            _ => return,
        };
        let cancel = CancellationToken::new();
        let (tx, mut rx) = watch::channel(0u64);
        let fut = export::export(source, format, path, base_name, tx, cancel.clone());
        let view_progress = view.clone();
        let progress_task = cx.spawn(async move |cx| {
            while rx.changed().await.is_ok() {
                let rows = *rx.borrow_and_update();
                if view_progress
                    .update(cx, |v, cx| {
                        on_progress(v, rows);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let _ = view.update(cx, |v, cx| {
            store(
                v,
                Some(ExportJob {
                    rows: 0,
                    cancel: cancel.clone(),
                    _task: progress_task,
                }),
            );
            cx.notify();
        });
        let result = fut.await;
        let _ = view.update(cx, |v, cx| {
            store(v, None);
            cx.notify();
        });
        let _ = window_handle.update(cx, |_, window, cx| {
            let note = match &result {
                Ok(r) => {
                    let mut text = format!(
                        "Exported {} to {}",
                        format::rows(r.rows),
                        r.path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    );
                    if r.truncated_cells > 0 {
                        text.push_str(&format!(
                            " · {} cells cut to the Excel limit",
                            format::count(r.truncated_cells)
                        ));
                    }
                    Notification::success(text)
                }
                Err(CoreError::Cancelled) => Notification::info("Export cancelled"),
                Err(e) => Notification::error(format!("Export failed: {e}")).autohide(false),
            };
            window.push_notification(note, cx);
        });
    })
    .detach();
}

fn default_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| {
            d.download_dir()
                .map(|p| p.to_path_buf())
                .or_else(|| Some(d.home_dir().to_path_buf()))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn sanitize_file_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.trim().is_empty() { "result".into() } else { s }
}
