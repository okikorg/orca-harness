# Design: skills

Date: 2026-08-22
Status: implemented — `crates/tool-extensions/src/skills`, `crates/cli/src/skills.rs`

## Motivation

MCP gave the host a way to hand the agent *capabilities* it did not ship
with: a config entry names a server, the CLI connects it, and its tools
join the agent's tool set. Skills are the same idea one level up — a way
to hand the agent *procedures* it did not ship with: a folder with a
`SKILL.md` in it that says how this project releases, how this team
writes migrations, what the house review checklist is.

The plumbing transfers directly from MCP; the payload shape does not.
That distinction drives everything below:

|  | MCP | Skills |
|---|---|---|
| Source | config entry (a command) | files discovered on disk |
| Cost of a reload | seconds per server (`npx`) | microseconds (a `read_dir`) |
| What it adds | **N tools**, one per remote tool | **one tool** (`skill`), N catalog entries |
| What a call returns | a remote result | instruction text for the model |
| Failure mode | server won't connect | `SKILL.md` won't parse |

What *is* mirrored, one-for-one: a cloneable shared handle read by
`build_agent` and the TUI, a `WorkerCmd::Reload*` that the UI sends after
editing config, a picker overlay with per-item state, config persistence
that survives restarts, headless parity, and the rule that one broken
item is reported and skipped, never fatal.

## Prior art: fx

