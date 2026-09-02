use std::collections::VecDeque;

use super::{node_text, Flowchart, LAYER_HEIGHT, MIN_NODE_WIDTH};
use crate::view::cell_width;

#[derive(Clone, Copy, Default)]
pub(super) struct Position {
    pub(super) left: usize,
    pub(super) top: usize,
    pub(super) width: usize,
}

impl Position {
    pub(super) fn center(self) -> usize {
        self.left + self.width / 2
    }
}

pub(super) struct Layout {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) positions: Vec<Position>,
}

impl Layout {
    pub(super) fn new(chart: &Flowchart, available: usize) -> Option<Self> {
        let width = (available >= MIN_NODE_WIDTH + 4).then_some(available)?;
        let inner = width.saturating_sub(4);
        let layers = dependency_layers(chart);
        let mut positions = vec![Position::default(); chart.nodes.len()];
        for (level, nodes) in layers.iter().enumerate() {
            let slot = inner / nodes.len().max(1);
            if slot < MIN_NODE_WIDTH {
                return None;
            }
            for (column, node_index) in nodes.iter().enumerate() {
                let node = &chart.nodes[*node_index];
                let desired = cell_width(&node_text(node)) + 4;
                let maximum = slot.saturating_sub(1).max(MIN_NODE_WIDTH);
                let node_width = desired.clamp(MIN_NODE_WIDTH, maximum);
                let center = 2 + column * slot + slot / 2;
                positions[*node_index] = Position {
                    left: center.saturating_sub(node_width / 2),
                    top: level * LAYER_HEIGHT,
                    width: node_width,
                };
            }
        }
        let feedback_margin = chart
            .edges
            .iter()
            .any(|edge| positions[edge.from].top >= positions[edge.to].top)
            as usize;
        Some(Self {
            width,
            height: layers.len().saturating_sub(1) * LAYER_HEIGHT + 3 + feedback_margin,
            positions,
        })
    }
}

fn dependency_layers(chart: &Flowchart) -> Vec<Vec<usize>> {
    let mut graph = vec![Vec::new(); chart.nodes.len()];
    for edge in &chart.edges {
        if edge.from != edge.to && !reachable(edge.to, edge.from, &graph) {
            graph[edge.from].push(edge.to);
        }
    }
    let mut incoming = vec![0usize; chart.nodes.len()];
    for targets in &graph {
        for target in targets {
            incoming[*target] += 1;
        }
    }
    let mut queue: VecDeque<usize> = incoming
        .iter()
        .enumerate()
        .filter_map(|(node, count)| (*count == 0).then_some(node))
        .collect();
    let mut levels = vec![0usize; chart.nodes.len()];
    while let Some(node) = queue.pop_front() {
        for target in &graph[node] {
            levels[*target] = levels[*target].max(levels[node] + 1);
            incoming[*target] -= 1;
            if incoming[*target] == 0 {
                queue.push_back(*target);
            }
        }
    }
    let mut layers = vec![Vec::new(); levels.iter().copied().max().unwrap_or(0) + 1];
    for (node, level) in levels.into_iter().enumerate() {
        layers[level].push(node);
    }
    layers
}

fn reachable(start: usize, target: usize, graph: &[Vec<usize>]) -> bool {
    let mut seen = vec![false; graph.len()];
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        if node == target {
            return true;
        }
        if !seen[node] {
            seen[node] = true;
            stack.extend(&graph[node]);
        }
    }
    false
}
