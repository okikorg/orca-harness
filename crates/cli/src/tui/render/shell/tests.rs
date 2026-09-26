use super::*;
use ratatui::layout::Rect;

#[test]
fn every_mode_takes_its_own_colour_without_a_fill() {
    let t = theme();
    let render = |mode: &str| StatusBar::new().push(mode_label(mode)).line(40, t.dim);
    let plan = render("plan · 1 plan");
    assert!(plan.to_string().starts_with(" plan · 1 plan"), "{plan}");
    let style_of = |mode: &str| {
        render(mode)
            .spans
            .iter()
            .find(|s| s.content == mode)
            .map(|s| s.style)
            .unwrap()
    };
    let styles: Vec<_> = ["plan", "orchestrate", "auto", "yolo"]
        .into_iter()
        .map(style_of)
        .collect();
    assert!(styles.iter().all(|style| style.bg.is_none()), "no fill");
    for (index, style) in styles.iter().enumerate() {
        assert!(
            styles[index + 1..].iter().all(|other| other.fg != style.fg),
            "modes share a colour: {styles:?}"
        );
    }
    assert_eq!(render("").to_string().trim(), "");
}

#[test]
fn every_colour_theme_gives_the_modes_distinct_colours() {
    use crate::view::ThemeName;
    for name in ThemeName::ALL
        .into_iter()
        .filter(|name| *name != ThemeName::Mono)
    {
        let m = crate::view::theme_for(name).modes;
        let fgs = [m.plan.fg, m.orchestrate.fg, m.auto.fg, m.yolo.fg];
        for (index, fg) in fgs.iter().enumerate() {
            assert!(fg.is_some());
            assert!(!fgs[index + 1..].contains(fg), "{name:?}: {fgs:?}");
        }
    }
}

#[test]
fn the_context_meter_takes_its_level_colour() {
    let t = theme();
    let spans = |tokens| context_spans(tokens, Some(100), false);
    let joined = |tokens| {
        spans(tokens)
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    };
    for tokens in [30, 75, 95] {
        assert_eq!(joined(tokens), context_segment(tokens, Some(100), false));
    }
    let colour = |tokens| spans(tokens)[0].style;
    if glyphs().meter.is_some() {
        assert_eq!(colour(30), t.accent);
        assert_eq!(colour(75), t.warn);
        assert_eq!(colour(95), t.error);
    } else {
        assert_eq!(colour(30), t.dim);
        assert_eq!(colour(95), t.error);
    }
}

#[test]
fn a_shorter_hint_drops_its_last_piece_only() {
    assert_eq!(shorter_hint("a · b · c"), "a · b");
    assert_eq!(shorter_hint("alone"), "alone");
}

#[test]
fn inspector_content_width_comes_from_the_padded_block() {
    let area = Rect::new(0, 0, 50, 20);
    let block = SplitKind::SideBySide.block(ratatui::style::Style::default());
    assert_eq!(
        SplitKind::SideBySide.content_width(area),
        block.inner(area).width as usize
    );
    assert_eq!(block.inner(area).right(), area.right() - 1);
}

#[test]
fn split_falls_back_to_stacking_on_narrow_terminals() {
    assert_eq!(
        SplitKind::for_area(ViewMode::Split, 120, 24),
        SplitKind::SideBySide
    );
    assert_eq!(
        SplitKind::for_area(ViewMode::Split, 90, 30),
        SplitKind::Stacked
    );
    assert_eq!(SplitKind::for_area(ViewMode::Split, 90, 12), SplitKind::Off);
    assert_eq!(
        SplitKind::for_area(ViewMode::Classic, 200, 60),
        SplitKind::Off
    );
    let [top, bottom] = SplitKind::Stacked.areas(Rect::new(0, 0, 90, 30));
    assert_eq!(top.width, 90);
    assert_eq!(top.height + bottom.height, 30);
}