[fx](https://fx.sh/) (Vercel Labs, Zig — `vercel-labs/fx`) ships skills
and is close enough to this harness in spirit to be worth copying from
deliberately rather than accidentally. What it does:

- **A `skill` tool**, params `name`, `location`, `resource`, `offset`
  (`src/tools/skills/skill.zig`, registered in `src/builtins/tools.zig:1054`).
  `resource` is an "optional relative text resource within the selected
  skill. Defaults to SKILL.md"; `offset` pages the body and the result
  carries `next_offset`. A second tool, `install_skill`, fetches skills
  from GitHub / `skills.sh` into a managed root.
- **A catalog as its own context section.** fx's context contract lists
  the stable prefix order as `system_prompt,
  effective_custom_tool_guidance, visible_skills, …, mcp_server_catalog`,
  with loaded bodies arriving separately as `explicit_skill_chunks` in
  the per-step overlay (`src/core/workspace/context_contract.zig:546`).
  Skills and MCP get the same treatment: bounded catalog in context, load
  on demand through a tool.
- **Compatibility roots.** Workspace: `.fx/skills`, `skills`,
  `.opencode/skills`, `.codex/skills`, `.claude/skills`, `.agents/skills`,
  `.claw/skills`, scanned at the workspace root *and each ancestor below
  home*. Home: the managed root plus the same set
  (`src/builtins/skills.zig`).
- **Duplicates are preserved, not shadowed**; the catalog advertises each
  skill's `location` and the model passes it back to disambiguate.
- **Frontmatter**: `name` required (falls back to the directory name),
  `description` optional and allowed to be a block scalar (`>`, `>-`,
  `|`), unknown fields ignored "for compatibility with other agents".
- **Loading is explicit** — "discovering a skill does not add its
  instructions to every prompt."
- `/skills` with `list | show | add | install | create | remove | path`,
  plus `$` in the composer to search skills.

Adopted here: the compatibility roots, `resource`, `offset`/`next_offset`,
frontmatter tolerance (block scalars, unknown fields ignored), and
explicit-load-only, and installing from a repository. Deliberately
different: no install tool for the model (§6), and
shadowing instead of `location` (§3). Deferred with a reason: the
catalog-as-context-section (§4).

## Tool or Extension?

Both, and the split matters. The **tool is a Tool**; the **catalog is the
part that would be an Extension**.

`Extension` hooks (`before_model`, `before_tool`, `around_tool`, …) exist
for cross-cutting behavior the model does not choose: approvals,
truncation, retries, session recording. Loading a skill is the opposite —
a capability invoked deliberately, at a moment only the model knows — so
it is a Tool, by the same reasoning the kernel/subagent design used for
the same fork (`2026-08-20-kernel-subagent-tools-design.md`). MCP
resolved identically: `tool-extensions::mcp` produces Tools, and the lifecycle
(connect, reload, hold) lives in the host at `crates/cli/src/mcp.rs`, not
in an Extension. Skills copy that split: `crates/tool-extensions/src/skills` for the
tool, `crates/cli/src/skills.rs` for discovery and reload.

Keeping a *catalog* in front of the model every turn, however, is exactly
a `before_model` concern — that is where fx puts it. §4 explains why v1
does not, and what would have to change in the kernel first.

Note also what remains a non-goal in either arrangement: a bounded
catalog of names and descriptions is not the same thing as skill
*bodies* entering the context unbidden. The first is a menu; the second
is auto-injection.

The CLI's `/extensions` list is a third, unrelated sense of the word:
skills are not `ExtensionSpec` entries, they get their own `/skills`
overlay the way MCP got `/mcp`, because the list is discovered at runtime
rather than compiled in. Only the *config override shape* is borrowed
from `extensions` (§5).

## Non-goals

- **Executable skill bundles.** A skill is text. A skill directory may
  hold *text* resources the tool can read (§3), but nothing in it is ever
  executed by the loader. A body may *tell* the agent to run
  `scripts/check.sh`; the agent then runs it with `shell` under the
  normal approval gate.
- **An install tool for the model.** fx exposes `install_skill` to the
  agent. Installing is user-driven here: a skill body is instructions the
  model will follow, so an agent that can install its own instructions
  closes a loop that should stay open. `/skills add` is typed by a
  person — see §6.
- **Auto-injection of bodies.** A skill's instructions never enter the
  context unless the model calls the tool.
- **Frontmatter semantics beyond `name` and `description`.** No
  `allowed-tools`, no `model`, no triggers. Unknown keys are parsed past
  and ignored, matching fx, so files written for other agents load here.
- **Ancestor-directory scanning.** fx walks every directory from the
  workspace up to home. Here the workspace root is explicit (`--workspace`)
  and every file tool is rooted at it; making skills the one thing that
  leaks in from a parent directory would surprise. Revisit if monorepo
  users ask.

## 1. The skill format

One directory per skill, holding `SKILL.md`:

```
.orca/skills/
  release/
    SKILL.md
    checklist.md      ← text resource; loadable via the tool's `resource`
```

```markdown
---
name: release
description: Cut a release — version bump, changelog, tag, publish
---

1. Confirm `main` is green: `cargo test --workspace`.
2. Bump the workspace version in `Cargo.toml`.
...
```

Parsing rules, deliberately narrow (no YAML crate — the workspace has
none, and the house rule for this kind of thing is no new dependencies):

- The file must begin with `---\n`. The frontmatter ends at the next line
  that is exactly `---`. Everything after it is the body.
- Inside, flat `key: value` lines are read; `name` and `description` are
  recognized, everything else is skipped. Values are trimmed.
- `description` may be a block scalar (`|`, `>`, `>-`) with the
  continuation lines indented — **not** optional polish: the
  compatibility roots (§2) mean real `.claude/skills` files land here,
  and block-scalar descriptions are common in them. Quoted single-line
  values unwrap too. No lists, no nesting, no anchors.
- `name` defaults to the directory name when absent (as in fx) and must
  match `[a-z0-9][a-z0-9_-]*`.
- `description` is required and non-blank — it is the only thing the
  model sees before deciding to load the skill, so a skill without one is
  a skill that will never be used. (fx makes it optional; a skill with no
  description is dead weight in a catalog, so this one is stricter.)
- Anything else (no fence, no `description`, unreadable file, bad name)
  is a **load failure**: recorded with its reason, rendered in `/skills`,
  skipped from the catalog. Exactly the shape of a failed MCP connection
  (`crates/cli/src/mcp.rs:132`).

## 2. Discovery and precedence

Roots are scanned in this order, first match winning a name:

**Workspace** (`<workspace root>/…`, no ancestor walk)

1. `.orca/skills`
2. `skills`
3. `.claude/skills`
4. `.agents/skills`
5. `.codex/skills`
6. `.opencode/skills`

**User**

7. `$ORCA_CONFIG_DIR/skills` — next to `config.json`
   (`~/.config/orcacode/skills`)
8. `~/.claude/skills`
9. `~/.agents/skills`
10. `~/.codex/skills`
11. `~/.config/opencode/skills`

Roots 3–6 and 8–11 are compatibility roots, copied from fx's list (minus
`.claw`, which can be added the day someone wants it). They cost one
`read_dir` each and they are the difference between "skills work" and
"skills work after you move your files".

