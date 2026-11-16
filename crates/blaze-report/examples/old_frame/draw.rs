//! The frames: findings grouped by place, the lines each frame shows, the window over long
//! lines, the marks drawn on the code or under it, labels, fixes, header and footer.

use blaze_own::{AString, AVec, Alloc};
use blaze_text::Writes;

use super::lex::{Class, Classes, highlighter};
use super::text::{Cell, Diff, Ink, Pen, Plant, Show, Style, Under, push_cells, safe_width};
use super::{Caps, Colour, Counts, Finding, Input, digits, ink, line_col};

/// The glyphs of the frame, Unicode or ASCII.
struct Glyphs {
    top: &'static str,
    bottom: &'static str,
    bar: &'static str,
    gap: &'static str,
    ellipsis: &'static str,
    ellipsis_width: u32,
    dot: &'static str,
}

const UNICODE: Glyphs =
    Glyphs { top: "╭─", bottom: "╰─", bar: "│", gap: "⋮", ellipsis: "…", ellipsis_width: 1, dot: " · " };
const ASCII: Glyphs =
    Glyphs { top: ",-", bottom: "`-", bar: "|", gap: ":", ellipsis: "...", ellipsis_width: 3, dot: " - " };

/// Where each line of a file starts.
struct Lines<'a> {
    starts: AVec<'a, u32>,
    len: u32,
}

impl<'a> Lines<'a> {
    fn of(text: &[u8], alloc: Alloc<'a>) -> Lines<'a> {
        let mut starts = AVec::new_in(alloc);
        starts.push(0);
        for (at, &byte) in text.iter().enumerate() {
            if byte == b'\n' {
                starts.push(at as u32 + 1);
            }
        }
        Lines { starts, len: text.len() as u32 }
    }

    fn count(&self) -> u32 {
        self.starts.len() as u32
    }

    fn of_byte(&self, byte: u32) -> u32 {
        self.starts.partition_point(|&start| start <= byte).saturating_sub(1) as u32
    }

    fn start(&self, line: u32) -> u32 {
        self.starts[line as usize]
    }

    /// Past the line's last byte: before its `\n`, and before a `\r` ahead of that.
    fn end(&self, line: u32, text: &[u8]) -> u32 {
        match self.starts.get(line as usize + 1) {
            Some(&next) => {
                let end = next - 1;
                if end > self.start(line) && text[end as usize - 1] == b'\r' { end - 1 } else { end }
            }
            None => self.len,
        }
    }
}

/// A span to draw: its bytes, its lines, its ink and label, and which finding it is of.
#[derive(Clone, Copy)]
struct Drawn<'t> {
    start: u32,
    end: u32,
    first: u32,
    last: u32,
    ink: Ink,
    label: &'t str,
}

/// A label to place: where its mark starts on the screen, and its ink.
#[derive(Clone, Copy)]
struct Label<'t> {
    at: u32,
    ink: Ink,
    text: &'t str,
}

/// The horizontal window every line of a frame is drawn through: `offset` columns of the
/// line skipped, `budget` columns to draw in.
#[derive(Clone, Copy)]
struct Window {
    offset: u32,
    budget: u32,
    ellipsis: u32,
}

impl Window {
    /// The left ellipsis's width, the line columns drawn up to, and whether the line is cut on
    /// the right.
    fn span(&self, line_width: u32) -> (u32, u32, bool) {
        // An empty line stays empty: nothing of it is hidden.
        let left = if self.offset > 0 && line_width > 0 { self.ellipsis } else { 0 };
        let right = line_width > self.offset + self.budget.saturating_sub(left);
        let limit = self.offset + self.budget.saturating_sub(left + if right { self.ellipsis } else { 0 });
        (left, limit, right)
    }

    /// Where `cell` is on the screen (from the code's first column), if it is drawn.
    fn screen(&self, cell: &Cell<'_>, line_width: u32) -> Option<u32> {
        let (left, limit, _) = self.span(line_width);
        (cell.col >= self.offset && cell.col + cell.width <= limit).then(|| cell.col - self.offset + left)
    }
}

