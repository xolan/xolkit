//! The MCP servers page: servers xolkit connects to and passes on to agents. Servers are added
//! here or copied from an agent's own config; their secrets go to the OS keyring.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt,
    button::{Button, ButtonVariant, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    notification::Notification,
    scroll::ScrollableElement,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{
    Action as Step, Kind, Provider, Server, ServerConfig, agent_servers, secret_key,
    valid_server_name,
};
use xolkit_daemon::{Request, ServerState};

use super::{Selection, Shelf, cell, divider, fact, list_body, matches, section};
use crate::settings;

/// The fields of the add and edit dialog.
pub(super) struct ServerInputs {
    name: Entity<InputState>,
    command: Entity<InputState>,
    url: Entity<InputState>,
    /// `KEY=value` (local) or `Header: value` (remote), one a line, saved in `servers.toml`.
    plain: Entity<TextareaState>,
    /// The same, saved in the OS keyring.
    secret: Entity<TextareaState>,
}

impl ServerInputs {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> ServerInputs {
        let line = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let lines = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 6)
                    .placeholder(placeholder)
            })
        };
        ServerInputs {
            name: line("github", window, cx),
            command: line("npx -y @modelcontextprotocol/server-github", window, cx),
            url: line("https://example.com/mcp", window, cx),
            plain: lines("DEBUG=1  or  X-Team: core", window, cx),
            secret: lines("GITHUB_TOKEN=…  or  Authorization: Bearer …", window, cx),
        }
    }
}

/// `KEY=value` or `Key: value` lines as pairs; blank lines are skipped.
fn pairs(text: &str, remote: bool) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let split = if remote {
            line.split_once(':')
        } else {
            line.split_once('=')
        };
        let Some((key, value)) = split else {
            let shape = if remote { "Header: value" } else { "KEY=value" };
            return Err(format!("“{line}” isn't {shape}"));
        };
        out.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    Ok(out)
}

