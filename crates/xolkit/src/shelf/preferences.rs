//! The Settings page: the agents xolkit is registered with, GitHub access, the daemon, the
//! theme, and about.

use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt,
    button::{Button, ButtonVariant, ButtonVariants},
    h_flex,
    input::Input,
    menu::{DropdownMenu, PopupMenuItem},
    notification::Notification,
    radio::{Radio, RadioGroup},
    scroll::ScrollableElement,
    spinner::Spinner,
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{
    GITHUB_TOKEN, GitHubAccess, Provider, Registration, get_secret, github_rate_limit, register,
    unregister,
};
use xolkit_daemon::Request;

use super::{Page, Shelf, count, divider, icon_line, icon_row, section};
use crate::settings;
use crate::theme;

const NEW_TOKEN_URL: &str = "https://github.com/settings/personal-access-tokens/new";
/// The idle timeouts to pick from, in minutes.
const IDLE_CHOICES: [u64; 4] = [1, 5, 15, 60];

impl Shelf {
    /// Asks GitHub for the request limit the current choice gets, which also proves the token works.
    pub(super) fn check_access(&mut self, cx: &mut Context<Self>) {
        let env = self.env.clone();
        let access = self.settings().github;
        self.access_status = None;
        self.access_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let token = settings::github_token(access, &env)?;
                    let (limit, remaining) = github_rate_limit(token.as_deref())?;
                    anyhow::Ok(format!(
                        "GitHub allows {} requests an hour with this choice; {} are left.",
                        thousands(limit),
                        thousands(remaining)
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                this.access_status =
                    Some(result.map(Into::into).map_err(|e| format!("{e:#}").into()));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Looks in the keyring for a saved token.
    pub(super) fn check_token(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(async { get_secret(GITHUB_TOKEN).ok().flatten().is_some() })
                .await;
            this.update(cx, |this, cx| {
                this.has_token = saved;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_access(&mut self, access: GitHubAccess, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.settings().clone();
        settings.github = access;
        self.save_settings(settings, Vec::new(), None, window, cx);
        self.check_access(cx);
    }

    fn save_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.token_input.read(cx).value().trim().to_owned();
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_graphic()) {
            window.push_notification(
                Notification::error("That doesn't look like a GitHub token"),
                cx,
            );
            return;
        }
        self.token_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.has_token = true;
        self.request(
            Request::SetSecret {
                key: GITHUB_TOKEN.to_owned(),
                value: Some(token),
            },
            Some("Token saved in the keyring".into()),
            window,
            cx,
        );
        self.set_access(GitHubAccess::Token, window, cx);
    }

    fn remove_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.has_token = false;
        self.request(
            Request::SetSecret {
                key: GITHUB_TOKEN.to_owned(),
                value: None,
            },
            Some("Token removed".into()),
            window,
            cx,
        );
        self.check_access(cx);
    }

    /// Asks to register xolkit with an agent, or to take everything xolkit wrote out of it.
    fn confirm_registration(
        &mut self,
        provider: Provider,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let exe = self.exe(cx);
        let steps = if on {
            register(&self.env, provider, &exe)
        } else {
            unregister(&self.env, provider, &self.library.lock, self.settings())
        };
        if steps.is_empty() {
            return;
        }
        let lines: Vec<String> = steps.iter().map(|s| s.describe(&self.env)).collect();
        let (title, ok, done) = if on {
            (
                format!("Register xolkit with {}?", provider.name()),
                "Register",
                format!("Registered with {}", provider.name()),
            )
        } else {
            (
                format!("Unregister xolkit from {}?", provider.name()),
                "Unregister",
                format!("Unregistered from {}", provider.name()),
            )
        };
        let note = if on {
            "Sessions started from now on connect to xolkit. Ones already running don't."
        } else {
            "This takes out everything xolkit wrote there: its server entry, the router skill \
             and plugins installed natively."
        };
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let (shelf, steps, done) = (shelf.clone(), steps.clone(), done.clone());
            alert
                .title(title.clone())
                .width(px(640.))
                .description(
                    v_flex().gap_2().child(note).child(
                        v_flex()
                            .gap_1()
                            .p_2()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .children(lines.clone()),
                    ),
                )
                .show_cancel(true)
                .ok_text(ok)
                .ok_variant(if on {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Danger
                })
                .on_ok(move |_, window, cx| {
                    shelf.update(cx, |shelf, cx| {
                        shelf.run(steps.clone(), Some(done.clone().into()), window, cx)
                    });
                    true
                })
        });
    }

    fn set_router(
        &mut self,
        provider: Provider,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut settings = self.settings().clone();
        settings.router.retain(|p| *p != provider);
        if on {
            settings.router.push(provider);
            settings.router.sort();
        }
        self.save_settings(settings, Vec::new(), None, window, cx);
    }

    /// Registration, router skill and where it was found, for each agent xolkit registers with.
    fn registration_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        Provider::ALL
            .into_iter()
            .map(|provider| {
                let found = self.found.get(&provider);
                let state = self
                    .registrations
                    .get(&provider)
                    .cloned()
                    .unwrap_or(Registration::Missing);
                let (text, color) = match &state {
                    Registration::Current => ("Registered".to_owned(), theme.success),
                    Registration::Missing => ("Not registered".to_owned(), theme.muted_foreground),
                    Registration::Other(command) => (
                        format!("Registered to start {command}; register again to use this copy"),
                        theme.warning,
                    ),
                };
                let place = match found {
                    Some(place) => format!("Found at {}", self.env.tilde(place)),
                    None => "Not found on this computer".to_owned(),
                };
                let registered = state != Registration::Missing;
                let router_on = self.settings().router.contains(&provider);
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(div().font_medium().child(provider.name()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(place),
                                    )
                                    .child(div().text_sm().text_color(color).child(text)),
                            )
                            .when(state != Registration::Current, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!(
                                        "register-{}",
                                        provider.cli()
                                    )))
                                    .small()
                                    .primary()
                                    .label("Register…")
                                    .disabled(self.busy || found.is_none())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_registration(provider, true, window, cx)
                                    })),
                                )
                            })
                            .when(registered, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!(
                                        "unregister-{}",
                                        provider.cli()
                                    )))
                                    .small()
                                    .ghost()
                                    .label("Unregister…")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_registration(provider, false, window, cx)
                                    })),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .pl_4()
                            .child(
                                Switch::new(SharedString::from(format!(
                                    "router-{}",
                                    provider.cli()
                                )))
                                .small()
                                .checked(router_on)
                                .disabled(self.busy)
                                .on_change(cx.listener(
                                    move |this, on: &bool, window, cx| {
                                        this.set_router(provider, *on, window, cx)
                                    },
                                )),
                            )
                            .child(v_flex().child(div().text_sm().child("Router skill")).child(
                                div().text_xs().text_color(theme.muted_foreground).child(
                                    "One xolkit skill in the agent's skills folder whose \
                                         description lists what your skills are for. Turn it on \
                                         if the agent misses skills it should use.",
                                ),
                            )),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn set_idle(&mut self, minutes: u64, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.settings().clone();
        settings.idle_minutes = minutes;
        self.save_settings(settings, Vec::new(), None, window, cx);
    }

    pub(super) fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let active_theme = cx.theme().theme_name().clone();
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let access = self.settings().github;
        let selected = GitHubAccess::ALL.iter().position(|a| *a == access);
        let status = match &self.access_status {
            None => icon_row(Spinner::new().small(), "Checking GitHub access…").text_color(muted),
            Some(Ok(text)) => {
                icon_row(IconName::CircleCheck, text.clone()).text_color(theme.success)
            }
            Some(Err(error)) => {
                icon_row(IconName::CircleAlert, error.clone()).text_color(theme.danger)
            }
        };
        let token_note = if self.has_token {
            "A token is saved in the OS keyring."
        } else {
            "Use a fine-grained token with read-only access to public repositories. It is saved \
             in the OS keyring."
        };
        let token_form =
            v_flex()
                .gap_2()
                .pl_6()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .w(px(340.))
                                .child(Input::new(&self.token_input).small().mask_toggle()),
                        )
                        .child(
                            Button::new("save-token")
                                .small()
                                .primary()
                                .label("Save token")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_token(window, cx)),
                                ),
                        )
                        .when(self.has_token, |row| {
                            row.child(
                                Button::new("remove-token")
                                    .small()
                                    .ghost()
                                    .label("Remove")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.remove_token(window, cx)
                                    })),
                            )
                        }),
                )
                .child(div().text_xs().text_color(muted).child(token_note))
                .child(
                    h_flex().child(
                        Button::new("new-token")
                            .small()
                            .link()
                            .label("Create a token on GitHub")
                            .on_click(|_, _, cx| cx.open_url(NEW_TOKEN_URL)),
                    ),
                );
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().text_lg().font_semibold().child("Settings"))
                    .child(
                        Button::new("reload")
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("Reload")
                            .tooltip("Read the library again (Ctrl+R)")
                            .on_click(cx.listener(|this, _, _, cx| this.reload_all(cx))),
                    ),
            )
            .child(
                div()
                    .id("settings")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .max_w(px(680.))
                            .child(section("Agents"))
                            .child(div().text_sm().text_color(muted).child(
                                "Registering adds one MCP server entry to the agent's own config, \
                                 through its own tool. Skills, plugins and servers then reach the \
                                 agent through xolkit; nothing else in its folders changes, \
                                 unless you turn on the router skill or install a plugin natively.",
                            ))
                            .child(v_flex().gap_4().children(self.registration_rows(cx)))
                            .child(div().text_sm().font_medium().child("Other MCP clients"))
                            .child(div().text_sm().text_color(muted).child(
                                "Any other agent that speaks MCP can use xolkit: add a stdio \
                                 server that runs this command. It gets the Other switches.",
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded(theme.radius)
                                            .bg(theme.muted)
                                            .text_sm()
                                            .font_family(theme.mono_font_family.clone())
                                            .child(self.other_command(cx)),
                                    )
                                    .child(
                                        Button::new("copy-command")
                                            .small()
                                            .ghost()
                                            .icon(IconName::Copy)
                                            .tooltip("Copy")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                let command = this.other_command(cx);
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    command,
                                                ));
                                                window.push_notification(
                                                    Notification::success("Copied"),
                                                    cx,
                                                );
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .child("Served by the active profile"),
                            )
                            .child(div().text_sm().text_color(muted).child(format!(
                                "The “{}” profile gives skills, plugins and servers to the agents \
                                 turned on here.",
                                self.settings().active().name
                            )))
                            .child(
                                v_flex()
                                    .gap_3()
                                    .children(self.agent_rows(self.settings().active_index(), cx)),
                            )
                            .child(divider(cx))
                            .child(section("GitHub access"))
                            .child(div().text_sm().text_color(muted).child(
                                "Skills download from GitHub. Without a token GitHub allows 60 \
                                 requests an hour, and each download uses two or three.",
                            ))
                            .child(
                                RadioGroup::vertical("github-access")
                                    // The label's own line, not the radio's 17 px one, so it
                                    // lines up with the circle.
                                    .children(GitHubAccess::ALL.map(|access| {
                                        let label = access.label();
                                        Radio::new(label)
                                            .accessibility_label(label)
                                            .child(div().line_height(icon_line()).child(label))
                                    }))
                                    .selected_index(selected)
                                    .on_change(cx.listener(|this, ix: &usize, window, cx| {
                                        this.set_access(GitHubAccess::ALL[*ix], window, cx)
                                    })),
                            )
                            .when(access == GitHubAccess::Token, |this| this.child(token_form))
                            .child(div().text_sm().child(status))
                            .child(divider(cx))
                            .child(section("Daemon"))
                            .child(div().text_sm().text_color(muted).child(
                                "The daemon serves agent sessions and connects to MCP servers. \
                                 The first session or the app starts it; it exits once no \
                                 session and no app has used it for this long.",
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("idle")
                                            .small()
                                            .outline()
                                            .label(format!(
                                                "Exit after {} idle",
                                                count(
                                                    self.settings().idle_minutes as usize,
                                                    "minute"
                                                )
                                            ))
                                            .dropdown_menu({
                                                let shelf = cx.weak_entity();
                                                let current = self.settings().idle_minutes;
                                                move |menu, _, _| {
                                                    let mut menu = menu;
                                                    for minutes in IDLE_CHOICES {
                                                        let shelf = shelf.clone();
                                                        menu = menu.item(
                                                            PopupMenuItem::new(count(
                                                                minutes as usize,
                                                                "minute",
                                                            ))
                                                            .checked(minutes == current)
                                                            .on_click(move |_, window, cx| {
                                                                shelf
                                                                    .update(cx, |shelf, cx| {
                                                                        shelf.set_idle(
                                                                            minutes, window, cx,
                                                                        )
                                                                    })
                                                                    .ok();
                                                            }),
                                                        );
                                                    }
                                                    menu
                                                }
                                            }),
                                    )
                                    .child(
                                        Button::new("open-connections")
                                            .small()
                                            .label("Connections")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_page(Page::Connections, window, cx)
                                            })),
                                    ),
                            )
                            .child(divider(cx))
                            .child(section("Theme"))
                            .child(div().text_sm().text_color(muted).child(
                                "Base16 schemes dropped into ~/.xolkit/themes show up here too.",
                            ))
                            .child(
                                h_flex().child(
                                    Button::new("theme")
                                        .small()
                                        .outline()
                                        .icon(IconName::Palette)
                                        .label(active_theme.clone())
                                        .dropdown_menu(move |menu, _, cx| {
                                            let mut menu = menu.scrollable(true).max_h(px(420.));
                                            for (name, _) in theme::names(cx) {
                                                let checked = name == active_theme;
                                                let pick = name.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(name)
                                                        .checked(checked)
                                                        .on_click(move |_, _, cx| {
                                                            theme::select(&pick, cx)
                                                        }),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                            )
                            .child(divider(cx))
                            .child(section("About"))
                            .child(div().text_sm().text_color(muted).child(format!(
                                "xolkit {}, released under the MIT License.",
                                env!("CARGO_PKG_VERSION")
                            )))
                            .child(self.render_app_update(cx))
                            .child(
                                h_flex().child(
                                    Button::new("licenses")
                                        .small()
                                        .icon(IconName::FileText)
                                        .label("Third-party licenses")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_page(Page::Licenses, window, cx)
                                        })),
                                ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Shelf {
    /// The command another MCP client runs to use xolkit.
    fn other_command(&self, cx: &App) -> String {
        let exe = self.exe(cx);
        let exe = exe.to_string_lossy();
        if exe.contains(' ') {
            format!("\"{exe}\" mcp")
        } else {
            format!("{exe} mcp")
        }
    }
}

/// `5000` as `5,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::thousands;

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(60), "60");
        assert_eq!(thousands(5000), "5,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