/// The frame's fixed facts while it is drawn.
struct Frame<'i, 'a> {
    caps: Caps,
    glyphs: &'static Glyphs,
    text: &'i [u8],
    lines: &'i Lines<'a>,
    classes: &'i Classes<'a>,
    gutter: u32,
    window: Window,
    plant: Plant,
}

pub(super) fn frames(
    out: &mut AString<'_>,
    input: &Input<'_>,
    order: &[u32],
    caps: Caps,
    counts: Counts,
    alloc: Alloc<'_>,
    plant: Plant,
) {
    let mut pen = Pen::new(out, caps);
    let mut at = 0;
    let mut lines: Option<(u32, Lines<'_>)> = None;
    while at < order.len() {
        let first = &input.findings[order[at] as usize];
        let text = input.sources[first.file as usize].text;
        if lines.as_ref().is_none_or(|(file, _)| *file != first.file) {
            lines = Some((first.file, Lines::of(text, alloc)));
        }
        let Some((_, file_lines)) = lines.as_ref() else { return };
        // A group: the findings whose primary lines (and one each side) touch the first's.
        let reach = |finding: &Finding<'_>| {
            let first = file_lines.of_byte(finding.primary.start);
            let last = file_lines.of_byte(finding.primary.end.max(finding.primary.start + 1) - 1).max(first);
            (first.saturating_sub(1), last + 1)
        };
        let (_, mut high) = reach(first);
        let mut end = at + 1;
        while let Some(&next) = order.get(end) {
            let next = &input.findings[next as usize];
            let (low, next_high) = reach(next);
            if next.file != first.file || low > high {
                break;
            }
            high = high.max(next_high);
            end += 1;
        }
        if at > 0 {
            pen.end();
        }
        group(&mut pen, input, &order[at..end], file_lines, counts, alloc, plant);
        at = end;
    }
}

