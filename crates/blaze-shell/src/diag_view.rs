//! Thin GPU renderer for structured diagnostic cards & inline Markdown backed by the pure
//! `#![no_std]` sans-IO parser crate [`blaze_diag_parse`].

use blaze_own::Alloc;

pub use blaze_diag_parse::{
    ClassifiedLine, DiagSeverity, MdInline, classify_line, looks_like_diagnostic_output,
    looks_like_file_path, parse_inline_markdown_in, split_trailing_source_tag,
};

use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, HoverKind, Scene, TextFace};

fn severity_palette(sev: DiagSeverity) -> ([f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
    // (card_bg, rail_col, badge_bg, badge_fg)
    match sev {
        DiagSeverity::Error => (
            theme::rgba(0xe0, 0x6c, 0x75, 0.08),
            theme::DANGER,
            theme::rgba(0xe0, 0x6c, 0x75, 0.18),
            theme::DANGER,
        ),
        DiagSeverity::Warning => (
            theme::rgba(0xe5, 0xb5, 0x67, 0.075),
            theme::rgb(0xe5, 0xb5, 0x67),
            theme::rgba(0xe5, 0xb5, 0x67, 0.18),
            theme::rgb(0xe5, 0xb5, 0x67),
        ),
        DiagSeverity::Note => (
            theme::rgba(0x58, 0x9d, 0xf6, 0.065),
            theme::SERIES_BLUE,
            theme::rgba(0x58, 0x9d, 0xf6, 0.16),
            theme::SERIES_BLUE,
        ),
        DiagSeverity::Help => (
            theme::rgba(0x4c, 0xc3, 0x8a, 0.065),
            theme::OK,
            theme::rgba(0x4c, 0xc3, 0x8a, 0.16),
            theme::OK,
        ),
        DiagSeverity::Info => (
            theme::rgba(0x9a, 0xa0, 0xaa, 0.05),
            theme::INK_4,
            theme::rgba(0x9a, 0xa0, 0xaa, 0.14),
            theme::INK_2,
        ),
    }
}

/// Flow and render inline-Markdown monospace prose + clickable `` `code` `` chips with clean
/// word-boundary wrapping. Returns vertical height consumed in logical pixels.
#[allow(clippy::too_many_arguments)]
pub fn layout_inline_markdown(
    scene: &mut Scene,
    text: &str,
    x0: f32,
    y0: f32,
    max_w: f32,
    first_line_indent: f32,
    first_line_right_reserve: f32,
    base_color: [f32; 4],
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let spans = parse_inline_markdown_in(text, Alloc::global());
    if spans.is_empty() {
        return m.ch;
    }

    enum Atom {
        Word {
            text: String,
            color: [f32; 4],
            bold: bool,
            italic: bool,
            underline: bool,
            copy: Option<String>,
        },
        CodeChip {
            text: String,
            is_path: bool,
        },
        Space,
    }

    fn push_prose_words(
        s: &str,
        color: [f32; 4],
        bold: bool,
        italic: bool,
        underline: bool,
        copy: Option<String>,
        out: &mut Vec<Atom>,
    ) {
        let mut cur = String::new();
        for ch in s.chars() {
            if ch.is_whitespace() {
                if !cur.is_empty() {
                    out.push(Atom::Word {
                        text: std::mem::take(&mut cur),
                        color,
                        bold,
                        italic,
                        underline,
                        copy: copy.clone(),
                    });
                }
                if !matches!(out.last(), Some(Atom::Space)) {
                    out.push(Atom::Space);
                }
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            out.push(Atom::Word {
                text: cur,
                color,
                bold,
                italic,
                underline,
                copy,
            });
        }
    }

    let mut atoms: Vec<Atom> = Vec::new();
    for sp in spans.iter() {
        match sp {
            MdInline::Text(t) => push_prose_words(t.as_str(), base_color, false, false, false, None, &mut atoms),
            MdInline::Bold(t) => push_prose_words(t.as_str(), theme::INK_1, true, false, false, None, &mut atoms),
            MdInline::Italic(t) => push_prose_words(t.as_str(), base_color, false, true, false, None, &mut atoms),
            MdInline::Link { label, url } => {
                push_prose_words(
                    label.as_str(),
                    theme::SERIES_BLUE,
                    false,
                    false,
                    true,
                    Some(url.as_str().to_owned()),
                    &mut atoms,
                );
            }
            MdInline::Code(code) => {
                let s = code.as_str();
                let is_path = looks_like_file_path(s);
                atoms.push(Atom::CodeChip {
                    text: s.to_owned(),
                    is_path,
                });
            }
        }
    }

    let line_h = m.ch + 4.0;
    let space_w = m.cw;
    let chip_pad_x = 4.5f32;

    let mut line_idx = 0usize;
    let mut cur_x = x0 + first_line_indent;
    let mut cur_y = y0;
    let line_limit = |idx: usize| -> f32 {
        let reserve = if idx == 0 { first_line_right_reserve } else { 0.0 };
        (x0 + (max_w - reserve).max(80.0)).max(x0 + first_line_indent + 40.0)
    };
    let mut at_line_start = first_line_indent <= 0.5;

    for atom in atoms {
        match atom {
            Atom::Space => {
                if !at_line_start {
                    cur_x += space_w;
                }
            }
            Atom::CodeChip { text, is_path } => {
                let avail_full = (max_w - 8.0).max(60.0);
                let max_chars = ((avail_full - chip_pad_x * 2.0) / m.cw).floor().max(6.0) as usize;
                let shown = if text.chars().count() > max_chars {
                    crate::view::truncate_middle(&text, max_chars)
                } else {
                    text.clone()
                };
                let text_w = shown.chars().count() as f32 * m.cw;
                let chip_w = text_w + chip_pad_x * 2.0;

                if !at_line_start && cur_x + chip_w > line_limit(line_idx) {
                    line_idx += 1;
                    cur_y += line_h;
                    cur_x = x0;
                }
                let chip_bg = if is_path {
                    theme::rgba(0x58, 0x9d, 0xf6, 0.12)
                } else {
                    theme::BG_CHIP
                };
                let chip_border = if is_path {
                    theme::rgba(0x58, 0x9d, 0xf6, 0.34)
                } else {
                    theme::LINE
                };
                let chip_fg = if is_path { theme::SERIES_BLUE } else { theme::INK_1 };
                scene.round_rect(
                    cur_x,
                    cur_y - 1.0,
                    cur_x + chip_w,
                    cur_y + m.ch + 1.0,
                    4.0,
                    chip_bg,
                    1.0,
                    chip_border,
                    clip_id,
                );
                scene.text(cur_x + chip_pad_x, cur_y, &shown, chip_fg, false, false, clip_id);
                let hit = HitRect::new(cur_x, cur_y - 1.0, cur_x + chip_w, cur_y + m.ch + 1.0);
                scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(text.clone()));
                scene.push_hover_clipped(
                    hit,
                    clip_id,
                    HoverKind::Tooltip {
                        title: text,
                        subtitle: Some("click to copy".to_owned()),
                    },
                );
                cur_x += chip_w;
                at_line_start = false;
            }
            Atom::Word { text, color, bold, italic, underline, copy } => {
                let is_attached_punct = text.len() <= 2
                    && text
                        .chars()
                        .all(|c| matches!(c, ':' | ',' | ';' | '.' | ')' | ']' | '!' | '?'));
                let w = text.chars().count() as f32 * m.cw;
                if !at_line_start && !is_attached_punct && cur_x + w > line_limit(line_idx) {
                    line_idx += 1;
                    cur_y += line_h;
                    cur_x = x0;
                }
                let max_line_w = (line_limit(line_idx) - cur_x).max(40.0);
                if w > max_line_w && w > (max_w - 8.0).max(40.0) {
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        let avail_cols = ((line_limit(line_idx) - cur_x).max(40.0) / m.cw)
                            .floor()
                            .max(1.0) as usize;
                        let take_b = rest
                            .char_indices()
                            .nth(avail_cols)
                            .map_or(rest.len(), |(bi, _)| bi);
                        let chunk = &rest[..take_b];
                        let cw = chunk.chars().count() as f32 * m.cw;
                        scene.text(cur_x, cur_y, chunk, color, bold, italic, clip_id);
                        cur_x += cw;
                        rest = &rest[take_b..];
                        if !rest.is_empty() {
                            line_idx += 1;
                            cur_y += line_h;
                            cur_x = x0;
                        }
                    }
                } else {
                    scene.text(cur_x, cur_y, &text, color, bold, italic, clip_id);
                    if underline {
                        scene.over_round_rect(
                            cur_x,
                            cur_y + m.ch - 1.5,
                            cur_x + w,
                            cur_y + m.ch - 0.5,
                            0.0,
                            color,
                            clip_id,
                        );
                    }
                    if let Some(target) = copy {
                        let hit = HitRect::new(cur_x, cur_y, cur_x + w, cur_y + m.ch);
                        scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(target.clone()));
                        scene.push_hover_clipped(
                            hit,
                            clip_id,
                            HoverKind::Tooltip {
                                title: target,
                                subtitle: Some("click to copy".to_owned()),
                            },
                        );
                    }
                    cur_x += w;
                }
                at_line_start = false;
            }
        }
    }

    (cur_y - y0) + m.ch
}

