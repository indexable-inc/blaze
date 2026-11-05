//! The tiers island under the grid. The daemon samples the window while it runs and compiles
//! the functions it spends its time in again with LLVM; this shows what it says of that
//! (`blaze_live::tiers`): the hottest functions with the level each runs at, the ladder of
//! levels, and what the daemon did of late, a promotion lit for a moment as it lands.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Align2, Color32, FontId, Galley, Painter, Rect, Sense, pos2, vec2};
use blaze_live::format::{LEVELS, Tiers};

use crate::theme::Theme;

/// How often the daemon is asked: its word changes a few times a second, not every frame.
const READ_EVERY: Duration = Duration::from_millis(250);
/// How long a new event stays lit.
const LIT: Duration = Duration::from_millis(2500);
/// How old the daemon's word may be before the footer says so, in milliseconds.
const STALE_MS: u64 = 3000;
/// The tag of each level on its badge.
const TAGS: [&str; 3] = ["CL", "O3", "PGO"];

pub struct TiersPanel {
    tiers: Option<Tiers>,
    read_at: Instant,
    /// When the newest event read so far happened, and when each since came in.
    newest: u64,
    lit: Vec<(u64, Instant)>,
    /// Why there is nothing to wait for, when tiering is off.
    off: Option<String>,
    /// Why the daemon could not be asked, the last time it was.
    failed: Option<String>,
}

impl TiersPanel {
    pub fn new(off: Option<String>) -> TiersPanel {
        TiersPanel {
            tiers: None,
            read_at: Instant::now() - READ_EVERY,
            newest: 0,
            lit: Vec::new(),
            off,
            failed: None,
        }
    }

    pub fn refresh(&mut self) {
        if self.read_at.elapsed() < READ_EVERY {
            return;
        }
        self.read_at = Instant::now();
        (self.tiers, self.failed) = match blaze_live::tiers() {
            Ok(tiers) => (tiers, None),
            Err(why) => (None, Some(format!("tiering: {why}"))),
        };
        let now = now_ms();
        if let Some(tiers) = &self.tiers {
            for event in &tiers.events {
                // An event from before the window looked is not new to whoever watches.
                let recent = now.saturating_sub(event.at) < LIT.as_millis() as u64;
                if event.at > self.newest && recent {
                    self.lit.push((event.at, Instant::now()));
                }
            }
            self.newest = tiers.events.iter().map(|it| it.at).fold(self.newest, u64::max);
        }
        self.lit.retain(|(_, at)| at.elapsed() < LIT);
    }

    /// Whether an event is lit, so the window keeps drawing while it fades.
    pub fn lit(&self) -> bool {
        !self.lit.is_empty()
    }

    pub fn show(&self, ui: &mut egui::Ui, t: &Theme) {
        let m = &t.metrics;
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let painter = ui.painter_at(rect);
        let text = FontId::proportional(m.tier_font_size);
        let row = m.tier_row_height;

        let title = Rect::from_min_size(rect.min, vec2(rect.width(), row + 2.0));
        painter.text(
            title.left_center(),
            Align2::LEFT_CENTER,
            "Tiers",
            FontId::proportional(m.tier_font_size + 2.0),
            t.fg,
        );
        ladder(ui, &painter, title, t);

        let body = Rect::from_min_max(pos2(rect.left(), title.bottom() + 8.0), rect.max);
        let Some(tiers) = &self.tiers else {
            let quiet = self
                .failed
                .as_deref()
                .or(self.off.as_deref())
                .unwrap_or("tiering: waiting for the daemon");
            painter.text(body.left_top(), Align2::LEFT_TOP, quiet, text.clone(), t.comment);
            painter.text(
                body.left_top() + vec2(0.0, row),
                Align2::LEFT_TOP,
                "The daemon samples this window while it runs and compiles the functions it \
                 spends its time in again with LLVM, swapping each in as a patch.",
                text,
                t.gutter_fg,
            );
            return;
        };

        let footer = Rect::from_min_max(pos2(body.left(), body.bottom() - row), body.max);
        let split = body.left() + body.width() * m.tier_split;
        let functions = Rect::from_min_max(body.min, pos2(split - 12.0, footer.top()));
        let events = Rect::from_min_max(pos2(split + 12.0, body.top()), footer.right_top());
        painter.vline(
            split,
            functions.top()..=functions.bottom() - 4.0,
            egui::Stroke::new(1.0, t.tier_track),
        );
        self.functions(ui, &painter, functions, tiers, t);
        self.events(ui, &painter, events, tiers, t);

        let mut said = format!(
            "{} samples in the last {:.1} s   ·   the sampler takes {:.1}% of a core",
            thousands(tiers.samples),
            tiers.window_ms as f64 / 1000.0,
            tiers.sampler_permille as f64 / 10.0
        );
        let quiet_for = now_ms().saturating_sub(tiers.at);
        if quiet_for > STALE_MS {
            said.push_str(&format!("   ·   last word from the daemon {}", ago(quiet_for)));
        }
        let said = line(ui, &said, text, t.comment, footer.width());
        painter.galley(
            pos2(footer.left(), footer.center().y - said.size().y / 2.0),
            said,
            t.comment,
        );
    }

