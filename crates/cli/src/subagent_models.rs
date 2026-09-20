//! User-selected worker models; no provider-specific model shortlist.
use crate::{Endpoint, Provider};
use orca_harness_core::Model;
use orca_harness_tools::SubagentModel;
use std::sync::Arc;

pub(crate) fn provider_endpoint(endpoint: &Endpoint, provider: Provider) -> Endpoint {
    // An explicit budget is safe to reuse for another model behind the same
    // provider adapter. Across providers, accepted effort names and request
    // semantics can differ, so retain the previous fail-safe of leaving them
    // unset. Codex is the known exception within one adapter: its request API
    // does not support an output-token cap (also enforced by CLI validation).
    let same_provider = provider == endpoint.provider;
    Endpoint {
        provider,
        base_url: if same_provider {
            endpoint.base_url.clone()
        } else {
            provider.base_url().into()
        },
        api_key: if same_provider {
            endpoint.api_key.clone()
        } else {
            provider.resolve_key()
        },
        reasoning_effort: if same_provider {
            endpoint.reasoning_effort.clone()
        } else {
            None
        },
        max_output_tokens: if same_provider && provider != Provider::OpenAiCodex {
            endpoint.max_output_tokens
        } else {
            None
        },
        request_session_id: Some(orca_harness_extensions::new_session_id()),
        model_retries: Arc::default(),
        ..endpoint.clone()
    }
}

