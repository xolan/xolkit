//! Profiles: the switcher in the title bar, the page that adds, edits and removes them, and
//! sharing them as a code or a `.xolkit` file.

use std::path::PathBuf;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt, WindowExt,
    button::{Button, ButtonVariant, ButtonVariants},
    h_flex,
    input::{Input, Textarea},
    menu::{DropdownMenu, PopupMenuItem},
    notification::Notification,
    scroll::ScrollableElement,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{
    Agent, Downloads, Kind, Profile, Shared, decode_code, encode_code, export_file, fetch_import,
    import_steps, native_changes, native_steps, read_export,
};

use super::{Page, Selection, Shelf, cell, count, divider, list_body, matches, section};
use crate::settings;

/// Most steps the switch dialog lists.
const MAX_STEPS: usize = 14;
/// Longest value the import dialog shows; a longer one is cut and marked, never hidden.
const MAX_VALUE: usize = 2000;

/// `text` quoted, with control characters escaped, and cut at `MAX_VALUE` characters with a note.
fn quoted(text: &str) -> String {
    let length = text.chars().count();
    if length <= MAX_VALUE {
        return format!("{text:?}");
    }
    let short: String = text.chars().take(MAX_VALUE).collect();
    format!("{short:?}… (cut: {length} characters in all)")
}

/// A profile on its way in: from a code, or from a file with its local skills unpacked.
#[derive(Clone)]
struct Incoming {
    shared: Shared,
    /// Where a `.xolkit` file's local skills were unpacked.
    staged: Option<PathBuf>,
}

impl Shelf {
    pub(super) fn visible_profiles(&self, cx: &App) -> impl Iterator<Item = (usize, &Profile)> {
        let filter = self.filter_text(cx);
        self.settings()
            .profiles
            .iter()
            .enumerate()
            .filter(move |(_, p)| matches(&filter, &[&p.name]))
    }

    /// Changes a profile and saves it. For the profile in use, plugins installed natively follow:
    /// turning an agent off turns its native plugins off there too.
    pub(super) fn edit_profile(
        &mut self,
        ix: usize,
        edit: impl FnOnce(&mut Profile),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut settings = self.settings().clone();
        edit(&mut settings.profiles[ix]);
        let native = if ix == settings.active_index() {
            native_steps(
                &self.library.lock,
                &self.library.plugins,
                &settings.profiles[ix],
            )
        } else {
            Vec::new()
        };
        self.save_settings(settings, native, None, window, cx);
    }

    /// Makes another profile the active one. What xolkit serves changes at once; plugins
    /// installed natively are turned on and off in the agents, after a confirmation.
    pub(super) fn switch_profile(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix == self.settings().active_index() || self.busy {
            return;
        }
        let mut settings = self.settings().clone();
        let from = settings.active().clone();
        settings.profile = settings.profiles[ix].name.clone();
        let to = settings.active().clone();
        let (lock, plugins) = (&self.library.lock, &self.library.plugins);
        let changes = native_changes(lock, plugins, &from, &to);
        // Every native plugin is set as the profile has it, so one a failed switch left behind
        // is set right too; only the changes are worth asking about.
        let native = native_steps(lock, plugins, &to);
        let done: SharedString = format!("Switched to {}", to.name).into();
        if changes.is_empty() {
            self.save_settings(settings, native, Some(done), window, cx);
            return;
        }
        // The dialog doesn't scroll, so a long list ends in a count.
        let mut lines: Vec<String> = changes
            .iter()
            .take(MAX_STEPS)
            .map(|step| step.describe(&self.env))
            .collect();
        if changes.len() > MAX_STEPS {
            lines.push(format!("…and {} more", changes.len() - MAX_STEPS));
        }
        let title = format!("Switch to “{}”?", to.name);
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let (shelf, settings, native, done) = (
                shelf.clone(),
                settings.clone(),
                native.clone(),
                done.clone(),
            );
            alert
                .title(title.clone())
                .description(
                    v_flex()
                        .gap_2()
                        .child(
                            "Skills, plugins and servers xolkit serves switch at once. Plugins \
                             installed natively in an agent are switched there too:",
                        )
                        .child(
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
                .width(px(640.))
                .show_cancel(true)
                .ok_text("Switch")
                .on_ok(move |_, window, cx| {
                    shelf.update(cx, |shelf, cx| {
                        shelf.save_settings(
                            settings.clone(),
                            native.clone(),
                            Some(done.clone()),
                            window,
                            cx,
                        );
                    });
                    true
                })
        });
    }