    fn functions(&self, ui: &egui::Ui, painter: &Painter, area: Rect, tiers: &Tiers, t: &Theme) {
        let m = &t.metrics;
        let text = FontId::proportional(m.tier_font_size);
        let mono = FontId::monospace(m.tier_font_size);
        if tiers.functions.is_empty() {
            let quiet = "no samples in the window's own code yet";
            painter.text(area.left_top(), Align2::LEFT_TOP, quiet, text, t.comment);
            return;
        }
        // As many as fit above the footer, however tall the island is.
        let fits = (area.height() / m.tier_row_height).floor() as usize;
        for (nth, function) in tiers.functions.iter().take(m.tier_rows.min(fits)).enumerate() {
            let top = area.top() + nth as f32 * m.tier_row_height;
            let row =
                Rect::from_min_size(pos2(area.left(), top), vec2(area.width(), m.tier_row_height));
            let y = row.center().y;
            let color = level_color(t, function.level);
            let mut x = row.left();

            badge(painter, pos2(x, y), function.level, t);
            x += m.tier_badge_width + 10.0;

            let ours = function.level != 0;
            let name = line(
                ui,
                short(&function.name),
                mono.clone(),
                if ours { t.fg } else { t.comment },
                m.tier_name_width,
            );
            painter.galley(pos2(x, y - name.size().y / 2.0), name, t.fg);
            x += m.tier_name_width + 10.0;

            let track = Rect::from_min_size(
                pos2(x, y - m.tier_bar_height / 2.0),
                vec2(m.tier_bar_width, m.tier_bar_height),
            );
            let radius = m.tier_bar_height / 2.0;
            painter.rect_filled(track, radius, t.tier_track);
            let share = (function.share.min(1000) as f32 / 1000.0).max(0.01);
            let mut bar = track;
            bar.set_width(track.width() * share);
            painter.rect_filled(bar, radius, color);
            x += m.tier_bar_width + 8.0;

            let percent = format!("{:.1}%", function.share as f64 / 10.0);
            let width = 46.0;
            painter.text(pos2(x + width, y), Align2::RIGHT_CENTER, percent, mono.clone(), t.fg);
            x += width + 10.0;

            if !function.note.is_empty() && x < row.right() {
                let note = line(ui, &function.note, text.clone(), t.comment, row.right() - x);
                painter.galley(pos2(x, y - note.size().y / 2.0), note, t.comment);
            }

            let level = match function.level {
                0 => "not compiled by the daemon".to_owned(),
                level => format!("level {level}, {}", blaze_live::format::level_name(level)),
            };
            let mut about =
                format!("{}\n{level}   ·   {} samples", function.name, thousands(function.samples));
            if !function.note.is_empty() {
                about.push_str(&format!("\n{}", function.note));
            }
            ui.interact(row, egui::Id::unique(("life-tier", nth)), Sense::hover())
                .on_hover_text(about);
        }
    }

    fn events(&self, ui: &egui::Ui, painter: &Painter, area: Rect, tiers: &Tiers, t: &Theme) {
        let m = &t.metrics;
        let text = FontId::proportional(m.tier_font_size);
        painter.text(area.left_top(), Align2::LEFT_TOP, "Lately", text.clone(), t.comment);
        let now = now_ms();
        let first = area.top() + m.tier_row_height + 2.0;
        if tiers.events.is_empty() {
            let quiet = "nothing promoted yet";
            painter.text(pos2(area.left(), first), Align2::LEFT_TOP, quiet, text, t.gutter_fg);
            return;
        }
        let fits = ((area.bottom() - first) / m.tier_row_height).floor() as usize;
        for (nth, event) in tiers.events.iter().rev().take(m.tier_events.min(fits)).enumerate() {
            let top = first + nth as f32 * m.tier_row_height;
            let row =
                Rect::from_min_size(pos2(area.left(), top), vec2(area.width(), m.tier_row_height));
            let y = row.center().y;
            let color = level_color(t, event_level(&event.text));
            let lit = self.lit.iter().find(|(at, _)| *at == event.at);
            if let Some((_, since)) = lit {
                let fade = 1.0 - since.elapsed().as_secs_f32() / LIT.as_secs_f32();
                painter.rect_filled(
                    row.expand2(vec2(6.0, 0.0)),
                    4.0,
                    color.gamma_multiply(0.22 * fade.max(0.0)),
                );
            }
            painter.circle_filled(pos2(row.left() + 3.0, y), 3.0, color);
            let age = ago(now.saturating_sub(event.at));
            let age = line(ui, &age, text.clone(), t.comment, f32::INFINITY);
            let age_width = age.size().x;
            painter.galley(pos2(row.right() - age_width, y - age.size().y / 2.0), age, t.comment);
            let width = row.width() - 14.0 - age_width - 10.0;
            let what = line(
                ui,
                &event.text,
                text.clone(),
                if lit.is_some() { color } else { t.fg },
                width,
            );
            painter.galley(pos2(row.left() + 14.0, y - what.size().y / 2.0), what, t.fg);
        }
    }
}

