//! The library's skills and plugins: lists with a switch per agent, details, updates and
//! uninstall.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt, button::Button,
    button::ButtonVariants, h_flex, notification::Notification, scroll::ScrollableElement,
    spinner::Spinner, text::TextView, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{
    Agent, Kind, Plugin, PluginUpdate, Provider, Skill, SkillUpdate, check_plugin_update,
    check_skill_update, fetch_plugin_update, fetch_update,
};

#[derive(Clone)]
pub(super) enum ItemUpdate {
    Skill(SkillUpdate),
    Plugin(PluginUpdate),
}

use super::change::{Change, open_change_dialog};
use super::{
    Page, SEARCH_DELAY, Selection, Shelf, cell, count, divider, fact, list_body, matches, section,
    warning,
};
use crate::settings;

impl Shelf {
    pub(super) fn plugin(&self, id: &str) -> Option<&Plugin> {
        self.library.plugin(id)
    }

    pub(super) fn visible_skills(&self, cx: &App) -> impl Iterator<Item = &Skill> {
        let filter = self.filter_text(cx);
        self.library
            .skills
            .iter()
            .filter(move |s| matches(&filter, &[&s.name, &s.description]))
    }

    pub(super) fn visible_plugins(&self, cx: &App) -> impl Iterator<Item = &Plugin> {
        let filter = self.filter_text(cx);
        self.library
            .plugins
            .iter()
            .filter(move |p| matches(&filter, &[&p.id, &p.description]))
    }

    fn has_update(&self, name: &str) -> bool {
        matches!(self.updates.get(name), Some(Ok(Some(_))))
    }

