//! File → Edit config file: the project's `baselynx.yaml` in an editor. The text is checked as
//! you type, with the error underlined; Save writes only a file baselynx can read, keeps the
//! previous one as `baselynx.yaml.bak`, and reloads the project from it.

use std::path::PathBuf;

use baselynx_core::config;
use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent, Rope, RopeExt as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Workspace, open_config_project};
use crate::ui::tokens;

pub(super) struct ConfigEditor {
    path: PathBuf,
    editor: Entity<EditorState>,
    /// The text as on disk, to tell whether there is anything to save.
    saved: String,
    error: Option<String>,
    _sub: Subscription,
}

/// Emitted after a save: the workspace reloads the project.
#[derive(Clone)]
pub(super) struct ConfigSaved;

impl EventEmitter<ConfigSaved> for ConfigEditor {}

impl Workspace {
    pub(super) fn edit_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.project.config_file.clone().map(PathBuf::from) else {
            window.push_notification(
                Notification::warning(
                    "This project was not opened from a config file. File → Open config file… opens one.",
                ),
                cx,
            );
            return;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                window.push_notification(
                    Notification::error(format!("{}: {e}", path.display())).autohide(false),
                    cx,
                );
                return;
            }
        };
        let view = cx.new(|cx| ConfigEditor::new(path.clone(), text, window, cx));
        let sub = cx.subscribe_in(&view, window, move |this, _, _: &ConfigSaved, window, cx| {
            window.close_dialog(cx);
            this.reload_config(path.clone(), window, cx);
        });
        self._subs.push(sub);
        let title = format!("Edit {}", self.project.config_file.clone().unwrap_or_default());
        let v2 = view.clone();
        window.open_dialog(cx, move |d, _, cx| {
            d.title(title.clone()).w(tokens::ui(1000., cx)).child(v2.clone())
        });
        view.update(cx, |v, cx| v.editor.update(cx, |e, cx| e.focus(window, cx)));
    }

    /// The project again from its file: after a save, or to pick up an edit made elsewhere.
    pub(super) fn reload_config(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.guard_then(window, cx, move |this, window, cx| {
            this.save_now(cx);
            match open_config_project(&this.store, &path) {
                Ok((project, report)) => {
                    for t in &this.tabs {
                        t.close(cx);
                    }
                    this.save_config_passwords(&report, cx);
                    this.load_project(project, window, cx);
                    this.refresh_backup_status(cx);
                    window.push_notification(
                        Notification::success(format!(
                            "Saved {}: {} added, {} updated, {} removed",
                            config::FILE_NAME,
                            report.added,
                            report.updated,
                            report.removed
                        )),
                        cx,
                    );
                }
                Err(e) => window.push_notification(Notification::error(e.to_string()).autohide(false), cx),
            }
        });
    }
}

impl ConfigEditor {
    fn new(path: PathBuf, text: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("yaml")
                .line_number(true)
                .default_value(text.clone())
        });
        let sub = cx.subscribe(&editor, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.check(cx);
            }
        });
        let mut this = Self {
            path,
            editor,
            saved: text,
            error: None,
            _sub: sub,
        };
        this.check(cx);
        this
    }

    fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// Parses the text; a mistake is shown below and underlined where the parser points.
    fn check(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let prefix = format!("{}: ", self.path.display());
        self.error = config::parse(&text, &self.path).err().map(|e| {
            // The dialog's title already names the file; the parser's source excerpt is noise.
            let e = e.to_string();
            let e = e.strip_prefix(&prefix).unwrap_or(&e);
            let e = e.strip_prefix("error: ").unwrap_or(e);
            let e = e.lines().next().unwrap_or(e).trim();
            // Name the line the underline is on.
            match (config::error_position(e), config::locate_error(e, &text)) {
                (Some((l, c)), Some((l2, _))) if l != l2 => {
                    e.replacen(&format!("line {l} column {c}"), &format!("line {l2}"), 1)
                }
                _ => e.to_string(),
            }
        });
        let mark = self.error.clone().and_then(|e| {
            let (line, col) = config::locate_error(&e, &text)?;
            let rope = Rope::from(text.as_str());
            let start = text
                .split_inclusive('\n')
                .take(line.saturating_sub(1))
                .map(str::len)
                .sum::<usize>()
                + col.saturating_sub(1);
            let start = start.min(text.len());
            let end = text[start..].find('\n').map_or(text.len(), |n| start + n).max(start);
            let end = if end == start { (start + 1).min(text.len()) } else { end };
            Some((rope.offset_to_position(start)..rope.offset_to_position(end), e))
        });
        self.editor.update(cx, |e, cx| {
            if let Some(set) = e.diagnostics_mut() {
                set.clear();
                if let Some((range, message)) = mark {
                    set.push(Diagnostic::new(range, message).with_severity(DiagnosticSeverity::Error));
                }
            }
            cx.notify();
        });
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        match config::save(&self.path, &text) {
            Ok(_) => {
                self.saved = text;
                cx.emit(ConfigSaved);
            }
            Err(e) => {
                self.error = Some(e.to_string());
                cx.notify();
            }
        }
    }

    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = std::fs::read_to_string(&self.path).unwrap_or_else(|_| self.saved.clone());
        self.saved = text.clone();
        self.editor.update(cx, |e, cx| e.set_value(text, window, cx));
        self.check(cx);
    }
}

impl Render for ConfigEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let changed = self.text(cx) != self.saved;
        let can_save = changed && self.error.is_none();
        let muted = tokens::secondary_text(cx);
        v_flex()
            .gap_2()
            .child(
                div()
                    .h(tokens::ui(560., cx))
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .overflow_hidden()
                    .child(Editor::new(&self.editor).bordered(false).h_full()),
            )
            .child(match &self.error {
                Some(e) => div().text_sm().text_color(tokens::error_text(cx)).child(e.clone()),
                None => div().text_sm().text_color(muted).child(if changed {
                    "The file is valid. Save writes it, keeps the previous one as baselynx.yaml.bak and reloads the project."
                } else {
                    "No changes. The keys are described in baselynx.example.yaml."
                }),
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("config-revert")
                            .label("Revert to the file")
                            .small()
                            .ghost()
                            .disabled(!changed)
                            .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("config-cancel")
                            .label("Cancel")
                            .small()
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("config-save")
                            .label("Save and reload")
                            .small()
                            .primary()
                            .disabled(!can_save)
                            .when(can_save, |b| b.on_click(cx.listener(|this, _, _, cx| this.save(cx)))),
                    ),
            )
    }
}
