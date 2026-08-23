# Orca Harness crate diagram

This diagram shows the workspace's **runtime internal crate dependencies**. External crates from crates.io are omitted. Development-only dependencies are listed below because they do not participate in the shipped dependency graph.

```mermaid
flowchart TD
    CLI[orcacode\ninteractive CLI / host]
    CORE[orca-harness-core\nagent execution kernel\ndeterministic tool dispatcher]
    EXT[orca-harness-extensions\nevent stream / policy / retries\ntruncation / usage metering]
    TOOLS[orca-harness-tools\nshell / file I/O / search]
    WEB[orca-harness-tools-web\nweb fetch / web search]
    MCP[orca-harness-tools-mcp\nstdio MCP client]
    SKILLS[orca-harness-tools-skills\nSKILL.md discovery]
    OPENAI[orca-harness-model-openai\nOpenAI-compatible adapter]
    ROUTER[orca-harness-model-openrouter\nOpenRouter adapter / catalog]

    CLI --> CORE
    CLI --> EXT
    CLI --> TOOLS
    CLI --> WEB
    CLI --> MCP
    CLI --> SKILLS
    CLI --> OPENAI
    CLI --> ROUTER

    EXT --> CORE
    TOOLS --> CORE
    WEB --> CORE
    MCP --> CORE
    SKILLS --> CORE
    OPENAI --> CORE
    ROUTER --> CORE
    ROUTER --> OPENAI

    classDef kernel fill:#16324f,stroke:#8ecae6,color:#fff
    class CORE kernel
```

## Dependency shape

- `orca-harness-core` is the dependency center and has no workspace-crate dependencies.
- `orca-harness-extensions` and the four tool crates plus the two model adapter crates depend directly on the core.
- `orca-harness-model-openrouter` additionally reuses the OpenAI adapter.
- `orcacode` is the composition root: it wires the kernel, extensions, tools, model adapters, and terminal UI together.

## Development-only dependencies

The following internal edges are used only by tests/examples and are intentionally not shown above:

- `orca-harness-tools` → `orca-harness-extensions`
- `orca-harness-extensions` → `orca-harness-model-openai`
- `orca-harness-extensions` → `orca-harness-tools`

The kernel fan-out measurements are recorded in [`benchmarks/results/kernel`](../benchmarks/results/kernel/).