    /// Asks for a name, then adds a profile (`renaming` is `None`) or renames one.
    fn open_name_dialog(
        &mut self,
        renaming: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = renaming.map(|ix| self.settings().profiles[ix].name.clone());
        let (title, ok, note) = match &current {
            Some(name) => (format!("Rename “{name}”"), "Rename", None),
            None => (
                "New profile".to_owned(),
                "Add",
                Some(format!(
                    "It starts as a copy of “{}”.",
                    self.settings().active().name
                )),
            ),
        };
        let input = self.profile_name.clone();
        input.update(cx, |input, cx| {
            input.set_value(current.unwrap_or_default(), window, cx)
        });
        let shelf = cx.entity();
        window.open_alert_dialog(cx, {
            let input = input.clone();
            move |alert, _, cx| {
                let (input, shelf) = (input.clone(), shelf.clone());
                alert
                    .title(title.clone())
                    .description(v_flex().gap_2().child(Input::new(&input)).when_some(
                        note.clone(),
                        |this, note| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(note),
                            )
                        },
                    ))
                    .show_cancel(true)
                    .ok_text(ok)
                    .on_ok(move |_, window, cx| {
                        let name = input.read(cx).value().to_string();
                        shelf.update(cx, |shelf, cx| {
                            shelf.name_profile(renaming, &name, window, cx)
                        })
                    })
            }
        });
        // The dialog takes focus as it opens; hand it to the field after that.
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx))
        });
    }

    /// Adds or renames a profile and shows it; says what is wrong with the name otherwise.
    fn name_profile(
        &mut self,
        renaming: Option<usize>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut settings = self.settings().clone();
        let named = match renaming {
            Some(ix) => settings.rename(ix, name).map(|()| ix),
            None => settings.add(name),
        };
        let ix = match named {
            Ok(ix) => ix,
            Err(e) => {
                window.push_notification(Notification::error(format!("{e:#}")), cx);
                return false;
            }
        };
        let name = settings.profiles[ix].name.clone();
        self.save_settings(settings, Vec::new(), None, window, cx);
        self.open_page(Page::Profiles, window, cx);
        self.select(Some(Selection::Profile(name)), cx);
        true
    }

    fn confirm_remove_profile(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.settings().profiles[ix].name.clone();
        let title = format!("Remove “{name}”?");
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (shelf, name) = (shelf.clone(), name.clone());
            alert
                .title(title.clone())
                .description("Skills, plugins and servers stay installed; only the profile goes.")
                .show_cancel(true)
                .ok_text("Remove")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    shelf.update(cx, |shelf, cx| shelf.remove_profile(&name, window, cx));
                    true
                })
        });
    }

    fn remove_profile(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.settings().position(name) else {
            return;
        };
        let mut settings = self.settings().clone();
        if let Err(e) = settings.remove(ix) {
            window.push_notification(Notification::error(format!("{e:#}")), cx);
            return;
        }
        self.select(None, cx);
        self.save_settings(
            settings,
            Vec::new(),
            Some(format!("Removed {name}").into()),
            window,
            cx,
        );
    }

    /// Copies the profile's code to the clipboard.
    fn copy_code(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let shared = Shared::from_profile(&self.library, &self.settings().profiles[ix]);
        let code = encode_code(&shared);
        cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
        let local = shared.local_skills();
        let message = if local.is_empty() {
            format!("Copied the code ({} characters)", code.len())
        } else {
            format!(
                "Copied the code. It leaves out {}, which exist only on this computer; export a \
                 file to share those.",
                local.join(", ")
            )
        };
        window.push_notification(Notification::success(message), cx);
    }

    /// Copies a `xolkit://` link to the profile.
    fn copy_link(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let shared = Shared::from_profile(&self.library, &self.settings().profiles[ix]);
        let link = format!("xolkit://import/{}", encode_code(&shared));
        cx.write_to_clipboard(ClipboardItem::new_string(link));
        window.push_notification(Notification::success("Copied the link"), cx);
    }

    /// Saves the profile as a `.xolkit` file, with its local-only skills.
    fn export_profile(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.settings().profiles[ix].clone();
        let shared = Shared::from_profile(&self.library, &profile);
        let library = self.library.clone();
        let dir = std::env::home_dir().unwrap_or_default();
        let file_name = format!("{}.xolkit", profile.name.replace(['/', '\\'], "-"));
        let path = cx.prompt_for_new_path(&dir, Some(&file_name));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let result = cx
                .background_spawn({
                    let path = path.clone();
                    async move { export_file(&library, &shared, &path) }
                })
                .await;
            cx.update(|window, cx| match result {
                Ok(()) => window.push_notification(
                    Notification::success(format!("Saved {}", path.display())),
                    cx,
                ),
                Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a code (or uses `code`, from a `xolkit://` link), then previews the import.
    pub(super) fn open_import_dialog(
        &mut self,
        code: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(code) = code {
            match decode_code(&code) {
                Ok(shared) => self.preview_import(
                    Incoming {
                        shared,
                        staged: None,
                    },
                    window,
                    cx,
                ),
                Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
            }
            return;
        }
        let input = self.import_input.clone();
        input.update(cx, |input, cx| input.set_value("", window, cx));
        let shelf = cx.entity();
        window.open_alert_dialog(cx, {
            let input = input.clone();
            move |alert, _, cx| {
                let (input, shelf) = (input.clone(), shelf.clone());
                alert
                    .title("Import a profile")
                    .width(px(560.))
                    .description(
                        v_flex()
                            .gap_2()
                            .child("Paste a profile code or link:")
                            .child(Textarea::new(&input))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("You'll see what it installs before anything is downloaded."),
                            ),
                    )
                    .show_cancel(true)
                    .ok_text("Continue")
                    .on_ok(move |_, window, cx| {
                        let code = input.read(cx).value().to_string();
                        match decode_code(&code) {
                            Ok(shared) => {
                                // After this dialog closes.
                                let incoming = Incoming { shared, staged: None };
                                let shelf = shelf.clone();
                                window.defer(cx, move |window, cx| {
                                    shelf.update(cx, |shelf, cx| {
                                        shelf.preview_import(incoming, window, cx)
                                    })
                                });
                                true
                            }
                            Err(e) => {
                                window.push_notification(
                                    Notification::error(format!("{e:#}")),
                                    cx,
                                );
                                false
                            }
                        }
                    })
            }
        });
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx))
        });
    }

    /// Asks for a `.xolkit` file, then previews the import.
    fn import_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        let env = self.env.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let read = cx
                .background_spawn(async move { read_export(&env, &path) })
                .await;
            this.update_in(cx, |this, window, cx| match read {
                Ok((shared, staged)) => this.preview_import(
                    Incoming {
                        shared,
                        staged: Some(staged),
                    },
                    window,
                    cx,
                ),
                Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Shows what importing a profile installs, from where, then downloads and imports it.
    fn preview_import(&mut self, incoming: Incoming, window: &mut Window, cx: &mut Context<Self>) {
        let shared = &incoming.shared;
        let library = &self.library;
        // Everything that would run or install, in full: servers first, as they run commands,
        // then plugins, which can bring hooks and servers of their own, then skills. The list
        // scrolls. Every value the sharer typed is quoted, so none can pass for another line.
        let mut lines: Vec<String> = Vec::new();
        for (name, server) in &shared.servers {
            if library.servers.contains_key(name) {
                lines.push(format!("Server {name}: kept as you have it"));
                continue;
            }
            let config = &server.config;
            lines.push(format!("Server {name}: add {}", quoted(&config.summary())));
            let plain = if config.is_remote() {
                &config.headers
            } else {
                &config.env
            };
            for (key, value) in plain {
                lines.push(format!("    {} = {}", quoted(key), quoted(value)));
            }
            if let Some(cwd) = &config.cwd {
                lines.push(format!(
                    "    in the folder {}",
                    quoted(&cwd.display().to_string())
                ));
            }
            if !config.secrets.is_empty() {
                let keys: Vec<String> = config.secrets.iter().map(|k| quoted(k)).collect();
                lines.push(format!(
                    "    then save {} under MCP servers",
                    keys.join(", ")
                ));
            }
        }
        for (id, item) in &shared.plugins {
            lines.push(if library.plugin(id).is_some() {
                format!("Plugin {id}: installed already")
            } else {
                format!(
                    "Plugin {id}: download {} at {}",
                    quoted(&item.source),
                    item.commit
                )
            });
        }
        for (name, item) in &shared.skills {
            lines.push(if library.skill(name).is_some() {
                format!("Skill {name}: installed already")
            } else if item.is_local() {
                match incoming.staged {
                    Some(_) => format!("Skill {name}: from the file"),
                    None => format!(
                        "Skill {name}: not in the code; it exists only on the sharer's computer"
                    ),
                }
            } else {
                let place = if item.folder.is_empty() {
                    item.source.clone()
                } else {
                    format!("{}/{}", item.source, item.folder)
                };
                format!(
                    "Skill {name}: download {} at {}",
                    quoted(&place),
                    item.commit
                )
            });
        }

        let name = self.settings().free_name(&shared.name);
        let title = format!("Import “{name}”?");
        let shelf = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let (shelf, incoming) = (shelf.clone(), incoming.clone());
            alert
                .title(title.clone())
                .width(px(680.))
                .description(
                    v_flex()
                        .gap_2()
                        .child("It is added as a new profile. Downloads come from GitHub, pinned to the commits listed; review what you don't know.")
                        .child(
                            v_flex()
                                .id("import-preview")
                                .max_h(px(360.))
                                .overflow_y_scroll()
                                .gap_1()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .when(lines.is_empty(), |this| this.child("It has nothing turned on."))
                                .children(lines.clone()),
                        ),
                )
                .show_cancel(true)
                .ok_text("Import")
                .on_ok(move |_, window, cx| {
                    shelf.update(cx, |shelf, cx| shelf.import(incoming.clone(), window, cx));
                    true
                })
        });
    }

    fn import(&mut self, incoming: Incoming, window: &mut Window, cx: &mut Context<Self>) {
        let env = self.env.clone();
        let library = self.library.clone();
        let access = self.settings().github;
        let client = settings::client(cx);
        self.busy = true;
        cx.notify();
        let name = incoming.shared.name.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let token = settings::github_token(access, &env).ok().flatten();
                    let downloads: Downloads =
                        fetch_import(&env, &library, &incoming.shared, token.as_deref())?;
                    let steps = import_steps(
                        &env,
                        &library,
                        &incoming.shared,
                        &downloads,
                        incoming.staged.as_deref(),
                    );
                    let result = steps.and_then(|steps| settings::execute(&client, steps));
                    if result.is_err() {
                        downloads.discard();
                    }
                    if let Some(staged) = &incoming.staged {
                        let _ = std::fs::remove_dir_all(staged);
                    }
                    result
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        window.push_notification(
                            Notification::success(format!("Imported {name}")),
                            cx,
                        );
                        this.open_page(Page::Profiles, window, cx);
                    }
                    Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The title bar's switcher: the profiles, the active one checked.
    pub(super) fn profile_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let names: Vec<String> = self
            .settings()
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let active = self.settings().active_index();
        let shelf = cx.weak_entity();
        Button::new("profile")
            .ghost()
            .small()
            .icon(Icon::new(Lucide::Layers))
            .label(format!("Profile: {}", self.settings().active().name))
            .tooltip("Profile")
            .disabled(self.busy || self.loading)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.scrollable(true).max_h(px(420.));
                for (ix, name) in names.iter().enumerate() {
                    let shelf = shelf.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(ix == active)
                            .on_click(move |_, window, cx| {
                                shelf
                                    .update(cx, |shelf, cx| shelf.switch_profile(ix, window, cx))
                                    .ok();
                            }),
                    );
                }
                let shelf = shelf.clone();
                menu.separator()
                    .item(
                        PopupMenuItem::new("Manage profiles…").on_click(move |_, window, cx| {
                            shelf
                                .update(cx, |shelf, cx| shelf.open_page(Page::Profiles, window, cx))
                                .ok();
                        }),
                    )
            })
    }

    pub(super) fn render_profiles(&self, cx: &mut Context<Self>) -> AnyElement {
        let profiles: Vec<_> = self.visible_profiles(cx).collect();
        let rows: Vec<_> = profiles
            .into_iter()
            .map(|(ix, profile)| self.profile_row(ix, profile, cx))
            .collect();
        v_flex()
            .size_full()
            .child(
                self.page_header("Profiles", Some(rows.len()), cx)
                    .child(
                        Button::new("import-profile")
                            .small()
                            .icon(Lucide::Download)
                            .label("Import")
                            .disabled(self.busy)
                            .dropdown_menu({
                                let shelf = cx.weak_entity();
                                move |menu, _, _| {
                                    let (code, file) = (shelf.clone(), shelf.clone());
                                    menu.item(PopupMenuItem::new("Paste a code…").on_click(
                                        move |_, window, cx| {
                                            code.update(cx, |shelf, cx| {
                                                shelf.open_import_dialog(None, window, cx)
                                            })
                                            .ok();
                                        },
                                    ))
                                    .item(
                                        PopupMenuItem::new("Open a .xolkit file…").on_click(
                                            move |_, window, cx| {
                                                file.update(cx, |shelf, cx| {
                                                    shelf.import_file(window, cx)
                                                })
                                                .ok();
                                            },
                                        ),
                                    )
                                }
                            }),
                    )
                    .child(
                        Button::new("new-profile")
                            .small()
                            .icon(IconName::Plus)
                            .label("New profile…")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_name_dialog(None, window, cx)
                            })),
                    ),
            )
            .child(list_body("profile-list", rows, &self.list_scroll, cx))
            .into_any_element()
    }

    fn profile_row(&self, ix: usize, profile: &Profile, cx: &mut Context<Self>) -> AnyElement {
        let active = ix == self.settings().active_index();
        let tag = match (ix == 0, active) {
            (true, true) => Some("default, in use"),
            (true, false) => Some("default"),
            (false, true) => Some("in use"),
            (false, false) => None,
        };
        let control = if active {
            cell()
                .text_color(cx.theme().muted_foreground)
                .child(IconName::Check)
                .into_any_element()
        } else {
            cell()
                .child(
                    Button::new(SharedString::from(format!("use-profile-{ix}")))
                        .small()
                        .label("Switch")
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.switch_profile(ix, window, cx)
                        })),
                )
                .into_any_element()
        };
        let selection = Selection::Profile(profile.name.clone());
        let selected = self.selected.as_ref() == Some(&selection);
        self.row(
            format!("profile-{ix}"),
            selected,
            profile.name.clone(),
            tag,
            summary(profile),
            vec![control],
            cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
            cx,
        )
    }

    /// A switch per agent, turning serving it on or off in profile `ix`.
    pub(super) fn agent_rows(&self, ix: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let agents = self.settings().profiles[ix].agents.clone();
        Agent::ALL
            .into_iter()
            .map(|agent| {
                let on = agents.contains(&agent);
                let note = match agent.provider() {
                    Some(provider) => match self.found.get(&provider) {
                        Some(place) => format!("Found at {}", self.env.tilde(place)),
                        None => "Not found on this computer".to_owned(),
                    },
                    None => "Any other agent that runs xolkit mcp".to_owned(),
                };
                h_flex()
                    .gap_3()
                    .child(
                        gpui_kit::component::switch::Switch::new(SharedString::from(format!(
                            "agent-{ix}-{}",
                            agent.key()
                        )))
                        .checked(on)
                        .disabled(self.busy)
                        .on_change(cx.listener(
                            move |this, on: &bool, window, cx| {
                                this.edit_profile(ix, |p| p.set_agent(agent, *on), window, cx)
                            },
                        )),
                    )
                    .child(
                        v_flex()
                            .child(div().text_sm().child(agent.name()))
                            .child(div().text_xs().text_color(muted).child(note)),
                    )
                    .into_any_element()
            })
            .collect()
    }

    pub(super) fn profile_detail(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let profile = &self.settings().profiles[ix];
        let active = ix == self.settings().active_index();
        let muted = cx.theme().muted_foreground;
        let switch = Button::new("switch-profile")
            .primary()
            .label(if active { "In use" } else { "Switch" })
            .disabled(active || self.busy)
            .on_click(cx.listener(move |this, _, window, cx| this.switch_profile(ix, window, cx)));
        let note = if ix == 0 {
            "The default profile can't be renamed or removed."
        } else if active {
            "Switch to another profile to remove this one."
        } else {
            ""
        };
        let manage = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("rename-profile")
                    .small()
                    .icon(Icon::new(Lucide::Pencil))
                    .label("Rename…")
                    .disabled(ix == 0 || self.busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_name_dialog(Some(ix), window, cx)
                    })),
            )
            .child(
                Button::new("remove-profile")
                    .small()
                    .danger()
                    .icon(Lucide::Trash)
                    .label("Remove…")
                    .disabled(ix == 0 || active || self.busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_remove_profile(ix, window, cx)
                    })),
            );
        let share = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("copy-code")
                    .small()
                    .icon(IconName::Copy)
                    .label("Copy code")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.copy_code(ix, window, cx)),
                    ),
            )
            .child(
                Button::new("copy-link")
                    .small()
                    .icon(IconName::ExternalLink)
                    .label("Copy link")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.copy_link(ix, window, cx)),
                    ),
            )
            .child(
                Button::new("export-profile")
                    .small()
                    .icon(Lucide::Download)
                    .label("Export file…")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.export_profile(ix, window, cx)),
                    ),
            );
        v_flex()
            .size_full()
            .child(self.detail_header(
                profile.name.clone(),
                (ix == 0).then(|| "Default profile".to_owned()),
                switch,
                cx,
            ))
            .child(
                div()
                    .id("profile-detail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .child(manage)
                            .when(!note.is_empty(), |this| {
                                this.child(div().text_xs().text_color(muted).child(note))
                            })
                            .child(divider(cx))
                            .child(section("Share"))
                            .child(div().text_sm().text_color(muted).child(
                                "A code or link lists where each skill and plugin comes from, \
                                 never their files or your secrets. Server settings not saved as \
                                 secrets are shared as typed. A file also carries skills that \
                                 exist only on this computer.",
                            ))
                            .child(share)
                            .child(divider(cx))
                            .child(section("Agents"))
                            .child(v_flex().gap_3().children(self.agent_rows(ix, cx)))
                            .child(divider(cx))
                            .children(self.profile_contents(ix, cx)),
                    ),
            )
            .into_any_element()
    }

    /// Everything in the library, with a switch per agent the profile serves, changing the
    /// profile only (and, for the profile in use, what agents get at once).
    fn profile_contents(&self, ix: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let profile = &self.settings().profiles[ix];
        let muted = cx.theme().muted_foreground;
        if profile.agents.is_empty() {
            return vec![
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Turn on an agent to pick what it gets.")
                    .into_any_element(),
            ];
        }
        let mut contents = Vec::new();
        let lists: [(&str, Kind, Vec<String>); 3] = [
            (
                "Skills",
                Kind::Skill,
                self.library.skills.iter().map(|s| s.name.clone()).collect(),
            ),
            (
                "Plugins",
                Kind::Plugin,
                self.library.plugins.iter().map(|p| p.id.clone()).collect(),
            ),
            (
                "MCP servers",
                Kind::Server,
                self.library.servers.keys().cloned().collect(),
            ),
        ];
        for (title, kind, names) in lists {
            if names.is_empty() {
                continue;
            }
            contents.push(self.item_header(title, &profile.agents, cx));
            for name in names {
                contents.push(self.profile_item(ix, kind, &name, cx));
            }
        }
        if contents.is_empty() {
            contents.push(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("The library is empty.")
                    .into_any_element(),
            );
        }
        contents
    }

    fn item_header(&self, title: &str, agents: &[Agent], cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .pt_2()
            .gap_3()
            .child(div().flex_1().font_semibold().child(title.to_owned()))
            .children(agents.iter().map(|agent| {
                cell()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(agent.label())
            }))
            .into_any_element()
    }

    fn profile_item(
        &self,
        ix: usize,
        kind: Kind,
        name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let profile = &self.settings().profiles[ix];
        let active = ix == self.settings().active_index();
        let switches = profile.agents.iter().map(|&agent| {
            let item = name.to_owned();
            self.switch(
                format!("profile-{ix}-{kind:?}-{}-{name}", agent.key()),
                profile.switched_on(kind, name, agent),
                false,
                agent.label(),
                cx.listener(move |this, on: &bool, window, cx| {
                    if active {
                        this.set_item(kind, &item, &[agent], *on, window, cx);
                    } else {
                        this.edit_profile(ix, |p| p.set(kind, &item, agent, *on), window, cx);
                    }
                }),
            )
        });
        h_flex()
            .gap_3()
            .text_sm()
            .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
            .children(switches)
            .into_any_element()
    }
}

