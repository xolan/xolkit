//! The Connections page: the agent sessions connected to xolkit, the MCP servers it connects to,
//! the processes behind both, and what happened lately. Read from the daemon every two seconds
//! while the page is open.

use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
    button::{Button, ButtonVariants},
    h_flex,
    scroll::ScrollableElement,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_daemon::{Event, Request, ServerState, ServerStatus, SessionStatus, Status};

use super::{Selection, Shelf, ago, divider, empty_state, fact, matches, section};
use crate::settings;

/// How often the page asks the daemon how things are.
const POLL: Duration = Duration::from_secs(2);
/// Events the page lists.
const SHOWN_EVENTS: usize = 80;

fn state_label(state: &ServerState) -> String {
    match state {
        ServerState::Starting => "connecting".to_owned(),
        ServerState::Running => "connected".to_owned(),
        ServerState::NeedsSecret { key } => format!("needs {key}"),
        ServerState::Failed { .. } => "failed".to_owned(),
        ServerState::Stopped => "stopped".to_owned(),
    }
}

impl Shelf {
    /// Asks the daemon how things are, starting it if it isn't running.
    pub(super) fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let client = settings::client(cx);
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_spawn(async move { client.lock().unwrap().status() })
                .await;
            this.update(cx, |this, cx| {
                this.status = Some(status.map_err(|e| format!("{e:#}").into()));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keeps the status fresh while the Connections page is open.
    pub(super) fn watch_status(&mut self, cx: &mut Context<Self>) {
        self.status_task = Some(cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.refresh_status(cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(POLL).await;
            }
        }));
    }

    fn status(&self) -> Option<&Status> {
        self.status.as_ref()?.as_ref().ok()
    }