fn group(
    pen: &mut Pen<'_, '_>,
    input: &Input<'_>,
    group: &[u32],
    lines: &Lines<'_>,
    counts: Counts,
    alloc: Alloc<'_>,
    plant: Plant,
) {
    let caps = pen.caps;
    let glyphs = if caps.unicode { &UNICODE } else { &ASCII };
    let lead = &input.findings[group[0] as usize];
    let source = &input.sources[lead.file as usize];
    let text = source.text;

    // The spans, primary then secondary, each with its lines.
    let mut marks: AVec<'_, Drawn<'_>> = AVec::new_in(alloc);
    for &at in group {
        let finding = &input.findings[at as usize];
        let label = match (finding.primary.label.is_empty(), group.len()) {
            (true, 2..) => finding.message,
            _ => finding.primary.label,
        };
        let spans = core::iter::once((finding.primary, ink(finding.severity), label))
            .chain(finding.secondary.iter().map(|mark| (*mark, Ink::Secondary, mark.label)));
        for (mark, ink, label) in spans {
            let first = lines.of_byte(mark.start);
            let last = if mark.end > mark.start { lines.of_byte(mark.end - 1).max(first) } else { first };
            marks.push(Drawn { start: mark.start, end: mark.end, first, last, ink, label });
        }
    }

    // The lines shown: each span's, one each side, a fix's, and the item's header.
    let mut shown: AVec<'_, u32> = AVec::new_in(alloc);
    for mark in marks.iter() {
        if mark.first > 0 {
            shown.push(mark.first - 1);
        }
        if mark.last - mark.first <= 3 {
            shown.extend(mark.first..=mark.last);
        } else {
            shown.push(mark.first);
            shown.push(mark.last);
        }
        if mark.last + 1 < lines.count() {
            shown.push(mark.last + 1);
        }
    }
    let mut fixes: AVec<'_, (u32, u32, u32)> = AVec::new_in(alloc);
    for &at in group {
        let finding = &input.findings[at as usize];
        if let (Some(first), Some(last)) = (finding.fix.first(), finding.fix.last()) {
            let from = lines.of_byte(first.start);
            let to = lines.of_byte(last.end.max(last.start + 1) - 1).max(from);
            shown.extend(from..=to);
            fixes.push((at, from, to));
        }
    }
    shown.sort_unstable();
    shown.dedup();
    let kind = highlighter(source.shown);
    let item = lead.item.and_then(|item| kind.head(text, item));
    if let (Some(item), Some(&top)) = (item, shown.first()) {
        let line = lines.of_byte(item.at);
        if line < top {
            match top - line <= 3 {
                true => shown.extend(line..top),
                false => shown.push(line),
            }
        }
    }
    shown.sort_unstable();
    shown.dedup();
    // A gap of one line is that line, not a `⋮`.
    let mut filled: AVec<'_, u32> = AVec::new_in(alloc);
    for (at, &line) in shown.iter().enumerate() {
        if at > 0 && line == shown[at - 1] + 2 {
            filled.push(line - 1);
        }
        filled.push(line);
    }
    let shown = filled;
    let (Some(&top), Some(&bottom)) = (shown.first(), shown.last()) else { return };

    let gutter = digits(bottom + 1);
    let width = caps.width();
    let budget = width.saturating_sub(gutter + 3).max(1);
    let classes = Classes::of(kind, text,lines.start(top), lines.end(bottom, text), alloc);
    let mut cells: AVec<'_, Cell<'_>> = AVec::new_in(alloc);

    // The window: around where the lead's span starts, when its line does not fit.
    let ellipsis = glyphs.ellipsis_width;
    let mut window = Window { offset: 0, budget, ellipsis };
    {
        let line = lines.of_byte(lead.primary.start);
        let width = build(&mut cells, text, lines, &classes, line, caps.unicode, plant);
        if width > budget {
            let focus = cells
                .iter()
                .find(|cell| cell.byte >= lead.primary.start)
                .map_or(width, |cell| cell.col);
            let span = cells
                .iter()
                .filter(|cell| cell.byte >= lead.primary.start && cell.byte < lead.primary.end)
                .map(|cell| cell.width)
                .sum::<u32>();
            if focus + span.min(budget / 2) + ellipsis + 1 > budget {
                let want = focus.saturating_sub(budget / 3);
                window.offset = cells.iter().map(|cell| cell.col).find(|&col| col >= want).unwrap_or(want);
            }
        }
    }
    let frame = Frame { caps, glyphs, text, lines, classes: &classes, gutter, window, plant };

    // One headline for findings that say the same thing (the owner's two `?` on one line).
    for (index, &at) in group.iter().enumerate() {
        let finding = &input.findings[at as usize];
        let said = group[..index].iter().any(|&it| {
            let it = &input.findings[it as usize];
            it.severity == finding.severity && it.message == finding.message
        });
        if !said {
            headline(pen, finding);
        }
    }
    header(pen, &frame, input, lead, item);

    let mut labels: AVec<'_, Label<'_>> = AVec::new_in(alloc);
    for (at, &line) in shown.iter().enumerate() {
        if at > 0 && line > shown[at - 1] + 1 {
            pen.spaces(gutter + 1);
            pen.put(Style::DIM, glyphs.gap, 1);
            pen.end();
        }
        let line_width = build(&mut cells, text, lines, &classes, line, caps.unicode, plant);
        let line_end = lines.end(line, text);
        // Ink the cells of each span on this line, and collect the labels of those ending here.
        labels.clear();
        let mut marked = false;
        for (index, mark) in marks.iter().enumerate() {
            if line < mark.first || line > mark.last {
                continue;
            }
            marked = true;
            let first_text = cells.iter().find(|cell| !cell.is_space()).map_or(line_end, |cell| cell.byte);
            let from = if line == mark.first { mark.start } else { first_text };
            let to = if line == mark.last { mark.end } else { line_end };
            let mut hit = false;
            for at in 0..cells.len() {
                let next = cells.get(at + 1).map_or(line_end, |cell| cell.byte);
                let cell = &mut cells[at];
                if cell.byte < to && next > from {
                    hit = true;
                    if mark.ink >= cell.ink {
                        cell.ink = mark.ink;
                        cell.mark = index as u32;
                    }
                }
            }
            if !hit {
                // A point: the cell it is in, or one past the line's end.
                match cells.iter().rposition(|cell| cell.byte <= from && from < line_end) {
                    Some(at) => {
                        if mark.ink >= cells[at].ink {
                            cells[at].ink = mark.ink;
                            cells[at].mark = index as u32;
                        }
                    }
                    None => cells.push(Cell {
                        byte: line_end,
                        col: line_width,
                        width: 1,
                        show: Show::Spaces(1),
                        class: Class::Plain,
                        diff: Diff::Same,
                        ink: mark.ink,
                        mark: index as u32,
                    }),
                }
            }
            if line == mark.last && !mark.label.is_empty() {
                let width = cells.last().map_or(0, |cell| cell.col + cell.width);
                let at = cells
                    .iter()
                    .filter(|cell| cell.mark == index as u32)
                    .find_map(|cell| frame.window.screen(cell, width))
                    .unwrap_or(0);
                labels.push(Label { at, ink: mark.ink, text: mark.label });
            }
        }
        let code_end = code_line(pen, &frame, &cells, Some(line + 1), marked);
        labels.sort_unstable_by_key(|label| label.at);
        // A label said once on a line: the first of its marks carries it.
        labels.dedup_by(|later, first| later.text == first.text && later.ink == first.ink);
        if caps.curly && caps.colour != Colour::None {
            lens(pen, &frame, &labels, code_end);
        } else {
            carets(pen, &frame, &cells, &labels);
        }
        for &(at, _, last) in fixes.iter() {
            if last == line {
                fix(pen, &frame, &input.findings[at as usize], &mut cells);
            }
        }
    }
    footer(pen, &frame, input, group, counts);
}

