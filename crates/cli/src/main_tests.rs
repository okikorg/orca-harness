#[cfg(test)]
mod main_tests {
    use std::path::PathBuf;

    use tokio::sync::mpsc;

    use orca_harness_core::{Context, Message, ToolResult};
    use orca_harness_extensions::MEMORY_GUIDANCE;
    use orca_harness_tools::{SubagentSpawn, Workspace};

    use crate::instructions;
    use crate::mode::{Mode, ModeHandle};
    use crate::msg::Provider;
    use crate::plan::PlanArea;
    use crate::runtime::{context_from, rewind_cut, subagent_extensions};
    use crate::{resolve_theme, select_provider, system_prompt, Config, Planning};

    #[test]
    fn theme_prefers_explicit_then_stored_then_default() {
        assert_eq!(resolve_theme(None), "default");
        assert_eq!(resolve_theme(Some("mono".to_string())), "mono");
        crate::config::save_theme("nord").unwrap();
        assert_eq!(resolve_theme(None), "nord");
        assert_eq!(resolve_theme(Some("mono".to_string())), "mono");
    }

    /// The prompt must keep telling the model to plan and batch independent
    /// calls — dropping this silently reverts the agent to one call per turn.
    #[test]
    fn system_prompt_instructs_concurrent_batching() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("execute concurrently"));
        assert!(prompt.contains("plan the batch"));
        assert!(prompt.contains("one response"));
    }

    #[test]
    fn system_prompt_requires_bounded_subagent_delegation() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("Delegate to subagents deliberately"));
        assert!(prompt.contains("exact result or deliverable expected"));
        assert!(prompt.contains("explicit stopping condition"));
        assert!(prompt.contains("Avoid open-ended delegation"));
    }

    /// The advertised tool list must match what build_agent registers:
    /// web_fetch is always on, search/crawl only with a Firecrawl key.
    #[test]
    fn system_prompt_advertises_web_tools_to_match_registration() {
        let ws = Workspace::new(PathBuf::from("."));
        let without = system_prompt(&ws, false);
        assert!(without.contains("web_fetch"));
        assert!(!without.contains("web_search"));
        let with = system_prompt(&ws, true);
        assert!(with.contains("web_search"));
        assert!(with.contains("web_crawl"));
    }

    /// `skill` is the one registered tool the prompt must *not* name.
    /// Whether it exists depends on the skill folders and on /skills
    /// toggles, both of which move mid-session, while this string is
    /// built once and re-pushed verbatim by /clear — so a mention here
    /// would eventually advertise a tool that is not registered. The
    /// tool's own description carries the explanation and the catalog.
    #[test]
    fn system_prompt_leaves_skills_to_the_tool_description() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(!prompt.contains("skill"), "{prompt}");
    }

    #[test]
    fn system_prompt_advertises_persistent_compute_and_subagent() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("pykernel"));
        assert!(prompt.contains("bun_repl"));
        assert!(!prompt.contains("processes), kernel (persistent Python"));
        assert!(prompt.contains("subagent"));
    }

    /// The prompt must keep telling the model to plan ahead of each call and
    /// to keep intermediate state in the persistent compute tool that fits.
    #[test]
    fn system_prompt_instructs_planning_and_persistent_compute_state() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("Plan before every tool call"));
        assert!(prompt.contains("pykernel for persistent Python state"));
        assert!(prompt.contains("bun_repl for persistent JavaScript"));
        assert!(prompt.contains("fanning out"));
    }

    /// The task list is a registered tool, so the prompt names it and
    /// says what keeping it current means.
    #[test]
    fn system_prompt_advertises_the_task_list() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("todo_write"));
        assert!(prompt.contains("Do not use todo_write by default"));
        assert!(prompt.contains("explicitly asks for a todo plan"));
        assert!(prompt.contains("5+ distinct steps"));
        assert!(prompt.contains("single-step or few-step work"));
        assert!(prompt.contains("in_progress"));
    }

    #[test]
    fn system_prompt_advertises_scoped_memory_tools() {
        let prompt = system_prompt(&Workspace::new(PathBuf::from(".")), false);
        assert!(prompt.contains("memory_search"));
        assert!(prompt.contains("global and current-workspace memory"));
        assert!(prompt.contains("memory_manage"));
        assert!(prompt.contains("save, update, or forget durable memory"));
        assert!(prompt.contains(MEMORY_GUIDANCE));
        assert!(prompt.contains("Memory recall is automatic"));
        assert!(prompt.contains("Do not call memory_search merely to repeat"));
        assert!(prompt.contains("or when you identify stable, directly stated user information"));
        assert!(prompt.contains("likely to help in future sessions"));
        assert!(prompt.contains("do not store ordinary conversation, one-off task details"));
        assert!(prompt.contains("duplicates of recalled memory"));
        assert!(prompt.contains("workspace scope (is_global=false)"));
        assert!(prompt.contains("Choose global scope (is_global=true)"));
        assert!(prompt.contains("When approval is enabled"));
        assert!(prompt.contains("approval gate is the user's final decision"));
        assert!(!prompt.contains("only when the user explicitly asks"));
    }

    /// `system_prompt` is a fixed string with tests asserting what is and
    /// is not in it; the user's instructions are appended by the caller,
    /// never folded in, so no AGENTS.md can change what it returns.
    #[test]
    fn instructions_append_to_the_prompt_without_entering_it() {
        let ws = Workspace::new(PathBuf::from("."));
        let base = system_prompt(&ws, false);
        let instructions = instructions::Instructions {
            sources: vec![instructions::Source {
                label: "AGENTS.md".into(),
                body: "never mention skill".into(),
                bytes: 20,
                truncated: false,
            }],
        };
        let mut composed = system_prompt(&ws, false);
        composed.push_str(&instructions.block().unwrap());

        assert!(composed.starts_with(&base), "the base prompt is unchanged");
        assert!(composed.contains("never mention skill"));
        // ...and the function itself still returns the base prompt only.
        assert_eq!(system_prompt(&ws, false), base);
    }

    fn user(text: &str) -> Message {
        Message::User {
            content: text.into(),
            images: Vec::new(),
        }
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls: Vec::new(),
        }
    }

    /// A three-turn conversation: system, then user/assistant pairs.
    fn conversation() -> Vec<Message> {
        vec![
            Message::System {
                content: "sys".into(),
            },
            user("one"),
            assistant("a1"),
            user("two"),
            assistant("a2"),
            user("three"),
            assistant("a3"),
        ]
    }

    #[test]
    fn rewind_cuts_on_a_user_boundary() {
        let messages = conversation();
        // One turn: cut at the last user message, dropping it and what
        // followed it.
        assert_eq!(rewind_cut(&messages, 1), Some((5, 1)));
        assert_eq!(rewind_cut(&messages, 2), Some((3, 2)));
        // Every cut lands on a user message, never mid-turn.
        for turns in 1..=3 {
            let (cut, _) = rewind_cut(&messages, turns).unwrap();
            assert!(matches!(messages[cut], Message::User { .. }), "{turns}");
        }
    }

    #[test]
    fn rewinding_past_the_start_rewinds_everything_it_can() {
        let messages = conversation();
        // Three turns exist; asking for ten keeps the system prompt.
        assert_eq!(rewind_cut(&messages, 10), Some((1, 3)));
        let (cut, dropped) = rewind_cut(&messages, 10).unwrap();
        assert_eq!(dropped, 3);
        let kept = context_from(&messages[..cut]);
        assert_eq!(kept.messages().len(), 1);
        assert!(matches!(kept.messages()[0], Message::System { .. }));
    }

    #[test]
    fn rewinding_a_conversation_with_no_turns_is_a_no_op() {
        let only_system = vec![Message::System {
            content: "sys".into(),
        }];
        assert_eq!(rewind_cut(&only_system, 1), None);
        assert_eq!(rewind_cut(&[], 1), None);
    }

    /// The remaining transcript must never end in tool calls with no
    /// results — a chat-completions endpoint rejects that shape.
    #[test]
    fn rewind_never_leaves_dangling_tool_calls() {
        let messages = vec![
            Message::System {
                content: "sys".into(),
            },
            user("one"),
            Message::Assistant {
                content: None,
                tool_calls: vec![orca_harness_core::ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({}),
                }],
            },
            Message::Tool {
                results: vec![ToolResult {
                    call_id: "c1".into(),
                    tool_name: "shell".into(),
                    output: serde_json::json!("ok"),
                    is_error: false,
                }],
            },
            user("two"),
        ];
        let (cut, _) = rewind_cut(&messages, 1).unwrap();
        let kept = context_from(&messages[..cut]);
        // The call and its result are both kept, or both dropped.
        if let Some(Message::Assistant { tool_calls, .. }) = kept.messages().last() {
            assert!(tool_calls.is_empty());
        }
        assert_eq!(kept.messages().len(), 4);
    }

    #[test]
    fn context_from_copies_the_messages_it_is_given() {
        let messages = conversation();
        let rebuilt = context_from(&messages[..3]);
        assert_eq!(rebuilt.messages().len(), 3);
        assert_eq!(
            serde_json::to_string(rebuilt.messages()).unwrap(),
            serde_json::to_string(&messages[..3]).unwrap()
        );
    }

    /// Plan mode must reach every level of the agent tree. Spawning is
    /// denied in plan mode, so the case this covers is the reachable
    /// one: a subagent already running when the user flips `/mode`.
    #[tokio::test]
    async fn spawned_subagents_inherit_the_plan_gate() {
        use orca_harness_core::{ToolCall, ToolDecision};

        let (ui, _rx) = mpsc::unbounded_channel();
        let mode = ModeHandle::new(Mode::Normal);
        let spawn = SubagentSpawn {
            id: 1,
            parent_id: None,
            depth: 0,
            call_id: "c1".into(),
            task: "do the thing".into(),
            identity: Some(orca_harness_tools::SubagentIdentity::new(
                "openrouter",
                "anthropic/claude-sonnet-5",
            )),
        };
        let settings = orca_harness_tools::SubagentDepth::default();
        let extensions = subagent_extensions(&spawn, &ui, &mode, &PlanArea::new(), &settings);
        let names: Vec<&str> = extensions.iter().map(|ext| ext.name()).collect();
        assert!(names.contains(&"plan-mode"), "{names:?}");
        assert!(names.contains(&"truncation"), "{names:?}");

        let gate = extensions
            .iter()
            .find(|ext| ext.name() == "plan-mode")
            .expect("a plan gate");
        let call = ToolCall {
            id: "c2".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({}),
        };
        // The inner gate shares the session's handle, so flipping the
        // mode reaches an already-spawned agent.
        assert!(matches!(
            gate.before_tool(&call).await.unwrap(),
            ToolDecision::Continue
        ));
        mode.set(Mode::Plan);
        assert!(matches!(
            gate.before_tool(&call).await.unwrap(),
            ToolDecision::Deny { .. }
        ));
    }

    fn planning(mode: Mode) -> Planning {
        Planning {
            mode: ModeHandle::new(mode),
            area: PlanArea::new(),
        }
    }

    /// A planning episode briefs the model once and names no file: what
    /// to write, and whether to write anything, is the agent's call.
    #[test]
    fn a_planning_episode_briefs_once_and_names_no_file() {
        let planning = planning(Mode::Plan);
        let mut context = Context::new();
        context.push_system("base prompt");

        assert!(planning.open_episode(&mut context), "the first turn briefs");
        let briefing = match context.messages().last() {
            Some(Message::System { content }) => content.clone(),
            other => panic!("expected a system briefing, got {other:?}"),
        };
        assert!(briefing.contains("docs/plan/"), "{briefing}");
        assert!(briefing.contains("You choose the name"), "{briefing}");
        assert!(briefing.contains("Decide for yourself"), "{briefing}");
        // Nothing has been written, so nothing is claimed.
        assert!(planning.area.written().is_empty());

        // Later turns in the same episode add nothing.
        let before = context.messages().len();
        assert!(!planning.open_episode(&mut context));
        assert_eq!(context.messages().len(), before);
    }

    /// Normal mode never briefs, so an ordinary session's context is
    /// untouched by any of this.
    #[test]
    fn normal_mode_opens_no_episode() {
        let planning = planning(Mode::Normal);
        let mut context = Context::new();
        context.push_system("base prompt");
        assert!(!planning.open_episode(&mut context));
        assert_eq!(context.messages().len(), 1, "no briefing pushed");
    }

    /// Ending an episode and planning again briefs afresh — the model in
    /// the second episode has to be told the rules too.
    #[test]
    fn a_second_episode_briefs_again() {
        let planning = planning(Mode::Plan);
        let mut context = Context::new();
        assert!(planning.open_episode(&mut context));
        planning.area.record("docs/plan/2026-08-22-first.md");

        let written = planning.area.end();
        assert_eq!(written, ["docs/plan/2026-08-22-first.md".to_string()]);
        assert!(planning.open_episode(&mut context));
        assert!(planning.area.written().is_empty(), "a fresh list");
    }

    /// `--plan` starts a session read-only; `--yolo` starts it with
    /// approvals off. When both are given, plan wins: read-only and
    /// unprompted is coherent, writable and unprompted by accident is
    /// not.
    #[test]
    fn plan_and_yolo_flags_pick_the_mode() {
        let base = Config {
            provider: Provider::Local,
            model: "m".into(),
            base_url: "u".into(),
            api_key: None,
            firecrawl_key: None,
            openrouter: false,
            list_models: false,
            workspace: PathBuf::from("."),
            prompt: None,
            json: false,
            auto_approve: false,
            max_steps: 4,
            subagent_depth: 1,
            continue_latest: false,
            resume_id: None,
            no_session: true,
            theme: "default".into(),
            plan: false,
            yolo: false,
        };
        assert_eq!(base.mode(), Mode::Normal);
        assert_eq!(
            Config {
                plan: true,
                ..base.clone()
            }
            .mode(),
            Mode::Plan
        );
        assert_eq!(
            Config {
                yolo: true,
                ..base.clone()
            }
            .mode(),
            Mode::Yolo
        );
        // Plan outranks yolo when both are asked for.
        assert_eq!(
            Config {
                plan: true,
                yolo: true,
                ..base
            }
            .mode(),
            Mode::Plan
        );
    }

    #[test]
    fn provider_precedence_is_explicit_not_inferred_from_url_text() {
        assert_eq!(
            select_provider(true, true, Some(Provider::OpenAiCodex)),
            Provider::OpenRouter
        );
        assert_eq!(
            select_provider(false, true, Some(Provider::OpenAiCodex)),
            Provider::Local
        );
        assert_eq!(
            select_provider(false, false, Some(Provider::OpenAiCodex)),
            Provider::OpenAiCodex
        );
        assert_eq!(select_provider(false, false, None), Provider::OpenAi);
    }
}
