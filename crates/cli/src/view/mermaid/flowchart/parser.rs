use super::EdgeKind;
use crate::view::mermaid::normalize_label;

pub(super) struct ParsedConnector<'a> {
    pub(super) rest: &'a str,
    pub(super) label: Option<String>,
    pub(super) kind: EdgeKind,
}

pub(super) fn parse_connector(input: &str) -> Option<ParsedConnector<'_>> {
    let input = input.trim_start();
    for (marker, kind) in [
        ("<-->", EdgeKind::Bidirectional),
        ("-.->", EdgeKind::DashedArrow),
        ("==>", EdgeKind::Arrow),
        ("-->", EdgeKind::Arrow),
        ("--x", EdgeKind::Cross),
        ("--o", EdgeKind::Circle),
        ("---", EdgeKind::Plain),
    ] {
        if let Some(mut rest) = input.strip_prefix(marker) {
            let mut label = None;
            rest = rest.trim_start();
            if let Some(pipe) = rest.strip_prefix('|') {
                let end = pipe.find('|')?;
                label = Some(normalize_label(&pipe[..end]));
                rest = &pipe[end + 1..];
            }
            return Some(ParsedConnector { rest, label, kind });
        }
    }
    for (open, close, kind) in [
        ("--", "-->", EdgeKind::Arrow),
        ("-.", ".->", EdgeKind::DashedArrow),
        ("==", "==>", EdgeKind::Arrow),
    ] {
        let Some(rest) = input.strip_prefix(open) else {
            continue;
        };
        if let Some(end) = rest.find(close) {
            let label = normalize_label(&rest[..end]);
            return Some(ParsedConnector {
                rest: &rest[end + close.len()..],
                label: (!label.is_empty()).then_some(label),
                kind,
            });
        }
    }
    None
}