Each root is `<root>/<skill-name>/SKILL.md`. Within a root, entries sort
by name, so the catalog is stable across runs and the prompt prefix stays
cache-friendly.

**Duplicates shadow.** The first root that supplies a name wins; later
ones are dropped from the catalog and shown greyed in `/skills` with
their source, so a collision is visible rather than silent. fx instead
keeps duplicates and has the model pass a `location` to disambiguate.
Shadowing is the call here because it keeps the tool's contract to a
single `name`, and because "the workspace's version of `review` beats the
one in my home directory" is the behavior a user expects. The cost is
that a shadowed skill is unreachable except by renaming — acceptable, and
visible in the overlay.

Roots are **injected, not derived**: `discover(workspace_root,
home, user_config_root)`, with `main.rs`/`headless.rs` resolving
`config_path()?.parent()` and `HOME` at the call site. This is a
test-isolation requirement, not taste. `config.rs` stubs
`read_config`/`write_config` under `#[cfg(test)]` but *not*
`config_path()`; `McpServers::reload()` gets away with that because it
only reads config, while `Skills::reload()` touches the filesystem and
would otherwise scan the developer's real `~/.claude/skills`. Setting
`ORCA_CONFIG_DIR`/`HOME` in a test is process-global and races across
test threads — precisely what the thread-local config stub exists to
avoid. No test in the repo mutates env vars today and none should start
here; tests pass temp dirs, the way `crates/tools/tests` uses `temp_ws()`.

Reading outside the workspace is also why this cannot live in
`crates/tools`: every tool there resolves paths through
`Workspace::resolve`, which rejects absolute paths and any climb above
the root (`crates/tools/src/workspace.rs:42`). Skills legitimately read
`~/.claude/skills`, so they get their own crate rather than a hole in
that invariant.

## 3. `skill` tool (`crates/tool-extensions/src/skills`)

New crate `orca-harness-tool-extensions::skills`,
a sibling of `tool-extensions::mcp` with the same framing: outside the core tool
set, host opt-in. Contents: discovery + the frontmatter parser
(`skill.rs`), and `SkillTool` (`tool.rs`).

One tool, not one tool per skill. Registering `skill__release`,
`skill__review`, … the way MCP does would inflate the tool list the model
re-reads every turn with N entries that all do the same thing.

### Model-facing schema

Tool name: `skill`. Description (the catalog rides here in v1 — §4):

```
Load a skill: the project's own instructions for a task, written by the
user. Call this before starting work a skill covers, then follow what it
says; use `resource` to pull a file the skill points at. Available:
  release — Cut a release: version bump, changelog, tag, publish
  migrate — Write and apply a database migration the house way
```

| Param | Type | Notes |
|---|---|---|
| `name` | string, required | A skill name from the catalog; also an `enum` in the schema, so a hallucinated name fails at decode. |
| `resource` | string, optional | Relative path to a text file inside that skill's directory. Defaults to `SKILL.md`. |
| `offset` | integer, optional | Byte offset into the file; default 0. Pages long bodies. |

Response JSON:

```json
{
  "name": "release",
  "source": "workspace:.orca/skills",
  "path": ".orca/skills/release/SKILL.md",
  "instructions": "1. Confirm `main` is green…",
  "resources": ["checklist.md", "reference/api.md"],
  "nextOffset": 8192
}
```

- `resources` is listed only for the `SKILL.md` load: it tells the model
  what else it may pull without a `list_dir` round trip. Text files only
  (see below), relative to the skill directory, capped at ~50 entries.
- `nextOffset` is present only when the file was cut short; absent means
  the read reached the end. Chunk size 8k chars, matching
  `ReadToolResultTool`'s default.
- Each catalog line clips its description at 160 characters. The catalog
  sits in the schema the model re-reads every turn, and the
  compatibility roots mean a user's entire `~/.claude/skills` collection
  can arrive at once — in practice that is twenty-odd entries on a
  working machine, so paragraph-long descriptions are not free. The full
  text is one call away.