/// The cells of `line`, its width returned.
fn build<'t>(
    cells: &mut AVec<'_, Cell<'t>>,
    text: &'t [u8],
    lines: &Lines<'_>,
    classes: &Classes<'_>,
    line: u32,
    unicode: bool,
    plant: Plant,
) -> u32 {
    cells.clear();
    let start = lines.start(line);
    let end = lines.end(line, text);
    let bytes = &text[start as usize..end as usize];
    push_cells(cells, bytes, start, 0, &|byte| classes.at(byte), Diff::Same, unicode, plant)
}

/// The look of a code cell: its class, its place in a fix, and its mark where the terminal
/// draws the mark on the code.
fn cell_style(cell: &Cell<'_>, caps: Caps) -> Style {
    let mut style = match cell.diff {
        Diff::Same => cell.class.style(),
        Diff::Removed => Style { strike: true, dim: true, ..Style::fg(1) },
        Diff::Inserted => Style::fg(2),
    };
    if caps.curly && cell.ink != Ink::None {
        style.under = if cell.ink == Ink::Secondary { Under::Single } else { Under::Curly };
        style.under_colour = cell.ink.colour();
    }
    style
}

/// The gutter (a line number, `+` for a fix's line, or nothing) and the bar.
fn gutter(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, number: Option<u32>, plus: bool, strong: bool) {
    match (number, plus) {
        (Some(number), _) => {
            pen.spaces(frame.gutter - digits(number));
            pen.style(if strong { Style::PLAIN } else { Style::DIM });
            pen.out.int(number);
            pen.cols += digits(number);
        }
        (None, true) => {
            pen.spaces(frame.gutter - 1);
            pen.put(Style::fg(2), "+", 1);
        }
        (None, false) => pen.spaces(frame.gutter),
    }
    pen.put(Style::PLAIN, " ", 1);
    pen.put(Style::DIM, frame.glyphs.bar, 1);
    pen.put(Style::PLAIN, " ", 1);
}

