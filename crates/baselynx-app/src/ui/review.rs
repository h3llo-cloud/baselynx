//! Commit review dialog.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::ui::table_view::TableView;
use crate::ui::tokens;

/// Runs the commit; called by the button and, except for red connections, by Enter.
pub type CommitHandler = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct CommitReview {
    pub title: String,
    pub summary: String,
    pub statements: Vec<String>,
    pub target: String,
    pub red: bool,
    pub error: Option<String>,
    pub failing: Option<usize>,
    pub busy: bool,
}

impl CommitReview {
    pub fn new(title: String, summary: String, statements: Vec<String>, target: String, red: bool) -> Self {
        Self {
            title,
            summary,
            statements,
            target,
            red,
            error: None,
            failing: None,
            busy: false,
        }
    }

    pub fn set_busy(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        self.error = None;
        self.failing = None;
        cx.notify();
    }

    pub fn set_error(&mut self, message: String, failing: Option<usize>, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(message);
        self.failing = failing;
        cx.notify();
    }

    pub fn open(
        review: Entity<CommitReview>,
        on_commit: CommitHandler,
        owner: WeakEntity<TableView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (title, red, target) = {
            let r = review.read(cx);
            (r.title.clone(), r.red, r.target.clone())
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let busy = review.read(cx).busy;
            let on_commit = on_commit.clone();
            let enter_commit = on_commit.clone();
            let owner = owner.clone();
            let label = if red {
                format!("Commit to {target}")
            } else {
                "Commit".to_string()
            };
            dialog
                .title(title.clone())
                .w(tokens::ui(760., cx))
                .child(review.clone())
                // Enter commits, except to a red connection: there the button must be used.
                .on_ok(move |_, window, cx| {
                    if !red {
                        enter_commit(window, cx);
                    }
                    false
                })
                .on_close(move |_, _, cx| {
                    let _ = owner.update(cx, |t, cx| t.review_closed(cx));
                })
                .footer(
                    DialogFooter::new()
                        .gap_2()
                        .child(DialogClose::new().child(Button::new("review-cancel").label("Cancel").outline()))
                        .child(
                            Button::new("review-commit")
                                .label(label)
                                .loading(busy)
                                .disabled(busy)
                                .when(red, |b| b.danger())
                                .when(!red, |b| b.primary())
                                .on_click(move |_, window, cx| on_commit(window, cx)),
                        ),
                )
        });
    }
}

impl Render for CommitReview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let failing = self.failing;
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(tokens::secondary_text(cx))
                    .child(self.summary.clone()),
            )
            .child(
                v_flex()
                    .id("review-statements")
                    .max_h_80()
                    .overflow_y_scroll()
                    .gap_1()
                    .p_2()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .children(self.statements.iter().enumerate().map(|(i, s)| {
                        h_flex()
                            .id(("stmt", i))
                            .gap_2()
                            .items_start()
                            .px_1()
                            .when(failing == Some(i), |d| {
                                d.bg(tokens::removed_fill(cx))
                                    .border_l_2()
                                    .border_color(tokens::removed(cx))
                            })
                            .child(
                                div()
                                    .w_6()
                                    .flex_shrink_0()
                                    .text_color(tokens::secondary_text(cx))
                                    .child(format!("{}", i + 1)),
                            )
                            .child(div().whitespace_normal().child(format!("{s};")))
                    })),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(
                    div()
                        .p_2()
                        .rounded(cx.theme().radius)
                        .bg(tokens::removed_fill(cx))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_sm()
                        .text_color(tokens::error_text(cx))
                        .whitespace_normal()
                        .child(format!("Nothing was applied. {e}")),
                )
            })
    }
}