/// Render a contiguous group of uncolored `stderr` or diagnostic `stdout` lines as crisp
/// monospace diagnostic cards with severity rails, pill badges, clickable inline Markdown
/// `` `code` `` chips, and verbatim column alignment for unstructured CLI stderr.
#[allow(clippy::too_many_arguments)]
pub fn layout_stderr_diagnostic_group(
    scene: &mut Scene,
    lines: &[(i64, &str)],
    exit_code: i32,
    x0: f32,
    y0: f32,
    width: f32,
    first_card_right_reserve: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    struct Card<'a> {
        severity: DiagSeverity,
        badge: Option<&'a str>,
        id: Option<&'a str>,
        header_loc: Option<&'a str>,
        entries: Vec<CardEntry<'a>>,
    }
    enum CardEntry<'a> {
        Prose(&'a str),
        Verbatim(&'a str),
        SubNote { severity: DiagSeverity, badge: &'a str, body: &'a str },
        Location(&'a str),
        CodeFrame(&'a str),
        Summary { summary: &'a str, command: Option<&'a str> },
    }

    let has_inline_md = |s: &str| {
        parse_inline_markdown_in(s, Alloc::global())
            .iter()
            .any(|sp| !matches!(sp, MdInline::Text(_)))
    };

    let mut cards: Vec<Card<'_>> = Vec::new();
    for &(_, raw) in lines {
        if raw.trim().is_empty() {
            continue;
        }
        match classify_line(raw, exit_code) {
            ClassifiedLine::Header { severity, badge, id, location, body } => {
                let mut entries = Vec::new();
                if !body.is_empty() {
                    entries.push(CardEntry::Prose(body));
                }
                cards.push(Card {
                    severity,
                    badge: Some(badge),
                    id,
                    header_loc: location,
                    entries,
                });
            }
            ClassifiedLine::SubNote { severity, badge, body } => {
                if let Some(last) = cards.last_mut() {
                    last.entries.push(CardEntry::SubNote { severity, badge, body });
                } else {
                    cards.push(Card {
                        severity,
                        badge: Some(badge),
                        id: None,
                        header_loc: None,
                        entries: vec![CardEntry::Prose(body)],
                    });
                }
            }
            ClassifiedLine::Location(loc) => {
                if let Some(last) = cards.last_mut() {
                    last.entries.push(CardEntry::Location(loc));
                } else {
                    cards.push(Card {
                        severity: if exit_code != 0 { DiagSeverity::Error } else { DiagSeverity::Note },
                        badge: None,
                        id: None,
                        header_loc: None,
                        entries: vec![CardEntry::Location(loc)],
                    });
                }
            }
            ClassifiedLine::CodeFrame(frame) => {
                if let Some(last) = cards.last_mut() {
                    last.entries.push(CardEntry::CodeFrame(frame));
                } else {
                    cards.push(Card {
                        severity: if exit_code != 0 { DiagSeverity::Error } else { DiagSeverity::Note },
                        badge: None,
                        id: None,
                        header_loc: None,
                        entries: vec![CardEntry::CodeFrame(frame)],
                    });
                }
            }
            ClassifiedLine::Summary { has_errors, summary, command } => {
                cards.push(Card {
                    severity: if has_errors { DiagSeverity::Error } else { DiagSeverity::Help },
                    badge: None,
                    id: None,
                    header_loc: None,
                    entries: vec![CardEntry::Summary { summary, command }],
                });
            }
            ClassifiedLine::Plain(txt) => {
                let is_structured_card = cards.last().is_some_and(|c| c.badge.is_some());
                let entry = if is_structured_card || has_inline_md(txt) {
                    CardEntry::Prose(txt)
                } else {
                    CardEntry::Verbatim(txt)
                };
                if let Some(last) = cards.last_mut() {
                    last.entries.push(entry);
                } else {
                    cards.push(Card {
                        severity: if exit_code != 0 { DiagSeverity::Error } else { DiagSeverity::Info },
                        badge: None,
                        id: None,
                        header_loc: None,
                        entries: vec![entry],
                    });
                }
            }
        }
    }

    if cards.is_empty() {
        return 0.0;
    }

    let mut cur_y = y0;
    let x1 = x0 + width;
    for (ci, card) in cards.iter().enumerate() {
        if ci > 0 {
            cur_y += 6.0;
        }
        let card_y0 = cur_y;
        let (card_bg, rail_col, badge_bg, badge_fg) = severity_palette(card.severity);
        let bg_idx = scene.rects.len();
        scene.round_rect(x0, card_y0, x1, card_y0 + 24.0, 6.0, card_bg, 0.0, [0.0; 4], clip_id);
        let rail_idx = scene.rects.len();
        scene.round_rect(x0, card_y0, x0 + 2.5, card_y0 + 24.0, 1.25, rail_col, 0.0, [0.0; 4], clip_id);

        let inner_x = x0 + 12.0;
        let inner_w = (x1 - 12.0 - inner_x).max(80.0);
        let right_reserve = if ci == 0 { first_card_right_reserve } else { 0.0 };
        cur_y += 7.0;

        let mut badge_indent = 0.0f32;
        if let Some(badge) = card.badge {
            let bw = m.measure(badge, TextFace::UiSmall) + 12.0;
            let bh = m.ch - 1.0;
            scene.round_rect(
                inner_x,
                cur_y + 0.5,
                inner_x + bw,
                cur_y + 0.5 + bh,
                bh * 0.5,
                badge_bg,
                1.0,
                [rail_col[0], rail_col[1], rail_col[2], 0.40],
                clip_id,
            );
            scene.ui_small(inner_x + 6.0, cur_y + 0.5, badge, badge_fg, true, false, clip_id);
            let mut hx = inner_x + bw + 8.0;

            if let Some(id) = card.id {
                let id_w = id.chars().count() as f32 * m.cw + 10.0;
                scene.round_rect(
                    hx,
                    cur_y + 0.5,
                    hx + id_w,
                    cur_y + 0.5 + bh,
                    4.0,
                    theme::BG_CHIP,
                    1.0,
                    theme::LINE,
                    clip_id,
                );
                scene.text(hx + 5.0, cur_y, id, theme::INK_2, false, false, clip_id);
                let hit = HitRect::new(hx, cur_y, hx + id_w, cur_y + m.ch);
                scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(id.to_owned()));
                scene.push_hover_clipped(
                    hit,
                    clip_id,
                    HoverKind::Tooltip {
                        title: id.to_owned(),
                        subtitle: Some("click to copy fix id".to_owned()),
                    },
                );
                hx += id_w + 8.0;
            }

            if let Some(loc) = card.header_loc {
                let avail = (x1 - 12.0 - right_reserve - hx).max(40.0);
                let max_ch = (avail / m.cw).floor().max(8.0) as usize;
                let shown = crate::view::truncate_middle(loc, max_ch);
                let lw = shown.chars().count() as f32 * m.cw;
                scene.text(hx, cur_y, &shown, theme::SERIES_BLUE, false, false, clip_id);
                let hit = HitRect::new(hx, cur_y, hx + lw, cur_y + m.ch);
                scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(loc.to_owned()));
                scene.push_hover_clipped(
                    hit,
                    clip_id,
                    HoverKind::Tooltip {
                        title: loc.to_owned(),
                        subtitle: Some("click to copy location".to_owned()),
                    },
                );
            }

            if card.id.is_some() || card.header_loc.is_some() {
                cur_y += m.ch + 5.0;
            } else {
                badge_indent = bw + 8.0;
            }
        }

        let body_color = match card.severity {
            DiagSeverity::Error => theme::INK_1,
            DiagSeverity::Warning => theme::rgb(0xef, 0xd8, 0xb0),
            DiagSeverity::Note | DiagSeverity::Info => theme::INK_2,
            DiagSeverity::Help => theme::rgb(0xc6, 0xe6, 0xd4),
        };

        for (ei, entry) in card.entries.iter().enumerate() {
            let indent = if ei == 0 { badge_indent } else { 0.0 };
            let reserve = if ei == 0 && card.id.is_none() && card.header_loc.is_none() {
                right_reserve
            } else {
                0.0
            };
            match *entry {
                CardEntry::Summary { summary, command } => {
                    let mut sx = inner_x;
                    scene.text(sx, cur_y, summary, body_color, true, false, clip_id);
                    sx += summary.chars().count() as f32 * m.cw;
                    if let Some(cmd) = command {
                        sx += 10.0;
                        let avail = (x1 - 12.0 - reserve - sx - 10.0).max(40.0);
                        let max_ch = (avail / m.cw).floor().max(8.0) as usize;
                        let shown = crate::view::truncate_middle(cmd, max_ch);
                        let cw = shown.chars().count() as f32 * m.cw + 10.0;
                        scene.round_rect(
                            sx,
                            cur_y - 1.0,
                            sx + cw,
                            cur_y + m.ch + 1.0,
                            4.0,
                            theme::BG_CHIP,
                            1.0,
                            theme::LINE,
                            clip_id,
                        );
                        scene.text(sx + 5.0, cur_y, &shown, theme::INK_1, false, false, clip_id);
                        let hit = HitRect::new(sx, cur_y - 1.0, sx + cw, cur_y + m.ch + 1.0);
                        scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(cmd.to_owned()));
                        scene.push_hover_clipped(
                            hit,
                            clip_id,
                            HoverKind::Tooltip {
                                title: cmd.to_owned(),
                                subtitle: Some("click to copy command".to_owned()),
                            },
                        );
                    }
                    cur_y += m.ch + 3.0;
                }
                CardEntry::Verbatim(raw) => {
                    let avail_w = (inner_w - reserve).max(80.0);
                    let max_cols = (avail_w / m.cw).floor().max(16.0) as usize;
                    for seg in crate::view::wrap_lines(raw, max_cols) {
                        scene.text(inner_x, cur_y, seg, body_color, false, false, clip_id);
                        cur_y += m.ch + 2.0;
                    }
                }
                CardEntry::Prose(txt) => {
                    let (main_txt, src_tag) = split_trailing_source_tag(txt);
                    let h = layout_inline_markdown(
                        scene,
                        main_txt,
                        inner_x,
                        cur_y,
                        inner_w,
                        indent,
                        reserve,
                        body_color,
                        m,
                        clip_id,
                    );
                    cur_y += h + 3.0;
                    if let Some(tag) = src_tag {
                        let tw = tag.chars().count() as f32 * m.cw + 10.0;
                        let tx = (x1 - 12.0 - tw).max(inner_x);
                        let tag_y = cur_y - 1.0;
                        scene.round_rect(
                            tx,
                            tag_y,
                            tx + tw,
                            tag_y + m.ch - 1.0,
                            4.0,
                            theme::rgba(0x88, 0x90, 0x9e, 0.10),
                            1.0,
                            theme::LINE_FAINT,
                            clip_id,
                        );
                        scene.text(tx + 5.0, tag_y, tag, theme::INK_4, false, false, clip_id);
                        let hit = HitRect::new(tx, tag_y, tx + tw, tag_y + m.ch);
                        scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(tag.to_owned()));
                        cur_y += m.ch + 2.0;
                    }
                }
                CardEntry::SubNote { severity, badge, body } => {
                    let (_, sr, sbg, sfg) = severity_palette(severity);
                    let bw = m.measure(badge, TextFace::UiSmall) + 10.0;
                    let bh = m.ch - 2.0;
                    scene.round_rect(
                        inner_x,
                        cur_y + 1.0,
                        inner_x + bw,
                        cur_y + 1.0 + bh,
                        bh * 0.5,
                        sbg,
                        1.0,
                        [sr[0], sr[1], sr[2], 0.35],
                        clip_id,
                    );
                    scene.ui_small(inner_x + 5.0, cur_y + 1.0, badge, sfg, true, false, clip_id);
                    let h = layout_inline_markdown(
                        scene,
                        body,
                        inner_x,
                        cur_y,
                        inner_w,
                        bw + 7.0,
                        0.0,
                        theme::INK_2,
                        m,
                        clip_id,
                    );
                    cur_y += h + 3.0;
                }
                CardEntry::Location(loc) => {
                    scene.text(inner_x, cur_y, "→ ", theme::INK_4, false, false, clip_id);
                    let lx = inner_x + 2.0 * m.cw;
                    let max_ch = ((inner_w - 2.0 * m.cw) / m.cw).floor().max(10.0) as usize;
                    let shown = crate::view::truncate_middle(loc, max_ch);
                    let lw = shown.chars().count() as f32 * m.cw;
                    scene.text(lx, cur_y, &shown, theme::SERIES_BLUE, false, false, clip_id);
                    let hit = HitRect::new(lx, cur_y, lx + lw, cur_y + m.ch);
                    scene.push_hit_clipped(hit, clip_id, HitAction::CopyText(loc.to_owned()));
                    scene.push_hover_clipped(
                        hit,
                        clip_id,
                        HoverKind::Tooltip {
                            title: loc.to_owned(),
                            subtitle: Some("click to copy location".to_owned()),
                        },
                    );
                    cur_y += m.ch + 3.0;
                }
                CardEntry::CodeFrame(frame) => {
                    let max_ch = (inner_w / m.cw).floor().max(16.0) as usize;
                    let shown = crate::view::truncate_chars(frame, max_ch);
                    if let Some((gutter, code)) = shown.split_once('|') {
                        let gw = (gutter.chars().count() + 1) as f32 * m.cw;
                        scene.text(inner_x, cur_y, gutter, theme::INK_4, false, false, clip_id);
                        scene.text(
                            inner_x + gutter.chars().count() as f32 * m.cw,
                            cur_y,
                            "│",
                            theme::LINE_STRONG,
                            false,
                            false,
                            clip_id,
                        );
                        let is_caret =
                            code.trim_start().starts_with('^') || code.trim_start().starts_with('-');
                        let code_col = if is_caret { rail_col } else { theme::INK_2 };
                        scene.text(inner_x + gw, cur_y, code, code_col, is_caret, false, clip_id);
                    } else {
                        scene.text(inner_x, cur_y, &shown, theme::INK_3, false, false, clip_id);
                    }
                    cur_y += m.ch + 2.0;
                }
            }
        }

        cur_y += 4.0;
        scene.rects[bg_idx].rect[3] = cur_y;
        scene.rects[rail_idx].rect[3] = cur_y;
    }

    cur_y - y0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_stderr_diagnostic_group_renders_separate_error_and_note_cards_with_code_chips() {
        let mut scene = Scene::new();
        let lines = [
            (10i64, "rg: ripgrep requires at least one pattern to execute a search"),
            (
                20i64,
                "note: `crates/globset/src/serde_impl.rs`: no unit checks this file (by the `mod` in `crates/globset/src/lib.rs`) when a `#[cfg]` is false (engine_body_plan.rs)",
            ),
        ];
        let h = layout_stderr_diagnostic_group(
            &mut scene,
            &lines,
            2,
            16.0,
            20.0,
            720.0,
            90.0,
            CellMetrics::default(),
            0,
        );
        assert!(h > 60.0);
        let copy_hits: Vec<&str> = scene
            .hit_regions
            .iter()
            .filter_map(|r| match &r.action {
                HitAction::CopyText(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(copy_hits.contains(&"crates/globset/src/serde_impl.rs"));
        assert!(copy_hits.contains(&"mod"));
        assert!(copy_hits.contains(&"#[cfg]"));
        assert!(copy_hits.contains(&"engine_body_plan.rs"));
        assert!(
            scene
                .text_runs
                .iter()
                .filter(|r| r.text == "ripgrep" || r.text == "unit")
                .all(|r| r.face == TextFace::Mono)
        );
    }

    #[test]
    fn plain_cli_stderr_preserves_monospace_column_alignment() {
        let mut scene = Scene::new();
        let line = " - [deleted]         (none)     -> origin/feat/branch-hover-commit-graph";
        let lines = [(1i64, "From https://github.com/indexable-inc/blaze"), (2i64, line)];
        let h = layout_stderr_diagnostic_group(
            &mut scene,
            &lines,
            0,
            16.0,
            20.0,
            900.0,
            0.0,
            CellMetrics::default(),
            0,
        );
        assert!(h > 20.0);
        assert!(
            scene
                .text_runs
                .iter()
                .any(|r| r.face == TextFace::Mono && r.text == line)
        );
    }
}