/// Existing workers own a catalog snapshot, so do not replace its shared routes mid-tree.
pub(crate) fn save_assignment(
    endpoint: &Endpoint,
    manager: &orca_harness_tools::SubagentManager,
    tier: &str,
    provider: Provider,
    model: String,
) -> Result<(), String> {
    if !manager.active().is_empty() {
        return Err("wait for running subagents to finish, then select the model again".into());
    }
    let candidate = provider_endpoint(endpoint, provider);
    crate::config::save_subagent_model(
        tier,
        crate::config::SubagentModelSelection {
            provider: provider.label().into(),
            model,
            base_url: candidate.base_url,
        },
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

pub(crate) fn choices(endpoint: &Endpoint) -> Vec<SubagentModel<Arc<dyn Model>>> {
    build(endpoint, None)
}

pub(crate) fn choices_with_ui(
    endpoint: &Endpoint,
    ui: tokio::sync::mpsc::UnboundedSender<crate::msg::UiMsg>,
) -> Vec<SubagentModel<Arc<dyn Model>>> {
    build(endpoint, Some(ui))
}

fn build(
    endpoint: &Endpoint,
    ui: Option<tokio::sync::mpsc::UnboundedSender<crate::msg::UiMsg>>,
) -> Vec<SubagentModel<Arc<dyn Model>>> {
    crate::config::stored_subagent_models()
        .into_iter()
        .filter_map(|(tier, selection)| {
            let provider = Provider::from_label(&selection.provider)?;
            let mut worker = provider_endpoint(endpoint, provider);
            worker.base_url = selection.base_url;
            worker.model = selection.model;
            let id = format!("{tier}/{}/{}", provider.label(), worker.model);
            let description = format!("user-selected {} / {}", provider.label(), worker.model);
            let label = format!("subagent {id} ({}/{})", provider.label(), worker.model);
            let model = worker.build_model_for_ui_with_label(ui.clone(), Some(label));
            Some(
                SubagentModel::new(id, description, model).identity(provider.label(), worker.model),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::{Context, ModelResponse};
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    async fn one_shot_completion() -> (String, oneshot::Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0_u8; 4096];
            let body = loop {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "connection closed before request arrived");
                raw.extend_from_slice(&buf[..read]);
                let Some(split) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                if raw.len() >= split + 4 + length {
                    break serde_json::from_slice(&raw[split + 4..split + 4 + length]).unwrap();
                }
            };
            tx.send(body).unwrap();
            let response = json!({
                "choices": [{"message": {"content": "done", "tool_calls": []}}]
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                response.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        (format!("http://{addr}/v1"), rx)
    }

    fn endpoint() -> Endpoint {
        Endpoint {
            provider: Provider::Local,
            base_url: "http://localhost:12345/v1".into(),
            api_key: None,
            model: "parent".into(),
            reasoning_effort: None,
            max_output_tokens: None,
            prompt_cache: false,
            request_session_id: None,
            model_retries: Arc::default(),
            model_gates: Default::default(),
            subagent_settings: Default::default(),
        }
    }

    #[test]
    fn subagent_catalog_uses_saved_provider_models_and_restores_fixed_route() {
        let endpoint = endpoint();
        for (tier, provider, model) in [
            ("local", "local", "my-installed-model:tag"),
            ("flash", "openai", "my-fast-model"),
            ("mid", "vercel", "vendor/my-model"),
            ("frontier", "openai-codex", "my-frontier-model"),
        ] {
            crate::config::save_subagent_model(
                tier,
                crate::config::SubagentModelSelection {
                    provider: provider.into(),
                    model: model.into(),
                    base_url: Provider::from_label(provider).unwrap().base_url().into(),
                },
            )
            .unwrap();
        }
        let choices = choices(&endpoint);
        assert_eq!(choices.len(), 4);
        for choice in &choices {
            let identity = choice.identity.as_ref().unwrap();
            assert!(choice
                .id
                .ends_with(&format!("{}/{}", identity.provider, identity.model)));
        }
        let settings = crate::subagent_settings::configured(1, None);
        assert!(settings.set_model_route(Some("mid".into())));
        crate::config::save_subagent_settings(&settings).unwrap();
        let reloaded = crate::subagent_settings::configured(1, None);
        assert_eq!(
            reloaded.default_model().as_deref(),
            Some("mid/vercel/vendor/my-model")
        );
        assert_eq!(crate::config::stored_subagent_models().len(), 4);
    }

    #[test]
    fn subagent_provider_selection_preserves_active_custom_endpoint() {
        let endpoint = endpoint();
        assert_eq!(
            provider_endpoint(&endpoint, Provider::Local).base_url,
            endpoint.base_url
        );
        let remote = provider_endpoint(&endpoint, Provider::OpenAi);
        assert_eq!(remote.base_url, Provider::OpenAi.base_url());
        assert_eq!(endpoint.model, "parent");
    }

    #[tokio::test]
    async fn same_openrouter_route_sends_parent_budgets_for_worker_model() {
        let (base_url, captured) = one_shot_completion().await;
        let mut parent = endpoint();
        parent.provider = Provider::OpenRouter;
        parent.base_url = base_url;
        parent.model = "openrouter/auto".into();
        parent.reasoning_effort = Some("low".into());
        parent.max_output_tokens = Some(4096);

        let mut worker = provider_endpoint(&parent, Provider::OpenRouter);
        worker.model = "z-ai/glm-5".into();
        assert_eq!(worker.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(worker.max_output_tokens, Some(4096));

        let mut context = Context::new();
        context.push_user("work");
        let response = worker.build_model().generate(&context, &[]).await.unwrap();
        assert!(matches!(response, ModelResponse::Final { .. }));
        let request = captured.await.unwrap();
        assert_eq!(request["model"], "z-ai/glm-5");
        assert_eq!(request["reasoning"], json!({"effort": "low"}));
        assert_eq!(request["max_tokens"], 4096);
    }

    #[test]
    fn cross_provider_and_codex_unsupported_budgets_are_not_inherited() {
        let mut parent = endpoint();
        parent.provider = Provider::OpenRouter;
        parent.reasoning_effort = Some("low".into());
        parent.max_output_tokens = Some(4096);

        let cross_provider = provider_endpoint(&parent, Provider::Anthropic);
        assert_eq!(cross_provider.reasoning_effort, None);
        assert_eq!(cross_provider.max_output_tokens, None);

        parent.provider = Provider::OpenAiCodex;
        let codex = provider_endpoint(&parent, Provider::OpenAiCodex);
        assert_eq!(codex.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(codex.max_output_tokens, None);
    }

    #[test]
    fn subagent_catalog_has_no_implicit_model_choices() {
        assert!(choices(&endpoint()).is_empty());
    }
}

#[cfg(test)]
mod live_assignment_tests {
    use super::*;
    use orca_harness_core::{
        CancellationToken, Context, ModelError, ModelResponse, Tool, ToolContext, ToolSchema,
    };
    use orca_harness_tools::{SubagentDepth, SubagentManager, SubagentTool};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NestedWorker {
        release: CancellationToken,
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Model for NestedWorker {
        async fn generate(
            &self,
            _: &Context,
            _: &[ToolSchema],
        ) -> Result<ModelResponse, ModelError> {
            self.release.cancelled().await;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(ModelResponse::tool_calls(vec![
                    orca_harness_core::ToolCall {
                        id: "nested".into(),
                        name: "subagent".into(),
                        arguments: serde_json::json!({"task": "nested", "background": false}),
                    },
                ]))
            } else {
                Ok(ModelResponse::final_text("done"))
            }
        }
    }

    #[tokio::test]
    async fn subagent_assignment_waits_for_existing_tree_without_breaking_nested_spawns() {
        let settings = SubagentDepth::new(2);
        let release = CancellationToken::new();
        let model = Arc::new(NestedWorker {
            release: release.clone(),
            calls: AtomicUsize::new(0),
        });
        let manager = SubagentManager::from_settings(settings.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = SubagentTool::with_tools(model.clone(), Arc::new(Vec::new))
            .models([SubagentModel::new("flash/local/old", "old", model.clone())])
            .max_depth(settings.clone())
            .background(manager.clone(), move |event| {
                let _ = tx.send(event);
            });
        assert!(settings.set_model_route(Some("flash".into())));
        let endpoint = Endpoint {
            provider: Provider::Local,
            base_url: Provider::Local.base_url().into(),
            api_key: None,
            model: "parent".into(),
            reasoning_effort: None,
            max_output_tokens: None,
            prompt_cache: false,
            request_session_id: None,
            model_retries: Arc::default(),
            model_gates: Default::default(),
            subagent_settings: settings.clone(),
        };
        save_assignment(&endpoint, &manager, "flash", Provider::Local, "old".into()).unwrap();
        tool.call(
            serde_json::json!({"task": "outer"}),
            &ToolContext {
                call_id: "outer".into(),
                tool_name: "subagent".into(),
                cancellation: CancellationToken::new(),
                deadline: None,
            },
        )
        .await
        .unwrap();
        let err = save_assignment(&endpoint, &manager, "flash", Provider::Local, "new".into())
            .unwrap_err();
        assert!(err.contains("wait for running subagents"));
        assert_eq!(
            crate::config::stored_subagent_models()["flash"].model,
            "old"
        );
        release.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            3,
            "nested worker must execute"
        );
        assert!(manager.active().is_empty());
        save_assignment(&endpoint, &manager, "flash", Provider::Local, "new".into()).unwrap();
        assert_eq!(
            crate::config::stored_subagent_models()["flash"].model,
            "new"
        );
    }
}
