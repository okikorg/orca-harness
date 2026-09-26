//! Where the inspector sits in split view, and its pane chrome.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Borders, Padding};

use super::super::super::ViewMode;

/// Breathing room between inspector content and the terminal edge. The
/// renderer asks the padded block for its inner width, so previews wrap to
/// the real content box rather than compensating with scattered subtraction.
const INSPECTOR_PADDING: Padding = Padding::right(1);
/// Narrowest terminal that fits the inspector beside the transcript.
pub(crate) const SPLIT_MIN_WIDTH: usize = 100;
/// Shortest terminal that fits the inspector under the transcript.
const STACK_MIN_HEIGHT: usize = 18;

/// Where the inspector goes in split view. Geometry is decided here once;
/// the transcript wrap width and the wheel routing both follow it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SplitKind {
    Off,
    /// Inspector on the right, 42% of the width.
    SideBySide,
    /// Too narrow for a second column: inspector under the transcript.
    Stacked,
}

impl SplitKind {
    pub(crate) fn side_by_side(mode: ViewMode, width: usize) -> bool {
        mode == ViewMode::Split && width >= SPLIT_MIN_WIDTH
    }

    pub(crate) fn for_area(mode: ViewMode, width: usize, height: usize) -> Self {
        if Self::side_by_side(mode, width) {
            Self::SideBySide
        } else if mode == ViewMode::Split && height >= STACK_MIN_HEIGHT {
            Self::Stacked
        } else {
            Self::Off
        }
    }

    /// `[conversation, inspector]`; both are the whole area when off.
    pub(super) fn areas(self, area: Rect) -> [Rect; 2] {
        match self {
            Self::SideBySide => self.areas_at(area, 58),
            Self::Stacked => self.areas_at(area, 60),
            Self::Off => [area, area],
        }
    }

    /// `[first, second]` with `first_percent` of the width (side by side)
    /// or the height (stacked) going to the first pane.
    pub(crate) fn areas_at(self, area: Rect, first_percent: u16) -> [Rect; 2] {
        let constraints = [
            Constraint::Percentage(first_percent),
            Constraint::Percentage(100 - first_percent),
        ];
        match self {
            Self::SideBySide => Layout::horizontal(constraints).areas(area),
            Self::Stacked => Layout::vertical(constraints).areas(area),
            Self::Off => [area, area],
        }
    }

    /// The second pane's chrome: a divider on the side it shares with the
    /// first pane and a padded outer edge.
    pub(crate) fn block(self, border_style: ratatui::style::Style) -> Block<'static> {
        let borders = match self {
            Self::Stacked => Borders::TOP,
            Self::SideBySide | Self::Off => Borders::LEFT,
        };
        Block::default()
            .borders(borders)
            .border_style(border_style)
            .padding(INSPECTOR_PADDING)
    }

    pub(crate) fn content_width(self, area: Rect) -> usize {
        self.block(ratatui::style::Style::default())
            .inner(area)
            .width as usize
    }
}
