//! Add / edit connection dialog.

use std::rc::Rc;

use baselynx_core::model::{ConnectionColor, ConnectionConfig, SslMode, parse_connection_url};
use baselynx_core::session;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::ui::tokens;

enum TestState {
    Idle,
    Running(#[allow(dead_code)] Task<()>),
    Ok(String),
    Failed(String),
}

pub struct ConnectionForm {
    base: ConnectionConfig,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    ssl_mode: SslMode,
    color: ConnectionColor,
    save_password: bool,
    read_only: bool,
    test: TestState,
    error: Option<String>,
    filling: bool,
    _subs: Vec<Subscription>,
}

/// What the form produced.
pub struct FormResult {
    pub config: ConnectionConfig,
    /// `Some` when the user typed a password.
    pub password: Option<String>,
}

impl ConnectionForm {
    pub fn new(base: ConnectionConfig, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |v: &str, ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (v, ph) = (v.to_string(), ph.to_string());
            cx.new(|cx| InputState::new(window, cx).default_value(v).placeholder(ph))
        };
        let name = input(&base.name, "Name shown in the sidebar", window, cx);
        let host = input(&base.host, "localhost", window, cx);
        let port = input(&base.port.to_string(), "5432", window, cx);
        let database = input(&base.database, "postgres", window, cx);
        let user = input(&base.user, "postgres", window, cx);
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .default_value(password.unwrap_or_default())
                .placeholder(if base.save_password {
                    "Saved in the OS keyring"
                } else {
                    "Asked on connect"
                })
        });
        let mut subs = Vec::new();
        for i in [&name, &host, &port, &database, &user, &password] {
            let mut last_len = i.read(cx).value().len();
            subs.push(
                cx.subscribe_in(i, window, move |this, input, ev: &InputEvent, window, cx| {
                    if !matches!(ev, InputEvent::Change) {
                        return;
                    }
                    let text = input.read(cx).value().to_string();
                    // A paste arrives as one edit that adds several characters; typing adds one.
                    let pasted = text.len() > last_len + 1;
                    last_len = text.len();
                    if pasted
                        && !this.filling
                        && let Some(url) = parse_connection_url(&text)
                    {
                        this.fill_from_url(url, window, cx);
                    }
                }),
            );
        }
        Self {
            ssl_mode: base.ssl_mode,
            color: base.color,
            save_password: base.save_password,
            read_only: base.read_only,
            base,
            name,
            host,
            port,
            database,
            user,
            password,
            test: TestState::Idle,
            error: None,
            filling: false,
            _subs: subs,
        }
    }

    /// Pasting a `postgres://` URL into any field fills every field from it.
    pub fn fill_from_url(&mut self, url: baselynx_core::model::ParsedUrl, window: &mut Window, cx: &mut Context<Self>) {
        self.filling = true;
        let set = |input: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| {
            input.update(cx, |i, cx| i.set_value(v, window, cx));
        };
        let host = url.host.clone().unwrap_or_else(|| "localhost".into());
        set(&self.host, host.clone(), window, cx);
        set(&self.port, url.port.unwrap_or(5432).to_string(), window, cx);
        set(
            &self.database,
            url.database.clone().unwrap_or_else(|| "postgres".into()),
            window,
            cx,
        );
        set(&self.user, url.user.clone().unwrap_or_default(), window, cx);
        set(&self.password, url.password.clone().unwrap_or_default(), window, cx);
        let name_now = self.name.read(cx).value().to_string();
        if name_now.is_empty() || parse_connection_url(&name_now).is_some() || name_now == self.base.name {
            let db = url.database.clone().unwrap_or_default();
            let suggested = if db.is_empty() { host } else { format!("{db} @ {host}") };
            set(&self.name, suggested, window, cx);
        }
        if let Some(m) = url.ssl_mode {
            self.ssl_mode = m;
        }
        self.filling = false;
        cx.notify();
    }

    fn read(&self, cx: &App) -> Result<FormResult, String> {
        let v = |i: &Entity<InputState>| i.read(cx).value().trim().to_string();
        let mut config = self.base.clone();
        config.name = v(&self.name);
        config.host = v(&self.host);
        config.database = v(&self.database);
        config.user = v(&self.user);
        config.port = v(&self.port)
            .parse()
            .map_err(|_| "The port must be a number from 1 to 65535.".to_string())?;
        config.ssl_mode = self.ssl_mode;
        config.color = self.color;
        config.save_password = self.save_password;
        config.read_only = self.read_only;
        if config.host.is_empty() {
            return Err("Enter a host.".into());
        }
        if config.name.is_empty() {
            config.name = format!("{} @ {}", config.database, config.host);
        }
        let pw = self.password.read(cx).value().to_string();
        Ok(FormResult {
            config,
            password: if pw.is_empty() { None } else { Some(pw) },
        })
    }

    fn run_test(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let r = match self.read(cx) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let fut = session::connect(&r.config, r.password);
        self.test = TestState::Running(cx.spawn(async move |this, cx| {
            let result = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(s) => TestState::Ok(format!("Connected · PostgreSQL {}", s.server_version())),
                    Err(e) => TestState::Failed(e.to_string()),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Opens the dialog. `on_save` gets the result; returning `Err` keeps it open.
    pub fn open(
        title: &'static str,
        base: ConnectionConfig,
        password: Option<String>,
        window: &mut Window,
        cx: &mut App,
        on_save: impl Fn(FormResult, &mut Window, &mut App) + 'static,
    ) {
        let form = cx.new(|cx| ConnectionForm::new(base, password, window, cx));
        let on_save = Rc::new(on_save);
        let first = form.read(cx).name.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let form = form.clone();
            let on_save = on_save.clone();
            let test_form = form.clone();
            dialog
                .title(title)
                .w(tokens::ui(560., cx))
                .child(form.clone())
                .on_ok(move |_, window, cx| match form.read(cx).read(cx) {
                    Ok(r) => {
                        let on_save = on_save.clone();
                        window.defer(cx, move |window, cx| on_save(r, window, cx));
                        true
                    }
                    Err(e) => {
                        form.update(cx, |f, cx| {
                            f.error = Some(e);
                            cx.notify();
                        });
                        false
                    }
                })
                .footer(
                    DialogFooter::new()
                        .gap_2()
                        .child(
                            Button::new("conn-test")
                                .label("Test")
                                .outline()
                                .on_click(move |_, window, cx| {
                                    test_form.update(cx, |f, cx| f.run_test(window, cx));
                                }),
                        )
                        .child(div().flex_1())
                        .child(DialogClose::new().child(Button::new("conn-cancel").label("Cancel").outline()))
                        .child(DialogAction::new().child(Button::new("conn-save").label("Save").primary())),
                )
        });
        first.update(cx, |i, cx| i.focus(window, cx));
    }
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kube = self.base.kube.clone();
        let secret_backed = kube.as_ref().is_some_and(|k| k.credentials.secret.is_some());
        let ssl = self.ssl_mode;
        let color = self.color;
        let test = match &self.test {
            TestState::Idle => None,
            TestState::Running(_) => Some(("Connecting…".to_string(), false)),
            TestState::Ok(s) => Some((s.clone(), false)),
            TestState::Failed(e) => Some((e.clone(), true)),
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(tokens::secondary_text(cx))
                    .child(if kube.is_some() {
                        "A database in Kubernetes: baselynx opens a port-forward and reads the credentials from its secret."
                    } else {
                        "Paste a postgres:// URL into any field to fill them all."
                    }),
            )
            .child(
                v_form()
                    .child(field().label("Name").child(Input::new(&self.name)))
                    .when_some(kube.clone(), |form, k| {
                        let credentials = match &k.credentials.secret {
                            Some(s) => format!("secret {}/{s}", k.namespace),
                            None => "saved in the OS keyring".to_string(),
                        };
                        form.child(field().label("Kubernetes").child(
                            v_flex()
                                .text_sm()
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(k.describe())
                                .child(div().text_color(tokens::secondary_text(cx)).child(k.kubeconfig.clone()))
                                .child(div().text_color(tokens::secondary_text(cx)).child(credentials)),
                        ))
                    })
                    .when(kube.is_none(), |form| form.child(
                        field().label("Host").child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&self.host)))
                                .child(div().w_20().child(Input::new(&self.port))),
                        ),
                    )
                    .child(field().label("Database").child(Input::new(&self.database)))
                    .child(field().label("User").child(Input::new(&self.user)))
                    .child(
                        field()
                            .label("Password")
                            .child(Input::new(&self.password).mask_toggle()),
                    ))
                    .child(
                        field()
                            .label("SSL")
                            .child(h_flex().gap_1().children(SslMode::ALL.map(|m| {
                                Button::new(SharedString::from(format!("ssl-{}", m.label())))
                                    .label(m.label())
                                    .small()
                                    .when(ssl == m, |b| b.primary())
                                    .when(ssl != m, |b| b.outline())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ssl_mode = m;
                                        cx.notify();
                                    }))
                            }))),
                    )
                    .child(
                        field()
                            .label("Colour")
                            .child(h_flex().gap_1().children(ConnectionColor::ALL.map(|c| {
                                let dot = tokens::connection(c, cx);
                                Button::new(SharedString::from(format!("color-{}", c.label())))
                                    .small()
                                    .when(color == c, |b| b.primary())
                                    .when(color != c, |b| b.outline())
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .when_some(dot, |this, d| this.child(div().size_2().rounded_full().bg(d)))
                                            .child(c.label()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.color = c;
                                        cx.notify();
                                    }))
                            }))),
                    )
                    .child({
                        let replica = kube.as_ref().and_then(|k| k.replica.clone());
                        let ro = self.read_only;
                        let mode = |id: &'static str, label: String, on: bool| {
                            Button::new(id).small().label(label).when(on, |b| b.primary()).when(!on, |b| b.outline())
                        };
                        field().label("Mode").child(
                            h_flex()
                                .gap_1()
                                .child(
                                    mode(
                                        "mode-rw",
                                        if kube.is_some() { "Read-write (primary)".into() } else { "Read-write".into() },
                                        !ro,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.read_only = false;
                                        cx.notify();
                                    })),
                                )
                                .child(
                                    mode(
                                        "mode-ro",
                                        match &replica {
                                            Some(r) => format!("Read-only (replica {r})"),
                                            None => "Read-only".into(),
                                        },
                                        ro,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.read_only = true;
                                        cx.notify();
                                    })),
                                ),
                        )
                    })
                    .when(!secret_backed, |form| form.child(
                        field().child(
                            Checkbox::new("save-password")
                                .label("Save password in the OS keyring")
                                .checked(self.save_password)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.save_password = *checked;
                                    cx.notify();
                                })),
                        ),
                    )),
            )
            .when_some(test, |this, (text, failed)| {
                this.child(
                    div()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .whitespace_normal()
                        .when(failed, |d| d.text_color(tokens::error_text(cx)))
                        .child(text),
                )
            })
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_sm().text_color(tokens::error_text(cx)).child(e))
            })
    }
}

