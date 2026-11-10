//! Discover: skills from skills.sh and plugins from the configured marketplaces.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, WindowExt, button::Button, button::ButtonVariants,
    h_flex, input::Input, notification::Notification, scroll::ScrollableElement, spinner::Spinner,
    text::TextView, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{
    Offer, PluginSource, SkillHit, fetch_offers, fetch_plugin, fetch_skill, search_skills,
    valid_marketplace,
};

use super::change::{Change, open_change_dialog};
use super::{
    SEARCH_DELAY, Selection, Shelf, cell, divider, empty_state, fact, icon_line, list_body,
    matches, warning,
};
use crate::settings;

/// Most marketplace plugins to list at once; the filter narrows the rest.
pub(super) const MAX_OFFERS: usize = 200;

impl Shelf {
    /// Searches skills.sh for the filter text once typing pauses.
    pub(super) fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.filter_text(cx);
        self.search_error = None;
        if query.chars().count() < 2 {
            self.hits.clear();
            self.searching = false;
            self.search_task = None;
            return;
        }
        self.searching = true;
        // Replacing the task cancels the previous, still-waiting search.
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let result = cx
                .background_spawn(async move { search_skills(&query) })
                .await;
            this.update(cx, |this, cx| {
                this.searching = false;
                match result {
                    Ok(hits) => {
                        this.hits = hits;
                        this.list_scroll.set_offset(Point::default());
                    }
                    Err(e) => this.search_error = Some(format!("{e:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Downloads a search result so its files can be reviewed and installed.
    pub(super) fn fetch(&mut self, hit: SkillHit, cx: &mut Context<Self>) {
        let env = self.env.clone();
        let access = self.settings().github;
        self.fetching = true;
        let selection = Selection::Hit(hit.id.clone());
        // shortcut: a download still running when the selection changes is dropped, and its
        // files are only removed on the next start (clean_staging); cancel it if that matters.
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            // Waits like the search does, so arrowing through results downloads only where it stops.
            cx.background_executor().timer(SEARCH_DELAY).await;
            let result = cx
                .background_spawn(async move {
                    // Without a usable token the download still works, within the anonymous limit.
                    let token = settings::github_token(access, &env).ok().flatten();
                    fetch_skill(&env, &hit.source, &hit.skill_id, token.as_deref())
                })
                .await;
            this.update(cx, |this, cx| {
                if this.selected.as_ref() != Some(&selection) {
                    if let Ok(fetched) = result {
                        fetched.discard();
                    }
                    return;
                }
                this.fetching = false;
                match result {
                    Ok(fetched) => this.set_fetched(Some(fetched)),
                    Err(e) => this.fetch_error = Some(format!("{e:#}").into()),
                }
                this.refresh_skill_md();
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn load_offers(&mut self, cx: &mut Context<Self>) {
        let env = self.env.clone();
        let access = self.settings().github;
        let repos = self.settings().marketplaces.clone();
        self.loading_offers = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (offers, errors) = cx
                .background_spawn(async move {
                    let token = settings::github_token(access, &env).ok().flatten();
                    fetch_offers(&repos, token.as_deref())
                })
                .await;
            this.update(cx, |this, cx| {
                this.loading_offers = false;
                this.offers = Some(offers);
                this.offer_errors = errors;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn offer(&self, id: &str) -> Option<&Offer> {
        self.offers.as_ref()?.iter().find(|o| o.id == id)
    }

    pub(super) fn visible_offers(&self, cx: &App) -> impl Iterator<Item = &Offer> {
        let filter = self.filter_text(cx);
        self.offers
            .iter()
            .flatten()
            .filter(move |o| matches(&filter, &[&o.id, &o.description]))
    }

    fn add_marketplace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.marketplace_input.read(cx).value().trim().to_owned();
        let repo = repo
            .trim_start_matches("https://github.com/")
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_owned();
        if !valid_marketplace(&repo) {
            window.push_notification(
                Notification::error(format!("“{repo}” is not a GitHub owner/repository")),
                cx,
            );
            return;
        }
        let mut settings = self.settings().clone();
        if settings.marketplaces.contains(&repo) {
            return;
        }
        settings.marketplaces.push(repo.clone());
        self.marketplace_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.offers = None;
        self.save_settings(
            settings,
            Vec::new(),
            Some(format!("Added {repo}").into()),
            window,
            cx,
        );
        // The settings changed in memory at once, so this reads the new list.
        self.load_offers(cx);
    }

    fn remove_marketplace(&mut self, repo: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut settings = self.settings().clone();
        settings.marketplaces.retain(|m| m != repo);
        self.offers = None;
        self.save_settings(settings, Vec::new(), None, window, cx);
        self.load_offers(cx);
    }

    /// Asks to install the downloaded search result.
    pub(super) fn install_hit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selection::Hit(id)) = &self.selected else {
            return;
        };
        let installed = self
            .hits
            .iter()
            .find(|h| &h.id == id)
            .is_some_and(|hit| self.installed_skill(hit));
        if let Some(fetched) = self.fetched.clone()
            && !installed
            && !self.busy
        {
            open_change_dialog(self, Change::InstallSkill(fetched), window, cx);
        }
    }

    /// Downloads the selected marketplace plugin, then asks to install it.
    pub(super) fn install_offer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selection::Offer(id)) = &self.selected else {
            return;
        };
        let Some(offer) = self.offer(id).cloned() else {
            return;
        };
        if self.library.plugin(&offer.id).is_some() || self.busy || self.fetching {
            return;
        }
        let env = self.env.clone();
        let access = self.settings().github;
        self.fetching = true;
        self.fetch_error = None;
        let selection = Selection::Offer(offer.id.clone());
        self.fetch_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let token = settings::github_token(access, &env).ok().flatten();
                    fetch_plugin(&env, &offer, token.as_deref())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.fetching = false;
                if this.selected.as_ref() != Some(&selection) {
                    if let Ok(fetched) = result {
                        fetched.discard();
                    }
                    return;
                }
                match result {
                    Ok(fetched) => {
                        this.set_fetched_plugin(Some(fetched.clone()));
                        open_change_dialog(this, Change::InstallPlugin(fetched), window, cx);
                    }
                    Err(e) => this.fetch_error = Some(format!("{e:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn installed_skill(&self, hit: &SkillHit) -> bool {
        self.library.has_skill_from(&hit.skill_id, &hit.source)
            || self.library.has_skill_from(&hit.name, &hit.source)
    }

    pub(super) fn render_find_skills(&self, cx: &mut Context<Self>) -> AnyElement {
        let short_query = self.filter_text(cx).chars().count() < 2;
        let body = if short_query {
            empty_state(
                IconName::Search,
                "Search skills.sh",
                "Type at least two characters to search the skills.sh directory.",
                cx,
            )
            .into_any_element()
        } else if let Some(error) = &self.search_error {
            empty_state(IconName::CircleAlert, "Search failed", error, cx).into_any_element()
        } else if self.searching && self.hits.is_empty() {
            empty_state(IconName::Search, "Searching…", "", cx).into_any_element()
        } else {
            let rows = self.hits.iter().map(|hit| self.hit_row(hit, cx)).collect();
            list_body("hit-list", rows, &self.list_scroll, cx)
        };
        v_flex()
            .size_full()
            .child(self.page_header("Find skills", None, cx))
            .child(self.column_header(&["Installs"], cx))
            .child(body)
            .into_any_element()
    }

    fn hit_row(&self, hit: &SkillHit, cx: &mut Context<Self>) -> AnyElement {
        let selection = Selection::Hit(hit.id.clone());
        let selected = self.selected.as_ref() == Some(&selection);
        let installs = cell()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(compact(hit.installs))
            .into_any_element();
        self.row(
            format!("hit-{}", hit.id),
            selected,
            hit.name.clone(),
            self.installed_skill(hit).then_some("installed"),
            hit.source.clone(),
            vec![installs],
            cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
            cx,
        )
    }

    pub(super) fn hit_detail(&self, hit: &SkillHit, cx: &mut Context<Self>) -> AnyElement {
        let installed = self.installed_skill(hit);
        let install = Button::new("install")
            .primary()
            .icon(Lucide::Download)
            .label(if installed { "Installed" } else { "Install…" })
            .disabled(installed || self.fetched.is_none() || self.busy)
            .on_click(cx.listener(|this, _, window, cx| this.install_hit(window, cx)));
        let status = if self.fetching {
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().small())
                .child("Downloading for review…")
                .into_any_element()
        } else if let Some(error) = &self.fetch_error {
            div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element()
        } else {
            self.fetched_files(cx)
        };
        v_flex()
            .size_full()
            .child(self.detail_header(hit.name.clone(), Some(hit.source.clone()), install, cx))
            .child(
                div()
                    .id("hit-detail")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .p_4()
                            .gap_3()
                            .child(fact("Installs", hit.installs.to_string(), cx))
                            .child(status)
                            .child(divider(cx))
                            .child(
                                TextView::markdown("hit-md", self.skill_md.clone())
                                    .selectable(true),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The downloaded files, with a warning when the skill ships code an agent could run.
    fn fetched_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(fetched) = &self.fetched else {
            return div().into_any_element();
        };
        let scripts = fetched.files.iter().filter(|f| is_code(f)).count();
        v_flex()
            .gap_1()
            .text_sm()
            .when(scripts > 0, |this| {
                this.child(warning(
                    format!(
                        "Includes {scripts} script file(s) an agent may run. Review them first."
                    ),
                    cx,
                ))
            })
            .children(fetched.files.iter().map(|file| {
                h_flex()
                    .gap_2()
                    .line_height(icon_line())
                    .text_color(cx.theme().muted_foreground)
                    .when(is_code(file), |row| {
                        row.text_color(cx.theme().warning)
                            .child(IconName::TriangleAlert)
                    })
                    .child(file.clone())
            }))
            .into_any_element()
    }

    pub(super) fn render_find_plugins(&self, cx: &mut Context<Self>) -> AnyElement {
        let warnings: Vec<_> = self
            .offer_errors
            .iter()
            .map(|error| warning(error.clone(), cx))
            .collect();
        let marketplaces = self.settings().marketplaces.clone();
        let muted = cx.theme().muted_foreground;
        let bar = v_flex()
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().text_color(muted).child("Marketplaces"))
                    .children(marketplaces.iter().map(|repo| {
                        let remove = repo.clone();
                        Button::new(SharedString::from(format!("market-{repo}")))
                            .xsmall()
                            .outline()
                            .label(repo.clone())
                            .icon(IconName::Close)
                            .tooltip("Remove this marketplace")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_marketplace(&remove, window, cx)
                            }))
                    }))
                    .child(div().flex_1())
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&self.marketplace_input).small()),
                    )
                    .child(
                        Button::new("add-marketplace")
                            .small()
                            .icon(IconName::Plus)
                            .label("Add")
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_marketplace(window, cx)),
                            ),
                    ),
            );
        let (count, body) = match &self.offers {
            _ if marketplaces.is_empty() => (
                None,
                empty_state(
                    IconName::Inbox,
                    "No marketplaces yet",
                    "Add a GitHub repository with a .claude-plugin/marketplace.json, such as \
                     anthropics/claude-plugins-official.",
                    cx,
                )
                .into_any_element(),
            ),
            None => (
                None,
                empty_state(IconName::Search, "Reading your marketplaces…", "", cx)
                    .into_any_element(),
            ),
            Some(_) => {
                let matching: Vec<&Offer> = self.visible_offers(cx).collect();
                let mut rows: Vec<_> = matching
                    .iter()
                    .take(MAX_OFFERS)
                    .map(|offer| self.offer_row(offer, cx))
                    .collect();
                if matching.len() > MAX_OFFERS {
                    let note = format!(
                        "Showing the first {MAX_OFFERS} of {}. Type to narrow it down.",
                        matching.len()
                    );
                    rows.push(
                        div()
                            .p_4()
                            .text_sm()
                            .text_color(muted)
                            .child(note)
                            .into_any_element(),
                    );
                }
                (
                    Some(matching.len()),
                    list_body("offer-list", rows, &self.list_scroll, cx),
                )
            }
        };
        v_flex()
            .size_full()
            .child(self.page_header("Find plugins", count, cx))
            .child(bar)
            .children(warnings)
            .child(self.column_header(&["Version"], cx))
            .child(body)
            .into_any_element()
    }