/// A line of code through the window; returns where its drawn text ends on the screen.
fn code_line(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, cells: &[Cell<'_>], number: Option<u32>, strong: bool) -> u32 {
    gutter(pen, frame, number, number.is_none(), strong);
    let width = cells.last().map_or(0, |cell| cell.col + cell.width);
    let (left, limit, right) = frame.window.span(width);
    let mut at = 0;
    if left > 0 {
        pen.put(Style::DIM, frame.glyphs.ellipsis, left);
        at = left;
    }
    for cell in cells {
        if cell.col < frame.window.offset {
            continue;
        }
        if cell.col + cell.width > limit {
            break;
        }
        let want = cell.col - frame.window.offset + left;
        if want > at {
            pen.spaces(want - at);
            at = want;
        }
        pen.put(cell_style(cell, frame.caps), cell.text(), cell.width);
        at += cell.width;
    }
    if right {
        pen.put(Style::DIM, frame.glyphs.ellipsis, frame.window.ellipsis);
        at += frame.window.ellipsis;
    }
    if at == 0 && pen.out.ends_with(' ') {
        // An empty line ends at its bar: no trailing space.
        pen.out.pop();
        pen.cols -= 1;
    }
    pen.end();
    at
}

/// A label's look: its ink, code in it bold.
fn label_style(class: Class, ink: Ink, quiet: bool) -> Style {
    let base = Style { fg: ink.colour(), dim: quiet, ..Style::PLAIN };
    match class {
        Class::Code => Style { bold: true, ..base },
        Class::Escape => Style { inverse: true, ..base },
        _ => base,
    }
}

/// The cells of words with code in backticks.
fn words<'t>(cells: &mut AVec<'_, Cell<'t>>, text: &'t str, unicode: bool) -> u32 {
    cells.clear();
    let mut col = 0;
    for (at, part) in text.split('`').enumerate() {
        if at > 0 {
            col = push_cells(cells, b"`", u32::MAX, col, &|_| Class::Tick, Diff::Same, unicode, Plant::Clean);
        }
        let class = if at % 2 == 1 { Class::Code } else { Class::Plain };
        col = push_cells(cells, part.as_bytes(), u32::MAX, col, &|_| class, Diff::Same, unicode, Plant::Clean);
    }
    col
}

