//! Bespoke native Metal GPU Schema & Data-Profile Card renderer for `schema` / `describe`.
//!
//! Renders structured `__blaze_schema` records produced by `blaze_sh::schema::infer_schema_value`
//! with:
//! - Indented GPU vector tree connectors (`│`, `├`, `└`) for nested records/tables/lists,
//! - Color-coded type & semantic pattern chips (`x.y.z`, `hex-64`, `registry+https://...`, `enum`),
//! - Proportional non-null presence fill bars (emerald / amber / coral),
//! - Native 8-bar GPU numeric histograms (`num:...`) and stacked cardinality bars (`cat:...`),
//! - Clickable field paths that insert `.package.version` directly into the prompt!

use blaze_sh::{Record, Table, Value};

use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, Scene, TextFace, truncate_chars};

/// Returns `true` if `rec` is a structured schema record emitted by `blaze-sh`'s `schema`/`describe`.
pub fn is_schema_record(rec: &Record<'_>) -> bool {
    matches!(rec.get(blaze_sh::schema::SCHEMA_MARKER_KEY), Some(Value::Bool(true)))
}

struct ParsedSchemaRow<'a> {
    path: &'a str,
    name: &'a str,
    depth: usize,
    ty: &'a str,
    pattern: &'a str,
    present: u64,
    total: u64,
    sample: &'a str,
    hist: &'a str,
}

fn extract_rows<'a>(tbl: &'a Table<'a>) -> Vec<ParsedSchemaRow<'a>> {
    let mut out = Vec::with_capacity(tbl.nrows());
    for i in 0..tbl.nrows() {
        let path = tbl.get_by_name(i, "path").and_then(Value::as_str).unwrap_or(".");
        let name = tbl.get_by_name(i, "name").and_then(Value::as_str).unwrap_or(path);
        let depth = match tbl.get_by_name(i, "depth") {
            Some(Value::Int(d)) if *d >= 0 => *d as usize,
            _ => 0,
        };
        let ty = tbl.get_by_name(i, "type").and_then(Value::as_str).unwrap_or("any");
        let pattern = tbl.get_by_name(i, "pattern").and_then(Value::as_str).unwrap_or("");
        let present = match tbl.get_by_name(i, "present") {
            Some(Value::Int(n)) if *n >= 0 => *n as u64,
            _ => 0,
        };
        let total = match tbl.get_by_name(i, "total") {
            Some(Value::Int(n)) if *n >= 0 => *n as u64,
            _ => 1,
        };
        let sample = tbl.get_by_name(i, "sample").and_then(Value::as_str).unwrap_or("");
        let hist = tbl.get_by_name(i, "hist").and_then(Value::as_str).unwrap_or("");
        out.push(ParsedSchemaRow { path, name, depth, ty, pattern, present, total, sample, hist });
    }
    out
}

