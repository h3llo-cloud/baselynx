//! Backups taken from the app: the schedule from the config file, run once at start-up for
//! databases whose last backup is older than their interval, and "Back up now". What is on
//! disk for each connection is cached in [`BackupQueue::status`] for the Backups view and the
//! status bar. Restores go through [`RestoreDialog`], which shows every guardrail and asks
//! for the database's name; the core checks everything again before it writes.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use baselynx_core::backup::{self, BackupJob, Preflight};
use baselynx_core::config::{BackupFormat, BackupPlan, BackupSettings};
use baselynx_core::model::ConnectionConfig;
use baselynx_core::runtime;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::Workspace;
use crate::format;
use crate::ui::tokens;

/// Backups waiting and the one running. One at a time: pg_dump is heavy on the source.
#[derive(Default)]
pub(super) struct BackupQueue {
    pub(super) waiting: VecDeque<Uuid>,
    pub(super) running: Option<(Uuid, Instant)>,
    /// Backups queued since the queue was last empty, and how many of them have finished:
    /// "Backing up orders-db (2 of 5)".
    batch_total: usize,
    batch_done: usize,
    /// What is on disk per connection, read in the background.
    pub(super) status: HashMap<Uuid, backup::Status>,
}

impl BackupQueue {
    pub(super) fn is_queued(&self, id: Uuid) -> bool {
        self.running.is_some_and(|(r, _)| r == id) || self.waiting.contains(&id)
    }

    fn push(&mut self, id: Uuid) -> bool {
        if self.is_queued(id) {
            return false;
        }
        if self.running.is_none() && self.waiting.is_empty() {
            self.batch_total = 0;
            self.batch_done = 0;
        }
        self.batch_total += 1;
        self.waiting.push_back(id);
        true
    }
}

impl Workspace {
    pub(super) fn backup_settings(&self) -> BackupSettings {
        self.project
            .backups
            .clone()
            .unwrap_or_else(|| BackupSettings::default_in(self.store.root()))
    }

    /// Reads what is on disk for every connection of the project, off the UI thread.
    pub(super) fn refresh_backup_status(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self.project.connections.iter().map(|c| c.id).collect();
        self.refresh_backup_status_of(ids, cx);
    }

    fn refresh_backup_status_of(&mut self, ids: Vec<Uuid>, cx: &mut Context<Self>) {
        let settings = self.backup_settings();
        let configs: Vec<ConnectionConfig> = ids
            .iter()
            .filter_map(|id| self.project.connection(*id).cloned())
            .collect();
        let project = self.project.id;
        let fut = runtime::spawn_blocking(move || {
            Ok(configs
                .iter()
                .map(|c| (c.id, backup::status(&settings, c)))
                .collect::<Vec<_>>())
        });
        cx.spawn(async move |this, cx| {
            let Ok(found) = fut.await else { return };
            let _ = this.update(cx, |this, cx| {
                // A project switched in the meantime has its own statuses.
                if this.project.id != project {
                    return;
                }
                this.backups.status.extend(found);
                cx.notify();
            });
        })
        .detach();
    }

    /// "Backing up orders-db (2 of 5)" while backups run.
    pub(super) fn backup_progress(&self) -> Option<String> {
        let (id, _) = self.backups.running?;
        let name = self.project.connection(id).map(|c| c.name.as_str()).unwrap_or("?");
        let total = self.backups.batch_total.max(1);
        Some(if total == 1 {
            format!("Backing up {name}")
        } else {
            format!("Backing up {name} ({} of {total})", self.backups.batch_done + 1)
        })
    }

    /// Connections whose last backup attempt failed.
    pub(super) fn failed_backups(&self) -> usize {
        self.project
            .connections
            .iter()
            .filter(|c| self.backups.status.get(&c.id).is_some_and(|s| s.failure.is_some()))
            .count()
    }

