//! `/refine` — propose one skill from this session's trajectory. The
//! worker produces the proposal (crate::refine) and routes the
//! accept/reject decision through the standard approval gate (y/n/a/A);
//! `/refine undo` deletes the most recently applied skill.

use super::super::*;

pub(crate) fn refine_command(app: &mut App, args: &str, worker: &mpsc::UnboundedSender<WorkerCmd>) {
    let cmd = match args.split_whitespace().next() {
        None => WorkerCmd::Refine,
        Some("undo") => WorkerCmd::RefineUndo,
        Some(other) => {
            push_error(
                app,
                format!("unknown: /refine {other} — use /refine [undo]"),
            );
            return;
        }
    };
    let starting = matches!(cmd, WorkerCmd::Refine);
    if worker.send(cmd).is_err() {
        push_error(app, "worker is gone; restart orcacode");
        return;
    }
    if starting {
        push_notice(app, "reviewing the trajectory for one skill proposal…");
    }
}