/// "Claude, Codex · 3 skills, 1 plugin and 2 servers on", from what the profile has saved.
fn summary(profile: &Profile) -> String {
    if profile.agents.is_empty() {
        return "No agents".to_owned();
    }
    let on = |items: &std::collections::BTreeMap<String, xolkit_core::States>| {
        items
            .values()
            .filter(|states| {
                profile
                    .agents
                    .iter()
                    .any(|agent| states.get(agent) == Some(&true))
            })
            .count()
    };
    let agents: Vec<&str> = profile.agents.iter().map(|a| a.label()).collect();
    format!(
        "{} · {}, {} and {} on",
        agents.join(", "),
        count(on(&profile.skills), "skill"),
        count(on(&profile.plugins), "plugin"),
        count(on(&profile.servers), "server")
    )
}

#[cfg(test)]
mod tests {
    use xolkit_core::{Agent, Kind, Profile};

    use super::summary;

    #[test]
    fn summarizes_agents_and_what_is_on() {
        let mut profile = Profile::new("Work", vec![]);
        assert_eq!(summary(&profile), "No agents");
        profile.set_agent(Agent::Claude, true);
        profile.set(Kind::Skill, "a", Agent::Claude, true);
        profile.set(Kind::Skill, "b", Agent::Claude, false);
        // Saved for an agent the profile doesn't serve.
        profile.set(Kind::Skill, "c", Agent::Codex, true);
        profile.set(Kind::Plugin, "p@m", Agent::Claude, true);
        assert_eq!(
            summary(&profile),
            "Claude · 1 skill, 1 plugin and 0 servers on"
        );
    }
}
