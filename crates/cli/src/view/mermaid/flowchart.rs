mod layout;
mod parser;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use self::{
    layout::{Layout, Position},
    parser::parse_connector,
};
use super::normalize_label;
use crate::view::{cell_width, truncate_line, Theme};

const LAYER_HEIGHT: usize = 7;
const MIN_NODE_WIDTH: usize = 7;

#[derive(Clone, Copy, Default)]
enum NodeShape {
    #[default]
    Box,
    Rounded,
    Decision,
}

#[derive(Clone)]
struct Node {
    id: String,
    label: String,
    shape: NodeShape,
}

#[derive(Clone, Copy)]
enum EdgeKind {
    Arrow,
    DashedArrow,
    Bidirectional,
    Cross,
    Circle,
    Plain,
}

impl EdgeKind {
    fn dashed(self) -> bool {
        matches!(self, Self::DashedArrow)
    }

    fn target(self) -> Option<&'static str> {
        match self {
            Self::Arrow | Self::DashedArrow | Self::Bidirectional => Some("▼"),
            Self::Cross => Some("×"),
            Self::Circle => Some("○"),
            Self::Plain => None,
        }
    }
}

struct Edge {
    from: usize,
    to: usize,
    label: Option<String>,
    kind: EdgeKind,
}

#[derive(Default)]
pub(super) struct Flowchart {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl Flowchart {
    pub(super) fn parse(source: &[&str]) -> Option<Self> {
        let mut chart = Self::default();
        for raw in source {
            let statement = raw.trim().trim_end_matches(';').trim();
            if statement.is_empty() || statement.starts_with("%%") {
                continue;
            }
            if is_ignored_directive(statement) {
                continue;
            }
            if chart.parse_edge_chain(statement) {
                continue;
            }
            let node = parse_node(statement)?;
            if !statement[node.consumed..].trim().is_empty() {
                return None;
            }
            chart.upsert_node(node);
        }
        (!chart.nodes.is_empty()).then_some(chart)
    }

    fn parse_edge_chain(&mut self, statement: &str) -> bool {
        let Some(first) = parse_node(statement) else {
            return false;
        };
        let mut from = self.upsert_node(first.clone());
        let mut rest = &statement[first.consumed..];
        let mut parsed_edge = false;
        loop {
            let Some(connector) = parse_connector(rest) else {
                return parsed_edge && rest.trim().is_empty();
            };
            let Some(target) = parse_node(connector.rest) else {
                return false;
            };
            let to = self.upsert_node(target.clone());
            self.edges.push(Edge {
                from,
                to,
                label: connector.label,
                kind: connector.kind,
            });
            parsed_edge = true;
            from = to;
            rest = &connector.rest[target.consumed..];
            if rest.trim().is_empty() {
                return true;
            }
        }
    }

    fn upsert_node(&mut self, parsed: ParsedNode) -> usize {
        if let Some(index) = self.nodes.iter().position(|node| node.id == parsed.id) {
            if let Some(label) = parsed.label {
                self.nodes[index].label = label;
                self.nodes[index].shape = parsed.shape;
            }
            return index;
        }
        let label = parsed.label.unwrap_or_else(|| parsed.id.clone());
        self.nodes.push(Node {
            id: parsed.id,
            label,
            shape: parsed.shape,
        });
        self.nodes.len() - 1
    }

    pub(super) fn render(
        &self,
        width: usize,
        indent: &str,
        theme: &Theme,
    ) -> Option<Vec<Line<'static>>> {
        let layout = Layout::new(self, width.saturating_sub(cell_width(indent)))?;
        let mut canvas = Canvas::new(layout.width, layout.height);
        for edge in &self.edges {
            canvas.draw_edge(
                edge,
                &layout.positions[edge.from],
                &layout.positions[edge.to],
            );
        }
        for (node, position) in self.nodes.iter().zip(&layout.positions) {
            canvas.draw_node(node, position);
        }
        Some(canvas.lines(indent, theme))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ink {
    Edge,
    EdgeLabel,
    Node,
    NodeText,
}

#[derive(Clone)]
struct Cell {
    connections: u8,
    dashed: bool,
    symbol: Option<String>,
    ink: Ink,
    continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            connections: 0,
            dashed: false,
            symbol: None,
            ink: Ink::Edge,
            continuation: false,
        }
    }
}

