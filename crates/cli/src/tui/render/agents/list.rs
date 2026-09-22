//! Event-invalidated spawn tree, status projection, and terminal table cache.
use super::{status_label, status_mark, transcript_elapsed};
use crate::tui::components::tree::TreeBranch;
use crate::tui::format::elapsed_label;
use crate::tui::state::{AgentTab, App, SubagentTranscriptStatus};
use crate::view;
use ratatui::text::Span;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentTreeRow {
    pub(crate) id: u64,
    pub(crate) prefix: String,
}

/// Membership, topology, status, and static labels change on events, not redraws.
/// Live elapsed labels are refreshed separately without rebuilding the tree.
pub(crate) struct AgentListCache {
    pub(crate) rows: Vec<AgentTreeRow>,
    labels: HashMap<u64, [String; 2]>,
    statuses: HashMap<u64, SubagentTranscriptStatus>,
    pub(super) counts: [usize; 4],
    style: crate::view::glyphs::UiStyle,
}

/// Fully formatted terminal lists survive scrolling, selection, and spinner redraws.
/// Lists containing live timers are formatted afresh while sharing topology/labels.
pub(crate) struct AgentTableCache {
    projection: Arc<AgentListCache>,
    tab: AgentTab,
    width: usize,
    theme: view::ThemeName,
    pub(crate) rows: Arc<Vec<Vec<Span<'static>>>>,
    created: std::time::Instant,
    terminal: bool,
}

pub(super) fn agent_table(
    app: &mut App,
    projection: &Arc<AgentListCache>,
    rows: &[AgentTreeRow],
    width: usize,
) -> Arc<Vec<Vec<Span<'static>>>> {
    let browser = app.agent_browser.as_ref().expect("browser open");
    let tab = browser.tab;
    let theme = view::theme_name();
    {
        if let Some(cache) = &browser.table_cache {
            if Arc::ptr_eq(&cache.projection, projection)
                && cache.tab == tab
                && cache.width == width
                && cache.theme == theme
                && (cache.terminal || cache.created.elapsed() < std::time::Duration::from_secs(1))
            {
                return Arc::clone(&cache.rows);
            }
        }
    }
    let terminal = rows
        .iter()
        .all(|row| !projection.statuses[&row.id].is_active());
    let labels = rows.iter().map(|row| {
        let mut labels = projection.labels[&row.id].clone();
        if projection.statuses[&row.id].is_active() {
            let transcript = &app.subagent_transcripts[&row.id];
            labels[1] = format!(
                "{} · {}",
                status_label(transcript.status),
                elapsed_label(transcript_elapsed(transcript))
            );
        }
        labels
    });
    let table = Arc::new(
        labels
            .map(|[title, metadata]| {
                let metadata = format!(" · {metadata}");
                let title_width = width.saturating_sub(4 + view::cell_width(&metadata));
                vec![
                    Span::raw(view::truncate_line(&title, title_width)),
                    Span::styled(metadata, view::theme().dim),
                ]
            })
            .collect(),
    );
    app.agent_browser
        .as_mut()
        .expect("browser open")
        .table_cache = Some(AgentTableCache {
        created: std::time::Instant::now(),
        terminal,
        projection: Arc::clone(projection),
        tab,
        width,
        theme,
        rows: Arc::clone(&table),
    });
    table
}

pub(super) fn agent_list_projection(app: &App) -> Arc<AgentListCache> {
    let style = crate::view::glyphs::ui_style();
    if let Some(cache) = app
        .agent_list_cache
        .borrow()
        .as_ref()
        .filter(|cache| cache.style == style)
    {
        return Arc::clone(cache);
    }
    let rows = build_agent_tree_rows(app);
    let mut labels = HashMap::new();
    let mut statuses = HashMap::new();
    let mut counts = [0; 4];
    for row in &rows {
        let transcript = &app.subagent_transcripts[&row.id];
        statuses.insert(row.id, transcript.status);
        counts[status_index(transcript.status)] += 1;
        labels.insert(
            row.id,
            [
                format!(
                    "{}{} #{} {}",
                    row.prefix,
                    status_mark(transcript.status),
                    transcript.id,
                    transcript.task
                ),
                format!(
                    "{} · {}",
                    status_label(transcript.status),
                    elapsed_label(transcript_elapsed(transcript))
                ),
            ],
        );
    }
    let cache = Arc::new(AgentListCache {
        rows,
        labels,
        statuses,
        counts,
        style,
    });
    *app.agent_list_cache.borrow_mut() = Some(Arc::clone(&cache));
    cache
}

