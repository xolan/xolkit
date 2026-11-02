//! The install, update and uninstall dialogs: which agents get the item, and every step that
//! will run.

use std::collections::BTreeSet;

use anyhow::Result;
use gpui_kit::component::{
    ActiveTheme, IconName, WindowExt, button::ButtonVariant, checkbox::Checkbox, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{
    Action as Step, Agent, Env, FetchedPlugin, FetchedSkill, Kind, Lock, Plugin, Provider,
    Settings, Skill, install_plugin, install_skill, native_install, native_toggle,
    uninstall_plugin, uninstall_skill, update_skill,
};

use super::discover::is_code;
use super::{Shelf, icon_line};

pub(super) enum Change {
    InstallSkill(FetchedSkill),
    /// Replaces installed skill `name` with a downloaded newer version.
    UpdateSkill {
        name: String,
        download: FetchedSkill,
    },
    UninstallSkill(Skill),
    InstallPlugin(FetchedPlugin),
    UninstallPlugin(Plugin),
    /// Installs an installed plugin natively in one more agent.
    NativeInstall {
        plugin: Plugin,
        provider: Provider,
    },
}

impl Change {
    fn name(&self) -> &str {
        match self {
            Change::InstallSkill(skill) => &skill.name,
            Change::UpdateSkill { name, .. } => name,
            Change::UninstallSkill(skill) => &skill.name,
            Change::InstallPlugin(fetched) => &fetched.plugin.id,
            Change::UninstallPlugin(plugin) | Change::NativeInstall { plugin, .. } => &plugin.id,
        }
    }

    fn installs(&self) -> bool {
        !matches!(self, Change::UninstallSkill(_) | Change::UninstallPlugin(_))
    }

    /// Whether the dialog asks which agents get the item.
    fn picks_agents(&self) -> bool {
        matches!(self, Change::InstallSkill(_) | Change::InstallPlugin(_))
    }

    /// Files nobody has reviewed yet, to list with scripts flagged.
    fn new_files(&self) -> &[String] {
        match self {
            Change::InstallSkill(skill)
            | Change::UpdateSkill {
                download: skill, ..
            } => &skill.files,
            _ => &[],
        }
    }
}

pub(super) struct ChangeForm {
    env: Env,
    change: Change,
    settings: Settings,
    lock: Lock,
    /// The agents the active profile serves; the install turns the item on for `chosen`.
    agents: Vec<Agent>,
    chosen: BTreeSet<Agent>,
}

impl ChangeForm {
    fn steps(&self) -> Result<Vec<Step>> {
        let chosen: Vec<Agent> = self.chosen.iter().copied().collect();
        Ok(match &self.change {
            Change::InstallSkill(skill) => {
                let mut steps = install_skill(&self.env, skill)?;
                let mut settings = self.settings.clone();
                for &agent in &chosen {
                    settings
                        .active_mut()
                        .set(Kind::Skill, &skill.name, agent, true);
                }
                steps.push(Step::SaveSettings {
                    settings: Box::new(settings),
                });
                steps
            }
            Change::UpdateSkill { name, download } => update_skill(&self.env, name, download)?,
            Change::UninstallSkill(skill) => {
                uninstall_skill(&self.env, &self.settings, &skill.name)
            }
            Change::InstallPlugin(fetched) => {
                install_plugin(&self.env, &self.settings, &self.lock, fetched, &chosen)?
            }
            Change::UninstallPlugin(plugin) => {
                uninstall_plugin(&self.env, &self.settings, &self.lock, plugin)
            }
            Change::NativeInstall { plugin, provider } => {
                let mut steps = native_install(&self.env, &self.lock, plugin, *provider)?;
                // Installed natively, it is on there; match what the profile has.
                let agent = Agent::from(*provider);
                if !self
                    .settings
                    .active()
                    .is_on(Kind::Plugin, &plugin.id, agent)
                {
                    steps.push(native_toggle(plugin, *provider, false));
                }
                steps
            }
        })
    }
}

impl Render for ChangeForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let steps = self.steps();
        let theme = cx.theme();
        let new_files = self.change.new_files();
        let summary = match &steps {
            Ok(steps) if steps.is_empty() => v_flex().child("Nothing to do."),
            Ok(steps) => v_flex().children(steps.iter().map(|step| step.describe(&self.env))),
            Err(e) => v_flex().text_color(theme.danger).child(format!("{e:#}")),
        };
        let native_note = match &self.change {
            Change::InstallPlugin(fetched) if fetched.plugin.needs_native() => Some(format!(
                "This plugin has {}, so it is also installed in Claude Code and Codex through \
                 their own tools, for the agents you pick.",
                fetched.plugin.parts.native.join(" and ")
            )),
            _ => None,
        };
        v_flex()
            .gap_3()
            .when(self.change.picks_agents(), |this| {
                let note = if self.agents.is_empty() {
                    "The active profile serves no agents yet; you can turn it on later."
                } else {
                    "Turn it on in the active profile for:"
                };
                this.child(div().text_sm().child(note))
                    .child(h_flex().gap_4().children(self.agents.iter().map(|&agent| {
                        Checkbox::new(SharedString::from(format!("change-{}", agent.key())))
                            .label(agent.label())
                            .checked(self.chosen.contains(&agent))
                            .on_change(cx.listener(move |this, on: &bool, _, cx| {
                                if *on {
                                    this.chosen.insert(agent);
                                } else {
                                    this.chosen.remove(&agent);
                                }
                                cx.notify();
                            }))
                    })))
            })
            .when_some(native_note, |this, note| {
                this.child(div().text_sm().text_color(theme.warning).child(note))
            })
            .child(
                summary
                    .gap_1()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .text_sm()
                    .text_color(theme.muted_foreground),
            )
            .when(!new_files.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Files (scripts an agent may run are flagged):")
                        .children(new_files.iter().take(40).map(|file| {
                            h_flex()
                                .gap_2()
                                .line_height(icon_line())
                                .when(is_code(file), |row| {
                                    row.text_color(theme.warning).child(IconName::TriangleAlert)
                                })
                                .child(file.clone())
                        }))
                        .when(new_files.len() > 40, |this| {
                            this.child(format!("…and {} more", new_files.len() - 40))
                        }),
                )
            })
    }
}