    /// Checks the selected managed item once, after the selection settles.
    pub(super) fn check_update(&mut self, cx: &mut Context<Self>) {
        let (name, plugin) = match &self.selected {
            Some(Selection::Skill(name)) if self.library.lock.skills.contains_key(name) => {
                (name.clone(), false)
            }
            Some(Selection::Plugin(id))
                if self
                    .library
                    .lock
                    .plugins
                    .get(id)
                    .is_some_and(|pin| !pin.marketplace.is_empty()) =>
            {
                (id.clone(), true)
            }
            _ => return,
        };
        if self.updates.contains_key(&name) {
            return;
        }
        let env = self.env.clone();
        let access = self.settings().github;
        // Waits like the search does, so arrowing through the list sends no requests.
        self.update_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let result = {
                let name = name.clone();
                cx.background_spawn(async move {
                    let token = settings::github_token(access, &env).ok().flatten();
                    if plugin {
                        check_plugin_update(&env, &name, token.as_deref())
                            .map(|u| u.map(ItemUpdate::Plugin))
                    } else {
                        check_skill_update(&env, &name, token.as_deref())
                            .map(|u| u.map(ItemUpdate::Skill))
                    }
                })
                .await
            };
            this.update(cx, |this, cx| {
                let result = result.map_err(|e| format!("{e:#}").into());
                this.updates.insert(name, result);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Downloads the selected item's update, then asks to install it.
    pub(super) fn download_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.clone() else {
            return;
        };
        let name = match &selection {
            Selection::Skill(name) | Selection::Plugin(name) => name.clone(),
            _ => return,
        };
        let Some(Ok(Some(update))) = self.updates.get(&name) else {
            return;
        };
        if self.busy || self.fetching {
            return;
        }
        let update = update.clone();
        let env = self.env.clone();
        let access = self.settings().github;
        self.fetching = true;
        self.fetch_error = None;
        self.fetch_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let token = settings::github_token(access, &env).ok().flatten();
                    match update {
                        ItemUpdate::Skill(update) => fetch_update(&env, &update, token.as_deref())
                            .map(|download| Change::UpdateSkill {
                                name: update.name,
                                download,
                            }),
                        ItemUpdate::Plugin(update) => {
                            fetch_plugin_update(&env, &update, token.as_deref())
                                .map(Change::UpdatePlugin)
                        }
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.selected.as_ref() != Some(&selection) {
                    if let Ok(change) = result {
                        match change {
                            Change::UpdateSkill { download, .. } => download.discard(),
                            Change::UpdatePlugin(download) => download.discard(),
                            _ => {}
                        }
                    }
                    return;
                }
                this.fetching = false;
                match result {
                    Ok(change) => {
                        match &change {
                            Change::UpdateSkill { download, .. } => {
                                this.set_fetched(Some(download.clone()))
                            }
                            Change::UpdatePlugin(download) => {
                                this.set_fetched_plugin(Some(download.clone()))
                            }
                            _ => {}
                        }
                        open_change_dialog(this, change, window, cx);
                    }
                    Err(e) => this.fetch_error = Some(format!("{e:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Whether a newer version is on GitHub, with the button that installs it.
    fn update_status(&self, name: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.library.lock.skills.contains_key(name)
            && !self
                .library
                .lock
                .plugins
                .get(name)
                .is_some_and(|pin| !pin.marketplace.is_empty())
        {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let line = |text: SharedString| h_flex().gap_2().text_sm().text_color(muted).child(text);
        let element = if self.fetching {
            line("Downloading the update…".into()).child(Spinner::new().small())
        } else if let Some(error) = &self.fetch_error {
            line(error.clone()).text_color(cx.theme().danger)
        } else {
            match self.updates.get(name) {
                None if self.update_task.is_some() => {
                    line("Checking for updates…".into()).child(Spinner::new().small())
                }
                None => return None,
                Some(Ok(None)) => line("Up to date with GitHub".into()),
                Some(Err(error)) => line(format!("Could not check for updates: {error}").into()),
                Some(Ok(Some(_))) => line("A newer version is on GitHub".into())
                    .text_color(cx.theme().foreground)
                    .child(div().flex_1())
                    .child(
                        Button::new("update")
                            .primary()
                            .small()
                            .icon(Lucide::Download)
                            .label("Update…")
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.download_update(window, cx)),
                            ),
                    ),
            }
        };
        Some(element.into_any_element())
    }

    pub(super) fn confirm_uninstall(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let change = match &self.selected {
            Some(Selection::Skill(name)) => self
                .library
                .skill(name)
                .cloned()
                .map(Change::UninstallSkill),
            Some(Selection::Plugin(id)) => self.plugin(id).cloned().map(Change::UninstallPlugin),
            Some(Selection::Server(name)) => {
                let name = name.clone();
                self.confirm_remove_server(&name, window, cx);
                None
            }
            _ => None,
        };
        if let Some(change) = change {
            open_change_dialog(self, change, window, cx);
        }
    }

    pub(super) fn render_skills(&self, cx: &mut Context<Self>) -> AnyElement {
        let skills: Vec<_> = self.visible_skills(cx).collect();
        let rows: Vec<_> = skills.into_iter().map(|s| self.skill_row(s, cx)).collect();
        let empty = self.library.skills.is_empty();
        v_flex()
            .size_full()
            .child(
                self.page_header("Skills", Some(rows.len()), cx).child(
                    Button::new("add-folder")
                        .small()
                        .icon(IconName::FolderOpen)
                        .label("Add folder…")
                        .tooltip("Add a skill folder from your disk")
                        .disabled(self.busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_skill_folder(window, cx)),
                        ),
                ),
            )
            .child(self.column_header(&self.installed_columns(), cx))
            .child(if empty {
                self.empty_library(
                    "No skills yet",
                    "Find skills under Discover, or add a skill folder from your disk.",
                    Page::FindSkills,
                    cx,
                )
            } else {
                list_body("skill-list", rows, &self.list_scroll, cx)
            })
            .into_any_element()
    }

    fn empty_library(
        &self,
        title: &str,
        body: &str,
        discover: Page,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        super::empty_state(IconName::Inbox, title, body, cx)
            .child(
                Button::new("discover")
                    .small()
                    .primary()
                    .icon(IconName::Search)
                    .label("Discover")
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.open_page(discover, window, cx)
                        }),
                    ),
            )
            .into_any_element()
    }

    fn skill_row(&self, skill: &Skill, cx: &mut Context<Self>) -> AnyElement {
        let name = skill.name.clone();
        let mut controls = self.item_switches(Kind::Skill, &name, false, cx);
        let selection = Selection::Skill(name.clone());
        controls.push(self.row_uninstall(format!("uninstall-skill-{name}"), &selection, cx));
        let selected = self.selected.as_ref() == Some(&selection);
        let tag = if self.has_update(&name) {
            Some("update available")
        } else if !self.library.lock.skills.contains_key(&name) {
            Some("local")
        } else {
            None
        };
        self.row(
            format!("skill-{name}"),
            selected,
            name,
            tag,
            skill.description.clone(),
            controls,
            cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
            cx,
        )
    }

    /// Asks for a skill folder, then copies it into the library.
    fn add_skill_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add skill".into()),
        });
        let env = self.env.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                match xolkit_core::add_local_skill(&env, &dir) {
                    Ok((name, mut steps)) => {
                        let mut settings = this.settings().clone();
                        for agent in this.agents().to_vec() {
                            settings.active_mut().set(Kind::Skill, &name, agent, true);
                        }
                        steps.push(xolkit_core::Action::SaveSettings {
                            settings: Box::new(settings),
                        });
                        this.run(steps, Some(format!("Added {name}").into()), window, cx);
                    }
                    Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_plugins(&self, cx: &mut Context<Self>) -> AnyElement {
        let plugins: Vec<_> = self.visible_plugins(cx).collect();
        let rows: Vec<_> = plugins
            .into_iter()
            .map(|p| self.plugin_row(p, cx))
            .collect();
        let empty = self.library.plugins.is_empty();
        v_flex()
            .size_full()
            .child(self.page_header("Plugins", Some(rows.len()), cx))
            .child(self.column_header(&self.installed_columns(), cx))
            .child(if empty {
                self.empty_library(
                    "No plugins yet",
                    "Find plugins in the marketplaces you add under Discover.",
                    Page::FindPlugins,
                    cx,
                )
            } else {
                list_body("plugin-list", rows, &self.list_scroll, cx)
            })
            .into_any_element()
    }

    fn plugin_row(&self, plugin: &Plugin, cx: &mut Context<Self>) -> AnyElement {
        let id = plugin.id.clone();
        let mut controls = self.item_switches(Kind::Plugin, &id, false, cx);
        let selection = Selection::Plugin(id.clone());
        controls.push(self.row_uninstall(format!("uninstall-plugin-{id}"), &selection, cx));
        let selected = self.selected.as_ref() == Some(&selection);
        let tag = if self.has_update(&id) {
            format!("{} · update available", plugin.marketplace)
        } else if plugin.needs_native() {
            format!("{} · native", plugin.marketplace)
        } else {
            plugin.marketplace.clone()
        };
        self.row(
            format!("plugin-{id}"),
            selected,
            plugin.name.clone(),
            Some(tag.as_str()),
            plugin.description.clone(),
            controls,
            cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
            cx,
        )
    }

    /// The agent columns, then an untitled one for each row's uninstall button.
    fn installed_columns(&self) -> Vec<&'static str> {
        let mut columns = self.agent_columns();
        columns.push("");
        columns
    }

