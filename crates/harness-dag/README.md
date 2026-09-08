# DAG workflows

`orca-harness-dag` is synchronous and depends only on `serde` and `serde_json`.
`Dag::validate` checks the submitted structure before `start` emits work;
`complete` returns more work or one terminal outcome. Hosts supply execution,
clocks, storage, cancellation, and model policy.

The tools crate's `WorkflowTool` reuses `SubagentTool` and `SubagentManager`.
The interactive CLI registers both against the same manager and completion inbox.

```json
{"action":"run","graph":[
  {"id":"dimensions","prompt":"Return review dimensions as a JSON string array","schema":"string[]"},
  {"id":"review","kind":"map","over":"dimensions","prompt":"Review the changes for {{ item }}"},
  {"id":"report","needs":["review"],"prompt":"Summarise {{ stages.review.output }}"}
]}
```

Supported schemas are strict `string[]` and `json[]`. A map automatically depends
on `over`, waits for every dependency, then expands in input order. Its output is
a JSON array of child answer strings. Map output can feed another map. Empty
expansion is legal and marks the terminal outcome degraded. Invalid schema output
gets one corrective retry. Other stage failures stop the run and cancel siblings.

`maxStages` defaults to 256, including virtual map nodes and expanded children;
hosts/callers may supply another positive bound. `timeoutSeconds` composes with
configured subagent deadlines and covers both execution and queue time. With no
configured deadline there is no implicit clock limit. Ordinary subagent per-worker
limits, tools, approval extensions, retry rules, and live concurrency still apply.

Only the run reserves parent delivery capacity, and it takes no execution slot.
Stage notifications settle the existing UI before advancing the DAG; hosts must
exclude those notifications from parent delivery. The CLI also excludes stages
from automatic parent inventories. `list` refuses unchanged snapshots; `output`
reads a stage on explicit request. Cancellation uses the existing manager jobs,
including generation changes on session reset. There is no coordinator task.

Outputs live in memory for the session, in a `WorkflowStore` the host creates
once beside the subagent manager and clones into every tool rebuild, so a run
survives a model switch. Nothing is written to disk, and `/clear` discards the
store along with the runs it cancels. A fresh `run` with `resumeFrom` reuses
matching stages from an earlier run in the same session; changing a stage
definition invalidates it and its descendants. Keys include canonical upstream definitions,
resolved model identity, schema, and system prompt. Lookup compares complete key
material, so a stage only replays against its own definition.
Replay assumes external inputs are unchanged; tool side effects are not replayed.

Terminal outcomes include sink outputs, every stage's status, degradation, timing,
and peak admitted work. `peakRunning` records actual concurrent workers using the existing manager slot
transitions. It is distinct from `peakAdmitted`, which includes queued workers. Live extension
of an existing graph, arbitrary branches/loops, and batched approval policy remain
outside v1; the host's existing approval policy applies to every stage.

Verification: `cargo test -p orca-harness-dag`,
`cargo test -p orca-harness-tools --test workflow --test subagent`, and the CLI
completion/browser/history tests. These require no live model or server.
