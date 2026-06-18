use super::*;
use gitcomet_state::session;

#[cfg_attr(test, allow(dead_code))]
pub(crate) const UPDATE_POSTPONE_SECONDS: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(test, allow(dead_code))]
pub(super) enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading { version: String },
    Applying,
    Failed(String),
}

impl GitCometView {
    pub(in crate::view) fn maybe_check_for_updates_on_startup(
        &mut self,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.view_mode != GitCometViewMode::Normal
            || std::env::var_os(update_check::UPDATE_CHECK_DISABLE_ENV).is_some()
        {
            return;
        }

        auto_update::cleanup_staging_if_safe();
        self.check_for_updates(false, cx);
    }

    pub(in crate::view) fn check_for_updates(
        &mut self,
        manual: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.view_mode != GitCometViewMode::Normal
            || std::env::var_os(update_check::UPDATE_CHECK_DISABLE_ENV).is_some()
        {
            if manual {
                self.update_phase = UpdatePhase::Failed(
                    "Update checks are disabled for this session.".to_string(),
                );
                self.sync_update_status_line();
                self.sync_update_ui(cx);
            }
            return;
        }

        self.update_phase = UpdatePhase::Checking;
        self.sync_update_status_line();
        self.sync_update_ui(cx);

        #[cfg(test)]
        let _ = (manual, cx);

        #[cfg(not(test))]
        {
            let http_client = cx.http_client();
            cx.spawn(
                async move |view: WeakEntity<GitCometView>, cx: &mut gpui::AsyncApp| {
                    let notice = update_check::fetch_update_notice(
                        env!("CARGO_PKG_VERSION"),
                        update_check::resolve_update_repo(),
                        http_client,
                    )
                    .await;

                    let _ = view.update(cx, |this, cx| {
                        this.handle_update_check_result(notice, manual, cx);
                    });
                },
            )
            .detach();
        }
    }

    pub(in crate::view) fn show_update_prompt(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(notice) = self.available_update.clone() else {
            if matches!(self.update_phase, UpdatePhase::Idle | UpdatePhase::UpToDate) {
                self.check_for_updates(true, cx);
            }
            return;
        };
        self.push_update_toast(&notice, cx);
    }

    pub(in crate::view) fn update_status_for_settings(&self) -> (SharedString, bool) {
        match &self.update_phase {
            UpdatePhase::Idle => ("Check for updates to see status.".into(), false),
            UpdatePhase::Checking => ("Checking for updates…".into(), false),
            UpdatePhase::UpToDate => ("You're up to date.".into(), false),
            UpdatePhase::Available => {
                let Some(notice) = self.available_update.as_ref() else {
                    return ("Update available".into(), false);
                };
                if session::is_update_dismissed_for_version(&notice.latest_version) {
                    return ("You're up to date.".into(), false);
                }
                (
                    format!("v{} available", notice.latest_version).into(),
                    true,
                )
            }
            UpdatePhase::Downloading { version } => {
                (format!("Downloading v{version}…").into(), false)
            }
            UpdatePhase::Applying => ("Applying update…".into(), false),
            UpdatePhase::Failed(message) => (message.clone().into(), false),
        }
    }

    pub(in crate::view) fn begin_upgrade_from_notice(
        &mut self,
        notice: &update_check::UpdateNotice,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(download_url) = notice.download_url.clone() else {
            self.push_toast(
                components::ToastKind::Warning,
                "No download package is available for this platform. Open the release page instead."
                    .to_string(),
                cx,
            );
            return;
        };
        self.update_phase = UpdatePhase::Downloading {
            version: notice.latest_version.clone(),
        };
        self.sync_update_status_line();
        self.sync_update_ui(cx);
        self.begin_update_download(download_url, notice.latest_version.clone(), cx);
    }

    pub(in crate::view) fn dismiss_update_version(
        &mut self,
        version: &str,
        cx: &mut gpui::Context<Self>,
    ) {
        let _ = session::persist_update_dismissed(version);
        self.available_update = None;
        self.update_phase = UpdatePhase::UpToDate;
        self.sync_update_status_line();
        self.sync_update_ui(cx);
    }

