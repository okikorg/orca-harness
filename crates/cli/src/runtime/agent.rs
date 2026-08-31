use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::Extension;
use orca_harness_extensions::{EventStream, Truncation};
use orca_harness_tools::SubagentSpawn;

use crate::auto_approval::AutoApproval;
use crate::mode::{ModeHandle, PlanGate};
use crate::msg::UiMsg;
use crate::plan::PlanArea;

/// The extensions every spawned subagent gets: a relay that tags its
/// events with the spawn's identity, context-safe tool-output truncation,
/// and the same plan gate the orchestrator has.
///
/// The gate has to be here as well, not only on the top-level agent, for
/// the same reason tool retry is mirrored into subagents: a restriction
/// that only holds at depth 0 is not a restriction. Spawning is itself
/// denied in plan mode, so this matters for one case — a subagent that
/// was already running when the user flipped `/mode`. Without it, that
/// inner agent would keep writing while the interface says the session
/// changes nothing.
pub(crate) fn subagent_extensions(
    spawn: &SubagentSpawn,
    ui: &mpsc::UnboundedSender<UiMsg>,
    mode: &ModeHandle,
    plan_area: &PlanArea,
    settings: &orca_harness_tools::SubagentDepth,
    plugin_hooks: Option<&Arc<orca_harness_tool_extensions::plugin_hooks::PluginHookExtension>>,
    auto_approval: Option<&AutoApproval>,
) -> Vec<Arc<dyn Extension>> {
    let ui = ui.clone();
    let (id, parent_id, depth) = (spawn.id, spawn.parent_id, spawn.depth);
    let call_id = spawn.call_id.clone();
    let _ = ui.send(UiMsg::SubagentStarted {
        id,
        parent_id,
        depth,
        call_id: call_id.clone(),
        task: spawn.task.clone(),
        identity: spawn.identity.clone(),
    });
    let mut extensions = vec![
        Arc::new(EventStream::from_fn(move |event| {
            let _ = ui.send(UiMsg::SubagentEvent {
                id,
                parent_id,
                depth,
                call_id: call_id.clone(),
                event,
            });
        })) as Arc<dyn Extension>,
        Arc::new(PlanGate::new(mode.clone(), plan_area.clone())) as Arc<dyn Extension>,
    ];
    if let Some(plugin_hooks) = plugin_hooks {
        extensions.push(plugin_hooks.clone());
    }
    if let Some(auto_approval) = auto_approval {
        extensions.push(Arc::new(auto_approval.clone()));
    }
    extensions
        .push(Arc::new(Truncation::new(settings.output_chars() as usize)) as Arc<dyn Extension>);
    extensions
}
