# Explore Orca Harness

**Goal:** Build a working mental model of the app — kernel, tools, extensions, CLI — and end with a short written map of how the pieces fit.

**Approach:** Walk the workspace in dependency order (core → adapters → tools → extensions → CLI), reading the key files of each layer, then run the tests and an example to see the whole thing actually work. Everything is read-only except the final write-up.

**Files touched:** only this plan file is updated (checkboxes + findings appended).

## Steps

- [ ] **Kernel first** — read `crates/harness-core/src`: the `Agent`, `Loop`, `Dispatcher`, `Tool`, `Extension` types and `Limits`. Verify: can name the core traits and say who calls whom (loop → dispatcher → tool) in one sentence each.
- [ ] **Model adapters** — skim `crates/model-openai` and `crates/model-openrouter`. Verify: can say how a `Model` trait impl maps to chat-completions and what streaming looks like.
- [ ] **Tools** — list the tool set in `crates/tools/src` (shell, process, pykernel, subagent, file tools, todo_write) and glance at one implementation. Verify: can name how a tool registers and what its input/output contract is.
- [ ] **Extensions** — read `crates/extensions/src`: event stream, tool policy, truncation, retry, usage, session record/resume. Verify: can name the lifecycle hooks each one subscribes to.
- [ ] **CLI** — read `crates/cli/src/main.rs` and `tui.rs`/`commands.rs` enough to trace one user turn: input → model → tool call → approval → render. Verify: can describe where plan mode gates tools (`mode.rs`, `plan.rs`).
- [ ] **Run it** — `cargo test --workspace` and `cargo run --example fake_llm`. Verify: tests pass and the example shows a multi-tool fan-out end to end.
- [ ] **Write the map** — append a one-page "how it fits together" summary to this file: layer diagram, the one-turn sequence, and 3 things that surprised me. Verify: someone who never read the code could follow it.