/// The three levels as badges, right-aligned in `title`: what CL, O3 and PGO mean.
fn ladder(ui: &egui::Ui, painter: &Painter, title: Rect, t: &Theme) {
    let m = &t.metrics;
    let text = FontId::proportional(m.tier_font_size);
    let names: Vec<Arc<Galley>> =
        LEVELS.iter().map(|name| line(ui, name, text.clone(), t.comment, f32::INFINITY)).collect();
    let (gap, arrow) = (6.0, 22.0);
    let width = names.iter().map(|it| m.tier_badge_width + gap + it.size().x).sum::<f32>()
        + arrow * (names.len() - 1) as f32;
    let y = title.center().y;
    let mut x = title.right() - width;
    for (nth, name) in names.into_iter().enumerate() {
        if nth > 0 {
            painter.text(
                pos2(x + arrow / 2.0, y),
                Align2::CENTER_CENTER,
                "›",
                text.clone(),
                t.gutter_fg,
            );
            x += arrow;
        }
        badge(painter, pos2(x, y), nth as u8 + 1, t);
        x += m.tier_badge_width + gap;
        let w = name.size().x;
        painter.galley(pos2(x, y - name.size().y / 2.0), name, t.comment);
        x += w;
    }
}

/// The tag of `level` in its colour, with its left edge at `at` and centred on it.
fn badge(painter: &Painter, at: egui::Pos2, level: u8, t: &Theme) {
    let m = &t.metrics;
    let color = level_color(t, level);
    let rect = Rect::from_min_size(
        pos2(at.x, at.y - (m.tier_row_height - 4.0) / 2.0),
        vec2(m.tier_badge_width, m.tier_row_height - 4.0),
    );
    painter.rect_filled(rect, 4.0, color.gamma_multiply(0.16));
    let tag = TAGS.get(usize::from(level).wrapping_sub(1)).copied().unwrap_or("–");
    let font = FontId::monospace(m.tier_font_size - 1.0);
    painter.text(rect.center(), Align2::CENTER_CENTER, tag, font, color);
}

fn level_color(t: &Theme, level: u8) -> Color32 {
    match level {
        1 => t.tier_cranelift,
        2 => t.tier_o3,
        3 => t.tier_pgo,
        _ => t.tier_none,
    }
}

/// The level an event names, the longest name first since each contains the one before.
fn event_level(text: &str) -> u8 {
    (0..LEVELS.len()).rev().find(|it| text.contains(LEVELS[*it])).map_or(0, |it| it as u8 + 1)
}

/// One line of text, cut with an ellipsis where it would pass `width`.
fn line(ui: &egui::Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat::simple(font, color),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    ui.fonts_mut(|f| f.layout_job(job))
}

/// The last two parts of a path, the crate left out: `life::Life::step` is `Life::step` and
/// `life::next_age` is `next_age`. A part in angle brackets is one part.
fn short(name: &str) -> &str {
    let bytes = name.as_bytes();
    let (mut depth, mut cuts, mut at) = (0i32, Vec::new(), 0);
    while at + 1 < bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'>' => depth -= 1,
            b':' if depth == 0 && bytes[at + 1] == b':' => {
                cuts.push(at);
                at += 1;
            }
            _ => {}
        }
        at += 1;
    }
    match cuts.as_slice() {
        [] => name,
        [_] if name.starts_with('<') => name,
        [only] => &name[only + 2..],
        [.., two_back, _] => &name[two_back + 2..],
    }
}

fn ago(ms: u64) -> String {
    match ms {
        0..10_000 => format!("{:.1} s ago", ms as f64 / 1000.0),
        10_000..60_000 => format!("{} s ago", ms / 1000),
        _ => format!("{} min ago", ms / 60_000),
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (nth, c) in digits.chars().enumerate() {
        if nth > 0 && (digits.len() - nth).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |it| it.as_millis() as u64)
}