struct Canvas {
    width: usize,
    cells: Vec<Vec<Cell>>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            cells: vec![vec![Cell::default(); width]; height],
        }
    }

    fn draw_edge(&mut self, edge: &Edge, from: &Position, to: &Position) {
        let start_x = from.center();
        let end_x = to.center();
        let start_y = from.top + 2;
        let end_y = to.top;
        let dashed = edge.kind.dashed();
        let feedback = end_y <= start_y;
        let label_anchor = if feedback {
            let gutter = if start_x <= self.width / 2 {
                0
            } else {
                self.width - 1
            };
            let out_y = start_y + 1;
            let in_y = to.top + 3;
            self.vertical(start_x, start_y, out_y, dashed);
            self.horizontal(start_x, gutter, out_y, dashed);
            self.vertical(gutter, out_y, in_y, dashed);
            self.horizontal(gutter, end_x, in_y, dashed);
            (start_x, gutter, out_y)
        } else if end_y.saturating_sub(start_y) > LAYER_HEIGHT {
            let gutter = if start_x <= self.width / 2 {
                0
            } else {
                self.width - 1
            };
            let out_y = start_y + 2;
            let in_y = end_y.saturating_sub(2);
            self.vertical(start_x, start_y, out_y, dashed);
            self.horizontal(start_x, gutter, out_y, dashed);
            self.vertical(gutter, out_y, in_y, dashed);
            self.horizontal(gutter, end_x, in_y, dashed);
            self.vertical(end_x, in_y, end_y, dashed);
            (start_x, gutter, out_y)
        } else {
            let middle = (start_y + end_y) / 2;
            self.vertical(start_x, start_y, middle, dashed);
            self.horizontal(start_x, end_x, middle, dashed);
            self.vertical(end_x, middle, end_y, dashed);
            (start_x, end_x, middle)
        };
        if let Some(marker) = edge.kind.target() {
            let marker = if feedback && marker == "▼" {
                "▲"
            } else {
                marker
            };
            let marker_y = if feedback {
                to.top + 3
            } else {
                end_y.saturating_sub(1)
            };
            self.symbol(end_x, marker_y, marker, Ink::Edge);
        }
        if matches!(edge.kind, EdgeKind::Bidirectional) {
            self.symbol(start_x, start_y + 1, "▲", Ink::Edge);
        }
        if let Some(label) = edge.label.as_deref() {
            self.edge_label(label, label_anchor);
        }
    }

    fn draw_node(&mut self, node: &Node, position: &Position) {
        let right = position.left + position.width - 1;
        let bottom = position.top + 2;
        let corners = match node.shape {
            NodeShape::Box => ("┌", "┐", "└", "┘"),
            NodeShape::Rounded | NodeShape::Decision => ("╭", "╮", "╰", "╯"),
        };
        self.symbol(position.left, position.top, corners.0, Ink::Node);
        self.symbol(right, position.top, corners.1, Ink::Node);
        self.symbol(position.left, bottom, corners.2, Ink::Node);
        self.symbol(right, bottom, corners.3, Ink::Node);
        for x in position.left + 1..right {
            self.symbol(x, position.top, "─", Ink::Node);
            self.symbol(x, bottom, "─", Ink::Node);
        }
        self.symbol(position.left, position.top + 1, "│", Ink::Node);
        self.symbol(right, position.top + 1, "│", Ink::Node);
        if matches!(node.shape, NodeShape::Decision) {
            self.symbol(position.center(), position.top, "◇", Ink::Node);
            self.symbol(position.center(), bottom, "◇", Ink::Node);
        }
        let label = truncate_line(&node_text(node), position.width.saturating_sub(4));
        let label_left = position.center().saturating_sub(cell_width(&label) / 2);
        self.text(label_left, position.top + 1, &label, Ink::NodeText);
    }

    fn edge_label(&mut self, label: &str, (start, end, y): (usize, usize, usize)) {
        let distance = start.abs_diff(end);
        let max = if distance > 5 { distance - 2 } else { 14 };
        let label = truncate_line(label, max.min(18));
        let text = format!(" {label} ");
        let midpoint = (start + end) / 2;
        let mut left = midpoint.saturating_sub(cell_width(&text) / 2);
        if start == end {
            left = (start + 2).min(self.width.saturating_sub(cell_width(&text)));
        }
        self.text(left, y, &text, Ink::EdgeLabel);
    }

    fn vertical(&mut self, x: usize, start: usize, end: usize, dashed: bool) {
        for y in start.min(end)..start.max(end) {
            self.connection(x, y, 4, dashed);
            self.connection(x, y + 1, 1, dashed);
        }
    }

    fn horizontal(&mut self, start: usize, end: usize, y: usize, dashed: bool) {
        for x in start.min(end)..start.max(end) {
            self.connection(x, y, 2, dashed);
            self.connection(x + 1, y, 8, dashed);
        }
    }

    fn connection(&mut self, x: usize, y: usize, direction: u8, dashed: bool) {
        if let Some(cell) = self.cells.get_mut(y).and_then(|row| row.get_mut(x)) {
            cell.connections |= direction;
            cell.dashed |= dashed;
        }
    }

    fn symbol(&mut self, x: usize, y: usize, symbol: &str, ink: Ink) {
        if let Some(cell) = self.cells.get_mut(y).and_then(|row| row.get_mut(x)) {
            cell.symbol = Some(symbol.to_string());
            cell.ink = ink;
            cell.continuation = false;
        }
    }

    fn text(&mut self, mut x: usize, y: usize, text: &str, ink: Ink) {
        for character in text.chars() {
            let symbol = character.to_string();
            let width = cell_width(&symbol).max(1);
            if x + width > self.width {
                break;
            }
            self.symbol(x, y, &symbol, ink);
            for continuation in 1..width {
                self.cells[y][x + continuation].continuation = true;
            }
            x += width;
        }
    }

    fn lines(self, indent: &str, theme: &Theme) -> Vec<Line<'static>> {
        self.cells
            .iter()
            .map(|row| {
                let last = row
                    .iter()
                    .rposition(|cell| cell.symbol.is_some() || cell.connections != 0)
                    .unwrap_or(0);
                let mut spans = vec![Span::raw(indent.to_string())];
                let mut text = String::new();
                let mut style = Style::default();
                for cell in &row[..=last] {
                    if cell.continuation {
                        continue;
                    }
                    let next_style = ink_style(cell.ink, theme);
                    if next_style != style && !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = next_style;
                    text.push_str(
                        cell.symbol
                            .as_deref()
                            .unwrap_or_else(|| connection_glyph(cell)),
                    );
                }
                if !text.is_empty() {
                    spans.push(Span::styled(text, style));
                }
                Line::from(spans)
            })
            .collect()
    }
}