    fn visible_sessions(&self, cx: &App) -> Vec<&SessionStatus> {
        let filter = self.filter_text(cx);
        self.status()
            .map(|s| {
                s.sessions
                    .iter()
                    .filter(|s| {
                        let cwd = s
                            .cwd
                            .as_ref()
                            .map(|c| c.display().to_string())
                            .unwrap_or_default();
                        matches(
                            &filter,
                            &[
                                s.agent.name(),
                                s.client.as_deref().unwrap_or_default(),
                                &cwd,
                            ],
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_connections(&self, cx: &App) -> Vec<&ServerStatus> {
        let filter = self.filter_text(cx);
        self.status()
            .map(|s| {
                s.servers
                    .iter()
                    .filter(|s| matches(&filter, &[&s.name, &state_label(&s.state)]))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_events(&self, cx: &App) -> Vec<&Event> {
        let filter = self.filter_text(cx);
        self.status()
            .map(|s| {
                s.events
                    .iter()
                    .rev()
                    .filter(|e| matches(&filter, &[&e.source, &e.message]))
                    .take(SHOWN_EVENTS)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn connection_rows(&self, cx: &App) -> Vec<Selection> {
        self.visible_sessions(cx)
            .into_iter()
            .map(|s| Selection::Session(s.id))
            .chain(
                self.visible_connections(cx)
                    .into_iter()
                    .map(|s| Selection::Connection(s.name.clone())),
            )
            .collect()
    }

    fn restart_daemon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The daemon stops; the next request starts a new one, and shims reconnect to it.
        self.request(
            Request::Shutdown,
            Some("Restarting the daemon".into()),
            window,
            cx,
        );
    }

    pub(super) fn render_connections(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.status {
            None => empty_state(IconName::Loader, "Asking the daemon…", "", cx).into_any_element(),
            Some(Err(error)) => empty_state(
                IconName::CircleAlert,
                "The daemon isn't answering",
                error,
                cx,
            )
            .child(
                Button::new("retry-status")
                    .small()
                    .primary()
                    .icon(IconName::RefreshCw)
                    .label("Try again")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_status(cx))),
            )
            .into_any_element(),
            Some(Ok(status)) => self.render_status(status, cx),
        };
        v_flex()
            .size_full()
            .child(self.page_header("Connections", None, cx))
            .child(body)
            .into_any_element()
    }

    fn render_status(&self, status: &Status, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let theme = cx.theme().clone();
        let heading = |title: &str, n: usize| {
            h_flex()
                .px_4()
                .pt_4()
                .pb_1()
                .gap_2()
                .border_b_1()
                .border_color(theme.border)
                .child(section(title))
                .child(div().text_sm().text_color(muted).child(n.to_string()))
        };
        let sessions = self.visible_sessions(cx);
        let servers = self.visible_connections(cx);
        let events = self.visible_events(cx);

        let mut rows: Vec<AnyElement> = Vec::new();
        let daemon = h_flex()
            .px_4()
            .py_3()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(Lucide::Cpu).text_color(muted))
            .child(
                v_flex()
                    .flex_1()
                    .child(div().font_medium().child(format!(
                        "xolkit daemon {} · pid {}",
                        status.version, status.pid
                    )))
                    .child(div().text_sm().text_color(muted).child(format!(
                        "Started {}; exits {} minutes after the last session and the app are gone",
                        ago(status.started),
                        status.idle_minutes
                    ))),
            )
            .child(
                Button::new("restart-daemon")
                    .small()
                    .icon(IconName::RefreshCw)
                    .label("Restart")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.restart_daemon(window, cx))),
            );
        rows.push(daemon.into_any_element());

        rows.push(heading("Agent sessions", sessions.len()).into_any_element());
        if sessions.is_empty() {
            rows.push(
                div()
                    .px_4()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child("No agent is connected. Agents connect when a session starts.")
                    .into_any_element(),
            );
        }
        for session in sessions {
            let selection = Selection::Session(session.id);
            let selected = self.selected.as_ref() == Some(&selection);
            let place = session
                .cwd
                .as_ref()
                .map(|c| self.env.tilde(c))
                .unwrap_or_else(|| "unknown folder".to_owned());
            let description = format!(
                "{place} · connected {} · {} calls{}",
                ago(session.connected),
                session.calls,
                if session.errors > 0 {
                    format!(", {} failed", session.errors)
                } else {
                    String::new()
                }
            );
            rows.push(self.row(
                format!("session-{}", session.id),
                selected,
                session.agent.name().to_owned(),
                session.client.as_deref(),
                description,
                Vec::new(),
                cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
                cx,
            ));
        }

        rows.push(heading("MCP servers", servers.len()).into_any_element());
        if servers.is_empty() {
            rows.push(
                div()
                    .px_4()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child("No server is on for any agent.")
                    .into_any_element(),
            );
        }
        for server in servers {
            let selection = Selection::Connection(server.name.clone());
            let selected = self.selected.as_ref() == Some(&selection);
            let mut parts = vec![
                if server.remote { "remote" } else { "local" }.to_owned(),
                format!("{} tools", server.tools),
            ];
            if let Some(pid) = server.pid {
                parts.push(format!("pid {pid}"));
            }
            if let Some(ms) = server.latency_ms {
                parts.push(format!("{ms} ms last call"));
            }
            let state = state_label(&server.state);
            rows.push(self.row(
                format!("connection-{}", server.name),
                selected,
                server.name.clone(),
                Some(state.as_str()),
                parts.join(" · "),
                Vec::new(),
                cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
                cx,
            ));
        }

        rows.push(heading("Recent events", events.len()).into_any_element());
        for event in events {
            rows.push(
                h_flex()
                    .px_4()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .child(
                        div()
                            .w(px(80.))
                            .flex_shrink_0()
                            .text_color(muted)
                            .child(ago(event.time)),
                    )
                    .child(
                        div()
                            .w(px(110.))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(muted)
                            .child(event.source.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(event.error, |this| this.text_color(theme.danger))
                            .child(event.message.clone()),
                    )
                    .into_any_element(),
            );
        }
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                v_flex()
                    .id("connections")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .children(rows),
            )
            .vertical_scrollbar(&self.list_scroll)
            .into_any_element()
    }

    pub(super) fn session_detail(&self, id: u64, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.status()?;
        let session = status.sessions.iter().find(|s| s.id == id)?;
        let source = format!("session {id}");
        let events: Vec<&Event> = status
            .events
            .iter()
            .rev()
            .filter(|e| e.source == source)
            .take(40)
            .collect();
        let disconnect = Button::new("disconnect")
            .danger()
            .label("Disconnect")
            .disabled(self.busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.request(
                    Request::Disconnect { session: id },
                    Some("Disconnected".into()),
                    window,
                    cx,
                )
            }));
        let mut facts = vec![
            fact("Agent", session.agent.name().to_owned(), cx),
            fact("Process", format!("xolkit mcp, pid {}", session.pid), cx),
            fact("Connected", ago(session.connected), cx),
            fact("Calls", session.calls.to_string(), cx),
        ];
        if let Some(client) = &session.client {
            facts.insert(1, fact("Client", client.clone(), cx));
        }
        if let Some(cwd) = &session.cwd {
            facts.push(fact("Folder", self.env.tilde(cwd), cx));
        }
        if let Some(last) = session.last_call {
            facts.push(fact("Last call", ago(last), cx));
        }
        Some(self.event_detail(
            format!("Session {id}"),
            session.agent.name().to_owned(),
            disconnect,
            facts,
            events,
            cx,
        ))
    }

    pub(super) fn connection_detail(
        &self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let status = self.status()?;
        let server = status.servers.iter().find(|s| s.name == name)?;
        let events: Vec<&Event> = status
            .events
            .iter()
            .rev()
            .filter(|e| e.source == name)
            .take(40)
            .collect();
        let stopped = server.state == ServerState::Stopped;
        let action = {
            let name = name.to_owned();
            Button::new("server-power")
                .label(if stopped { "Start" } else { "Stop" })
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let request = if stopped {
                        Request::RestartServer { name: name.clone() }
                    } else {
                        Request::StopServer { name: name.clone() }
                    };
                    this.request(request, None, window, cx)
                }))
        };
        let mut facts = vec![
            fact("State", state_label(&server.state), cx),
            fact("Tools", server.tools.to_string(), cx),
            fact("Calls", server.calls.to_string(), cx),
        ];
        if let ServerState::Failed { message } = &server.state {
            facts.push(fact("Error", message.clone(), cx));
        }
        if let Some(pid) = server.pid {
            facts.push(fact("Process", format!("pid {pid}"), cx));
        }
        if let Some(plugin) = &server.plugin {
            facts.push(fact("Plugin", plugin.clone(), cx));
        }
        let reconnect = {
            let name = name.to_owned();
            Button::new("reconnect")
                .small()
                .icon(IconName::RefreshCw)
                .label("Reconnect")
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.request(
                        Request::RestartServer { name: name.clone() },
                        None,
                        window,
                        cx,
                    )
                }))
        };
        let mut detail = self.event_detail(
            name.to_owned(),
            if server.remote {
                "Remote server"
            } else {
                "Local server"
            }
            .to_owned(),
            action,
            facts,
            events,
            cx,
        );
        detail = v_flex()
            .size_full()
            .child(detail)
            .child(h_flex().p_4().child(reconnect))
            .into_any_element();
        Some(detail)
    }

    fn event_detail(
        &self,
        title: String,
        subtitle: String,
        action: Button,
        facts: Vec<Div>,
        events: Vec<&Event>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        v_flex()
            .size_full()
            .child(self.detail_header(title, Some(subtitle), action, cx))
            .child(
                div()
                    .id("connection-detail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .child(v_flex().gap_1().children(facts))
                            .child(divider(cx))
                            .child(section("Log"))
                            .when(events.is_empty(), |this| {
                                this.child(div().text_sm().text_color(muted).child("Nothing yet."))
                            })
                            .children(events.into_iter().map(|e| {
                                v_flex()
                                    .text_sm()
                                    .child(div().text_xs().text_color(muted).child(ago(e.time)))
                                    .child(
                                        div()
                                            .when(e.error, |this| this.text_color(danger))
                                            .child(e.message.clone()),
                                    )
                            })),
                    ),
            )
            .into_any_element()
    }
}