/// A one-field dialog: project names and passwords.
pub struct TextPrompt {
    pub input: Entity<InputState>,
    pub masked: bool,
}

impl Render for TextPrompt {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(Input::new(&self.input).when(self.masked, |i| i.mask_toggle()))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn open_text_prompt(
    title: String,
    description: Option<String>,
    initial: String,
    masked: bool,
    confirm_label: &'static str,
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl Fn(String, &mut Window, &mut App) + 'static,
) {
    let input = cx.new(|cx| InputState::new(window, cx).masked(masked).default_value(initial));
    let prompt = cx.new(|_| TextPrompt {
        input: input.clone(),
        masked,
    });
    let on_confirm = Rc::new(on_confirm);
    let focus_input = input.clone();
    // Enter and the button both go through the dialog's `on_ok`.
    window.open_dialog(cx, move |dialog, _, cx| {
        let input = input.clone();
        let on_confirm = on_confirm.clone();
        dialog
            .title(title.clone())
            .w(tokens::ui(440., cx))
            .when_some(description.clone(), |d, text| {
                d.child(div().pb_2().text_sm().child(text))
            })
            .child(prompt.clone())
            .on_ok(move |_, window, cx| {
                let v = input.read(cx).value().to_string();
                let on_confirm = on_confirm.clone();
                window.defer(cx, move |window, cx| on_confirm(v, window, cx));
                true
            })
            .footer(
                DialogFooter::new()
                    .gap_2()
                    .child(DialogClose::new().child(Button::new("prompt-cancel").label("Cancel").outline()))
                    .child(DialogAction::new().child(Button::new("prompt-ok").label(confirm_label).primary())),
            )
    });
    focus_input.update(cx, |i, cx| i.focus(window, cx));
}