    /// At start-up: queues every scheduled backup that is due. Reading the manifests runs
    /// off the UI thread.
    pub(super) fn schedule_backups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.backup_settings();
        let planned: Vec<(Uuid, ConnectionConfig)> = self
            .project
            .connections
            .iter()
            .filter(|c| c.backup.is_some())
            .map(|c| (c.id, c.clone()))
            .collect();
        if planned.is_empty() {
            return;
        }
        let fut = runtime::spawn_blocking(move || {
            let now = chrono::Utc::now();
            Ok(planned
                .into_iter()
                .filter(|(_, c)| {
                    let plan = c.backup.as_ref().expect("filtered");
                    backup::is_due(plan, backup::last_backup(&settings, c), now)
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>())
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(due) = fut.await else { return };
            if due.is_empty() {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                let n = due.len();
                window.push_notification(
                    Notification::info(format!(
                        "Backing up {n} database{} on schedule",
                        if n == 1 { "" } else { "s" }
                    )),
                    cx,
                );
                for id in due {
                    this.backups.push(id);
                }
                this.pump_backups(window, cx);
            });
        })
        .detach();
    }

    /// "Back up now": the connection's schedule's formats, or the project's.
    pub(super) fn back_up_now(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.backups.push(id) {
            window.push_notification(Notification::info("A backup of this database is already under way"), cx);
            return;
        }
        self.pump_backups(window, cx);
        cx.notify();
    }

    /// The password of a connection that is not read from a Kubernetes secret.
    fn backup_password(&self, id: Uuid, cx: &mut Context<Self>) -> Task<Option<String>> {
        let Some(c) = self.project.connection(id) else {
            return Task::ready(None);
        };
        if c.kube.as_ref().is_some_and(|k| k.credentials.secret.is_some()) {
            return Task::ready(None);
        }
        if let Some(p) = self
            .conns
            .get(&id)
            .and_then(|rt| rt.password.clone())
            .or_else(|| self.config_passwords.get(&id).cloned())
        {
            return Task::ready(Some(p));
        }
        let secrets = self.secrets.clone();
        let fut = runtime::spawn_blocking(move || secrets.get(id));
        cx.background_spawn(async move { fut.await.ok().flatten() })
    }