/// Asks for confirmation, then sends the change to the daemon.
pub(super) fn open_change_dialog(
    shelf: &mut Shelf,
    change: Change,
    window: &mut Window,
    cx: &mut Context<Shelf>,
) {
    let name = change.name().to_owned();
    let installs = change.installs();
    let moves_download = matches!(
        change,
        Change::InstallSkill(_) | Change::UpdateSkill { .. } | Change::InstallPlugin(_)
    );
    let updates = matches!(change, Change::UpdateSkill { .. });
    let (verb, done) = match &change {
        Change::UpdateSkill { .. } => ("Update", format!("Updated {name}")),
        Change::NativeInstall { provider, .. } => (
            "Install",
            format!("Installed {name} in {}", provider.name()),
        ),
        _ if installs => ("Install", format!("Installed {name}")),
        _ => ("Uninstall", format!("Uninstalled {name}")),
    };
    let title = format!("{verb} “{name}”?");
    let agents = shelf.agents().to_vec();
    let form = cx.new(|_| ChangeForm {
        env: shelf.env.clone(),
        change,
        settings: shelf.library.settings.clone(),
        lock: shelf.library.lock.clone(),
        chosen: agents.iter().copied().collect(),
        agents,
    });
    let shelf = cx.entity();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (form, shelf, done, name) = (form.clone(), shelf.clone(), done.clone(), name.clone());
        alert
            .title(title.clone())
            .description(form.clone())
            .width(px(640.))
            .show_cancel(true)
            .ok_text(verb)
            .ok_variant(if installs {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Danger
            })
            .on_ok(move |_, window, cx| {
                let Ok(steps) = form.read(cx).steps() else {
                    return false;
                };
                if steps.is_empty() {
                    return false;
                }
                shelf.update(cx, |shelf, cx| {
                    if moves_download {
                        // The change moves the download into place; keep it from being discarded.
                        shelf.fetched = None;
                        shelf.fetched_plugin = None;
                    }
                    if updates {
                        // Checked again once the reload after the update finishes.
                        shelf.updates.remove(&name);
                    }
                    shelf.run(steps, Some(done.clone().into()), window, cx);
                });
                true
            })
    });
}