    fn offer_row(&self, offer: &Offer, cx: &mut Context<Self>) -> AnyElement {
        let selection = Selection::Offer(offer.id.clone());
        let selected = self.selected.as_ref() == Some(&selection);
        let version = cell()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(offer.version.clone())
            .into_any_element();
        let installed = self.library.plugin(&offer.id).is_some();
        let tag = if installed {
            format!("{} · installed", offer.marketplace)
        } else {
            offer.marketplace.clone()
        };
        self.row(
            format!("offer-{}", offer.id),
            selected,
            offer.name.clone(),
            Some(tag.as_str()),
            offer.description.clone(),
            vec![version],
            cx.listener(move |this, _, _, cx| this.select(Some(selection.clone()), cx)),
            cx,
        )
    }

    pub(super) fn offer_detail(&self, offer: &Offer, cx: &mut Context<Self>) -> AnyElement {
        let installed = self.library.plugin(&offer.id).is_some();
        let source = match &offer.source {
            PluginSource::GitHub { repo, folder, rev } => {
                let mut text = repo.clone();
                if !folder.is_empty() {
                    text = format!("{text}/{folder}");
                }
                if let Some(rev) = rev {
                    text = format!("{text} at {rev}");
                }
                Ok(text)
            }
            PluginSource::Unsupported(what) => Err(format!("xolkit can't install from {what}")),
        };
        let install = Button::new("install")
            .primary()
            .icon(Lucide::Download)
            .label(if installed { "Installed" } else { "Install…" })
            .disabled(installed || source.is_err() || self.busy || self.fetching)
            .on_click(cx.listener(|this, _, window, cx| this.install_offer(window, cx)));
        let status = if self.fetching {
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().small())
                .child("Downloading…")
                .into_any_element()
        } else if let Some(error) = &self.fetch_error {
            div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element()
        } else {
            div().into_any_element()
        };
        v_flex()
            .size_full()
            .child(self.detail_header(
                offer.name.clone(),
                Some(offer.marketplace_repo.clone()),
                install,
                cx,
            ))
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .child(offer.description.clone())
                    .child(
                        v_flex()
                            .gap_1()
                            .when(!offer.version.is_empty(), |this| {
                                this.child(fact("Version", offer.version.clone(), cx))
                            })
                            .child(match source {
                                Ok(text) => fact("Source", text, cx),
                                Err(text) => warning(text, cx),
                            }),
                    )
                    .child(status),
            )
            .into_any_element()
    }
}

/// `839k`, `1.2M`.
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Files an agent could execute.
pub(super) fn is_code(file: &str) -> bool {
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some(
            "sh" | "bash"
                | "zsh"
                | "fish"
                | "py"
                | "js"
                | "mjs"
                | "cjs"
                | "ts"
                | "rb"
                | "pl"
                | "php"
                | "ps1"
                | "bat"
                | "cmd"
                | "exe"
                | "bin"
                | "so"
                | "dll"
                | "jar"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::{compact, is_code};

    #[test]
    fn formats_counts_and_spots_code() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(839_034), "839k");
        assert_eq!(compact(1_250_000), "1.2M");
        assert!(is_code("scripts/run.SH"));
        assert!(!is_code("references/guide.md"));
    }
}