    pub(in crate::view) fn postpone_update(
        &mut self,
        version: &str,
        postpone_seconds: u64,
        cx: &mut gpui::Context<Self>,
    ) {
        let _ = session::persist_update_postponed(postpone_seconds);
        let _ = version;
        self.sync_update_status_line();
        self.sync_update_ui(cx);
    }

    #[cfg_attr(test, allow(dead_code))]
    fn handle_update_check_result(
        &mut self,
        notice: Option<update_check::UpdateNotice>,
        manual: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(notice) = notice else {
            if manual {
                self.update_phase = UpdatePhase::UpToDate;
                self.available_update = None;
                self.push_toast(
                    components::ToastKind::Success,
                    "You're up to date.".to_string(),
                    cx,
                );
            } else {
                self.update_phase = UpdatePhase::Idle;
            }
            self.sync_update_status_line();
            self.sync_update_ui(cx);
            return;
        };

        self.available_update = Some(notice.clone());
        self.update_phase = UpdatePhase::Available;
        self.sync_update_status_line();
        self.sync_update_ui(cx);

        if manual || self.should_prompt_for_update(&notice) {
            self.push_update_toast(&notice, cx);
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn should_prompt_for_update(&self, notice: &update_check::UpdateNotice) -> bool {
        session::should_show_update_toast(&notice.latest_version)
    }

    fn push_update_toast(&mut self, notice: &update_check::UpdateNotice, cx: &mut gpui::Context<Self>) {
        let mut actions = Vec::new();
        if let Some(ref download_url) = notice.download_url {
            actions.push(mod_helpers::ToastAction::StartUpdate {
                download_url: download_url.clone(),
                target_version: notice.latest_version.clone(),
                label: "Update now".to_string(),
            });
        }
        actions.push(mod_helpers::ToastAction::PostponeUpdate {
            version: notice.latest_version.clone(),
            postpone_seconds: UPDATE_POSTPONE_SECONDS,
            label: "Later".to_string(),
        });
        actions.push(mod_helpers::ToastAction::DismissUpdate {
            version: notice.latest_version.clone(),
            label: "Skip this version".to_string(),
        });
        actions.push(mod_helpers::ToastAction::OpenUrl {
            url: notice.releases_url.clone(),
            label: "Release notes".to_string(),
        });

        self.push_toast_with_actions(
            components::ToastKind::Warning,
            format!(
                "A newer GitComet version is available: {} (current {}).",
                notice.latest_version, notice.current_version
            ),
            actions,
            cx,
        );
    }

    fn should_show_update_badge(&self) -> bool {
        matches!(self.update_phase, UpdatePhase::Available)
            && self.available_update.as_ref().is_some_and(|notice| {
                !session::is_update_dismissed_for_version(&notice.latest_version)
            })
    }

    fn sync_update_ui(&mut self, cx: &mut gpui::Context<Self>) {
        let badge_visible = self.should_show_update_badge();
        self.title_bar.update(cx, |title_bar, cx| {
            title_bar.set_update_available(badge_visible, cx);
        });
        self.notify_settings_windows(cx);
    }

    pub(in crate::view) fn sync_update_status_line(&mut self) {
        let (line, _) = self.update_status_for_settings();
        self.update_status_line = line;
    }

    fn notify_settings_windows(&self, cx: &mut gpui::Context<Self>) {
        let status_line = self.update_status_line.clone();
        let ( _, update_available) = self.update_status_for_settings();
        let update_check_in_progress = matches!(self.update_phase, UpdatePhase::Checking);
        for window in cx.windows() {
            if let Some(settings) = window.downcast::<SettingsWindowView>() {
                let _ = settings.update(cx, |view, _window, cx| {
                    view.set_update_settings_state(
                        status_line.clone(),
                        update_available,
                        update_check_in_progress,
                        cx,
                    );
                });
            }
        }
    }

    pub(in crate::view) fn sync_settings_update_status(&mut self, cx: &mut gpui::Context<Self>) {
        self.sync_update_status_line();
        self.notify_settings_windows(cx);
    }
}
