//! Updating xolkit itself: a check on start, and the install and restart under Settings → About.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    h_flex,
    notification::Notification,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use xolkit_core::{Release, check_for_update, install_update, installed_by_package_manager};

use super::{Page, Shelf, icon_row};
use crate::settings;

pub(super) enum AppUpdate {
    Checking,
    Current,
    CheckFailed(SharedString),
    Available(Release),
    Installing(Release),
    /// Installed over the program file; runs once xolkit restarts.
    Installed(String),
}

impl Shelf {
    pub(super) fn check_app_update(&mut self, cx: &mut Context<Self>) {
        self.app_update = AppUpdate::Checking;
        self.app_update_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { check_for_update() }).await;
            this.update(cx, |this, cx| {
                this.app_update = match result {
                    Ok(Some(release)) => AppUpdate::Available(release),
                    Ok(None) => AppUpdate::Current,
                    Err(e) => AppUpdate::CheckFailed(format!("{e:#}").into()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn install_app_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let AppUpdate::Available(release) = &self.app_update else {
            return;
        };
        let exe = self.exe(cx);
        let release = release.clone();
        self.app_update = AppUpdate::Installing(release.clone());
        self.app_update_task = Some(cx.spawn_in(window, async move |this, cx| {
            let installing = release.clone();
            let result = cx
                .background_spawn(async move { install_update(&installing, &exe) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.app_update = match result {
                    Ok(()) => AppUpdate::Installed(release.version),
                    Err(e) => {
                        window.push_notification(Notification::error(format!("{e:#}")), cx);
                        AppUpdate::Available(release)
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Stops the daemon, so the next session starts the new version, then starts the program
    /// file, now the new version, and quits this one. Open agent sessions reconnect on their own.
    fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let exe = self.exe(cx);
        let _ = settings::call(&settings::client(cx), xolkit_daemon::Request::Shutdown);
        match std::process::Command::new(&exe).spawn() {
            Ok(_) => cx.quit(),
            Err(e) => window.push_notification(
                Notification::error(format!("could not start xolkit again: {e}")),
                cx,
            ),
        }
    }

    fn restart_button(&self, id: &'static str, cx: &mut Context<Self>) -> Button {
        Button::new(id)
            .small()
            .primary()
            .icon(IconName::RefreshCw)
            .label("Restart")
            // Quitting midway would leave a change half sent.
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, window, cx| this.restart(window, cx)))
    }

    /// In the title bar, while an update is waiting.
    pub(super) fn app_update_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        match &self.app_update {
            AppUpdate::Available(release) => Some(
                Button::new("app-update")
                    .ghost()
                    .small()
                    .icon(Icon::new(Lucide::Download))
                    .label(format!("xolkit {} is out", release.version))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_page(Page::Settings, window, cx)
                    })),
            ),
            AppUpdate::Installed(_) => Some(
                self.restart_button("app-restart", cx)
                    .ghost()
                    .label("Restart to update")
                    .tooltip("Start the new version of xolkit"),
            ),
            _ => None,
        }
    }

    /// The update state and its action, under Settings → About.
    pub(super) fn render_app_update(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, success, danger) = (theme.muted_foreground, theme.success, theme.danger);
        let check_again = || {
            Button::new("check-update")
                .small()
                .label("Check again")
                .on_click(cx.listener(|this, _, _, cx| this.check_app_update(cx)))
        };
        let release_notes = |release: &Release| {
            let url = release.url.clone();
            Button::new("release-notes")
                .small()
                .link()
                .label("Release notes")
                .on_click(move |_, _, cx| cx.open_url(&url))
        };
        let packaged = installed_by_package_manager(&self.exe(cx));
        let (status, actions): (Div, Vec<Button>) = match &self.app_update {
            AppUpdate::Checking => (
                icon_row(Spinner::new().small(), "Checking for updates…").text_color(muted),
                vec![],
            ),
            AppUpdate::Current => (
                icon_row(IconName::CircleCheck, "This is the latest version.").text_color(success),
                vec![check_again()],
            ),
            AppUpdate::CheckFailed(error) => (
                icon_row(
                    IconName::CircleAlert,
                    format!("Could not check for updates: {error}"),
                )
                .text_color(danger),
                vec![check_again()],
            ),
            AppUpdate::Available(release) => {
                let note = if packaged {
                    "Your package manager installed this copy; update it there."
                } else if !release.has_download() {
                    "The release has no download for this computer."
                } else {
                    "It replaces this copy, and runs once xolkit restarts."
                };
                let mut actions = vec![];
                if !packaged && release.has_download() {
                    actions.push(
                        Button::new("install-update")
                            .small()
                            .primary()
                            .icon(Icon::new(Lucide::Download))
                            .label(format!("Update to {}", release.version))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.install_app_update(window, cx)
                            })),
                    );
                }
                actions.push(release_notes(release));
                (
                    icon_row(
                        Icon::new(Lucide::Download),
                        format!("xolkit {} is out. {note}", release.version),
                    ),
                    actions,
                )
            }
            AppUpdate::Installing(release) => (
                icon_row(
                    Spinner::new().small(),
                    format!("Installing xolkit {}…", release.version),
                )
                .text_color(muted),
                vec![],
            ),
            AppUpdate::Installed(version) => (
                icon_row(
                    IconName::CircleCheck,
                    format!("xolkit {version} is installed. Restart to use it."),
                )
                .text_color(success),
                vec![self.restart_button("restart", cx)],
            ),
        };
        v_flex()
            .gap_2()
            .child(div().text_sm().child(status))
            .when(!actions.is_empty(), |this| {
                this.child(h_flex().gap_2().children(actions))
            })
    }
}