/// Words drawn in at most `room` columns, cut with an ellipsis when they do not fit.
fn words_in(pen: &mut Pen<'_, '_>, cells: &[Cell<'_>], room: u32, ellipsis: (&str, u32), style: &dyn Fn(Class) -> Style) {
    let total = cells.last().map_or(0, |cell| cell.col + cell.width);
    let cut = total > room;
    let limit = if cut { room.saturating_sub(ellipsis.1) } else { room };
    for cell in cells {
        if cell.col + cell.width > limit {
            break;
        }
        pen.put(style(cell.class), cell.text(), cell.width);
    }
    if cut && room >= ellipsis.1 {
        pen.put(style(Class::Plain), ellipsis.0, ellipsis.1);
    }
}

/// The labels at the end of the line, dimmed; one a line under it where they do not fit.
fn lens(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, labels: &[Label<'_>], code_end: u32) {
    if labels.is_empty() {
        return;
    }
    let budget = frame.window.budget;
    let total: u32 =
        labels.iter().map(|label| safe_width(label.text, frame.caps.unicode)).sum::<u32>() + 2 * (labels.len() as u32 - 1);
    if code_end + 3 + total <= budget {
        // Back onto the code line: it ended with a line feed; take it back off.
        pen.out.pop();
        pen.cols = frame.gutter + if code_end == 0 { 2 } else { 3 + code_end };
        pen.spaces(3);
        for (at, label) in labels.iter().enumerate() {
            if at > 0 {
                pen.spaces(2);
            }
            pen.safe(label_style(Class::Plain, label.ink, true), label.text);
        }
        pen.end();
        return;
    }
    for label in labels.iter().rev() {
        label_line(pen, frame, label);
    }
}

/// One label on a line of its own, under where its mark starts, moved left to fit.
fn label_line(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, label: &Label<'_>) {
    gutter(pen, frame, None, false, false);
    let budget = frame.window.budget;
    let width = safe_width(label.text, frame.caps.unicode);
    let start = if label.at + width <= budget { label.at } else { budget.saturating_sub(width).min(label.at) };
    pen.spaces(start);
    label_words(pen, frame, label, budget - start);
    pen.end();
}

fn label_words(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, label: &Label<'_>, room: u32) {
    let quiet = frame.caps.curly;
    let ink = label.ink;
    // A label's cells live only while drawn: a small vector in the label's own allocator would
    // be one more parameter; `words` needs an AVec, so it borrows the pen's string's allocator.
    let mut cells: AVec<'_, Cell<'_>> = AVec::new_in(*pen.out.allocator());
    words(&mut cells, label.text, frame.caps.unicode);
    let ellipsis = (frame.glyphs.ellipsis, frame.glyphs.ellipsis_width);
    words_in(pen, &cells, room, ellipsis, &|class| label_style(class, ink, quiet));
}

/// The line of `^` (a finding) and `-` (a secondary span) under the marked cells, the rightmost
/// label after them, and each other label on a line of its own.
fn carets(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, cells: &[Cell<'_>], labels: &[Label<'_>]) {
    let width = cells.last().map_or(0, |cell| cell.col + cell.width);
    let any = cells.iter().any(|cell| cell.ink != Ink::None && frame.window.screen(cell, width).is_some());
    let mut rest = labels.len();
    if any {
        gutter(pen, frame, None, false, false);
        let mut at = 0;
        for cell in cells {
            let Some(screen) = frame.window.screen(cell, width) else { continue };
            if cell.ink == Ink::None {
                continue;
            }
            if screen > at {
                pen.spaces(screen - at);
                at = screen;
            }
            let (glyph, style) = match cell.ink {
                Ink::Secondary => ("-", Style::fg(4)),
                ink => ("^", Style { bold: true, ..Style { fg: ink.colour(), ..Style::PLAIN } }),
            };
            for _ in 0..cell.width {
                pen.put(style, glyph, 1);
            }
            at += cell.width;
        }
        if let Some(last) = labels.last() {
            let room = frame.window.budget.saturating_sub(at + 1);
            if safe_width(last.text, frame.caps.unicode) <= room {
                pen.spaces(1);
                label_words(pen, frame, last, room);
                rest -= 1;
            }
        }
        pen.end();
    }
    for label in labels[..rest].iter().rev() {
        label_line(pen, frame, label);
    }
}

/// `error  the message`, wrapped at words under the message's first column.
fn headline(pen: &mut Pen<'_, '_>, finding: &Finding<'_>) {
    let caps = pen.caps;
    let word = finding.severity.label();
    let ink = ink(finding.severity);
    pen.put(Style { bold: true, ..Style { fg: ink.colour(), ..Style::PLAIN } }, word, word.len() as u32);
    pen.spaces(2);
    let indent = word.len() as u32 + 2;
    let room = caps.width().saturating_sub(indent).max(1);
    let mut cells: AVec<'_, Cell<'_>> = AVec::new_in(*pen.out.allocator());
    words(&mut cells, finding.message, caps.unicode);
    let style = |class: Class| match class {
        Class::Code => Style::BOLD,
        Class::Tick => Style::DIM,
        Class::Escape => Style { inverse: true, ..Style::PLAIN },
        _ => Style::PLAIN,
    };
    let mut col = 0;
    let mut at = 0;
    while at < cells.len() {
        if cells[at].is_space() {
            at += 1;
            continue;
        }
        let end = (at..cells.len()).find(|&it| cells[it].is_space()).unwrap_or(cells.len());
        let word: u32 = cells[at..end].iter().map(|cell| cell.width).sum();
        if col > 0 {
            if col + 1 + word > room {
                pen.end();
                pen.spaces(indent);
                col = 0;
            } else {
                pen.spaces(1);
                col += 1;
            }
        }
        for cell in &cells[at..end] {
            if col + cell.width > room && col > 0 {
                pen.end();
                pen.spaces(indent);
                col = 0;
            }
            pen.put(style(cell.class), cell.text(), cell.width);
            col += cell.width;
        }
        at = end;
    }
    pen.end();
}

/// `╭─ src/main.rs:2:18 · fn main`, the place a link where the terminal has them.
fn header(
    pen: &mut Pen<'_, '_>,
    frame: &Frame<'_, '_>,
    input: &Input<'_>,
    lead: &Finding<'_>,
    item: Option<super::lex::Head<'_>>,
) {
    let caps = frame.caps;
    let source = &input.sources[lead.file as usize];
    let (line, col) = line_col(frame.text, lead.primary.start);
    pen.spaces(frame.gutter + 1);
    pen.put(Style::DIM, frame.glyphs.top, 2);
    pen.spaces(1);
    let room = caps.width().saturating_sub(frame.gutter + 4);
    let path = safe_width(source.shown, caps.unicode);
    let place = 2 + digits(line) + digits(col);
    let name = item.map_or(0, |item| {
        3 + item.keyword.len() as u32
            + if item.name.is_empty() { 0 } else { 1 + collapsed_width(item.name, caps.unicode) }
    });
    let link = caps.hyperlinks && caps.colour != Colour::None;
    if link {
        pen.out.push_str("\x1b]8;;");
        url(pen.out, input.link, source.absolute, line, col);
        pen.out.push_str("\x1b\\");
    }
    if path + place <= room {
        pen.safe(Style::PLAIN, source.shown);
    } else {
        // The path's end, after an ellipsis: the file's name matters most.
        let keep = room.saturating_sub(place + frame.glyphs.ellipsis_width);
        let mut from = source.shown.len();
        let mut kept = 0;
        for (at, c) in source.shown.char_indices().rev() {
            let width = safe_width(&source.shown[at..at + c.len_utf8()], caps.unicode);
            if kept + width > keep {
                break;
            }
            kept += width;
            from = at;
        }
        pen.put(Style::DIM, frame.glyphs.ellipsis, frame.glyphs.ellipsis_width);
        pen.safe(Style::PLAIN, &source.shown[from..]);
    }
    pen.style(Style::DIM);
    pen.out.str(":").int(line).str(":").int(col);
    pen.cols += place;
    if link {
        pen.out.push_str("\x1b]8;;\x1b\\");
    }
    if let Some(item) = item
        && pen.cols + name <= caps.width()
    {
        pen.put(Style::DIM, frame.glyphs.dot, 3);
        pen.put(Style::DIM, item.keyword, item.keyword.len() as u32);
        if !item.name.is_empty() {
            pen.put(Style::DIM, " ", 1);
            let mut first = true;
            for word in item.name.split_ascii_whitespace() {
                if !first {
                    pen.put(Style::DIM, " ", 1);
                }
                first = false;
                pen.safe(Style::DIM, word);
            }
        }
    }
    pen.end();
}

fn collapsed_width(text: &str, unicode: bool) -> u32 {
    let mut width = 0;
    for (at, word) in text.split_ascii_whitespace().enumerate() {
        width += safe_width(word, unicode) + u32::from(at > 0);
    }
    width
}

/// `template` with `{path}` (percent-encoded), `{line}` and `{col}` filled in; every byte that
/// could end the escape (a control, a space, anything not ASCII) percent-encoded.
fn url(out: &mut AString<'_>, template: &str, path: &str, line: u32, col: u32) {
    let mut rest = template;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{path}") {
            for &byte in path.as_bytes() {
                encoded(out, byte, b"/-._~:");
            }
            rest = after;
        } else if let Some(after) = rest.strip_prefix("{line}") {
            out.int(line);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("{col}") {
            out.int(col);
            rest = after;
        } else {
            encoded(out, rest.as_bytes()[0], b"/-._~:?#[]@!$&'()*+,;=%{}");
            rest = &rest[1..];
            // A multi-byte character's later bytes: encoded one at a time.
            while !rest.is_char_boundary(0) {
                encoded(out, rest.as_bytes()[0], b"");
                rest = rest.get(1..).unwrap_or("");
            }
        }
    }
}

fn encoded(out: &mut AString<'_>, byte: u8, keep: &[u8]) {
    if byte.is_ascii_alphanumeric() || keep.contains(&byte) {
        out.push(char::from(byte));
    } else {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        out.push('%');
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0xf)]));
    }
}

