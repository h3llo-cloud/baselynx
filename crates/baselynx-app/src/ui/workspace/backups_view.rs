//! View → Backups: per connection, the schedule, the last backup, what is kept, what is
//! running and the last failure, with "Back up now" and the folder. It follows the workspace,
//! so a running backup shows as it happens.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::Workspace;
use crate::format;
use crate::ui::tokens;

pub(super) struct BackupsView {
    ws: WeakEntity<Workspace>,
    _observe: Subscription,
    /// Redraws the running backup's clock.
    _tick: Task<()>,
}

impl Workspace {
    pub(super) fn show_backups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_backup_status(cx);
        let ws = cx.entity();
        let view = cx.new(|cx| BackupsView::new(ws, cx));
        window.open_dialog(cx, move |d, _, cx| {
            d.title("Backups").w(tokens::ui(1080., cx)).child(view.clone())
        });
    }
}

impl BackupsView {
    fn new(ws: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&ws, |_, _, cx| cx.notify());
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            ws: ws.downgrade(),
            _observe: observe,
            _tick: tick,
        }
    }
}

/// Fixed column widths at 100 % zoom: connection, schedule, last backup, size, kept, state.
const WIDTHS: [f32; 5] = [200., 90., 200., 150., 100.];

impl Render for BackupsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(ws) = self.ws.upgrade() else {
            return div().into_any_element();
        };
        let w = ws.read(cx);
        let muted = tokens::secondary_text(cx);
        let error = tokens::error_text(cx);
        let settings = w.backup_settings();
        let now = chrono::Utc::now();
        let col = |i: usize, cx: &App| div().w(tokens::ui(WIDTHS[i], cx)).flex_shrink_0().min_w_0();

        let header = h_flex()
            .gap_2()
            .text_sm()
            .child(div().text_color(muted).child("Backups go to"))
            .child(div().flex_1().min_w_0().truncate().child(settings.directory.clone()))
            .child({
                let dir = std::path::PathBuf::from(&settings.directory);
                Button::new("open-backups-dir")
                    .label("Open folder")
                    .small()
                    .outline()
                    .disabled(!dir.is_dir())
                    .on_click(move |_, _, cx| cx.open_with_system(&dir))
            });

        let titles = h_flex()
            .gap_3()
            .py_1()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(col(0, cx).child("Connection"))
            .child(col(1, cx).child("Schedule"))
            .child(col(2, cx).child("Last backup"))
            .child(col(3, cx).child("Size"))
            .child(col(4, cx).child("Kept"))
            .child(div().flex_1().child("State"));

        let mut rows = v_flex().gap_0p5();
        for c in &w.project.connections {
            let id = c.id;
            let st = w.backups.status.get(&id);
            let last = st.and_then(|s| s.last());
            let schedule = c
                .backup
                .as_ref()
                .map(|p| format!("every {}", format::every(p.every_secs)))
                .unwrap_or_else(|| "—".into());
            let last_text = last
                .map(|r| {
                    let local = r.manifest.finished.with_timezone(&chrono::Local);
                    format!(
                        "{} · {}",
                        local.format("%Y-%m-%d %H:%M"),
                        format::ago(r.manifest.finished, now)
                    )
                })
                .unwrap_or_else(|| "never".into());
            let size_text = last
                .map(|r| {
                    let bytes: u64 = r.manifest.files.iter().map(|f| f.bytes).sum();
                    let formats: Vec<&str> = r.manifest.formats.iter().map(|f| f.label()).collect();
                    format!("{} · {}", format::bytes(bytes), formats.join(" + "))
                })
                .unwrap_or_default();
            let kept_text = st
                .filter(|s| !s.runs.is_empty())
                .map(|s| format!("{} · {}", s.runs.len(), format::bytes(s.total_bytes())))
                .unwrap_or_default();

            // What is happening, most urgent first.
            let running = w.backups.running.filter(|(r, _)| *r == id);
            let waiting = w.backups.waiting.contains(&id);
            let failure = st
                .and_then(|s| s.failure.clone())
                .filter(|f| last.is_none_or(|r| f.at > r.manifest.finished));
            let (state, state_color, tip): (String, Option<Hsla>, Option<String>) = if let Some((_, since)) = running {
                (
                    format!("Running · {}", format::duration(since.elapsed())),
                    Some(cx.theme().primary),
                    None,
                )
            } else if waiting {
                ("Waiting".into(), None, None)
            } else if let Some(f) = failure {
                (
                    format!("Failed {}: {}", format::ago(f.at, now), short_error(&f.error)),
                    Some(error),
                    Some(f.error.clone()),
                )
            } else if let Some(plan) = &c.backup {
                match st.and_then(|s| s.next_due(plan)) {
                    Some(next) if next > now => (format!("OK · next after {}", local_time(next, now)), None, None),
                    _ => ("Due: taken at the next start".into(), Some(cx.theme().warning), None),
                }
            } else if last.is_some() {
                ("OK · no schedule".into(), None, None)
            } else {
                (String::new(), None, None)
            };

            let dir = st.map(|s| s.dir.clone());
            let ws2 = self.ws.clone();
            let queued = running.is_some() || waiting;
            rows = rows.child(
                h_flex()
                    .gap_3()
                    .py_1()
                    .text_sm()
                    .child(
                        col(0, cx).child(
                            h_flex()
                                .gap_1p5()
                                .when_some(tokens::connection(c.color, cx), |d, col| {
                                    d.child(div().size_2().flex_shrink_0().rounded_full().bg(col))
                                })
                                .child(div().truncate().child(c.name.clone())),
                        ),
                    )
                    .child(col(1, cx).text_color(muted).child(schedule))
                    .child(col(2, cx).truncate().child(last_text))
                    .child(col(3, cx).truncate().text_color(muted).child(size_text))
                    .child(col(4, cx).truncate().text_color(muted).child(kept_text))
                    .child(
                        div()
                            .id(SharedString::from(format!("backup-state-{id}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when_some(state_color, |d, col| d.text_color(col))
                            .child(state)
                            .when_some(tip, |d, tip| {
                                d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("backup-now-{id}")))
                            .label("Back up now")
                            .xsmall()
                            .outline()
                            .disabled(queued)
                            .on_click(move |_, window, cx| {
                                let _ = ws2.update(cx, |w, cx| w.back_up_now(id, window, cx));
                            }),
                    )
                    .child({
                        let exists = dir.as_ref().is_some_and(|d| d.is_dir());
                        Button::new(SharedString::from(format!("backup-dir-{id}")))
                            .icon(gpui_kit::component::IconName::FolderOpen)
                            .xsmall()
                            .ghost()
                            .disabled(!exists)
                            .tooltip("Open this connection's backups")
                            .on_click(move |_, _, cx| {
                                if let Some(d) = &dir {
                                    cx.open_with_system(d);
                                }
                            })
                    }),
            );
        }
        if w.project.connections.is_empty() {
            rows = rows.child(div().py_4().text_color(muted).child("This project has no connections."));
        }

        v_flex()
            .gap_3()
            .child(header)
            .child(
                v_flex()
                    .child(titles)
                    .child(
                        div()
                            .id("backup-rows")
                            .max_h(tokens::ui(480., cx))
                            .overflow_y_scroll()
                            .child(rows),
                    ),
            )
            .child(
                div().text_xs().text_color(muted).child(
                    "A schedule comes from the backup: key of a database in the config file; a due backup is taken when baselynx starts. Back up now works for any connection.",
                ),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("backups-close")
                        .label("Close")
                        .small()
                        .outline()
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
            .into_any_element()
    }
}

/// The part of a pg_dump error worth a glance: its last line, after the tool's prefixes.
/// The whole text is in the tooltip.
fn short_error(s: &str) -> String {
    if s.contains("no password supplied") {
        return "no password: connect once, or put it in the config file".into();
    }
    let last = s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(s);
    let tail = ["error: ", "failed: "]
        .iter()
        .filter_map(|k| last.rfind(k).map(|i| &last[i + k.len()..]))
        .min_by_key(|t| t.len())
        .unwrap_or(last);
    tail.trim().to_string()
}

/// `14:30` today, `13.11 14:30` on another day.
fn local_time(t: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let (t, now) = (t.with_timezone(&chrono::Local), now.with_timezone(&chrono::Local));
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%d.%m %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::short_error;

    #[test]
    fn short_errors() {
        assert_eq!(
            short_error(
                "pg_dump from x failed: pg_dump: error: connection to server failed: fe_sendauth: no password supplied"
            ),
            "no password: connect once, or put it in the config file"
        );
        assert_eq!(
            short_error("pg_dump failed: pg_dump: error: query failed: ERROR:  permission denied for table t"),
            "ERROR:  permission denied for table t"
        );
        assert_eq!(short_error("timed out"), "timed out");
    }
}