fn status_index(status: SubagentTranscriptStatus) -> usize {
    match status {
        SubagentTranscriptStatus::Queued => 0,
        SubagentTranscriptStatus::Running => 1,
        SubagentTranscriptStatus::Idle
        | SubagentTranscriptStatus::Stopped
        | SubagentTranscriptStatus::Completed => 2,
        SubagentTranscriptStatus::Failed => 3,
    }
}

#[cfg(test)]
pub(crate) fn agent_tree_rows(app: &App) -> Vec<AgentTreeRow> {
    agent_list_projection(app).rows.clone()
}

/// Stable pre-order forest. Parent links, rather than spawn-id order or a
/// guessed indentation, keep concurrent root trees and deep descendants paired.
fn build_agent_tree_rows(app: &App) -> Vec<AgentTreeRow> {
    let ids = app
        .subagent_transcripts
        .keys()
        .copied()
        .collect::<HashSet<_>>();
    let mut children = HashMap::<Option<u64>, Vec<u64>>::new();
    for transcript in app.subagent_transcripts.values() {
        let parent = transcript.parent_id.filter(|parent| ids.contains(parent));
        children.entry(parent).or_default().push(transcript.id);
    }
    for siblings in children.values_mut() {
        siblings.sort_unstable();
    }

    let mut rows = Vec::with_capacity(ids.len());
    let mut visited = HashSet::new();
    let roots = children.get(&None).cloned().unwrap_or_default();
    for root in roots {
        push_tree(
            root,
            String::new(),
            None,
            &children,
            &mut visited,
            &mut rows,
        );
    }
    // Defensive orphan/cycle fallback: every retained transcript stays selectable.
    let mut remaining = ids.into_iter().collect::<Vec<_>>();
    remaining.sort_unstable();
    for id in remaining {
        if !visited.contains(&id) {
            push_tree(id, String::new(), None, &children, &mut visited, &mut rows);
        }
    }
    rows
}

fn push_tree(
    id: u64,
    ancestor_prefix: String,
    branch_is_last: Option<bool>,
    children: &HashMap<Option<u64>, Vec<u64>>,
    visited: &mut HashSet<u64>,
    rows: &mut Vec<AgentTreeRow>,
) {
    if !visited.insert(id) {
        return;
    }
    let branch = branch_is_last.map(|last| TreeBranch {
        indent: &ancestor_prefix,
        last,
    });
    let prefix = branch
        .as_ref()
        .map_or_else(|| ancestor_prefix.clone(), TreeBranch::prefix);
    rows.push(AgentTreeRow { id, prefix });

    let child_prefix = branch.as_ref().map_or_else(
        || ancestor_prefix.clone(),
        |branch| format!("{} ", branch.child_indent()),
    );
    let siblings = children
        .get(&Some(id))
        .map(Vec::as_slice)
        .unwrap_or_default();
    let count = siblings.len();
    for (index, &child) in siblings.iter().enumerate() {
        push_tree(
            child,
            child_prefix.clone(),
            Some(index + 1 == count),
            children,
            visited,
            rows,
        );
    }
}

pub(crate) fn agent_ids(app: &App) -> Vec<u64> {
    agent_browser_rows(app)
        .into_iter()
        .map(|row| row.id)
        .collect()
}

/// The spawn tree narrowed to the browser's tab, in tree order. Before
/// the browser opens the default tab applies, so the picker it opens
/// with is sized for what it will show.
pub(super) fn agent_browser_rows(app: &App) -> Vec<AgentTreeRow> {
    let tab = app
        .agent_browser
        .as_ref()
        .map_or_else(AgentTab::default, |browser| browser.tab);
    agent_rows_on(app, tab)
}

pub(super) fn agent_rows_on(app: &App, tab: AgentTab) -> Vec<AgentTreeRow> {
    let projection = agent_list_projection(app);
    projection
        .rows
        .iter()
        .filter(|row| tab.admits(projection.statuses[&row.id]))
        .cloned()
        .collect()
}