- Unknown or disabled name → `ToolError` naming the available skills (the
  model may be working from a schema captured earlier in the run).

### The `resource` containment invariant

`resource` is the one place this tool can be pointed at an arbitrary
path, and `Workspace::resolve` cannot help — it is rooted at the
workspace, and skill directories may live under `$HOME`. So
`tool-extensions::skills` enforces its own rule, and it is a real invariant with its
own tests:

1. Reject absolute paths and any `..` component before touching disk.
2. Canonicalize the joined path and the skill directory, and require the
   former to start with the latter. This is what closes the symlink
   escape that step 1 alone leaves open.
3. Reject non-UTF-8 content with an error pointing at `read_file` — a
   skill resource is text by definition.

### Semantics

- `Concurrency::Parallel`. A call is one file read.
- **Not gated.** `GATED_TOOLS` (`crates/cli/src/approval.rs:18`) covers
  tools that mutate the machine or egress; reading a text file the user
  put in their own project is neither. What the skill *tells* the agent
  to do hits the existing gates. (fx marks its skill tool
  `readsOnly = false`; here the containment rule above is what makes
  read-only true rather than aspirational.)
- **Read at call time, not at scan time.** Discovery keeps frontmatter
  and paths only; bodies are read fresh per call, so editing a `SKILL.md`
  mid-session takes effect without a reload (only the catalog needs one).
  The natural implementation — `read_to_string` then split on the fence —
  leaves the body in hand at scan time and will silently cache it if
  allowed to; the parser must return frontmatter and drop the rest, with
  a test asserting an edit after discovery is visible to the next call.
- Paging via `offset` is preferred to the generic truncation path: the
  extension would still cap an oversized body at 16k and hand back a
  `read_tool_result` handle, but paging a file whose length is known is
  cheaper and more predictable than recovering a truncated blob. The
  overlay shows each skill's file size from `fs::metadata`, so an
  oversized skill is visible rather than mysterious.

## 4. Where the catalog lives

fx puts it in the context as a bounded `visible_skills` section, refreshed
as part of the stable prefix. That is the better design and it is not
available here yet:

`Context` is **append-only** — `push`, `push_system`, `push_user`, …, and
`messages()` returns an immutable slice (`crates/harness-core/src/context.rs`).
There is no replace and no keyed section, so a `before_model` extension
could only *append* a catalog, once per model call, with no way to
supersede a stale copy after `/skills` toggles something. A faithful copy
of fx's design therefore needs a kernel affordance first — something like
a keyed pinned section that re-renders in place, which is a change to the
one part of this codebase whose stated rule is that it stays tiny.

**v1: the catalog rides in the tool's description and its `name` enum.**
This costs nothing extra, because the toggle path already rebuilds the
agent: `build_agent` re-reads the shared handle on every rebuild, exactly
as `WorkerCmd::ReloadExtensions` (`main.rs:542`) already proves. Baking
the catalog into `system_prompt()` instead would *not* work —
that string is built once (`main.rs:252`) and re-pushed verbatim by
`WorkerCmd::Clear`, so it would go stale with no path that reaches it.

The system prompt gets **no** mention at all — the one place this design
departs from the convention at `main.rs:919` that the prompt's tool list
matches registration. That convention guards against advertising a tool
that is not registered, and `skill` is exactly the tool whose
registration moves during a session: turn the last skill off in
`/skills` and `tool()` returns `None`, but the captured prompt string
would still be naming it. Since the tool's own description already
explains what a skill is and lists the catalog, the prompt line buys
nothing and costs a staleness bug; a test pins the omission and says why.

**Revisit when** a second consumer wants the same thing — an MCP server
catalog (fx has one in the same prefix), or project instructions. One
`Context::pinned(key, content)` serving three callers is worth the kernel
change; serving one is not.

## 5. CLI wiring

### `crates/cli/src/skills.rs` — the `Skills` handle

Mirrors `McpServers`: `#[derive(Clone, Default)]` over an
`Arc<RwLock<…>>`, captured by the agent-build closure and by the TUI.

