# `orca-harness-tools`

`orca-harness-tools` is the baseline capability set for an Orca Harness host. Its tools are ordinary `Tool` implementations registered with the core dispatcher; hosts decide which capabilities to expose and which require approval or policy checks.

## Tool groups

- Workspace-safe file operations: `read_file`, `write_file`, `edit_file`, `multi_edit`, `apply_patch`, `list_dir`, `glob`, and `grep`.
- Execution: `shell`, `process`, and persistent process-group support through an `Executor` (including local, SSH, and Docker execution).
- Compute: persistent `PyKernelTool` and `BunReplTool` sessions.
- Workflow: `AskTool`, `TodoWriteTool`, and `SubagentTool` for user questions, structured plans, and bounded in-process delegation.
- Restricted filesystem administration: `fs_admin_tools` supplies copy, rename, delete, create-folder, and file-info tools for shell-less hosts.

`Workspace` establishes the filesystem boundary. `core_tools`, `core_tools_with_executor`, and `core_tools_with_guard` assemble the common bundles. `multi_edit` validates every ordered replacement or append before the first write, and `MutationPreflight` can reject malformed mutations before approval.

```bash
cargo test -p orca-harness-tools
cargo run -p orca-harness-tools --example agent_with_tools
cargo run -p orca-harness-tools --example tool_fanout_perf
```

## Workspace role

These tools are host capabilities, not a sandbox. A host must choose the workspace root, executor, tool bundle, and approval or policy extensions before exposing them to a model. `Workspace` constrains filesystem tools to the configured root; command execution remains the responsibility of the selected `Executor` and host policy.

Related crates: [`orca-harness-core`](../harness-core) and [`orca-harness-extensions`](../extensions).
