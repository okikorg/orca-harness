use std::sync::Arc;

use tokio::sync::mpsc;

use orca_harness_core::Extension;
use orca_harness_extensions::EventStream;
use orca_harness_tools::SubagentSpawn;

use crate::auto_approval::AutoApproval;
use crate::mode::{ModeHandle, PlanGate};
use crate::msg::UiMsg;
use crate::plan::PlanArea;

/// The extensions every spawned subagent gets: a relay that tags its
/// events with the spawn's identity, context-safe tool-output truncation,
/// and the session's tool gate in its worker form.
///
/// The gate has to be here as well, not only on the top-level agent, for
/// the same reason tool retry is mirrored into subagents: a restriction
/// that only holds at depth 0 is not a restriction. Spawning is itself
/// denied in plan mode, so the reachable case is a subagent already
/// running when the user flipped `/mode` — without the mirror, that
/// inner agent would keep writing while the interface says the session
/// changes nothing. [`PlanGate::for_worker`] is the worker view of the
/// same shared handle: orchestrate does not propagate (its whole point
/// is to push work down to these workers), plan and yolo do.
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
        run: spawn.run,
        stage: spawn.stage.clone(),
    });
    let events = EventStream::from_fn(move |event| {
        let _ = ui.send(UiMsg::SubagentEvent {
            id,
            parent_id,
            depth,
            call_id: call_id.clone(),
            event,
        });
    });
    let execution_events = events.execution_marker();
    let mut extensions = vec![
        Arc::new(events) as Arc<dyn Extension>,
        Arc::new(PlanGate::for_worker(mode.clone(), plan_area.clone())) as Arc<dyn Extension>,
    ];
    let host_hooks = [
        plugin_hooks.map(|hooks| hooks.clone() as Arc<dyn Extension>),
        auto_approval.map(|approval| Arc::new(approval.clone()) as Arc<dyn Extension>),
    ];
    extensions.extend(super::subagents::extensions(
        settings,
        host_hooks.into_iter().flatten(),
    ));
    extensions.push(Arc::new(execution_events) as Arc<dyn Extension>);
    extensions
}