/// A fix's lines after the code they change: removed text struck through, inserted text
/// green; without colour, `[-removed-]{+inserted+}`.
fn fix<'t>(pen: &mut Pen<'_, '_>, frame: &Frame<'t, '_>, finding: &Finding<'t>, cells: &mut AVec<'_, Cell<'t>>) {
    const MOST: u32 = 12;
    let text = frame.text;
    let (Some(first), Some(last)) = (finding.fix.first(), finding.fix.last()) else { return };
    let from_line = frame.lines.of_byte(first.start);
    let to_line = frame.lines.of_byte(last.end.max(last.start + 1) - 1).max(from_line);
    let from = frame.lines.start(from_line);
    let to = frame.lines.end(to_line, text);
    let plain = frame.caps.colour == Colour::None;
    let unicode = frame.caps.unicode;
    let classes = frame.classes;
    let class_of = |byte: u32| classes.at(byte);
    let mut drawn = 0;
    let mut col = 0;
    cells.clear();
    let mut flush = |pen: &mut Pen<'_, '_>, cells: &mut AVec<'_, Cell<'t>>, col: &mut u32| {
        if drawn < MOST {
            code_line(pen, frame, cells, None, false);
        }
        drawn += 1;
        cells.clear();
        *col = 0;
    };
    let marker = |cells: &mut AVec<'_, Cell<'t>>, col: &mut u32, text: &'static str, diff: Diff| {
        cells.push(Cell {
            byte: u32::MAX,
            col: *col,
            width: 2,
            show: Show::Str(text),
            class: Class::Plain,
            diff,
            ink: Ink::None,
            mark: u32::MAX,
        });
        *col += 2;
    };
    let mut at = from;
    for edit in finding.fix {
        // The text kept before the edit, line by line.
        let kept = &text[at as usize..edit.start as usize];
        let mut base = at;
        for (index, piece) in kept.split(|&b| b == b'\n').enumerate() {
            if index > 0 {
                flush(pen, cells, &mut col);
            }
            let piece = piece.strip_suffix(b"\r").unwrap_or(piece);
            col = push_cells(cells, piece, base, col, &class_of, Diff::Same, unicode, frame.plant);
            base += piece.len() as u32 + 1;
        }
        if edit.end > edit.start {
            if plain {
                marker(cells, &mut col, "[-", Diff::Removed);
            }
            let removed = &text[edit.start as usize..edit.end as usize];
            col = push_cells(cells, removed, edit.start, col, &class_of, Diff::Removed, unicode, frame.plant);
            if plain {
                marker(cells, &mut col, "-]", Diff::Removed);
            }
        }
        if !edit.insert.is_empty() {
            if plain {
                marker(cells, &mut col, "{+", Diff::Inserted);
            }
            for (index, piece) in edit.insert.split('\n').enumerate() {
                if index > 0 {
                    flush(pen, cells, &mut col);
                }
                col = push_cells(cells, piece.as_bytes(), u32::MAX, col, &|_| Class::Plain, Diff::Inserted, unicode, frame.plant);
            }
            if plain {
                marker(cells, &mut col, "+}", Diff::Inserted);
            }
        }
        at = edit.end;
    }
    let kept = &text[at as usize..to.max(at) as usize];
    let mut base = at;
    for (index, piece) in kept.split(|&b| b == b'\n').enumerate() {
        if index > 0 {
            flush(pen, cells, &mut col);
        }
        let piece = piece.strip_suffix(b"\r").unwrap_or(piece);
        col = push_cells(cells, piece, base, col, &class_of, Diff::Same, unicode, frame.plant);
        base += piece.len() as u32 + 1;
    }
    flush(pen, cells, &mut col);
    if drawn > MOST {
        pen.spaces(frame.gutter + 1);
        pen.put(Style::DIM, frame.glyphs.gap, 1);
        pen.end();
    }
}