    /// A row's own uninstall button: selects the row and asks to confirm.
    pub(super) fn row_uninstall(
        &self,
        id: String,
        selection: &Selection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selection = selection.clone();
        cell()
            .child(
                Button::new(SharedString::from(id))
                    .ghost()
                    .small()
                    .icon(Lucide::Trash)
                    .tooltip("Remove… (Delete)")
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // The row's click would focus the list, taking focus from the dialog.
                        cx.stop_propagation();
                        this.select(Some(selection.clone()), cx);
                        this.confirm_uninstall(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn uninstall_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("uninstall")
            .danger()
            .icon(Lucide::Trash)
            .label("Uninstall…")
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, window, cx| this.confirm_uninstall(window, cx)))
    }

    /// Which agents get the item from the active profile, as a line per agent.
    fn served_to(&self, kind: Kind, name: &str, cx: &mut Context<Self>) -> Vec<Div> {
        let profile = self.settings().active();
        Agent::ALL
            .into_iter()
            .map(|agent| {
                let state = if !profile.serves(agent) {
                    "not served by this profile"
                } else if profile.is_on(kind, name, agent) {
                    "on"
                } else {
                    "off"
                };
                fact(agent.label(), state.to_owned(), cx)
            })
            .collect()
    }

    pub(super) fn skill_detail(&self, skill: &Skill, cx: &mut Context<Self>) -> AnyElement {
        let pin = self.library.lock.skills.get(&skill.name);
        let subtitle = Some(match pin {
            Some(pin) if pin.folder.is_empty() => pin.source.clone(),
            Some(pin) => format!("{}/{}", pin.source, pin.folder),
            None => "Added from your disk".to_owned(),
        });
        let mut facts = vec![fact("Folder", self.env.tilde(&skill.dir), cx)];
        if let Some(pin) = pin {
            let commit: String = pin.commit.chars().take(12).collect();
            facts.push(fact("Commit", commit, cx));
        }
        v_flex()
            .size_full()
            .child(self.detail_header(skill.title.clone(), subtitle, self.uninstall_button(cx), cx))
            .child(
                div()
                    .id("skill-detail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .child(skill.description.clone())
                            .children(self.update_status(&skill.name, cx))
                            .child(v_flex().gap_1().children(facts))
                            .child(section("In the active profile"))
                            .child(v_flex().gap_1().children(self.served_to(
                                Kind::Skill,
                                &skill.name,
                                cx,
                            )))
                            .child(divider(cx))
                            .child(
                                TextView::markdown("skill-md", self.skill_md.clone())
                                    .selectable(true),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn plugin_detail(&self, plugin: &Plugin, cx: &mut Context<Self>) -> AnyElement {
        let parts = &plugin.parts;
        let muted = cx.theme().muted_foreground;
        let native = self.library.lock.native(&plugin.id);
        let mut served = Vec::new();
        if !parts.skills.is_empty() {
            served.push(count(parts.skills.len(), "skill"));
        }
        if !parts.commands.is_empty() {
            served.push(count(parts.commands.len(), "command"));
        }
        if !parts.servers.is_empty() {
            served.push(count(parts.servers.len(), "MCP server"));
        }
        let served = if served.is_empty() {
            "nothing".to_owned()
        } else {
            served.join(", ")
        };
        let native_lines: Vec<Div> = if plugin.needs_native() {
            Provider::ALL
                .into_iter()
                .map(|provider| {
                    let state = if native.contains(&provider) {
                        "installed natively"
                    } else {
                        "not installed"
                    };
                    let install = (!native.contains(&provider)).then(|| {
                        let id = plugin.id.clone();
                        Button::new(SharedString::from(format!("native-{}", provider.cli())))
                            .small()
                            .label("Install natively…")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_native_install(&id, provider, window, cx)
                            }))
                    });
                    h_flex()
                        .gap_2()
                        .child(fact(provider.label(), state.to_owned(), cx).flex_1())
                        .children(install)
                })
                .collect()
        } else {
            Vec::new()
        };
        let pin = self.library.lock.plugins.get(&plugin.id);
        let version = if plugin.version.is_empty() {
            pin.map(|p| p.version.clone()).unwrap_or_default()
        } else {
            plugin.version.clone()
        };
        v_flex()
            .size_full()
            .child(self.detail_header(
                plugin.name.clone(),
                Some(plugin.marketplace.clone()),
                self.uninstall_button(cx),
                cx,
            ))
            .child(
                div()
                    .id("plugin-detail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .child(plugin.description.clone())
                            .child(
                                v_flex()
                                    .gap_1()
                                    .when(!version.is_empty(), |this| {
                                        this.child(fact("Version", version.clone(), cx))
                                    })
                                    .when_some(pin, |this, pin| {
                                        this.child(fact("Source", pin.pin.source.clone(), cx))
                                    })
                                    .child(fact("Served", served, cx)),
                            )
                            .children(self.update_status(&plugin.id, cx))
                            .child(section("In the active profile"))
                            .child(v_flex().gap_1().children(self.served_to(
                                Kind::Plugin,
                                &plugin.id,
                                cx,
                            )))
                            .when(plugin.needs_native(), |this| {
                                this.child(divider(cx))
                                    .child(section("Native installs"))
                                    .child(warning(
                                        format!(
                                            "This plugin has {}, which only work from an agent's \
                                             own folders. xolkit installs it there through the \
                                             agent's own tool, from xolkit's local marketplace.",
                                            parts.native.join(" and ")
                                        ),
                                        cx,
                                    ))
                                    .child(v_flex().gap_2().children(native_lines))
                            })
                            .when(!parts.commands.is_empty(), |this| {
                                this.child(divider(cx)).child(section("Commands")).child(
                                    v_flex()
                                        .gap_1()
                                        .text_sm()
                                        .children(parts.commands.iter().map(|c| {
                                            h_flex()
                                                .gap_2()
                                                .child(div().font_medium().child(c.name.clone()))
                                                .child(
                                                    div()
                                                        .text_color(muted)
                                                        .truncate()
                                                        .child(c.description.clone()),
                                                )
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    /// Asks to install a plugin natively in one more agent.
    fn confirm_native_install(
        &mut self,
        id: &str,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(plugin) = self.plugin(id).cloned() {
            open_change_dialog(self, Change::NativeInstall { plugin, provider }, window, cx);
        }
    }
}
