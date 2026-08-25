use super::harness::{handle_harness_event, handle_subagent_event};

use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

use crate::msg::{UiMsg, WorkerCmd};
use crate::tui::components::transcript::BlockSpacing;
use crate::view::theme;

use super::super::render::{replay_transcript, reset_conversation_ui};
use super::super::state::{App, ModelPicker, Overlay, RunState};
use super::super::{push_notice, push_wrapped_lines, start_next_queued_prompt};

pub(crate) fn handle_ui_msg(
    app: &mut App,
    msg: UiMsg,
    worker: &mpsc::UnboundedSender<WorkerCmd>,
    width: usize,
) {
    match msg {
        UiMsg::Event(event) => handle_harness_event(app, event, width),
        UiMsg::SubagentEvent {
            id,
            parent_id,
            depth,
            call_id,
            event,
        } => handle_subagent_event(app, id, parent_id, depth, call_id, event),
        UiMsg::Approval(request) => app.approval = Some(request),
        UiMsg::Ask(request) => app.ask = Some(crate::tui::components::ask::AskForm::new(request)),
        UiMsg::Models(result) => {
            let t = theme();
            let seed = app.picker_pending.take().unwrap_or_default();
            // Learn the active model's window from the catalog in passing.
            if let Ok(models) = &result {
                if let Some(info) = models.iter().find(|m| m.id == app.cfg.model_name) {
                    if info.context_length.is_some() {
                        app.context_window = info.context_length;
                    }
                }
            }
            match result {
                Ok(models) if models.is_empty() => {
                    app.push_line(Line::from(Span::styled("no models available", t.dim)));
                }
                Ok(models) => {
                    app.overlay = Some(Overlay::Models(ModelPicker::new(models, seed)));
                }
                Err(err) => app.push_line(Line::from(Span::styled(
                    format!("model list failed: {err}"),
                    t.error,
                ))),
            }
        }
        UiMsg::ModelChanged(id) => {
            app.cfg.model_name = id.clone();
            app.push_line(Line::from(Span::styled(
                format!("model: {id}"),
                theme().dim,
            )));
        }
        UiMsg::ContextWindow(window) => {
            // Best-effort discovery: never wipe a window the model picker
            // already stashed with a probe that found nothing.
            if window.is_some() {
                app.context_window = window;
            }
        }
        UiMsg::Compacted(result) => match result {
            Ok(report) => {
                // Until the next model step reports real usage, the
                // report's estimate is the best context figure we have.
                app.context_tokens = report.est_tokens_after as u64;
                let pct =
                    100.0 * report.est_tokens_after as f64 / report.est_tokens_before.max(1) as f64;
                app.push_line(Line::from(Span::styled(
                    format!(
                        "compacted: {} -> {} messages · ~{} -> ~{} est tokens ({pct:.1}%)",
                        report.messages_before,
                        report.messages_after,
                        report.est_tokens_before,
                        report.est_tokens_after,
                    ),
                    theme().dim,
                )));
                if report.elided_results > 0 {
                    app.push_line(Line::from(Span::styled(
                        format!(
                            "{} tool outputs ({} KB) elided to store, recoverable via read_tool_result",
                            report.elided_results,
                            report.elided_bytes / 1024,
                        ),
                        theme().dim,
                    )));
                }
            }
            Err(err) => {
                app.push_line(Line::from(Span::styled(
                    format!("compact: {err}"),
                    theme().dim,
                )));
            }
        },
        UiMsg::Notice(text) => {
            push_notice(app, text);
        }
        UiMsg::SessionCleared { id } => {
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("session {id} cleared · background work stopped"),
            );
        }
        UiMsg::ContextRewound { messages, notice } => {
            // The transcript is redrawn from the shortened context, but
            // the token totals are not conversation state — they record
            // what this session actually spent, and rewinding does not
            // un-spend it. Occupancy is left to the next model step.
            let spent = (
                app.tokens_in,
                app.tokens_out,
                app.cache_read_total,
                app.cache_write_total,
                app.usage_steps,
            );
            reset_conversation_ui(app);
            (
                app.tokens_in,
                app.tokens_out,
                app.cache_read_total,
                app.cache_write_total,
                app.usage_steps,
            ) = spent;
            push_notice(app, notice);
            replay_transcript(app, &messages, width);
        }
        UiMsg::SessionForked { id, parent } => {
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("forked to session {id} · {parent} is left as it was"),
            );
        }
        UiMsg::SessionLoaded { id, messages } => {
            reset_conversation_ui(app);
            app.cfg.session_id = Some(id.clone());
            push_notice(
                app,
                format!("resumed session {id} ({} messages)", messages.len()),
            );
            replay_transcript(app, &messages, width);
        }
        UiMsg::ProviderChanged { provider, model } => {
            app.cfg.provider = provider;
            app.cfg.model_name = model.clone();
            app.context_window = None;
            app.push_line(Line::from(Span::styled(
                format!("provider: {} · model: {model}", provider.label()),
                theme().dim,
            )));
        }
        UiMsg::RunDone(result) => {
            let completed = result.is_ok();
            let turn_elapsed = match &app.run {
                RunState::Running { started, .. } => Some(started.elapsed()),
                RunState::Idle => None,
            };
            // Flush any partial stream (interrupted mid-generation).
            if result.is_err() {
                app.commit_activity(width);
                let partial = if !app.text.trim().is_empty() {
                    Some(std::mem::take(&mut app.text))
                } else {
                    app.pending_assistant.take()
                };
                if let Some(partial) = partial {
                    app.push_markdown_block(&partial, width, BlockSpacing::Section);
                    // Interrupted output is often exactly what the user
                    // wanted to keep — that is why they interrupted.
                    app.last_answer = Some(partial);
                }
            }
            app.text.clear();
            app.run = RunState::Idle;
            app.approval = None;
            app.ask = None;
            if let Err(err) = result {
                let (style, label) = if err.to_lowercase().contains("cancel") {
                    (theme().dim, "interrupted".to_string())
                } else {
                    (theme().error, format!("run failed: {err}"))
                };
                let mut lines = Vec::new();
                push_wrapped_lines(&mut lines, &label, "  ", style, width);
                app.push_transcript_block(lines, BlockSpacing::Tight);
            }
            if completed {
                if let Some(elapsed) = turn_elapsed {
                    let calls = app.turn_tool_calls;
                    let plural = if calls == 1 { "" } else { "s" };
                    app.last_turn_summary = Some(format!(
                        "Turn took {:.1}s and took {calls} tool call{plural}",
                        elapsed.as_secs_f64(),
                    ));
                }
                start_next_queued_prompt(app, worker, width);
            }
        }
        UiMsg::ShellDone => {
            app.commit_activity(width);
            let elapsed = match &app.run {
                RunState::Running { started, .. } => Some(started.elapsed()),
                RunState::Idle => None,
            };
            app.run = RunState::Idle;
            app.approval = None;
            app.ask = None;
            if let Some(elapsed) = elapsed {
                app.last_turn_summary =
                    Some(format!("Shell command took {:.1}s", elapsed.as_secs_f64()));
            }
            start_next_queued_prompt(app, worker, width);
        }
    }
}