/// `╰─ fix: blaze check --fix · blaze explain <rule>`: what to do next, as far as it fits.
fn footer(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, input: &Input<'_>, group: &[u32], counts: Counts) {
    let caps = frame.caps;
    pen.spaces(frame.gutter + 1);
    pen.put(Style::DIM, frame.glyphs.bottom, 2);
    let room = caps.width();
    let mut first = true;
    let fixable = counts.fixable == 1 && group.iter().any(|&at| !input.findings[at as usize].fix.is_empty());
    if fixable && pen.cols + 1 + 22 <= room {
        pen.spaces(1);
        pen.put(Style::DIM, "fix: ", 5);
        pen.put(Style::PLAIN, "blaze check --fix", 17);
        first = false;
    }
    for (index, &at) in group.iter().enumerate() {
        let rule = input.findings[at as usize].rule;
        if rule.is_empty() || group[..index].iter().any(|&it| input.findings[it as usize].rule == rule) {
            continue;
        }
        let width = 14 + safe_width(rule, caps.unicode) + if first { 1 } else { 3 };
        if pen.cols + width > room {
            break;
        }
        if first {
            pen.spaces(1);
        } else {
            pen.put(Style::DIM, frame.glyphs.dot, 3);
        }
        first = false;
        pen.put(Style::DIM, "blaze explain ", 14);
        pen.safe(Style::PLAIN, rule);
    }
    pen.end();
}