/// Lay out a rich native GPU schema & data-profile card and return its total logical height.
#[allow(clippy::too_many_arguments)]
pub fn layout_schema_card(
    scene: &mut Scene,
    rec: &Record<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let kind = rec.get("kind").and_then(Value::as_str).unwrap_or("value");
    let summary = rec.get("summary").and_then(Value::as_str).unwrap_or(kind);
    let rows = match rec.get("fields") {
        Some(Value::Table(tbl)) => extract_rows(tbl),
        _ => Vec::new(),
    };

    let card_w = width.max(280.0);
    let x1 = x0 + card_w;
    let pad_x = 12.0;
    let header_h = m.ch + 12.0;
    let col_hdr_h = if rows.is_empty() { 0.0 } else { m.ch + 4.0 };
    let row_h = m.ch + 8.0;
    let body_h = rows.len() as f32 * row_h;
    let total_h = header_h + col_hdr_h + body_h + 8.0;

    // Card frame
    scene.round_rect(
        x0,
        y0,
        x1,
        y0 + total_h,
        8.0,
        theme::BG_SURFACE,
        1.0,
        theme::LINE,
        clip_id,
    );
    // Header subtle tint + bottom hairline
    scene.round_rect(
        x0 + 1.0,
        y0 + 1.0,
        x1 - 1.0,
        y0 + header_h,
        7.0,
        theme::rgba(0xff, 0xff, 0xff, 0.025),
        0.0,
        [0.0; 4],
        clip_id,
    );
    scene.line(x0 + 1.0, y0 + header_h, x1 - 1.0, y0 + header_h, 1.0, theme::LINE, clip_id);

    // Header content: kind pill + summary + subtle click hint
    let hdr_ty = y0 + (header_h - m.ch) * 0.5;
    let (pill_fg, pill_bg, pill_border) = type_badge_colors(kind);
    let kind_upper = kind.to_ascii_uppercase();
    let kind_w = m.measure_styled(&kind_upper, TextFace::MonoSmall, true) + 12.0;
    let pill_x0 = x0 + pad_x;
    let pill_y0 = y0 + (header_h - (m.ch * 0.88 + 4.0)) * 0.5;
    let pill_y1 = pill_y0 + m.ch * 0.88 + 4.0;
    scene.round_rect(pill_x0, pill_y0, pill_x0 + kind_w, pill_y1, 4.0, pill_bg, 1.0, pill_border, clip_id);
    scene.mono_small(pill_x0 + 6.0, hdr_ty, kind_upper, pill_fg, true, false, clip_id);

    let sum_x = pill_x0 + kind_w + 10.0;
    let hint_txt = "click path to query";
    let hint_w = m.measure(hint_txt, TextFace::UiSmall);
    let show_hint = card_w >= 460.0;
    let sum_right = if show_hint { x1 - pad_x - hint_w - 14.0 } else { x1 - pad_x };
    // Strip redundant leading `<kind> · ` prefix from `summary` since the kind pill already shows it!
    let clean_summary = summary
        .strip_prefix(kind)
        .and_then(|s| s.strip_prefix(" · "))
        .unwrap_or(summary);
    let sum_txt = fit_ui_text(clean_summary, (sum_right - sum_x).max(40.0), m, TextFace::Ui, true);
    scene.ui_text(sum_x, hdr_ty, sum_txt, theme::INK_1, true, false, clip_id);
    if show_hint {
        scene.ui_small(x1 - pad_x - hint_w, hdr_ty, hint_txt, theme::INK_4, false, false, clip_id);
    }

    if rows.is_empty() {
        return total_h;
    }

    // Compute clean aligned column geometry across all rows.
    let inner_x0 = x0 + pad_x;
    let inner_x1 = x1 - pad_x;
    let inner_w = (inner_x1 - inner_x0).max(240.0);

    let max_name_w = rows
        .iter()
        .map(|r| (r.depth as f32 * 16.0) + (r.name.chars().count() as f32 * m.cw))
        .fold(0.0f32, f32::max)
        .clamp(92.0, (inner_w * 0.28).max(110.0).min(210.0));

    let type_col_w = rows
        .iter()
        .map(|r| {
            let tw = m.measure_styled(r.ty, TextFace::MonoSmall, false) + 12.0;
            let pw = if r.pattern.is_empty() {
                0.0
            } else {
                6.0 + m.measure_styled(truncate_chars(r.pattern, 18).as_str(), TextFace::MonoSmall, false) + 10.0
            };
            tw + pw
        })
        .fold(0.0f32, f32::max)
        .clamp(84.0, (inner_w * 0.34).max(120.0).min(240.0));

    let show_fill = inner_w >= 420.0;
    let show_dist = inner_w >= 540.0;
    let fill_col_w = if show_fill { 68.0 } else { 0.0 };
    let dist_col_w = if show_dist { 62.0 } else { 0.0 };

    let col_name_x = inner_x0;
    let col_type_x = col_name_x + max_name_w + 12.0;
    let col_fill_x = col_type_x + type_col_w + 12.0;
    let col_dist_x = col_fill_x + if show_fill { fill_col_w + 10.0 } else { 0.0 };
    let col_sample_x = col_dist_x + if show_dist { dist_col_w + 12.0 } else { 0.0 };

    // Subtle column header strip
    let ch_y0 = y0 + header_h;
    let ch_ty = ch_y0 + (col_hdr_h - m.ch) * 0.5;
    scene.mono_small(col_name_x, ch_ty, "FIELD", theme::INK_4, false, false, clip_id);
    scene.mono_small(col_type_x, ch_ty, "TYPE / PATTERN", theme::INK_4, false, false, clip_id);
    if show_fill {
        scene.mono_small(col_fill_x, ch_ty, "FILL", theme::INK_4, false, false, clip_id);
    }
    if show_dist {
        scene.mono_small(col_dist_x, ch_ty, "DIST", theme::INK_4, false, false, clip_id);
    }
    if col_sample_x + 48.0 < inner_x1 {
        scene.mono_small(col_sample_x, ch_ty, "OVERVIEW", theme::INK_4, false, false, clip_id);
    }
    scene.line(x0 + 8.0, ch_y0 + col_hdr_h, x1 - 8.0, ch_y0 + col_hdr_h, 1.0, theme::LINE_FAINT, clip_id);

    // Precompute sibling continuation flags for crisp GPU vector tree connectors.
    let mut ry = ch_y0 + col_hdr_h + 3.0;
    for (idx, row) in rows.iter().enumerate() {
        let is_container = row.ty.starts_with("table")
            || row.ty.starts_with("record")
            || row.ty.starts_with("list<record>")
            || row.ty == "output";

        // Subtle tint behind container rows to visually group nested fields.
        if is_container {
            scene.round_rect(
                x0 + 6.0,
                ry,
                x1 - 6.0,
                ry + row_h - 1.0,
                4.5,
                theme::rgba(0x58, 0x9d, 0xf6, 0.055),
                0.0,
                [0.0; 4],
                clip_id,
            );
        }

        // Draw crisp 1px GPU vector tree connectors for nested depths (`depth >= 1`).
        let row_cy = ry + row_h * 0.5;
        if row.depth > 0 {
            let line_col = theme::rgba(0xff, 0xff, 0xff, 0.14);
            for d in 1..=row.depth {
                let cx = col_name_x + (d - 1) as f32 * 16.0 + 6.0;
                // Does any subsequent row before leaving depth `d` exist at depth `d`?
                let has_next_sibling = rows[idx + 1..]
                    .iter()
                    .take_while(|next| next.depth >= d)
                    .any(|next| next.depth == d);
                if d < row.depth {
                    if has_next_sibling {
                        scene.line(cx, ry, cx, ry + row_h, 1.0, line_col, clip_id);
                    }
                } else {
                    let y_end = if has_next_sibling { ry + row_h } else { row_cy };
                    scene.line(cx, ry, cx, y_end, 1.0, line_col, clip_id);
                    scene.line(cx, row_cy, cx + 8.0, row_cy, 1.0, line_col, clip_id);
                }
            }
        }

        let ty = ry + (row_h - m.ch) * 0.5;
        let name_x = col_name_x + row.depth as f32 * 16.0;
        let max_name_ch = ((col_type_x - 8.0 - name_x) / m.cw).floor().max(4.0) as usize;
        let name_txt = truncate_chars(row.name, max_name_ch);
        let name_col = if is_container { theme::INK_1 } else { theme::INK_2 };
        scene.text(name_x, ty, name_txt, name_col, is_container, false, clip_id);

        // Type chip + optional semantic pattern chip (`x.y.z`, `hex-64`, `registry+https://...`, `enum`)
        let chip_h = m.ch * 0.86 + 3.0;
        let chip_y0 = ry + (row_h - chip_h) * 0.5;
        let chip_y1 = chip_y0 + chip_h;
        let (tfg, tbg, tbd) = type_badge_colors(row.ty);
        let ty_w = m.measure_styled(row.ty, TextFace::MonoSmall, false) + 10.0;
        scene.round_rect(col_type_x, chip_y0, col_type_x + ty_w, chip_y1, 4.0, tbg, 1.0, tbd, clip_id);
        scene.mono_small(col_type_x + 5.0, ty, row.ty, tfg, false, false, clip_id);

        if !row.pattern.is_empty() {
            let pat_x = col_type_x + ty_w + 5.0;
            let pat_budget = (col_fill_x - 8.0 - pat_x).max(0.0);
            if pat_budget >= 28.0 {
                let max_pat_ch = ((pat_budget - 10.0) / (m.cw * 0.84)).floor().max(4.0) as usize;
                let pat_txt = truncate_chars(row.pattern, max_pat_ch.min(20));
                let pw = m.measure_styled(&pat_txt, TextFace::MonoSmall, false) + 10.0;
                let (pfg, pbg, pbd) = pattern_badge_colors(row.pattern);
                scene.round_rect(pat_x, chip_y0, pat_x + pw, chip_y1, 4.0, pbg, 1.0, pbd, clip_id);
                scene.mono_small(pat_x + 5.0, ty, pat_txt, pfg, false, false, clip_id);
            }
        }

        // Visual non-null presence / fill bar
        if show_fill && row.total > 0 {
            let frac = (row.present as f32 / row.total as f32).clamp(0.0, 1.0);
            let pct = (frac * 100.0).round() as u32;
            let bar_w = 34.0;
            let bar_y0 = row_cy - 2.0;
            let bar_y1 = row_cy + 2.0;
            scene.round_rect(col_fill_x, bar_y0, col_fill_x + bar_w, bar_y1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
            let fill_col = if pct >= 100 {
                theme::rgb(0x4e, 0xc9, 0x90)
            } else if pct >= 50 {
                theme::rgb(0xe5, 0xb5, 0x67)
            } else {
                theme::rgb(0xe0, 0x6c, 0x75)
            };
            if frac > 0.0 {
                let fw = (bar_w * frac).max(3.0);
                scene.round_rect(col_fill_x, bar_y0, col_fill_x + fw, bar_y1, 2.0, fill_col, 0.0, [0.0; 4], clip_id);
            }
            let pct_s = format!("{pct}%");
            scene.ui_small(
                col_fill_x + bar_w + 5.0,
                ty,
                pct_s,
                if pct < 100 { fill_col } else { theme::INK_4 },
                false,
                false,
                clip_id,
            );
        }

        // Native GPU mini-chart (`num:...` 8-bar histogram OR `cat:...` stacked distribution bar)
        if show_dist && !row.hist.is_empty() {
            draw_dist_spark(scene, row.hist, col_dist_x, ry + 3.0, dist_col_w - 4.0, row_h - 7.0, clip_id);
        }

        // Overview / sample column (measured cleanly against exact `UiSmall` advances so it never clips!)
        if !row.sample.is_empty() && col_sample_x + 40.0 < inner_x1 {
            let avail_w = (inner_x1 - col_sample_x - 4.0).max(20.0);
            let s_txt = fit_ui_text(row.sample, avail_w, m, TextFace::UiSmall, false);
            scene.ui_small(
                col_sample_x,
                ty,
                s_txt,
                if is_container { theme::INK_2 } else { theme::INK_3 },
                false,
                false,
                clip_id,
            );
        }

        // Click any schema row to insert `.package.version` right into the prompt!
        if !row.path.is_empty() && row.path != ".*" {
            scene.push_hit_clipped(
                HitRect::new(x0 + 6.0, ry, x1 - 6.0, ry + row_h),
                clip_id,
                HitAction::InsertToken(row.path.to_owned()),
            );
        }

        ry += row_h;
    }

    total_h
}

/// Draw either an 8-bin native Metal GPU histogram (`num:b0,b1,...`) or a proportional
/// stacked categorical distribution bar (`cat:c0,c1,...`).
fn draw_dist_spark(
    scene: &mut Scene,
    hist: &str,
    x0: f32,
    y0: f32,
    w: f32,
    h: f32,
    clip_id: u32,
) {
    if let Some(rest) = hist.strip_prefix("num:") {
        let mut bins = [0u32; 8];
        let mut max_v = 0u32;
        for (i, tok) in rest.split(',').take(8).enumerate() {
            let v = tok.parse::<u32>().unwrap_or(0);
            bins[i] = v;
            max_v = max_v.max(v);
        }
        if max_v == 0 {
            return;
        }
        let gap = 1.5;
        let bw = ((w - gap * 7.0) / 8.0).max(2.5);
        let base_y = y0 + h;
        // Faint baseline rule
        scene.line(x0, base_y, x0 + w, base_y, 1.0, theme::LINE_FAINT, clip_id);
        for (i, &v) in bins.iter().enumerate() {
            if v == 0 {
                continue;
            }
            let frac = v as f32 / max_v as f32;
            let bh = (h * frac).max(2.0);
            let bx = x0 + i as f32 * (bw + gap);
            scene.round_rect(
                bx,
                base_y - bh,
                bx + bw,
                base_y,
                1.5,
                theme::rgb(0x58, 0x9d, 0xf6),
                0.0,
                [0.0; 4],
                clip_id,
            );
        }
    } else if let Some(rest) = hist.strip_prefix("cat:") {
        let counts: Vec<u32> = rest.split(',').filter_map(|t| t.parse::<u32>().ok()).collect();
        let total: u32 = counts.iter().sum();
        if total == 0 {
            return;
        }
        const PALETTE: [[f32; 4]; 6] = [
            theme::rgb(0x56, 0xb6, 0xc2), // teal
            theme::rgb(0x58, 0x9d, 0xf6), // blue
            theme::rgb(0xc6, 0x78, 0xdd), // violet
            theme::rgb(0xe5, 0xb5, 0x67), // amber
            theme::rgb(0x6a, 0xab, 0x73), // sage
            theme::rgba(0x88, 0x90, 0x9e, 0.45), // other tail
        ];
        let bar_h = 5.0;
        let by0 = y0 + (h - bar_h) * 0.5;
        let by1 = by0 + bar_h;
        scene.round_rect(x0, by0, x0 + w, by1, 2.5, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
        let mut cx = x0;
        for (i, &c) in counts.iter().enumerate() {
            let seg_w = (w * (c as f32 / total as f32)).max(2.0).min(x0 + w - cx);
            if seg_w <= 0.5 {
                break;
            }
            let col = PALETTE[i.min(PALETTE.len() - 1)];
            scene.round_rect(cx, by0, cx + seg_w, by1, 2.0, col, 0.0, [0.0; 4], clip_id);
            cx += seg_w + 0.75;
            if cx >= x0 + w {
                break;
            }
        }
    }
}

fn type_badge_colors(ty: &str) -> ([f32; 4], [f32; 4], [f32; 4]) {
    let base = ty.trim_end_matches('?');
    if base.starts_with("table") || base.starts_with("record") || base == "output" {
        (
            theme::rgb(0x7c, 0xb7, 0xff),
            theme::rgba(0x58, 0x9d, 0xf6, 0.14),
            theme::rgba(0x58, 0x9d, 0xf6, 0.34),
        )
    } else if base.starts_with("list") {
        (
            theme::rgb(0xc6, 0x78, 0xdd),
            theme::rgba(0xc6, 0x78, 0xdd, 0.14),
            theme::rgba(0xc6, 0x78, 0xdd, 0.34),
        )
    } else if base == "string" {
        (
            theme::rgb(0x7c, 0xc9, 0x8b),
            theme::rgba(0x6a, 0xab, 0x73, 0.14),
            theme::rgba(0x6a, 0xab, 0x73, 0.34),
        )
    } else if matches!(base, "int" | "float") {
        (
            theme::rgb(0xe5, 0xb5, 0x67),
            theme::rgba(0xe5, 0xb5, 0x67, 0.14),
            theme::rgba(0xe5, 0xb5, 0x67, 0.34),
        )
    } else if matches!(base, "size" | "duration" | "date") {
        (
            theme::rgb(0x56, 0xb6, 0xc2),
            theme::rgba(0x56, 0xb6, 0xc2, 0.14),
            theme::rgba(0x56, 0xb6, 0xc2, 0.34),
        )
    } else if base == "bool" {
        (
            theme::rgb(0xd1, 0x9a, 0x66),
            theme::rgba(0xd1, 0x9a, 0x66, 0.14),
            theme::rgba(0xd1, 0x9a, 0x66, 0.34),
        )
    } else {
        (theme::INK_3, theme::BG_CHIP, theme::LINE)
    }
}

fn fit_ui_text(s: &str, max_w: f32, m: CellMetrics, face: TextFace, bold: bool) -> String {
    if m.measure_styled(s, face, bold) <= max_w {
        return s.to_owned();
    }
    let ell_w = m.measure_styled("…", face, bold);
    let budget = (max_w - ell_w).max(0.0);
    let mut out = String::new();
    let mut cur_w = 0.0f32;
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let cs = ch.encode_utf8(&mut buf);
        let cw = m.measure_styled(cs, face, bold);
        if cur_w + cw > budget {
            break;
        }
        out.push(ch);
        cur_w += cw;
    }
    out.push('…');
    out
}

fn pattern_badge_colors(pat: &str) -> ([f32; 4], [f32; 4], [f32; 4]) {
    if pat == "x.y.z" {
        (
            theme::rgb(0x56, 0xb6, 0xc2),
            theme::rgba(0x56, 0xb6, 0xc2, 0.14),
            theme::rgba(0x56, 0xb6, 0xc2, 0.36),
        )
    } else if pat.starts_with("hex") || pat == "uuid" {
        (
            theme::rgb(0x9b, 0xb8, 0xff),
            theme::rgba(0x7c, 0x9c, 0xf5, 0.14),
            theme::rgba(0x7c, 0x9c, 0xf5, 0.36),
        )
    } else if pat == "enum" || pat == "const" || pat == "const url" {
        (
            theme::rgb(0xd8, 0xa8, 0xf0),
            theme::rgba(0xc6, 0x78, 0xdd, 0.12),
            theme::rgba(0xc6, 0x78, 0xdd, 0.30),
        )
    } else if pat == "unique" {
        (
            theme::rgb(0x4e, 0xc9, 0x90),
            theme::rgba(0x4e, 0xc9, 0x90, 0.12),
            theme::rgba(0x4e, 0xc9, 0x90, 0.30),
        )
    } else {
        (
            theme::INK_2,
            theme::rgba(0xff, 0xff, 0xff, 0.05),
            theme::LINE_STRONG,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_own::{Alloc, Rc};
    use blaze_sh::PVec;

    #[test]
    fn renders_schema_card_with_histograms_and_clickable_paths() {
        let alloc = Alloc::global();
        let mut r1 = Record::new(alloc);
        r1.insert(Rc::from_str_in("name", alloc), Value::string("adler2", alloc));
        r1.insert(Rc::from_str_in("version", alloc), Value::string("2.0.1", alloc));
        r1.insert(Rc::from_str_in("size", alloc), Value::Int(12));
        let mut r2 = Record::new(alloc);
        r2.insert(Rc::from_str_in("name", alloc), Value::string("blaze-sh", alloc));
        r2.insert(Rc::from_str_in("version", alloc), Value::string("0.0.0", alloc));
        r2.insert(Rc::from_str_in("size", alloc), Value::Int(480));
        let mut pkgs = PVec::new(alloc);
        pkgs.push(Value::Record(r1));
        pkgs.push(Value::Record(r2));
        let mut lock = Record::new(alloc);
        lock.insert(Rc::from_str_in("version", alloc), Value::Int(4));
        lock.insert(Rc::from_str_in("package", alloc), Value::List(pkgs));

        let Value::Record(schema_rec) = blaze_sh::schema::infer_schema_value(&Value::Record(lock), alloc) else {
            panic!("expected schema record");
        };
        assert!(is_schema_record(&schema_rec));

        let mut scene = Scene::new();
        let h = layout_schema_card(&mut scene, &schema_rec, 16.0, 20.0, 820.0, CellMetrics::default(), 0);
        assert!(h > 100.0, "schema card should render multi-row tree, got {h}");
        assert!(
            scene
                .hit_regions
                .iter()
                .any(|hit| matches!(&hit.action, HitAction::InsertToken(tok) if tok == ".package.version")),
            "clicking `.package.version` row should insert `.package.version`"
        );
    }
}
