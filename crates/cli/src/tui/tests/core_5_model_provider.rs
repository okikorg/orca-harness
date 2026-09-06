    #[test]
    fn stale_catalog_reply_is_ignored() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.picker_pending = Some((
            9,
            ModelPickerTarget::Models {
                filter: "current".into(),
            },
        ));
        handle_ui_msg(
            &mut app,
            UiMsg::Models {
                request_id: 8,
                result: Ok(catalog()),
            },
            &tx,
            80,
        );
        assert!(app.overlay.is_none());
        assert_eq!(
            app.picker_pending,
            Some((
                9,
                ModelPickerTarget::Models {
                    filter: "current".into()
                }
            ))
        );
    }

    #[test]
    fn effort_command_opens_active_models_effort_picker_and_updates_effort() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.cfg.model_name = "acme/fast-1".into();

        slash_command(&mut app, "effort", &tx, 80);
        let Ok(WorkerCmd::ListModels { request_id, .. }) = rx.try_recv() else {
            panic!("/effort should fetch the model catalog");
        };
        assert_eq!(
            app.picker_pending,
            Some((request_id, ModelPickerTarget::ActiveModelEffort))
        );

        let mut models = catalog();
        models[0].reasoning = Some(orca_harness_model_providers::ReasoningCapabilities {
            supported_efforts: Some(
                orca_harness_model_providers::SupportedEfforts::Listed(vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                ]),
            ),
            default_effort: Some("medium".into()),
        });
        handle_ui_msg(
            &mut app,
            UiMsg::Models {
                request_id,
                result: Ok(models),
            },
            &tx,
            80,
        );

        let Some(Overlay::Efforts(picker)) = &app.overlay else {
            panic!("/effort should open the existing effort picker");
        };
        assert_eq!(picker.model_id, "acme/fast-1");
        assert_eq!(picker.picker.index(), 1);

        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        match rx.try_recv() {
            Ok(WorkerCmd::SetModel {
                id,
                reasoning_effort,
            }) => {
                assert_eq!(id, "acme/fast-1");
                assert_eq!(reasoning_effort.as_deref(), Some("high"));
            }
            other => panic!("expected effort update, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn catalog_reply_opens_the_picker_seeded_with_the_command_filter() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.picker_pending = Some((
            7,
            ModelPickerTarget::Models {
                filter: "acme".into(),
            },
        ));
        let request_id = app.picker_pending.as_ref().unwrap().0;
        handle_ui_msg(
            &mut app,
            UiMsg::Models {
                request_id,
                result: Ok(catalog()),
            },
            &tx,
            80,
        );
        let Some(Overlay::Models(picker)) = &app.overlay else {
            panic!("expected the model picker to open");
        };
        assert_eq!(picker.filter, "acme");
        assert_eq!(picker.filtered().len(), 2);
    }

    #[test]
    fn model_picker_uses_the_shared_tabular_window() {
        let picker = ModelPicker::new(catalog(), String::new());
        let lines = crate::tui::render::model_picker_lines(&picker, PICKER_ROWS + 2, 80);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(text[0].contains("1/3"), "{}", text[0]);
        let fast = text.iter().find(|line| line.contains("acme/fast-1")).unwrap();
        let smart = text
            .iter()
            .find(|line| line.contains("acme/smart-9"))
            .unwrap();
        let fast_detail = fast[..fast.find("32k ctx").unwrap()].chars().count();
        let smart_detail = smart[..smart.find("32k ctx").unwrap()].chars().count();
        assert_eq!(fast_detail, smart_detail);
        assert!(fast.contains("▸ acme/fast-1"), "{fast}");
    }

    #[test]
    fn picker_filters_navigates_and_switches_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Models(ModelPicker::new(
            catalog(),
            String::new(),
        )));

        // Typing narrows to the two acme models; Down selects the second.
        for c in "acme".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);

        assert!(app.overlay.is_none(), "picker closes on selection");
        match rx.try_recv() {
            Ok(WorkerCmd::SetModel {
                id,
                reasoning_effort,
            }) => {
                assert_eq!(id, "acme/smart-9");
                assert!(reasoning_effort.is_none());
            }
            other => panic!("expected SetModel, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn model_with_reasoning_metadata_opens_effort_picker_at_provider_default() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        let mut models = catalog();
        models[0].reasoning = Some(
            orca_harness_model_providers::ReasoningCapabilities {
                supported_efforts: Some(
                    orca_harness_model_providers::SupportedEfforts::Listed(vec![
                    "high".into(),
                    "medium".into(),
                    "low".into(),
                    ]),
                ),
                default_effort: Some("medium".into()),
            },
        );
        app.overlay = Some(Overlay::Models(ModelPicker::new(models, String::new())));

        press(&mut app, &tx, KeyCode::Enter);

        let Some(Overlay::Efforts(picker)) = &app.overlay else {
            panic!("model selection should open its effort picker");
        };
        assert_eq!(picker.model_id, "acme/fast-1");
        assert_eq!(picker.picker.index(), 1, "provider default is preselected");
        assert!(rx.try_recv().is_err(), "model is not changed before effort selection");

        press(&mut app, &tx, KeyCode::Enter);

        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetModel {
                id,
                reasoning_effort,
            }) => {
                assert_eq!(id, "acme/fast-1");
                assert_eq!(reasoning_effort.as_deref(), Some("medium"));
            }
            other => panic!("expected SetModel with effort, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn model_changed_retains_effort_for_the_status_line() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();

        handle_ui_msg(
            &mut app,
            UiMsg::ModelChanged {
                id: "acme/reasoner".into(),
                reasoning_effort: Some("high".into()),
            },
            &tx,
            80,
        );

        assert_eq!(app.cfg.model_name, "acme/reasoner");
        assert_eq!(app.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn picker_escape_closes_without_switching() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Models(ModelPicker::new(
            catalog(),
            String::new(),
        )));
        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "no command sent on cancel");
    }

    #[test]
    fn provider_without_key_requirement_switches_directly() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::Providers {
            picker: ListPicker::new(Provider::ALL.len()),
        });
        // Navigate to local (needs no key), independent of provider additions.
        for _ in 1..Provider::ALL.len() {
            press(&mut app, &tx, KeyCode::Down);
        }
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetProvider { provider, api_key }) => {
                assert_eq!(provider, Provider::Local);
                assert!(api_key.is_none());
            }
            other => panic!("expected SetProvider, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn api_key_prompt_masks_input_and_submits_on_enter() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        app.overlay = Some(Overlay::ApiKey {
            provider: Provider::OpenRouter,
            input: String::new(),
        });

        // Empty enter is ignored — no accidental keyless switch.
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_some());
        assert!(rx.try_recv().is_err());

        for c in "sk-or-abc".chars() {
            press(&mut app, &tx, KeyCode::Char(c));
        }
        // The rendered prompt shows bullets, never the key itself.
        let lines = flat_lines(&live_lines(&app, 80));
        assert!(!lines.contains("sk-or-abc"), "key must be masked: {lines}");
        assert!(lines.contains("•••••••••"));

        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(WorkerCmd::SetProvider { provider, api_key }) => {
                assert_eq!(provider, Provider::OpenRouter);
                assert_eq!(api_key.as_deref(), Some("sk-or-abc"));
            }
            other => panic!("expected SetProvider, got {:?}", other.is_ok()),
        }
        // The key survives to the next session via the config file.
        assert_eq!(
            crate::config::stored_key("openrouter").as_deref(),
            Some("sk-or-abc")
        );
    }

    #[test]
    fn slash_provider_opens_the_provider_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "provider", &tx, 80);
        match &app.overlay {
            Some(Overlay::Providers { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the provider overlay"),
        }
    }

    #[test]
    fn settings_menu_drills_into_the_matching_pickers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = test_app();
        slash_command(&mut app, "settings", &tx, 80);
        match &app.overlay {
            Some(Overlay::Settings { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the settings overlay"),
        }

        // Provider row: opens the provider picker preselected on the
        // active provider (local is the final built-in provider).
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Providers { picker }) => {
                assert_eq!(picker.index(), Provider::ALL.len() - 1)
            }
            _ => panic!("expected the provider overlay"),
        }

        // Model row: kicks off the same fetch-then-pick flow as /models.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 1),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ListModels { .. })));
        assert_eq!(
            app.picker_pending
                .as_ref()
                .map(|(_, target)| match target {
                    ModelPickerTarget::Models { filter } => filter.as_str(),
                    ModelPickerTarget::ActiveModelEffort => "effort",
                    ModelPickerTarget::Subagent { .. } => "subagent",
                }),
            Some("")
        );
        press(&mut app, &tx, KeyCode::Left);
        assert!(matches!(app.overlay, Some(Overlay::Settings { .. })));
        assert!(app.picker_pending.is_none());

        // Model fetch can also be cancelled as a whole with escape.
        press(&mut app, &tx, KeyCode::Enter);
        assert!(matches!(rx.try_recv(), Ok(WorkerCmd::ListModels { .. })));
        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert!(app.overlay_stack.is_empty());
        assert!(app.picker_pending.is_none());

        // Theme row: right opens the theme picker; left returns with the
        // settings cursor preserved, then right opens it again.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 2),
        });
        press(&mut app, &tx, KeyCode::Right);
        assert!(matches!(app.overlay, Some(Overlay::Themes { .. })));
        press(&mut app, &tx, KeyCode::Left);
        match &app.overlay {
            Some(Overlay::Settings { picker }) => assert_eq!(picker.index(), 2),
            _ => panic!("expected settings parent"),
        }
        press(&mut app, &tx, KeyCode::Right);
        assert!(matches!(app.overlay, Some(Overlay::Themes { .. })));
        press(&mut app, &tx, KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert!(app.overlay_stack.is_empty());

        // View row opens a picker preselected on the current layout.
        app.view_mode = ViewMode::Classic;
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 3),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Views { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the view overlay"),
        }

        // Down and enter selects Split using the same pattern as theme/provider.
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(app.view_mode == ViewMode::Split);
        assert_eq!(crate::config::stored_view().as_deref(), Some("split"));

        // Inspector row opens a picker and persists the selected default.
        app.inspector_mode = InspectorMode::Summary;
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 4),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Inspector { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the inspector overlay"),
        }
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert!(matches!(app.inspector_mode, InspectorMode::Debug));
        assert_eq!(crate::config::stored_inspector().as_deref(), Some("debug"));

        // Api key row on a keyless provider closes with an explanation.
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 5),
        });
        press(&mut app, &tx, KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(rx.try_recv().is_err(), "no command for a keyless provider");

        // Transcript spacing is a persisted picker and applies immediately.
        // The preference is process-global and seeded by every `App::new`,
        // so hold the guard while we transition it live.
        let _spacing = PREFERENCE_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        set_transcript_spacing(TranscriptSpacing::Comfortable);
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 7),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::TranscriptSpacing { picker }) => assert_eq!(picker.index(), 1),
            _ => panic!("expected transcript spacing overlay"),
        }
        press(&mut app, &tx, KeyCode::Up);
        press(&mut app, &tx, KeyCode::Enter);
        assert_eq!(transcript_spacing(), TranscriptSpacing::Compact);
        assert_eq!(
            crate::config::stored_transcript_spacing().as_deref(),
            Some("compact")
        );
        set_transcript_spacing(TranscriptSpacing::Comfortable);
        let _ = crate::config::save_transcript_spacing("comfortable");

        // The style row is the last one; it flips the glyph table live.
        set_ui_style(UiStyle::Minimal);
        app.overlay = Some(Overlay::Settings {
            picker: ListPicker::with_selected(SETTINGS_ROWS, 8),
        });
        press(&mut app, &tx, KeyCode::Enter);
        match &app.overlay {
            Some(Overlay::Style { picker }) => assert_eq!(picker.index(), 0),
            _ => panic!("expected the style overlay"),
        }
        press(&mut app, &tx, KeyCode::Down);
        press(&mut app, &tx, KeyCode::Enter);
        assert_eq!(ui_style(), UiStyle::Glyph);
        assert_eq!(crate::config::stored_style().as_deref(), Some("glyph"));
        assert_eq!(UiStyle::stored(), UiStyle::Glyph);
        set_ui_style(UiStyle::Minimal);
        let _ = crate::config::save_style("minimal");
    }

    #[test]
    fn subagent_model_picker_selects_across_providers_without_switching_parent() {
        use crate::tui::state::SubagentSetting;
        for setting in [
            SubagentSetting::LocalModel,
            SubagentSetting::FlashModel,
            SubagentSetting::MidModel,
            SubagentSetting::FrontierModel,
        ] {
            for provider in Provider::ALL {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let mut app = test_app();
                let parent = app.cfg.model_name.clone();
                let parent_window = app.context_window;
                let values = crate::tui::subagents::subagent_values(&app.cfg.subagent_depth, setting);
                let index = values.iter().position(|p| p == provider.label()).unwrap();
                app.overlay = Some(Overlay::SubagentValues {
                    setting,
                    picker: ListPicker::with_selected(values.len(), index),
                    values,
                });
                handle_overlay_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &tx,
                );
                let WorkerCmd::ListSubagentModels {
                    request_id,
                    provider: requested,
                } = rx.try_recv().unwrap()
                else {
                    panic!("expected subagent catalog request");
                };
                assert_eq!(requested, provider);
                handle_ui_msg(
                    &mut app,
                    UiMsg::Models {
                        request_id,
                        result: Ok(catalog()),
                    },
                    &tx,
                    100,
                );
                let mut terminal = ratatui::Terminal::new(
                    ratatui::backend::TestBackend::new(100, 12)).unwrap();
                terminal.draw(|frame| frame.render_widget(
                    ratatui::widgets::Paragraph::new(ratatui::text::Text::from(live_lines(&app, 100))),
                    frame.area())).unwrap();
                let buffer = terminal.backend().buffer();
                let rendered = (0..12).map(|y| (0..100)
                    .map(|x| buffer[(x, y)].symbol()).collect::<String>())
                    .collect::<Vec<_>>().join("\n");
                assert!(rendered.contains(provider.label()), "{rendered}");
                handle_overlay_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &tx,
                );
                let WorkerCmd::SetSubagentModel {
                    tier,
                    provider: selected,
                    model,
                } = rx.try_recv().unwrap()
                else {
                    panic!("expected tier assignment, not parent switch");
                };
                assert_eq!(tier, crate::tui::subagents::tier(setting).unwrap());
                assert_eq!(selected, provider);
                assert_eq!(model, catalog()[0].id);
                assert_eq!(app.cfg.model_name, parent);
                assert_eq!(app.context_window, parent_window);
            }
        }
    }
