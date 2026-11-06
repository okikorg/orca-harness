//! `orcacode --help` text.

pub(crate) const USAGE: &str = "\
orcacode — terminal host for Orca Harness

USAGE:
  orcacode [OPTIONS]                interactive session
  orcacode [OPTIONS] -p \"prompt\"    headless single run (streams to stdout)
  orcacode resume ID [OPTIONS]      resume a recorded session (same as --resume ID)
  orcacode update                   install the latest release
  orcacode plugin <COMMAND>         manage Agent Plugin packages

OPTIONS:
  --model NAME       model id (env ORCA_MODEL; otherwise selected from the
                     provider's live catalog)
  --base-url URL     API root; native Messages API with --anthropic,
                     otherwise OpenAI-compatible (env ORCA_BASE_URL;
                     default: api.openai.com if OPENAI_API_KEY is set,
                     otherwise http://localhost:11434/v1)
  --api-key KEY      API key (provider environment variable, including
                     ANTHROPIC_API_KEY for Anthropic,
                     AI_GATEWAY_API_KEY for Vercel AI Gateway,
                     CHEAPERINFERENCE_API_KEY for CheaperInference)
  --firecrawl-key K  Firecrawl key (env FIRECRAWL_API_KEY); enables the
                     web_search tool
  --openrouter       use OpenRouter (openrouter.ai) as the endpoint
  --anthropic        use the native Anthropic Messages API
  --provider LABEL   provider for this run, overriding the saved one:
                     openrouter, vercel, cheaperinference, openai,
                     openai-codex, anthropic, local (never saved)
  --list-providers   print the providers, their auth and saved models
                     and exit (--json for one JSON document)
  --list-models      print the endpoint's model catalog and exit
                     (--json for one JSON document)
  --workspace DIR    tool workspace root (default: current directory)
  --max-steps N      model invocations per run (default 48)
  --max-output-tokens N
                     output-token cap (Anthropic default: 8192)
  --effort LEVEL     reasoning effort (for example low, medium, high)
  --prompt-cache     enable provider prompt-cache hints (default)
  --no-prompt-cache  disable provider prompt-cache hints
  --tools NAMES      headless: comma-separated tool allowlist
  --bare             headless: skip user instructions, skills, memory,
                     MCP, compute, subagents, and web tools; defaults
                     --tools to read_file,grep,glob
  --subagent-depth N subagent nesting levels, positive integer (env ORCA_SUBAGENT_DEPTH;
                     default 1; /subagents adjusts it live in the TUI)
  --theme NAME       theme: default, mono, dracula,
                     solarized-dark, one-dark, monokai, nord, orca
  --plan             start in plan mode: read-only tools, and docs/plan/
                     the only writable directory — the agent decides
                     whether to write a plan (/mode opens a picker)
  --orchestrate      start in orchestrate mode: the agent investigates
                     and delegates substantial work; basic edits permitted;
                     spawned workers keep their full tools
  --normal           start in normal mode (default): gated tools ask for approval
  --auto             start in auto mode: safe tools and guarded
                     file edits run; risk-bearing actions are reviewed
  --yolo             start in yolo mode: every gated tool runs without
                     approval prompts, interactive or headless. The
                     status line reads yolo for the whole session
                     (/mode opens a picker)
  --continue         resume the latest recorded session for this workspace
  --resume ID        resume a recorded session by id (a unique prefix works)
  --no-session       do not record this session to disk
  --json             headless: emit NDJSON harness events on stdout
  --prompt-file PATH headless: read the prompt from a file (-p - reads stdin)
  --mcp-config JSON  headless: extra MCP servers for this run only, inline
                     JSON or a file: {\"name\":{\"command\":\"...\",
                     \"args\":[...],\"env\":{...},\"cwd\":\"...\"}};
                     never saved to config.json
  --approvals MODE   headless: deny (default) or stdio; stdio emits
                     approval_request events and reads decisions from stdin
                     (requires --json)
  --read-only        headless: no file writes, compute, delegation or memory
                     writes; shell needs approval; MCP tools stay usable
  --image PATH       headless: attach an image to the prompt (repeatable)
  --auto-approve     headless: allow shell/write/edit without approval
                     (implied by --yolo)
  -p, --prompt TEXT  headless prompt (- reads it from stdin)
  -h, --help         show this help
  -v, -V, --version  print the version and exit

In the TUI, /provider selects local, OpenAI API, OpenRouter, Vercel AI Gateway,
CheaperInference, Anthropic, or the OpenAI Codex ChatGPT-subscription provider.
Selecting openai-codex starts Orcacode's device login; an existing
official Codex login is also imported.
No API key is required. /models [filter] opens the model catalog,
and /settings changes the provider, model, theme, and API key.
API keys entered in the TUI, the active provider, the theme, and the
last model picked per provider are saved to
~/.config/orcacode/config.json and reused on later runs; flags and
environment variables above always win over the saved values.
Approval prompts accept y (once), a (always, this session), A (always,
saved for this workspace only; revoke in /settings), and n (deny).
Sessions are recorded under ~/.config/orcacode/sessions/ per workspace;
/sessions in the TUI lists and resumes them, /rewind drops the last
turns, and /fork branches the conversation into a new session.
Standing instructions are read at startup from AGENTS.md beside
config.json, in the workspace, and in the workspace's .orca/ folder.
";
