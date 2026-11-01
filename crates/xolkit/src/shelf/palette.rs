//! The command palette (`ctrl-k`): go to a page, reload, switch profiles, pick a theme.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, IndexPath, WindowExt,
    command::{Command, CommandGroup, CommandItem},
};
use gpui_kit::*;

use super::{Page, Shelf};
use crate::theme;

#[derive(Clone)]
enum Run {
    Open(Page),
    Reload,
    Profile(usize),
    Theme(SharedString),
}

/// A titled group of palette items, each with what it runs.
type Group = (&'static str, Vec<(CommandItem, Run)>);

impl Shelf {
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let active_theme = cx.theme().theme_name().clone();
        let page = |label: &'static str, icon: Icon, page: Page| {
            let item = CommandItem::new().label(label).icon(icon);
            (item.keywords(["go", "page"]), Run::Open(page))
        };
        let groups: Rc<Vec<Group>> = Rc::new(vec![
            (
                "Go to",
                vec![
                    page("Profiles", Icon::new(Lucide::Layers), Page::Profiles),
                    page(
                        "Connections",
                        Icon::new(Lucide::Activity),
                        Page::Connections,
                    ),
                    page("Skills", IconName::BookOpen.into(), Page::Skills),
                    page("Plugins", Icon::new(Lucide::Puzzle), Page::Plugins),
                    page("MCP servers", Icon::new(Lucide::Server), Page::Servers),
                    page("Discover skills", IconName::Search.into(), Page::FindSkills),
                    page(
                        "Discover plugins",
                        Icon::new(Lucide::Store),
                        Page::FindPlugins,
                    ),
                    page("Settings", IconName::Settings.into(), Page::Settings),
                    page("Licenses", IconName::FileText.into(), Page::Licenses),
                ],
            ),
            (
                "Actions",
                vec![(
                    CommandItem::new()
                        .label("Reload")
                        .icon(IconName::RefreshCw)
                        .keywords(["refresh", "rescan"]),
                    Run::Reload,
                )],
            ),
            (
                "Profile",
                self.settings()
                    .profiles
                    .iter()
                    .enumerate()
                    .map(|(ix, profile)| {
                        let item = CommandItem::new()
                            .label(profile.name.clone())
                            .icon(Icon::new(Lucide::Layers))
                            .checked(ix == self.settings().active_index())
                            .keywords(["profile", "switch"]);
                        (item, Run::Profile(ix))
                    })
                    .collect(),
            ),
            (
                "Theme",
                theme::names(cx)
                    .into_iter()
                    .map(|(name, mode)| {
                        let item = CommandItem::new()
                            .label(name.clone())
                            .icon(IconName::Palette)
                            .checked(name == active_theme)
                            .keywords(["theme", mode.name()]);
                        (item, Run::Theme(name))
                    })
                    .collect(),
            ),
        ]);

        self.palette
            .update(cx, |palette, cx| palette.set_query("", window, cx));
        let palette = self.palette.clone();
        let shelf = cx.weak_entity();
        // The palette's model is only installed by its first render, so focus it after that.
        let focus_on_mount = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let (palette, shelf, groups) = (palette.clone(), shelf.clone(), groups.clone());
            let focus_on_mount = focus_on_mount.clone();
            dialog
                .close_button(false)
                .p_0()
                .content(move |content, window, cx| {
                    if focus_on_mount.replace(false) {
                        let palette = palette.clone();
                        window.defer(cx, move |window, cx| {
                            palette.read(cx).focus_handle(cx).focus(window, cx)
                        });
                    }
                    let mut command = Command::new(&palette)
                        .bordered(false)
                        .placeholder("Type a command, a profile or a theme")
                        .max_h(px(400.));
                    for (label, items) in groups.iter() {
                        let items = items.iter().map(|(item, _)| item.clone());
                        command = command.group(CommandGroup::new().label(*label).items(items));
                    }
                    let (shelf, groups) = (shelf.clone(), groups.clone());
                    content.child(command.on_confirm(move |index: IndexPath, window, cx| {
                        let Some((_, run)) = groups
                            .get(index.section)
                            .and_then(|(_, items)| items.get(index.row))
                        else {
                            return;
                        };
                        let run = run.clone();
                        window.close_dialog(cx);
                        shelf
                            .update(cx, |shelf, cx| match run {
                                Run::Open(page) => {
                                    shelf.open_page(page, window, cx);
                                    shelf
                                        .filter
                                        .update(cx, |filter, cx| filter.focus(window, cx));
                                }
                                Run::Reload => shelf.reload_all(cx),
                                Run::Profile(ix) => shelf.switch_profile(ix, window, cx),
                                Run::Theme(name) => theme::select(&name, cx),
                            })
                            .ok();
                    }))
                })
        });
    }
}
