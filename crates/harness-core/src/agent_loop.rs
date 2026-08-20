//! The agent loop. Deliberately boring: hooks, model, dispatch, repeat.

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::dispatcher::Dispatcher;
use crate::error::HarnessError;
use crate::extension::ExtensionRegistry;
use std::sync::Arc;

use crate::extension::Extension;
use crate::limits::Limits;
use crate::model::{DeltaSink, Model, ModelDelta, ModelResponse};
use crate::tool::{ToolRegistry, ToolSchema};

pub(crate) struct LoopEnv<'a, M: Model> {
    pub model: &'a M,
    pub tools: &'a ToolRegistry,
    pub extensions: &'a ExtensionRegistry,
    pub dispatcher: &'a Dispatcher,
    pub limits: &'a Limits,
    pub cancellation: CancellationToken,
}

pub(crate) async fn run<M: Model>(
    env: LoopEnv<'_, M>,
    context: &mut Context,
) -> Result<String, HarnessError> {
    env.extensions.run_on_agent_start(context).await?;
    let outcome = drive(&env, context).await;
    if let Err(err) = &outcome {
        env.extensions.run_on_error(err).await;
    }
    env.extensions.run_on_agent_end(context).await;
    outcome
}

async fn drive<M: Model>(
    env: &LoopEnv<'_, M>,
    context: &mut Context,
) -> Result<String, HarnessError> {
    let schemas: Vec<ToolSchema> = env.tools.schemas();
    let mut steps: u32 = 0;

    loop {
        if env.cancellation.is_cancelled() {
            return Err(HarnessError::Cancelled);
        }
        if let Some(deadline) = env.limits.deadline {
            if Instant::now() >= deadline {
                return Err(HarnessError::DeadlineExceeded);
            }
        }
        if steps >= env.limits.max_steps {
            return Err(HarnessError::StepLimitExceeded);
        }
        steps += 1;

        env.extensions.run_before_model(context).await?;
        let response = guarded_generate(env, context, &schemas).await?;
        env.extensions.run_after_model(context, &response).await?;

        match response {
            ModelResponse::Final { text: answer, .. } => {
                context.push_assistant_text(answer.clone());
                return Ok(answer);
            }
            ModelResponse::ToolCalls { content, calls, .. } => {
                context.push_assistant_tool_calls(content, calls.clone());
                let results = env
                    .dispatcher
                    .execute(
                        calls,
                        env.tools,
                        env.extensions,
                        &env.cancellation,
                        env.limits.deadline,
                        env.limits.max_parallel_tools,
                    )
                    .await?;
                context.append_tool_results(results);
            }
        }
    }
}

/// Fans a model's deltas out to the extensions subscribed to them.
struct ExtensionDeltaSink<'a> {
    subscribers: &'a [Arc<dyn Extension>],
}

#[async_trait::async_trait]
impl DeltaSink for ExtensionDeltaSink<'_> {
    async fn emit(&self, delta: ModelDelta) {
        for ext in self.subscribers {
            ext.on_model_delta(&delta).await;
        }
    }
}

async fn guarded_generate<M: Model>(
    env: &LoopEnv<'_, M>,
    context: &Context,
    schemas: &[ToolSchema],
) -> Result<ModelResponse, HarnessError> {
    // The streaming path only exists when someone is listening; otherwise
    // the model's plain path runs untouched.
    let subscribers = env.extensions.model_delta_subscribers();
    let sink = ExtensionDeltaSink { subscribers };
    let generate = async {
        if subscribers.is_empty() {
            env.model.generate(context, schemas).await
        } else {
            env.model.generate_streaming(context, schemas, &sink).await
        }
    };
    tokio::select! {
        biased;
        _ = env.cancellation.cancelled() => Err(HarnessError::Cancelled),
        out = async {
            match env.limits.deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, generate)
                    .await
                    .map_err(|_| HarnessError::DeadlineExceeded)?
                    .map_err(HarnessError::Model),
                None => generate.await.map_err(HarnessError::Model),
            }
        } => out,
    }
}