fn node_text(node: &Node) -> String {
    if node.label == node.id {
        node.id.clone()
    } else {
        format!("{} · {}", node.id, node.label)
    }
}

fn ink_style(ink: Ink, theme: &Theme) -> Style {
    match ink {
        Ink::Edge => theme.accent,
        Ink::EdgeLabel => theme.code.remove_modifier(Modifier::ITALIC),
        Ink::Node => theme.strong,
        Ink::NodeText => theme.strong.add_modifier(Modifier::BOLD),
    }
}

fn connection_glyph(cell: &Cell) -> &'static str {
    match cell.connections {
        1 | 4 | 5 if cell.dashed => "┆",
        2 | 8 | 10 if cell.dashed => "┄",
        1 | 4 | 5 => "│",
        2 | 8 | 10 => "─",
        6 => "┌",
        12 => "┐",
        3 => "└",
        9 => "┘",
        7 => "├",
        13 => "┤",
        14 => "┬",
        11 => "┴",
        15 => "┼",
        _ => " ",
    }
}

fn is_ignored_directive(statement: &str) -> bool {
    let keyword = statement
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        keyword.as_str(),
        "class" | "classdef" | "style" | "linkstyle" | "click" | "subgraph" | "direction" | "end"
    )
}

#[derive(Clone)]
struct ParsedNode {
    id: String,
    label: Option<String>,
    shape: NodeShape,
    consumed: usize,
}

fn parse_node(input: &str) -> Option<ParsedNode> {
    let trimmed = input.trim_start();
    let leading = input.len() - trimmed.len();
    let id_end = trimmed
        .char_indices()
        .take_while(|(_, character)| {
            character.is_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    let id = trimmed[..id_end].to_string();
    let rest = &trimmed[id_end..];
    let Some(open) = rest.chars().next() else {
        return Some(ParsedNode {
            id,
            label: None,
            shape: NodeShape::Box,
            consumed: leading + id_end,
        });
    };
    let (close, shape) = match open {
        '[' => (']', NodeShape::Box),
        '(' => (')', NodeShape::Rounded),
        '{' => ('}', NodeShape::Decision),
        '>' => (']', NodeShape::Box),
        _ => {
            return Some(ParsedNode {
                id,
                label: None,
                shape: NodeShape::Box,
                consumed: leading + id_end,
            });
        }
    };
    let (label, shape_len) = delimited_label(rest, open, close)?;
    Some(ParsedNode {
        id,
        label: Some(normalize_label(label)),
        shape,
        consumed: leading + id_end + shape_len,
    })
}

fn delimited_label(input: &str, open: char, close: char) -> Option<(&str, usize)> {
    let mut depth = 0usize;
    for (index, character) in input.char_indices() {
        if character == open {
            depth += 1;
        } else if character == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some((&input[open.len_utf8()..index], index + close.len_utf8()));
            }
        }
    }
    None
}
