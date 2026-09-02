//! Native terminal rendering for the Mermaid forms models use most often.
//!
//! This intentionally supports a useful subset instead of pretending to be
//! Mermaid's browser renderer. Unsupported input returns `None`, allowing the
//! Markdown renderer to preserve the source as an ordinary code block.

mod flowchart;
mod sequence;

use ratatui::text::{Line, Span};

use self::{flowchart::Flowchart, sequence::Sequence};
use super::Theme;

pub(super) fn render(
    source: &[&str],
    width: usize,
    indent: &str,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    let (header_index, header) = source
        .iter()
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())?;
    let body = &source[header_index + 1..];
    let kind = header.split_whitespace().next()?.to_ascii_lowercase();
    match kind.as_str() {
        "flowchart" | "graph" => Some(Flowchart::parse(body)?.render(width, indent, theme)?),
        "sequencediagram" => Some(Sequence::parse(body)?.render(width, indent, theme)),
        _ => None,
    }
}

/// Keep an unfinished streaming fence geometrically stable. The complete
/// diagram replaces this single row only after the closing fence arrives.
pub(super) fn pending(indent: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled("│ ", theme.dim),
        Span::styled("Mermaid diagram · waiting for closing fence", theme.code),
    ])
}

fn normalize_label(label: &str) -> String {
    let label = label
        .trim()
        .trim_matches('"')
        .replace("<br/>", " / ")
        .replace("<br>", " / ")
        .replace("<br />", " / ")
        .replace("\\n", " / ");
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::*;
    use crate::view::default_theme;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn flowchart_renders_boxed_nodes_routed_edges_and_labels() {
        let source = [
            "flowchart TD",
            r"CLI[orcacode\ninteractive CLI / host]",
            "CORE[execution kernel]",
            "EXT[extensions]",
            "CLI --> CORE",
            "CLI -->|events| EXT",
            "EXT -.-> CORE",
            "classDef kernel fill:#16324f",
            "class CORE kernel",
        ];
        let rendered = render(&source, 80, "  ", &default_theme()).unwrap();
        let text = text(&rendered);
        assert!(
            text.contains("CLI · orcacode / interactive CLI / host"),
            "{text}"
        );
        assert!(text.contains("CORE · execution kernel"), "{text}");
        assert!(text.contains("EXT · extensions"), "{text}");
        assert!(text.contains("┌"), "{text}");
        assert!(text.contains("▼"), "{text}");
        assert!(text.contains('┆') || text.contains('┄'), "{text}");
        assert!(text.contains("events"), "{text}");
        assert!(!text.contains("classDef"), "{text}");
    }

    #[test]
    fn flowchart_preserves_supported_endpoint_semantics() {
        let source = ["flowchart LR", "A <--> B", "A --x C", "A --o D"];
        let text = text(&render(&source, 80, "", &default_theme()).unwrap());
        for needle in ["▲", "▼", "×", "○", "B", "C", "D"] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
    }

    #[test]
    fn flowchart_renders_decision_branches_and_merge() {
        let source = [
            "flowchart TD",
            "A[Start] --> B{Random choice?}",
            "B -->|Yes| C[Do the thing]",
            "B -->|No| D[Do something else]",
            "C --> E[Finish]",
            "D --> E",
        ];
        let rendered = render(&source, 100, "", &default_theme()).unwrap();
        let text = text(&rendered);
        for needle in [
            "A · Start",
            "B · Random choice?",
            "C · Do the thing",
            "D · Do something else",
            "E · Finish",
            "◇",
            "Yes",
            "No",
            "┬",
            "▼",
        ] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
        assert!(rendered.iter().all(|line| line.width() <= 100), "{text}");
    }

    #[test]
    fn sequence_renders_lifelines_and_distinct_message_endpoints() {
        let source = [
            "sequenceDiagram",
            "participant C as Client",
            "participant A as API",
            "C->A: signal",
            "C->>A: request",
            "A--xC: rejected",
            "A--)C: response",
        ];
        let rendered = render(&source, 60, "  ", &default_theme()).unwrap();
        let text = text(&rendered);
        assert!(text.contains("Client"), "{text}");
        assert!(text.contains("API"), "{text}");
        assert!(text.contains('▶'), "{text}");
        assert!(text.contains('×'), "{text}");
        assert!(text.contains('◁'), "{text}");
        assert!(text.contains('┄'), "{text}");
    }

    #[test]
    fn sequence_style_blocks_do_not_leak_into_native_output() {
        let source = [
            "sequenceDiagram",
            "participant A",
            "participant B",
            "rect rgb(20, 30, 40)",
            "A->>B: request",
            "end",
        ];
        let text = text(&render(&source, 60, "", &default_theme()).unwrap());
        assert!(text.contains("request"), "{text}");
        assert!(!text.contains("rect"), "{text}");
        assert!(!text.lines().any(|line| line.trim() == "end"), "{text}");
    }

    #[test]
    fn wide_sequence_keeps_lifelines_beyond_eight_participants() {
        let source = [
            "sequenceDiagram",
            "participant U as User",
            "participant W as Web App",
            "participant A as API Gateway",
            "participant S as Auth Service",
            "participant O as Order Service",
            "participant P as Payment Service",
            "participant I as Inventory Service",
            "participant D as Database",
            "participant Q as Queue",
            "participant N as Notification Service",
            "U->>W: Open checkout",
            "alt Payment approved",
            "P-->>A: Approved",
            "else Payment declined",
            "P-->>A: Declined",
            "end",
        ];
        let rendered = render(&source, 240, "", &default_theme()).unwrap();
        let text = text(&rendered);
        assert!(!text.lines().next().unwrap().contains(" │ "), "{text}");
        assert!(text.contains("Open checkout"), "{text}");
        assert!(text.contains("┌─ alt Payment approved"), "{text}");
        assert!(text.contains("├─ else Payment declined"), "{text}");
        assert!(text.contains("└─"), "{text}");
    }

    #[test]
    fn diagrams_never_exceed_the_requested_width() {
        let flowchart = [
            "flowchart LR",
            "A[A very long service name with detail] -->|a verbose operation| B[Another long service]",
        ];
        let sequence = [
            "sequenceDiagram",
            "participant Browser",
            "participant Application",
            "participant Database",
            "Browser->>Application: A request that is intentionally long",
            "Application->>Database: query",
        ];
        for source in [&flowchart[..], &sequence[..]] {
            let lines = render(source, 32, "  ", &default_theme()).unwrap();
            assert!(
                lines.iter().all(|line| line.width() <= 32),
                "{:?}",
                text(&lines)
            );
        }
    }

    #[test]
    fn unsupported_mermaid_returns_none_for_source_fallback() {
        assert!(render(&["mindmap", "root((Orca))"], 80, "", &default_theme()).is_none());
        assert!(render(&["flowchart TD", "A:::"], 80, "", &default_theme()).is_none());
        assert!(render(
            &["sequenceDiagram", "A<<->>B: unsupported"],
            80,
            "",
            &default_theme()
        )
        .is_none());
    }

    #[test]
    fn markdown_dispatches_mermaid_and_preserves_unsupported_source() {
        let diagram = crate::view::markdown_lines(
            "```mermaid\nflowchart TD\nCLI[host] --> CORE[kernel]\n```",
            80,
            "  ",
        );
        let diagram_text = text(&diagram);
        assert!(diagram_text.contains("CLI · host"), "{diagram_text}");
        assert!(diagram_text.contains("CORE · kernel"), "{diagram_text}");
        assert!(diagram_text.contains("┌"), "{diagram_text}");
        assert!(diagram_text.contains("▼"), "{diagram_text}");
        assert!(!diagram_text.contains("flowchart TD"), "{diagram_text}");

        let fallback =
            crate::view::markdown_lines("```mermaid\nmindmap\nroot((Orca))\n```", 80, "  ");
        let fallback_text = text(&fallback);
        assert!(fallback_text.contains("│ mindmap"), "{fallback_text}");
        assert!(fallback_text.contains("│ root((Orca))"), "{fallback_text}");
    }

    #[test]
    fn unfinished_streaming_mermaid_keeps_one_stable_placeholder() {
        let first = crate::view::markdown_lines("```mermaid\nflowchart TD\nA[Start]", 80, "  ");
        let later =
            crate::view::markdown_lines("```mermaid\nflowchart TD\nA[Start] --> B[Next]", 80, "  ");
        assert_eq!(text(&first), text(&later));
        assert_eq!(first.len(), 1);
        assert!(text(&first).contains("waiting for closing fence"));

        let complete = crate::view::markdown_lines(
            "```mermaid\nflowchart TD\nA[Start] --> B[Next]\n```",
            80,
            "  ",
        );
        let complete_text = text(&complete);
        assert!(!complete_text.contains("waiting for closing fence"));
        assert!(complete_text.contains("A · Start"), "{complete_text}");
        assert!(complete_text.contains("B · Next"), "{complete_text}");
    }

    #[test]
    fn flowchart_renders_feedback_loops_instead_of_falling_back_to_source() {
        let source = [
            "flowchart TD",
            "H[Host / caller]",
            "A[Agent]",
            "C[Context]",
            "L[agent_loop]",
            "M[Model]",
            "D[Dispatcher]",
            "T[Tool(s)]",
            "E[ExtensionRegistry]",
            "H --> A",
            "A --> C",
            "A --> L",
            "L --> E",
            "L --> M",
            "M -->|final| L",
            "M -->|tool calls| D",
            "D --> T",
            "T --> D",
            "D --> C",
            "L --> H",
        ];
        let rendered = render(&source, 120, "", &default_theme()).unwrap();
        let text = text(&rendered);
        for needle in [
            "H · Host / caller",
            "M · Model",
            "D · Dispatcher",
            "T · Tool(s)",
            "final",
            "tool calls",
            "▲",
        ] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
        assert!(!text.contains("flowchart TD"), "{text}");
        assert!(rendered.iter().all(|line| line.width() <= 120), "{text}");
    }

    #[test]
    fn flowchart_accepts_nested_subgraphs_and_local_direction() {
        let source = [
            "flowchart TB",
            "subgraph Outside[\"Surrounding system\"]",
            "CP[\"Control plane\\nscheduling · sessions · networking · tenancy · fleet\"]",
            "end",
            "subgraph Workspace[\"orca-harness Rust workspace\"]",
            "direction TB",
            "CLI[\"crates/cli\\norcacode terminal host\"]",
            "SDK[\"crates/sdk\\nembedding API\"]",
            "CORE[\"crates/harness-core\\nagent loop · dispatcher · limits · extensions hooks\"]",
            "end",
            "CP --> CLI",
            "CLI --> CORE",
            "CLI --> SDK",
            "SDK --> CORE",
            "CORE -->|\"model responses\"| CORE",
            "style CORE fill:#1f2937,color:#fff,stroke:#60a5fa,stroke-width:2px",
        ];
        let rendered = render(&source, 160, "", &default_theme()).unwrap();
        let text = text(&rendered);
        for needle in [
            "CP · Control plane / scheduling",
            "CLI · crates/cli / orcacode terminal host",
            "CORE · crates/harness-core / agent loop",
            "model responses",
            "▲",
        ] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
        assert!(!text.contains("direction TB"), "{text}");
        assert!(!text.contains("style CORE"), "{text}");
        assert!(rendered.iter().all(|line| line.width() <= 160), "{text}");
    }
}