```rust
pub enum SkillState {
    Loaded { root: String, bytes: u64 },
    Shadowed { root: String, by: String },
    Failed { root: String, reason: String },
}

impl Skills {
    pub fn new(workspace: &Path, config_dir: Option<PathBuf>, home: Option<PathBuf>) -> Self;
    pub fn for_session(workspace: &Path) -> Self;   // the three roots, resolved
    pub fn reload(&self) -> Vec<String>;            // status lines, changes only
    pub fn tool(&self) -> Option<Arc<dyn Tool>>;    // None when nothing is enabled
    pub fn catalog(&self) -> Vec<SkillEntry>;       // loaded, shadowed and failed rows
}
```

`reload` is synchronous — unlike MCP's, it only reads directories — so
the worker calls it inline.

Two deliberate departures from `mcp.rs`:

- **No reload diff.** `mcp.rs:6` justifies its add/removed/changed diff by
  reconnect cost; re-scanning eleven directories is microseconds.
  `reload()` discards and rebuilds, reporting only *failures* plus a
  one-line count when the set changed. An unchanged, healthy scan says
  nothing.
- **`tool()` returns `Option`.** With no skills present, no `skill` tool
  is registered at all — a tool with an empty enum is worse than absent.

### Config (`crates/cli/src/config.rs`)

Skills are discovered, not declared, so — unlike `mcp` — there is no
command to persist, only an override. This mirrors `extensions`:

```json
"skills": { "release": false }
```

`stored_skill_enabled(name) -> Option<bool>` / `save_skill_enabled(name,
bool)`, read exactly like `stored_extension` (`config.rs:107`): anything
but an explicit `false` is on. A skill dropped into a directory works
immediately, with no second step — and disabling one never loses it,
because the file on disk is the source of truth.

The file's header doc-comment (the documented config contract, which
already lists `mcp`) gains the `skills` block.

### Slash command and overlay (`commands.rs`, `tui.rs`)

`CommandSpec { name: "skills", description: "toggle skills, show one, or
reload from disk", category: "Session", takes_args: true }`.

- `/skills` — the picker: name, root, description, state (`loaded · 1.2k`
  / `shadowed by .orca/skills` / `failed — <reason>`), space toggles, esc
  closes; a toggle saves and sends `WorkerCmd::ReloadSkills`, exactly as
  `Overlay::Mcp` does (`tui.rs:1100`).
- `/skills show <name>` — print the resolved path, root, description, and
  size, for debugging a shadow or a parse failure. (fx has the same.)
- `/skills add <source> [--skill <name>] [--list] [--here]` — install.
  Sources follow the `npx skills` CLI, which is what the ecosystem
  settled on: `owner/repo`, `owner/repo@skill`, a GitHub/GitLab/skills.sh
  URL, a `/tree/<ref>/<path>` deep link, a local folder, or a pasted
  `npx skills add …` line. Destination is the managed root beside
  `config.json`; `--here` puts it in `.orca/skills` instead, because a
  skill fetched from someone else's repository should not appear as an
  untracked folder in the user's project unless they say so. Cloning
  runs in the worker (`WorkerCmd::InstallSkill`) so the interface stays
  live.
- `/skills create <name> [--global]` — scaffold a `SKILL.md` in
  `.orca/skills`, where a skill written *for this repository* belongs.
- `/skills remove <name>` — delete, but only under the two roots this
  host installs into. A skill in `~/.claude/skills` belongs to whatever
  put it there; the refusal names the path instead. Same rule as fx's
  managed-root check.
- `/skills reload` — rescan without a toggle.
- `/help` gains a line.

Copy, never symlink. The ecosystem CLI defaults to symlinks; here the
`skill` tool's `resource` containment (§3) is enforced by canonicalizing,
and a tree of symlinks would make that check pass or fail for reasons
unrelated to intent. Symlinks *inside* a source are skipped for the same
reason, and an install is capped at 200 files / 2 MiB so a mis-typed
source cannot copy a repository in.

Note for implementation: `filter_commands`'s tests (`commands.rs:146`)
assert exact vectors for `"el"`, `"m"`, and `"e"`; `skills` contains none
of those letters, so those vectors stand — but registry position matters
if an `"s"`-filter test is ever added, so slot it next to `sessions`.
`App` gains a `skills` field, which means touching the ~13
`mcp: Default::default()` test-helper sites in `tui.rs`.