fn lines(pairs: &BTreeMap<String, String>, remote: bool) -> String {
    let separator = if remote { ": " } else { "=" };
    pairs
        .iter()
        .map(|(k, v)| format!("{k}{separator}{v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A server as typed: its config, and the secrets to save (empty to keep the saved one).
fn read_form(
    name: &str,
    command: &str,
    url: &str,
    plain: &str,
    secret: &str,
) -> Result<(String, ServerConfig, BTreeMap<String, String>), String> {
    let name = name.trim().to_owned();
    if !valid_server_name(&name) {
        return Err("Name it with letters, digits, - and _, up to 32 characters.".to_owned());
    }
    let remote = !url.trim().is_empty();
    let secrets = pairs(secret, remote)?;
    let mut config = ServerConfig {
        secrets: secrets.keys().cloned().collect(),
        ..ServerConfig::default()
    };
    if remote {
        config.url = Some(url.trim().to_owned());
        config.headers = pairs(plain, true)?;
    } else {
        let mut words = command.split_whitespace().map(str::to_owned);
        config.command = words.next();
        config.args = words.collect();
        config.env = pairs(plain, false)?;
    }
    config.check().map_err(|e| format!("{e:#}"))?;
    Ok((name, config, secrets))
}

impl Shelf {
    pub(super) fn visible_servers(&self, cx: &App) -> impl Iterator<Item = Server> + use<> {
        let filter = self.filter_text(cx);
        self.library
            .all_servers()
            .into_iter()
            .filter(move |s| matches(&filter, &[&s.name, &s.config.summary()]))
    }

    fn server_state(&self, name: &str) -> Option<ServerState> {
        let Some(Ok(status)) = &self.status else {
            return None;
        };
        status
            .servers
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.state.clone())
    }

    pub(super) fn render_servers(&self, cx: &mut Context<Self>) -> AnyElement {
        let servers: Vec<_> = self.visible_servers(cx).collect();
        let rows: Vec<_> = servers.iter().map(|s| self.server_row(s, cx)).collect();
        let empty = rows.is_empty() && self.filter_text(cx).is_empty();
        let mut columns = self.agent_columns();
        columns.push("");
        v_flex()
            .size_full()
            .child(
                self.page_header("MCP servers", Some(rows.len()), cx)
                    .child(
                        Button::new("copy-servers")
                            .small()
                            .icon(IconName::Copy)
                            .label("Copy from agents…")
                            .tooltip("Copy servers set up in Claude Code or Codex")
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.open_copy_dialog(window, cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("add-server")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .label("Add server…")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_server_dialog(None, window, cx)
                            })),
                    ),
            )
            .child(self.column_header(&columns, cx))
            .child(if empty {
                super::empty_state(
                    IconName::Inbox,
                    "No MCP servers yet",
                    "Add a server by the command that starts it or by its URL. xolkit connects \
                     to it once and passes its tools on to every agent you turn it on for.",
                    cx,
                )
                .into_any_element()
            } else {
                list_body("server-list", rows, &self.list_scroll, cx)
            })
            .into_any_element()
    }

    fn server_row(&self, server: &Server, cx: &mut Context<Self>) -> AnyElement {
        let name = server.name.clone();
        let selection = Selection::Server(name.clone());
        let selected = self.selected.as_ref() == Some(&selection);
        let (controls, tag) = match &server.plugin {
            // A plugin's server goes with the plugin's switches.
            Some(plugin) => {
                let mut cells: Vec<AnyElement> = self
                    .agent_columns()
                    .iter()
                    .map(|_| cell().into_any_element())
                    .collect();
                cells.push(cell().into_any_element());
                (cells, format!("{} · from {plugin}", server.config.kind()))
            }
            None => {
                let mut controls = self.item_switches(Kind::Server, &name, false, cx);
                controls.push(self.row_uninstall(format!("remove-server-{name}"), &selection, cx));
                (controls, server.config.kind().to_owned())
            }
        };
        let tag = match self.server_state(&name) {
            Some(ServerState::NeedsSecret { key }) => format!("{tag} · needs {key}"),
            Some(ServerState::Failed { .. }) => format!("{tag} · failed"),
            _ => tag,
        };
        self.row(
            format!("server-{name}"),
            selected,
            name,
            Some(tag.as_str()),
            server.config.summary(),
            controls,
            cx.listener(move |this, _, _, cx| {
                this.select(Some(selection.clone()), cx);
                this.refresh_status(cx);
            }),
            cx,
        )
    }

    pub(super) fn server_detail(&self, name: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let server = self
            .library
            .all_servers()
            .into_iter()
            .find(|s| s.name == name)?;
        let config = &server.config;
        let mut facts = vec![fact(
            if config.is_remote() { "URL" } else { "Command" },
            config.summary(),
            cx,
        )];
        let plain = if config.is_remote() {
            &config.headers
        } else {
            &config.env
        };
        for (key, value) in plain {
            facts.push(fact(key, value.clone(), cx));
        }
        for key in &config.secrets {
            facts.push(fact(key, "saved in the OS keyring".to_owned(), cx));
        }
        let state = match self.server_state(name) {
            None => "Not connected: no agent has it on".to_owned(),
            Some(ServerState::Starting) => "Connecting…".to_owned(),
            Some(ServerState::Running) => "Connected".to_owned(),
            Some(ServerState::NeedsSecret { key }) => {
                format!("Waiting for {key}: edit the server to save it")
            }
            Some(ServerState::Failed { message }) => format!("Failed: {message}"),
            Some(ServerState::Stopped) => "Stopped".to_owned(),
        };
        let edit = server.plugin.is_none().then(|| {
            let name = name.to_owned();
            Button::new("edit-server")
                .icon(Lucide::Pencil)
                .label("Edit…")
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_server_dialog(Some(name.clone()), window, cx)
                }))
        });
        let restart = {
            let name = name.to_owned();
            Button::new("restart-server")
                .small()
                .icon(IconName::RefreshCw)
                .label("Reconnect")
                .disabled(self.busy || self.server_state(&name).is_none())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.request(
                        Request::RestartServer { name: name.clone() },
                        Some(format!("Reconnecting to {name}").into()),
                        window,
                        cx,
                    )
                }))
        };
        Some(
            v_flex()
                .size_full()
                .child(self.detail_header(
                    server.name.clone(),
                    Some(match &server.plugin {
                        Some(plugin) => format!("From the plugin {plugin}"),
                        None => format!("{} server", config.kind()),
                    }),
                    edit,
                    cx,
                ))
                .child(
                    div()
                        .id("server-detail")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(
                            v_flex()
                                .p_4()
                                .gap_3()
                                .child(v_flex().gap_1().children(facts))
                                .child(h_flex().gap_2().child(div().text_sm().flex_1().child(state)).child(restart))
                                .child(divider(cx))
                                .child(section("Tools"))
                                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!(
                                    "Agents see its tools as {name}__<tool>; in Claude Code, \
                                     mcp__xolkit__{name}__<tool>."
                                ))),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Opens the add dialog, or the edit dialog for server `editing`.
    pub(super) fn open_server_dialog(
        &mut self,
        editing: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = editing
            .as_ref()
            .and_then(|name| self.library.servers.get(name).cloned());
        if editing.is_some() && config.is_none() {
            return;
        }
        let config = config.unwrap_or_default();
        let remote = config.is_remote();
        let fields = [
            (&self.server_form.name, editing.clone().unwrap_or_default()),
            (
                &self.server_form.command,
                if remote {
                    String::new()
                } else {
                    config.summary()
                },
            ),
            (
                &self.server_form.url,
                config.url.clone().unwrap_or_default(),
            ),
        ];
        for (input, value) in fields {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        let areas = [
            (
                &self.server_form.plain,
                lines(if remote { &config.headers } else { &config.env }, remote),
            ),
            (
                &self.server_form.secret,
                lines(
                    &config
                        .secrets
                        .iter()
                        .map(|k| (k.clone(), String::new()))
                        .collect(),
                    remote,
                ),
            ),
        ];
        for (area, value) in areas {
            area.update(cx, |area, cx| area.set_value(value, window, cx));
        }
        let form = (
            self.server_form.name.clone(),
            self.server_form.command.clone(),
            self.server_form.url.clone(),
            self.server_form.plain.clone(),
            self.server_form.secret.clone(),
        );
        let title = match &editing {
            Some(name) => format!("Edit “{name}”"),
            None => "Add an MCP server".to_owned(),
        };
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (name, command, url, plain, secret) = form.clone();
            let label = |text: &'static str| div().text_sm().font_medium().child(text);
            let note = |text: &'static str| div().text_xs().text_color(muted).child(text);
            let shelf = shelf.clone();
            let editing = editing.clone();
            alert
                .title(title.clone())
                .width(px(600.))
                .description(
                    v_flex()
                        .gap_2()
                        .child(label("Name"))
                        .child(Input::new(&name).disabled(editing.is_some()))
                        .child(label("Command, for a local server"))
                        .child(Input::new(&command))
                        .child(label("Or URL, for a remote server"))
                        .child(Input::new(&url))
                        .child(label("Environment variables, or headers for a URL"))
                        .child(Textarea::new(&plain))
                        .child(label("Secrets"))
                        .child(Textarea::new(&secret))
                        .child(note(
                            "Secrets go to the OS keyring, never into servers.toml. When editing, \
                             leave a value empty to keep the saved one.",
                        )),
                )
                .show_cancel(true)
                .ok_text(if editing.is_some() { "Save" } else { "Add" })
                .on_ok(move |_, window, cx| {
                    let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
                    let area = |area: &Entity<TextareaState>| area.read(cx).value().to_string();
                    let typed = read_form(
                        &read(&name),
                        &read(&command),
                        &read(&url),
                        &area(&plain),
                        &area(&secret),
                    );
                    shelf.update(cx, |shelf, cx| match typed {
                        Ok((name, config, secrets)) => {
                            shelf.save_server(editing.is_none(), name, config, secrets, window, cx)
                        }
                        Err(message) => {
                            window.push_notification(Notification::error(message), cx);
                            false
                        }
                    })
                })
        });
    }

    /// Saves a server's secrets, then the server, through the daemon.
    fn save_server(
        &mut self,
        adding: bool,
        name: String,
        config: ServerConfig,
        secrets: BTreeMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if adding && self.library.all_servers().iter().any(|s| s.name == name) {
            window.push_notification(
                Notification::error(format!("There is a server named {name} already.")),
                cx,
            );
            return false;
        }
        let mut steps = vec![Step::SaveServer {
            name: name.clone(),
            config: Some(config),
        }];
        if adding {
            // A new server is on for the agents the profile serves.
            let mut settings = self.settings().clone();
            for agent in self.agents().to_vec() {
                settings.active_mut().set(Kind::Server, &name, agent, true);
            }
            steps.push(Step::SaveSettings {
                settings: Box::new(settings),
            });
        }
        self.busy = true;
        cx.notify();
        let client = settings::client(cx);
        let done = format!("Saved {name}");
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    for (key, value) in secrets.iter().filter(|(_, v)| !v.is_empty()) {
                        settings::call(
                            &client,
                            Request::SetSecret {
                                key: secret_key(&name, key),
                                value: Some(value.clone()),
                            },
                        )?;
                    }
                    settings::execute(&client, steps)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => window.push_notification(Notification::success(done), cx),
                    Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
        true
    }

    pub(super) fn confirm_remove_server(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.library.servers.get(name).cloned() else {
            return;
        };
        let name = name.to_owned();
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (shelf, name, config) = (shelf.clone(), name.clone(), config.clone());
            alert
                .title(format!("Remove “{name}”?"))
                .description("xolkit disconnects from it, and its saved secrets are deleted.")
                .show_cancel(true)
                .ok_text("Remove")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    shelf.update(cx, |shelf, cx| {
                        shelf.remove_server(&name, &config, window, cx)
                    });
                    true
                })
        });
    }

    fn remove_server(
        &mut self,
        name: &str,
        config: &ServerConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut settings = self.settings().clone();
        settings.forget(Kind::Server, name);
        let steps = vec![
            Step::SaveServer {
                name: name.to_owned(),
                config: None,
            },
            Step::SaveSettings {
                settings: Box::new(settings),
            },
        ];
        let secrets: Vec<String> = config.secrets.iter().map(|k| secret_key(name, k)).collect();
        let client = settings::client(cx);
        self.busy = true;
        let done = format!("Removed {name}");
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    settings::execute(&client, steps)?;
                    for key in secrets {
                        settings::call(&client, Request::SetSecret { key, value: None })?;
                    }
                    anyhow::Ok(())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => window.push_notification(Notification::success(done), cx),
                    Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
                }
                this.select(None, cx);
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Lists the servers set up in the agents' own configs, to copy some into xolkit. Their
    /// values become secrets, since agent configs keep tokens in plain text.
    fn open_copy_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut found: Vec<(Provider, String, ServerConfig)> = Vec::new();
        let mut errors = Vec::new();
        for provider in Provider::ALL {
            match agent_servers(&self.env, provider) {
                Ok(servers) => found.extend(servers.into_iter().map(|(n, c)| (provider, n, c))),
                Err(e) => errors.push(format!("{}: {e:#}", provider.name())),
            }
        }
        let taken: Vec<String> = self
            .library
            .all_servers()
            .into_iter()
            .map(|s| s.name)
            .collect();
        found.retain(|(_, name, _)| valid_server_name(name) && !taken.contains(name));
        found.dedup_by(|a, b| a.1 == b.1);
        let picked = cx.new(|_| found.iter().map(|_| true).collect::<Vec<bool>>());
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (shelf, found, picked) = (shelf.clone(), found.clone(), picked.clone());
            let danger = cx.theme().danger;
            let list = v_flex()
                .gap_2()
                .children(
                    errors
                        .iter()
                        .map(|e| div().text_sm().text_color(danger).child(e.clone())),
                )
                .when(found.is_empty(), |this| {
                    this.child("No servers to copy: the agents have none that xolkit lacks.")
                })
                .children(
                    found
                        .iter()
                        .enumerate()
                        .map(|(ix, (provider, name, config))| {
                            let picked = picked.clone();
                            let checked = picked.read(cx)[ix];
                            Checkbox::new(SharedString::from(format!("copy-{ix}")))
                                .label(format!(
                                    "{name} ({}, from {})",
                                    config.kind(),
                                    provider.label()
                                ))
                                .checked(checked)
                                .on_change(move |on: &bool, _, cx| {
                                    picked.update(cx, |picked, cx| {
                                        picked[ix] = *on;
                                        cx.notify();
                                    });
                                })
                        }),
                );
            alert
                .title("Copy servers from agents")
                .width(px(560.))
                .description(list)
                .show_cancel(true)
                .ok_text("Copy")
                .on_ok(move |_, window, cx| {
                    let picked = picked.read(cx).clone();
                    let chosen: Vec<(String, ServerConfig)> = found
                        .iter()
                        .zip(picked)
                        .filter(|(_, on)| *on)
                        .map(|((_, name, config), _)| (name.clone(), config.clone()))
                        .collect();
                    shelf.update(cx, |shelf, cx| shelf.copy_servers(chosen, window, cx));
                    true
                })
        });
    }

    fn copy_servers(
        &mut self,
        chosen: Vec<(String, ServerConfig)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if chosen.is_empty() {
            return;
        }
        let mut settings = self.settings().clone();
        let mut steps = Vec::new();
        let mut secrets = Vec::new();
        for (name, mut config) in chosen {
            let values = if config.is_remote() {
                std::mem::take(&mut config.headers)
            } else {
                std::mem::take(&mut config.env)
            };
            config.secrets = values.keys().cloned().collect();
            for (key, value) in values {
                secrets.push((secret_key(&name, &key), value));
            }
            for agent in self.agents().to_vec() {
                settings.active_mut().set(Kind::Server, &name, agent, true);
            }
            steps.push(Step::SaveServer {
                name,
                config: Some(config),
            });
        }
        steps.push(Step::SaveSettings {
            settings: Box::new(settings),
        });
        let client = settings::client(cx);
        self.busy = true;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    for (key, value) in secrets {
                        settings::call(
                            &client,
                            Request::SetSecret {
                                key,
                                value: Some(value),
                            },
                        )?;
                    }
                    settings::execute(&client, steps)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => window.push_notification(Notification::success("Copied"), cx),
                    Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{lines, read_form};

    #[test]
    fn reads_the_form() {
        let (name, config, secrets) = read_form(
            " gh ",
            "npx -y server-github",
            "",
            "DEBUG=1",
            "TOKEN=abc\n\n",
        )
        .unwrap();
        assert_eq!(name, "gh");
        assert_eq!(config.command.as_deref(), Some("npx"));
        assert_eq!(config.args, ["-y", "server-github"]);
        assert_eq!(config.env["DEBUG"], "1");
        assert_eq!(config.secrets, ["TOKEN"]);
        assert_eq!(secrets["TOKEN"], "abc");

        let (_, remote, _) = read_form(
            "docs",
            "",
            "https://x/mcp",
            "X-Team: core",
            "Authorization: Bearer t",
        )
        .unwrap();
        assert_eq!(remote.headers["X-Team"], "core");
        assert_eq!(remote.secrets, ["Authorization"]);
        assert!(read_form("bad name", "x", "", "", "").is_err());
        assert!(read_form("x", "", "", "", "").is_err());
        assert!(read_form("x", "cmd", "", "NOEQUALS", "").is_err());
        assert_eq!(lines(&remote.headers, true), "X-Team: core");
    }
}
