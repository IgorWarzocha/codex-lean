//! Default settings reuse selection pickers without changing live thread state.
//! Discovery identities protect cancelled and reopened menus from stale replies.

use super::*;

impl ChatWidget {
    pub(crate) fn open_settings(&mut self) {
        use codex_config::cli_settings::CliSetting;
        self.cli_setting_request_id = None;
        let mut items: Vec<_> = [
            CliSetting::Context,
            CliSetting::Retention,
            CliSetting::IdleRollover,
            CliSetting::Runtime,
            CliSetting::QuestionsOutsidePlan,
            CliSetting::MultiAgent,
            CliSetting::WaitAgent,
        ]
        .into_iter()
        .map(|setting| SelectionItem {
            name: setting.title().into(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenCliSetting(setting))
            })],
            dismiss_on_select: true,
            ..Default::default()
        })
        .collect();
        items.extend([
            SelectionItem {
                name: "Experimental features".into(),
                actions: vec![Box::new(|tx| tx.send(AppEvent::OpenExperimentalSettings))],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Voice settings".into(),
                actions: vec![Box::new(|tx| tx.send(AppEvent::OpenRealtimeSettings))],
                dismiss_on_select: true,
                ..Default::default()
            },
        ]);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Settings".into()),
            subtitle: Some(
                "Saved defaults, not live thread changes. Before startup: codex settings".into(),
            ),
            items,
            footer_hint: Some(standard_popup_hint_line()),
            ..SelectionViewParams::picker()
        });
    }

    fn cli_setting_params(
        &self,
        setting: codex_config::cli_settings::CliSetting,
        configured: String,
        cwd: String,
    ) -> SelectionViewParams {
        let items = setting
            .choices()
            .iter()
            .map(|choice| {
                let choice = choice.to_string();
                let cwd = cwd.clone();
                SelectionItem {
                    name: choice.clone(),
                    is_current: choice == configured,
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::PersistCliSetting {
                            setting,
                            choice: choice.clone(),
                            cwd: cwd.clone(),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        SelectionViewParams {
            title: Some(setting.title().into()),
            subtitle: Some(format!(
                "Configured for this project: {configured}. {}",
                setting.description()
            )),
            items,
            on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenSettings))),
            footer_hint: Some(standard_popup_hint_line()),
            ..SelectionViewParams::picker()
        }
    }

    pub(crate) fn open_cli_setting_loading(
        &mut self,
        setting: codex_config::cli_settings::CliSetting,
    ) -> uuid::Uuid {
        let request_id = uuid::Uuid::new_v4();
        self.cli_setting_request_id = Some(request_id);
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some("cli-setting-discovery"),
            title: Some(setting.title().into()),
            subtitle: Some(setting.description().into()),
            items: vec![SelectionItem {
                name: "Loading configured defaults…".into(),
                disabled_reason: Some("Waiting for the server".into()),
                ..Default::default()
            }],
            on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenSettings))),
            footer_hint: Some(standard_popup_hint_line()),
            ..SelectionViewParams::picker()
        });
        request_id
    }

    pub(crate) fn on_cli_setting_discovered(
        &mut self,
        request_id: uuid::Uuid,
        setting: codex_config::cli_settings::CliSetting,
        cwd: String,
        result: Result<String, String>,
    ) {
        if self.cli_setting_request_id != Some(request_id) {
            return;
        }
        self.cli_setting_request_id = None;
        let params = match result {
            Ok(configured) => self.cli_setting_params(setting, configured, cwd),
            Err(error) => SelectionViewParams {
                title: Some(setting.title().into()),
                subtitle: Some(format!(
                    "Could not read settings: {error}. Reopen /settings to retry."
                )),
                on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenSettings))),
                footer_hint: Some(standard_popup_hint_line()),
                ..SelectionViewParams::picker()
            },
        };
        // Cancellation, navigation and reopened pickers never resurrect stale discovery.
        self.bottom_pane
            .replace_selection_view_if_active("cli-setting-discovery", params);
    }

    pub(super) fn open_theme_picker(&mut self) {
        let codex_home = codex_utils_home_dir::find_codex_home().ok();
        let params = crate::theme_picker::build_theme_picker_params(
            self.local_settings.tui.theme.as_deref(),
            codex_home.as_deref(),
            self.last_rendered_width.get(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    pub(crate) fn open_experimental_popup(&mut self) {
        let Some(thread_id) = self.thread_id() else {
            self.add_info_message(
                "Experimental features are unavailable until startup completes.".to_string(),
                /*hint*/ None,
            );
            return;
        };
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.app_event_tx.send(AppEvent::FetchExperimentalFeatures {
            thread_id,
            response_tx,
        });
        let view = ExperimentalFeaturesView::new(
            Vec::new(),
            thread_id,
            Some(response_rx),
            self.app_event_tx.clone(),
            self.bottom_pane.list_keymap(),
        );
        self.bottom_pane.show_view(Box::new(view));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
    use codex_config::cli_settings::CliSetting;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test]
    async fn settings_selection_saves_defaults_without_mutating_thread_or_emitting_ops() {
        for (setting, configured, expected) in [
            (CliSetting::Context, "compaction", "notes"),
            (CliSetting::QuestionsOutsidePlan, "off", "on"),
        ] {
            let (mut chat, _sender, mut events, mut ops) =
                make_chatwidget_manual_with_sender().await;
            while events.try_recv().is_ok() {}
            let before = chat.config.context_strategy;
            let questions_before = chat
                .config
                .features
                .enabled(Feature::DefaultModeRequestUserInput);
            let request_id = chat.open_cli_setting_loading(setting);
            chat.on_cli_setting_discovered(
                request_id,
                setting,
                "/selected/project".into(),
                Ok(configured.into()),
            );
            chat.bottom_pane
                .handle_key_event(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
            chat.bottom_pane
                .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            let event = events.try_recv().unwrap();
            assert!(
                matches!(event, AppEvent::PersistCliSetting { setting: selected, choice, cwd } if selected == setting && choice == expected && cwd == "/selected/project")
            );
            assert_eq!(chat.config.context_strategy, before);
            assert_eq!(
                chat.config
                    .features
                    .enabled(Feature::DefaultModeRequestUserInput),
                questions_before
            );
            assert!(ops.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn settings_discovery_does_not_resurrect_cancelled_or_reopened_pickers() {
        use crate::chatwidget::tests::render_bottom_popup;
        let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
        let old = chat.open_cli_setting_loading(CliSetting::Context);
        chat.bottom_pane
            .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        while events.try_recv().is_ok() {}
        chat.on_cli_setting_discovered(old, CliSetting::Context, "/old".into(), Ok("notes".into()));
        assert!(!chat.has_active_view());
        let old = chat.open_cli_setting_loading(CliSetting::Context);
        chat.open_settings();
        let current = chat.open_cli_setting_loading(CliSetting::Runtime);
        chat.on_cli_setting_discovered(old, CliSetting::Context, "/old".into(), Ok("notes".into()));
        assert!(render_bottom_popup(&chat, 80).contains("Loading configured defaults"));
        chat.on_cli_setting_discovered(
            current,
            CliSetting::Runtime,
            "/new".into(),
            Ok("v8".into()),
        );
        let popup = render_bottom_popup(&chat, 80);
        assert!(popup.contains("Configured for this project: v8"));
        assert!(!popup.contains("Context management"));
    }
}