### Manifests

Root `Cargo.toml` `[workspace] members` gains `crates/tool-extensions/src/skills`;
`crates/cli/Cargo.toml` gains the dependency. The same two lines
`tool-extensions::mcp` needed.

### Worker, build, headless (`main.rs`, `headless.rs`)

- `mod skills;`, `WorkerCmd::ReloadSkills` → `for line in
  skills.reload().await { notice }` then `agent = build(&endpoint)`
  (`main.rs:547` pattern).
- Initial scan before the first build so skills are in the first agent
  (`main.rs:744` pattern), with status lines into the transcript.
- `build_agent` takes `&Skills` and does
  `if let Some(tool) = skills.tool() { agent = agent.tool_arc(tool) }`,
  registered next to the MCP tools.
- `TuiConfig` gains the handle so the overlay reads the same state the
  worker writes.
- `headless.rs` mirrors `:98-105`: scan, print failures to stderr,
  register.

## 6. Trust

A `SKILL.md` body becomes model instructions. For workspace and user
directories that is the same trust level as the repo code and
`CLAUDE.md`-style files the agent already reads, so local discovery is
fine as designed — with one new wrinkle the compatibility roots
introduce: a skill that arrives inside a cloned repository is now
loadable without the user ever having written it. The catalog surfaces
each skill's root in `/skills`, and disabling is one keypress, which is
the proportionate answer.

Installing from a repository raises the stakes: it turns "text the user
has in their tree" into "text fetched from a name someone typed once".
`/skills add` ships anyway, because the alternative is users running
`git clone` into the folder by hand with none of the guards. What it
keeps:

- **The model cannot install.** fx exposes an `install_skill` tool; here
  the only way in is a person typing `/skills add`. An agent that can
  fetch its own instructions is a loop worth leaving open.
- **The install says what it did**, naming every skill and its path, and
  ends with a line saying plainly that a skill body is instructions the
  agent follows. Nothing is installed silently.
- **Nothing is executed.** The loader copies text; a body that says "run
  `scripts/x.sh`" still goes through `shell` and its approval prompt.
- **Symlinks are dropped and size is capped** (see §5), so a source
  cannot smuggle in a link to somewhere else on the machine.

Not covered, and worth naming: there is no pinning, no update command,
and no provenance beyond the source string the user typed. A skill
installed today is whatever the default branch held at that moment.

## 7. Open forks

Positions taken, each now against a known alternative in fx.

1. **Catalog in the tool schema vs. a context section.** Taken: schema,
   because `Context` is append-only and the alternative is a kernel
   change (§4). Revisit with a second consumer.
2. **Shadowing vs. `location` disambiguation.** Taken: shadowing, first
   root wins, collisions shown in the overlay (§2). fx keeps duplicates
   and adds a `location` param.
3. **Text resources vs. text-only `SKILL.md`.** Taken: resources, with
   the containment invariant of §3. Executable bundles stay a non-goal.
4. **Who may install.** Taken: the user, never the model (§6). fx gives
   the agent an `install_skill` tool.
5. **Workspace root only vs. ancestor walk.** Taken: workspace root only
   (§Non-goals).

## 8. Tests

- **Crate**: frontmatter round-trip, including a block-scalar
  description and unknown keys; missing fence / missing description / bad
  name each yield a recorded failure, not a panic; body read at call time
  reflects an edit made after discovery; `offset` paging returns
  `nextOffset` and terminates; unknown name errors and names the
  alternatives.
- **Containment** (the security-relevant part): `resource` rejects
  `../`, absolute paths, and a symlink inside the skill directory
  pointing outside it; a non-UTF-8 resource errors instead of returning
  mojibake.
- **Discovery**: a skill in each root is found; precedence order holds;
  a duplicate is marked shadowed rather than dropped silently; catalog
  order is stable; a missing root is not an error.
- **CLI**: `stored_skill_enabled` defaults on and round-trips alongside
  the other config sections; `reload` reports failures and stays quiet on
  an unchanged healthy scan; disabled skills are absent from `catalog()`
  and from the schema enum; `tool()` is `None` with nothing enabled;
  `system_prompt` advertises `skill`.