    fn pump_backups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.backups.running.is_some() {
            return;
        }
        let Some(id) = self.backups.waiting.pop_front() else {
            return;
        };
        let Some(config) = self.project.connection(id).cloned() else {
            self.backups.batch_done += 1;
            self.pump_backups(window, cx);
            return;
        };
        self.backups.running = Some((id, Instant::now()));
        let settings = self.backup_settings();
        let plan = config
            .backup
            .clone()
            .unwrap_or_else(|| BackupPlan::manual(settings.formats.clone()));
        let password = self.backup_password(id, cx);
        let name = config.name.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let started = Instant::now();
            let password = password.await;
            let result = backup::run_backup(BackupJob {
                config,
                password,
                settings,
                plan,
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.backups.running = None;
                this.backups.batch_done += 1;
                match result {
                    Ok(run) => {
                        let bytes: u64 = run.manifest.files.iter().map(|f| f.bytes).sum();
                        let formats: Vec<&str> = run.manifest.formats.iter().map(|f| f.label()).collect();
                        window.push_notification(
                            Notification::success(format!(
                                "Backed up {name}: {} ({}) in {} s, from {}",
                                format::bytes(bytes),
                                formats.join(" + "),
                                started.elapsed().as_secs(),
                                run.manifest.source
                            )),
                            cx,
                        );
                    }
                    Err(e) => window.push_notification(
                        Notification::error(format!("Backup of {name} failed: {e}")).autohide(false),
                        cx,
                    ),
                }
                this.refresh_backup_status_of(vec![id], cx);
                this.pump_backups(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------ restore

    /// "Restore from backup…": pick a backup folder, see every check, type the name.
    pub(super) fn restore_backup(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.project.connection(id).cloned() else {
            return;
        };
        if !target.allow_restore {
            window.push_notification(
                Notification::warning(
                    "Restores into this connection are off. Set allow_restore: true for it in the config file.",
                )
                .autohide(false),
                cx,
            );
            return;
        }
        // A file inside the backup's folder (its manifest.json, say) names exactly one backup.
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Restore this backup".into()),
        });
        let settings = self.backup_settings();
        let password = self.backup_password(id, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else { return };
            let Some(picked) = paths.into_iter().next() else { return };
            let Some(run_dir) = backup_folder(&picked) else {
                let _ = this.update_in(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::error(format!(
                            "{} is not part of a backup (no manifest.json beside it)",
                            picked.display()
                        ))
                        .autohide(false),
                        cx,
                    );
                });
                return;
            };
            let password = password.await;
            let pre = backup::preflight(
                target.clone(),
                password.clone(),
                run_dir.clone(),
                settings.tools.clone(),
            )
            .await;
            let _ = this.update_in(cx, |this, window, cx| match pre {
                Ok(pre) => this.open_restore_dialog(target, password, run_dir, settings, pre, window, cx),
                Err(e) => window.push_notification(Notification::error(e.to_string()).autohide(false), cx),
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn open_restore_dialog(
        &mut self,
        target: ConnectionConfig,
        password: Option<String>,
        run_dir: PathBuf,
        settings: BackupSettings,
        pre: Preflight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| RestoreDialog::new(target, run_dir, pre, window, cx));
        let d2 = dialog.clone();
        let sub = cx.subscribe_in(&dialog, window, move |this, _, ev: &RestoreConfirmed, window, cx| {
            window.close_dialog(cx);
            this.run_restore(ev.clone(), password.clone(), settings.tools.clone(), window, cx);
        });
        self._subs.push(sub);
        window.open_dialog(cx, move |d, _, cx| {
            d.title("Restore from backup").w(tokens::ui(620., cx)).child(d2.clone())
        });
        dialog.update(cx, |d, cx| d.focus(window, cx));
    }

    fn run_restore(
        &mut self,
        ev: RestoreConfirmed,
        password: Option<String>,
        tools: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = ev.target.name.clone();
        let id = ev.target.id;
        window.push_notification(Notification::info(format!("Restoring into {name}…")), cx);
        let fut = backup::restore(ev.target, password, ev.run_dir, ev.format, tools, ev.typed);
        cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(r) => {
                        window.push_notification(
                            Notification::success(format!(
                                "Restored {} into {name} from {} in {} s",
                                r.database,
                                match r.format {
                                    BackupFormat::Dump => "the dump",
                                    BackupFormat::Sql => "the SQL files",
                                },
                                r.duration.as_secs()
                            ))
                            .autohide(false),
                            cx,
                        );
                        this.refresh_connection(id, window, cx);
                    }
                    Err(e) => window.push_notification(
                        Notification::error(format!("Restore into {name} did not run: {e}")).autohide(false),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// The backup a picked file belongs to; for a folder, the folder itself or the newest backup
/// in it.
fn backup_folder(picked: &Path) -> Option<PathBuf> {
    if picked.is_file() {
        let dir = picked.parent()?;
        return dir.join("manifest.json").is_file().then(|| dir.to_path_buf());
    }
    if picked.join("manifest.json").is_file() {
        return Some(picked.to_path_buf());
    }
    backup::list_runs(picked).into_iter().next().map(|r| r.dir)
}

#[derive(Clone)]
pub(super) struct RestoreConfirmed {
    target: ConnectionConfig,
    run_dir: PathBuf,
    format: BackupFormat,
    typed: String,
}

pub(super) struct RestoreDialog {
    target: ConnectionConfig,
    run_dir: PathBuf,
    pre: Preflight,
    format: BackupFormat,
    input: Entity<InputState>,
    _sub: Subscription,
}

impl EventEmitter<RestoreConfirmed> for RestoreDialog {}

impl RestoreDialog {
    fn new(
        target: ConnectionConfig,
        run_dir: PathBuf,
        pre: Preflight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("database name"));
        let sub = cx.subscribe(&input, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        let format = match &pre.manifest {
            Some(m) if !m.formats.contains(&BackupFormat::Dump) => BackupFormat::Sql,
            _ => BackupFormat::Dump,
        };
        Self {
            target,
            run_dir,
            pre,
            format,
            input,
            _sub: sub,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |i, cx| i.focus(window, cx));
    }

    fn ready(&self, cx: &App) -> bool {
        self.pre.passed()
            && self
                .pre
                .database
                .as_deref()
                .is_some_and(|db| self.input.read(cx).value().trim() == db)
    }
}

impl Render for RestoreDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = tokens::secondary_text(cx);
        let danger = cx.theme().danger;
        let ok_color = cx.theme().success;
        let database = self.pre.database.clone().unwrap_or_else(|| "?".into());
        let mut body = v_flex().gap_3().text_sm();

        // What goes where.
        if let Some(m) = &self.pre.manifest {
            let when = m.finished.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M");
            body = body.child(
                v_flex()
                    .gap_1()
                    .child(div().font_weight(FontWeight::MEDIUM).child("Backup"))
                    .child(format!("{} · database {} · taken {when}", m.connection, m.database))
                    .child(div().text_color(muted).child(format!("from {}", m.source)))
                    .child(div().text_color(muted).child(self.run_dir.display().to_string())),
            );
        }
        body = body.child(
            v_flex()
                .gap_1()
                .child(div().font_weight(FontWeight::MEDIUM).child("Into"))
                .child(format!("{} · database {database}", self.target.name)),
        );
        if self.pre.manifest.is_some() && !self.pre.same_connection {
            body = body.child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().warning.opacity(0.15))
                    .child(
                        Icon::new(gpui_kit::assets::IconName::TriangleAlert)
                            .small()
                            .text_color(cx.theme().warning),
                    )
                    .child("This backup was taken from another database. Check that it is the one you mean."),
            );
        }

        // The guardrails.
        let mut checks = v_flex().gap_1();
        for c in &self.pre.checks {
            let icon = if c.ok {
                Icon::new(gpui_kit::assets::IconName::CircleCheck).text_color(ok_color)
            } else {
                Icon::new(gpui_kit::assets::IconName::CircleX).text_color(danger)
            };
            checks = checks.child(
                h_flex().gap_2().items_start().child(icon.small()).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(c.label.clone())
                        .when(!c.detail.is_empty(), |d| {
                            d.child(div().text_color(muted).child(c.detail.clone()))
                        }),
                ),
            );
        }
        body = body.child(checks);

        // Format, when the backup has both.
        let both = self
            .pre
            .manifest
            .as_ref()
            .is_some_and(|m| m.formats.contains(&BackupFormat::Dump) && m.formats.contains(&BackupFormat::Sql));
        if both {
            let pick = |id: &'static str, f: BackupFormat, label: &'static str, cx: &mut Context<Self>| {
                let b = Button::new(id).label(label).small();
                let b = if self.format == f { b.primary() } else { b.outline() };
                b.on_click(cx.listener(move |this, _, _, cx| {
                    this.format = f;
                    cx.notify();
                }))
            };
            let sqlite = self.target.engine == baselynx_core::model::Engine::Sqlite;
            let (dump_label, sql_label) = if sqlite {
                ("the file copy", "the SQL files")
            } else {
                ("the dump (pg_restore)", "the SQL files (psql)")
            };
            body = body.child(
                h_flex()
                    .gap_2()
                    .child("Restore from")
                    .child(pick("fmt-dump", BackupFormat::Dump, dump_label, cx))
                    .child(pick("fmt-sql", BackupFormat::Sql, sql_label, cx)),
            );
        }

        let passed = self.pre.passed();
        let ready = self.ready(cx);
        body = body
            .when(passed, |d| {
                d.child(
                    v_flex()
                        .gap_1()
                        .child(format!(
                            "Type {database} to confirm. The restore runs in one transaction."
                        ))
                        .child(Input::new(&self.input).small()),
                )
            })
            .when(!passed, |d| {
                d.child(
                    div()
                        .text_color(danger)
                        .child("Restore is not possible until every check passes."),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("restore-cancel")
                            .label("Cancel")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("restore-go")
                            .label("Restore")
                            .danger()
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.ready(cx) {
                                    return;
                                }
                                cx.emit(RestoreConfirmed {
                                    target: this.target.clone(),
                                    run_dir: this.run_dir.clone(),
                                    format: this.format,
                                    typed: this.input.read(cx).value().to_string(),
                                });
                            })),
                    ),
            );
        body
    }
}
