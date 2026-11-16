//! The frames: findings grouped by place, the lines each frame shows (the enclosing item's
//! header and the span's lines), the window over long lines, the span marked on the code (or a
//! row of `^` under it), its label hung from it, fixes, the location row and the closing corner.

use blaze_own::{AString, AVec, Alloc};
use blaze_text::Writes;

use super::lex::{Class, Classes, highlighter};
use super::text::{Cell, Diff, Ink, Pen, Plant, Show, Style, Under, push_cells, safe_width};
use super::{Caps, Colour, Counts, Finding, Fixes, Input, digits, ink, line_col, tint};

/// The glyphs of the frame, Unicode or ASCII.
struct Glyphs {
    top: &'static str,
    bottom: &'static str,
    bar: &'static str,
    gap: &'static str,
    ellipsis: &'static str,
    ellipsis_width: u32,
    dot: &'static str,
    /// A label to the right of its span's first column: the hook under that column.
    hook_right: &'static str,
    /// A label to the left: the hook's corner under that column.
    hook_left: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    top: "╭─",
    bottom: "╰─",
    bar: "│",
    gap: "⋮",
    ellipsis: "…",
    ellipsis_width: 1,
    dot: " · ",
    hook_right: "╰─",
    hook_left: "─╯",
};
const ASCII: Glyphs = Glyphs {
    top: ",-",
    bottom: "`-",
    bar: "|",
    gap: ":",
    ellipsis: "...",
    ellipsis_width: 3,
    dot: " - ",
    hook_right: "`-",
    hook_left: "-'",
};

/// The narrowest a label wraps in to the right of its hook; narrower, it goes flush left.
const LABEL_ROOM: u32 = 24;

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

    /// Whether the line holds nothing but spaces and tabs.
    fn blank(&self, line: u32, text: &[u8]) -> bool {
        text[self.start(line) as usize..self.end(line, text) as usize].iter().all(|&b| b == b' ' || b == b'\t')
    }
}

/// A span to draw: its bytes, its lines, its ink and label.
#[derive(Clone, Copy)]
struct Drawn<'t> {
    start: u32,
    end: u32,
    first: u32,
    last: u32,
    ink: Ink,
    label: &'t str,
}

/// A label to hang: the screen column its span starts at (from the code's first column), its
/// ink and words.
#[derive(Clone, Copy)]
struct Label<'t> {
    at: u32,
    ink: Ink,
    text: &'t str,
}

/// The horizontal window a line is drawn through: `offset` columns of the line skipped,
/// `budget` columns to draw in.
#[derive(Clone, Copy)]
struct Window {
    offset: u32,
    budget: u32,
    ellipsis: u32,
}

impl Window {
    /// Around the mark starting at byte `focus`, when the line does not fit; else from the
    /// line's start, cut on the right.
    fn around(cells: &[Cell<'_>], focus: Option<u32>, budget: u32, ellipsis: u32) -> Window {
        let mut window = Window { offset: 0, budget, ellipsis };
        let width = cells.last().map_or(0, |cell| cell.col + cell.width);
        let Some(focus) = focus else { return window };
        if width <= budget {
            return window;
        }
        let at = cells.iter().find(|cell| cell.byte >= focus).map_or(width, |cell| cell.col);
        let span = cells.iter().filter(|cell| cell.ink != Ink::None && cell.col >= at).map(|cell| cell.width).sum::<u32>();
        if at + span.min(budget / 2) + ellipsis + 1 > budget {
            let want = at.saturating_sub(budget / 3);
            window.offset = cells.iter().map(|cell| cell.col).find(|&col| col >= want).unwrap_or(want);
        }
        window
    }

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
    /// The code's columns: the width less the gutter and its bar.
    budget: u32,
    plant: Plant,
    /// Scratch: wrapped words, a label's cells.
    alloc: Alloc<'a>,
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

    // The lines shown: each span's first and last, a fix's, and the enclosing item's header.
    let mut shown: AVec<'_, u32> = AVec::new_in(alloc);
    for mark in marks.iter() {
        shown.push(mark.first);
        shown.push(mark.last);
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
    let kind = highlighter(source.shown);
    for &at in group {
        let finding = &input.findings[at as usize];
        let line = lines.of_byte(finding.primary.start);
        if let Some(head) = kind.enclosing(text, finding.item, finding.primary.start, alloc) {
            let head = lines.of_byte(head);
            if head < line {
                shown.push(head);
            }
        }
    }
    shown.sort_unstable();
    shown.dedup();
    // A gap of one line is that line, dim, unless it is blank; a longer gap is a `⋮`.
    let mut filled: AVec<'_, u32> = AVec::new_in(alloc);
    for (at, &line) in shown.iter().enumerate() {
        if at > 0 && line == shown[at - 1] + 2 && !lines.blank(line - 1, text) {
            filled.push(line - 1);
        }
        filled.push(line);
    }
    let shown = filled;
    let (Some(&top), Some(&bottom)) = (shown.first(), shown.last()) else { return };

    let gutter = digits(bottom + 1);
    let budget = caps.width().saturating_sub(gutter + 3).max(1);
    let classes = Classes::of(kind, text, lines.start(top), lines.end(bottom, text), alloc);
    let frame = Frame { caps, glyphs, text, lines, classes: &classes, gutter, budget, plant, alloc };

    // One headline for findings that say the same thing (the owner's two `?` on one line).
    for (index, &at) in group.iter().enumerate() {
        let finding = &input.findings[at as usize];
        let said = group[..index].iter().any(|&it| {
            let it = &input.findings[it as usize];
            it.severity == finding.severity && it.message == finding.message
        });
        if !said {
            headline(pen, &frame, finding);
        }
    }
    location(pen, &frame, input, lead);

    let mut cells: AVec<'_, Cell<'_>> = AVec::new_in(alloc);
    let mut labels: AVec<'_, (u32, Label<'_>)> = AVec::new_in(alloc);
    let mut hung: AVec<'_, Label<'_>> = AVec::new_in(alloc);
    for (at, &line) in shown.iter().enumerate() {
        if at > 0 && line > shown[at - 1] + 2 {
            pen.spaces(gutter + 1);
            pen.put(Style::DIM, glyphs.gap, 1);
            pen.end();
        }
        let line_width = build(&mut cells, text, lines, &classes, line, caps.unicode, plant);
        let line_end = lines.end(line, text);
        // Ink the cells of each span on this line; note the labels of those ending here.
        labels.clear();
        let mut focus = None;
        for (index, mark) in marks.iter().enumerate() {
            if line < mark.first || line > mark.last {
                continue;
            }
            let first_text = cells.iter().find(|cell| !cell.is_space()).map_or(line_end, |cell| cell.byte);
            let from = if line == mark.first { mark.start } else { first_text };
            let to = if line == mark.last { mark.end } else { line_end };
            focus = focus.or(Some(from));
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
                labels.push((index as u32, Label { at: 0, ink: mark.ink, text: mark.label }));
            }
        }
        let window = Window::around(&cells, focus, budget, glyphs.ellipsis_width);
        let width = cells.last().map_or(0, |cell| cell.col + cell.width);
        hung.clear();
        for &(index, label) in labels.iter() {
            let at = cells.iter().filter(|cell| cell.mark == index).find_map(|cell| window.screen(cell, width)).unwrap_or(0);
            hung.push(Label { at, ..label });
        }
        code_line(pen, &frame, &cells, Some(line + 1), window, focus.is_some());
        if !(caps.curly && caps.colour != Colour::None) {
            carets(pen, &frame, &cells, window);
        }
        // A label said once on a line: the leftmost of its marks carries it. Hung rightmost first.
        hung.sort_unstable_by_key(|label| label.at);
        hung.dedup_by(|later, first| later.text == first.text && later.ink == first.ink);
        hung.reverse();
        label_rows(pen, &frame, &hung);
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

/// A class's look on the span's line: structure by brightness, one tint for literals.
fn class_style(class: Class, caps: Caps, plant: Plant) -> Style {
    match class {
        Class::Punct | Class::Comment | Class::Attribute | Class::Tick => Style::DIM,
        Class::Str | Class::Number => match tint(caps) {
            Some(rgb) => Style { rgb: Some(rgb), ..Style::PLAIN },
            None => Style::DIM,
        },
        _ if plant == Plant::LoudNames => Style { bold: true, ..Style::fg(1) },
        _ => Style::PLAIN,
    }
}

/// The look of a code cell: the span bold over every role; on the span's line (`line`) its
/// class's; a context line dim; a fix's removed text dim and struck, its inserted text green.
/// Where the terminal draws curly underlines, the span's is curly in its severity's colour and a
/// secondary span's single and uncoloured.
fn cell_style(cell: &Cell<'_>, line: bool, frame: &Frame<'_, '_>) -> Style {
    let caps = frame.caps;
    let span = cell.ink > Ink::Secondary && cell.diff == Diff::Same;
    let mut style = match cell.diff {
        Diff::Removed => Style { strike: true, ..Style::DIM },
        Diff::Inserted => Style::fg(2),
        Diff::Same if span => Style::BOLD,
        Diff::Same if !line => Style::DIM,
        Diff::Same => class_style(cell.class, caps, frame.plant),
    };
    style.inverse = cell.class == Class::Escape;
    if caps.curly && caps.colour != Colour::None && cell.ink != Ink::None && cell.diff == Diff::Same {
        (style.under, style.under_colour) = match cell.ink {
            Ink::Secondary => (Under::Single, None),
            ink => (Under::Curly, ink.colour()),
        };
    }
    style
}

/// The gutter (a line number, `+` for a fix's line, or nothing) and the bar, all quiet.
fn gutter(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, number: Option<u32>, plus: bool) {
    match (number, plus) {
        (Some(number), _) => {
            pen.spaces(frame.gutter - digits(number));
            pen.style(Style::DIM);
            pen.out.int(number);
            pen.cols += digits(number);
        }
        (None, true) => {
            pen.spaces(frame.gutter - 1);
            pen.put(Style::fg(2), "+", 1);
        }
        (None, false) => pen.spaces(frame.gutter),
    }
    pen.spaces(1);
    pen.put(Style::DIM, frame.glyphs.bar, 1);
    pen.spaces(1);
}

/// A line of code through `window`; `line` when the span is on it (else it is context, dim).
fn code_line(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, cells: &[Cell<'_>], number: Option<u32>, window: Window, line: bool) {
    gutter(pen, frame, number, number.is_none());
    let width = cells.last().map_or(0, |cell| cell.col + cell.width);
    let (left, limit, right) = window.span(width);
    let mut at = 0;
    if left > 0 {
        pen.put(Style::DIM, frame.glyphs.ellipsis, left);
        at = left;
    }
    for cell in cells {
        if cell.col < window.offset {
            continue;
        }
        if cell.col + cell.width > limit {
            break;
        }
        let want = cell.col - window.offset + left;
        if want > at {
            pen.spaces(want - at);
            at = want;
        }
        if cell.is_space() && cell.ink == Ink::None && cell.diff == Diff::Same {
            // A blank is a blank in any weight: no escape to say so.
            pen.spaces(cell.width);
        } else {
            pen.put(cell_style(cell, line, frame), cell.text(), cell.width);
        }
        at += cell.width;
    }
    if right {
        pen.put(Style::DIM, frame.glyphs.ellipsis, window.ellipsis);
        at += window.ellipsis;
    }
    if at == 0 && pen.out.ends_with(' ') {
        // An empty line ends at its bar: no trailing space.
        pen.out.pop();
        pen.cols -= 1;
    }
    pen.end();
}

/// The cells of words with code in backticks; their width returned.
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

/// A word of wrapped text: its cells `from..to`, its width, and whether it continues the word
/// before it (a word wider than a row, cut in pieces) with no space between.
#[derive(Clone, Copy)]
struct Word {
    from: u32,
    to: u32,
    width: u32,
    glued: bool,
}

/// `cells` (words between space cells) in rows of at most `first` columns, then `rest`: the
/// words, and each row's range of them. The last row never holds one word alone when the row
/// before it can give it a second: that word comes down with it.
fn wrap<'a>(cells: &[Cell<'_>], first: u32, rest: u32, alloc: Alloc<'a>) -> (AVec<'a, Word>, AVec<'a, (u32, u32)>) {
    let piece = first.min(rest).max(1);
    let mut words: AVec<'a, Word> = AVec::new_in(alloc);
    let mut at = 0;
    while at < cells.len() {
        if cells[at].is_space() {
            at += 1;
            continue;
        }
        let end = (at..cells.len()).find(|&it| cells[it].is_space()).unwrap_or(cells.len());
        let (mut from, mut width, mut glued) = (at, 0, false);
        for it in at..end {
            if width > 0 && width + cells[it].width > piece {
                words.push(Word { from: from as u32, to: it as u32, width, glued });
                (from, width, glued) = (it, 0, true);
            }
            width += cells[it].width;
        }
        words.push(Word { from: from as u32, to: end as u32, width, glued });
        at = end;
    }
    let mut rows: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
    let (mut col, mut row_from) = (0, 0);
    for (index, word) in words.iter().enumerate() {
        let room = if rows.is_empty() { first } else { rest };
        let need = if col == 0 || word.glued { word.width } else { 1 + word.width };
        if col > 0 && col + need > room {
            rows.push((row_from, index as u32));
            (row_from, col) = (index as u32, word.width);
        } else {
            col += need;
        }
    }
    if !words.is_empty() {
        rows.push((row_from, words.len() as u32));
    }
    let count = rows.len();
    if count >= 2 {
        let (last_from, last_to) = rows[count - 1];
        let (before_from, before_to) = rows[count - 2];
        let moved = words[before_to as usize - 1];
        let alone = words[last_from as usize];
        if last_to - last_from == 1
            && before_to - before_from >= 2
            && !alone.glued
            && !moved.glued
            && moved.width + 1 + alone.width <= rest
        {
            rows[count - 2].1 -= 1;
            rows[count - 1].0 -= 1;
        }
    }
    (words, rows)
}

/// One row of wrapped words.
fn wrapped_row(pen: &mut Pen<'_, '_>, cells: &[Cell<'_>], words: &[Word], (from, to): (u32, u32), style: &dyn Fn(Class) -> Style) {
    for index in from..to {
        let word = words[index as usize];
        if index > from && !word.glued {
            pen.spaces(1);
        }
        for cell in &cells[word.from as usize..word.to as usize] {
            pen.put(style(cell.class), cell.text(), cell.width);
        }
    }
}

/// The row of `^` (the span) and `-` (a secondary span) under the marked cells, where the
/// terminal draws no curly underline: `^` in the severity's colour, `-` dim.
fn carets(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, cells: &[Cell<'_>], window: Window) {
    let width = cells.last().map_or(0, |cell| cell.col + cell.width);
    if !cells.iter().any(|cell| cell.ink != Ink::None && window.screen(cell, width).is_some()) {
        return;
    }
    gutter(pen, frame, None, false);
    let mut at = 0;
    for cell in cells {
        let Some(screen) = window.screen(cell, width) else { continue };
        if cell.ink == Ink::None {
            continue;
        }
        if screen > at {
            pen.spaces(screen - at);
            at = screen;
        }
        let (glyph, style) = match cell.ink {
            Ink::Secondary => ("-", Style::DIM),
            ink => ("^", Style { fg: ink.colour(), ..Style::PLAIN }),
        };
        for _ in 0..cell.width {
            pen.put(style, glyph, 1);
        }
        at += cell.width;
    }
    pen.end();
}

/// A label's look: its severity's colour (a secondary span's: the ink), never dim, never bold.
fn label_style(class: Class, ink: Ink) -> Style {
    Style { fg: if ink == Ink::Secondary { None } else { ink.colour() }, inverse: class == Class::Escape, ..Style::PLAIN }
}

/// How a label hangs from its span.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hang {
    /// `╰─ label`, the hook under the span's first column.
    Right,
    /// `label ─╯`, the corner under the span's first column, the label starting at `start`.
    Left { start: u32 },
    /// `╰─ label` wrapped in the room to the hook's right.
    Wrapped,
    /// No hook: the label flush at the code's left edge, wrapped with a hanging indent.
    Flush,
}

/// The shape a label of `width` columns hangs in under column `at` of `budget`: to the right
/// when it fits, else mirrored to the left, else wrapped right of the hook when that leaves
/// [`LABEL_ROOM`], else flush left.
fn hang(at: u32, width: u32, budget: u32) -> Hang {
    if at + 3 + width <= budget {
        Hang::Right
    } else if width + 2 <= at {
        Hang::Left { start: at - 2 - width }
    } else if budget >= at + 3 + LABEL_ROOM {
        Hang::Wrapped
    } else {
        Hang::Flush
    }
}

/// The labels of one line, rightmost first, each hung from its span; on each row, a dim `│`
/// above each label still to come whose column the row leaves free.
fn label_rows(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, labels: &[Label<'_>]) {
    let unicode = frame.caps.unicode;
    let glyphs = frame.glyphs;
    let budget = frame.budget;
    let mut cells: AVec<'_, Cell<'_>> = AVec::new_in(frame.alloc);
    for (index, label) in labels.iter().enumerate() {
        let width = words(&mut cells, label.text, unicode);
        let shape = hang(label.at, width, budget);
        // Where the row's drawing starts, the room of its first row and the rest, and where
        // the rest start.
        let (start, first, rest, indent) = match shape {
            Hang::Right => (label.at, width.max(1), width.max(1), label.at + 3),
            Hang::Left { start } => (start, width.max(1), width.max(1), start),
            Hang::Wrapped => (label.at, budget - label.at - 3, budget - label.at - 3, label.at + 3),
            Hang::Flush => (0, budget, budget.saturating_sub(2).max(1), 2.min(budget - 1)),
        };
        let (words_of, rows) = wrap(&cells, first, rest, frame.alloc);
        let ink = label.ink;
        for (row, &range) in rows.iter().enumerate() {
            gutter(pen, frame, None, false);
            let mut col = 0;
            // The hooks of the labels still to come, left of this row's drawing.
            let edge = if row == 0 { start } else { indent };
            let mut previous = u32::MAX;
            for later in labels[index + 1..].iter().rev() {
                if later.at < edge && later.at != previous {
                    pen.spaces(later.at - col);
                    pen.put(Style::DIM, glyphs.bar, 1);
                    col = later.at + 1;
                    previous = later.at;
                }
            }
            pen.spaces(edge - col);
            if row == 0 && matches!(shape, Hang::Right | Hang::Wrapped) {
                pen.put(Style::DIM, glyphs.hook_right, 2);
                pen.spaces(1);
            }
            wrapped_row(pen, &cells, &words_of, range, &|class| label_style(class, ink));
            if let Hang::Left { .. } = shape {
                pen.spaces(1);
                pen.put(Style::DIM, glyphs.hook_left, 2);
            }
            pen.end();
        }
    }
}

/// `error  the message`, wrapped at words under the message's first column, never leaving one
/// word alone on its last row.
fn headline(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, finding: &Finding<'_>) {
    let caps = pen.caps;
    let word = finding.severity.label();
    let ink = ink(finding.severity);
    pen.put(Style { bold: true, ..Style { fg: ink.colour(), ..Style::PLAIN } }, word, word.len() as u32);
    pen.spaces(2);
    let indent = word.len() as u32 + 2;
    let room = caps.width().saturating_sub(indent).max(1);
    let mut cells: AVec<'_, Cell<'_>> = AVec::new_in(frame.alloc);
    words(&mut cells, finding.message, caps.unicode);
    let style = |class: Class| match class {
        Class::Code => Style::BOLD,
        Class::Tick => Style::DIM,
        Class::Escape => Style { inverse: true, ..Style::PLAIN },
        _ => Style::PLAIN,
    };
    let (words_of, rows) = wrap(&cells, room, room, frame.alloc);
    for (row, &range) in rows.iter().enumerate() {
        if row > 0 {
            pen.end();
            pen.spaces(indent);
        }
        wrapped_row(pen, &cells, &words_of, range, &style);
    }
    pen.end();
}

/// `╭─ src/main.rs:2:18`: the path in the normal ink (a link where the terminal has them),
/// the rest dim.
fn location(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, input: &Input<'_>, lead: &Finding<'_>) {
    let caps = frame.caps;
    let source = &input.sources[lead.file as usize];
    let (line, col) = line_col(frame.text, lead.primary.start);
    pen.spaces(frame.gutter + 1);
    pen.put(Style::DIM, frame.glyphs.top, 2);
    pen.spaces(1);
    let room = caps.width().saturating_sub(frame.gutter + 4);
    let path = safe_width(source.shown, caps.unicode);
    let place = 2 + digits(line) + digits(col);
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
    pen.end();
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

/// A fix's lines after the code they change: removed text dim and struck through, inserted
/// text green; without colour, `[-removed-]{+inserted+}`.
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
    let window = Window { offset: 0, budget: frame.budget, ellipsis: frame.glyphs.ellipsis_width };
    let mut drawn = 0;
    let mut col = 0;
    cells.clear();
    let mut flush = |pen: &mut Pen<'_, '_>, cells: &mut AVec<'_, Cell<'t>>, col: &mut u32| {
        if drawn < MOST {
            code_line(pen, frame, cells, None, window, false);
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

/// `╰─ fix: blaze check --fix · blaze explain <rule>`, all of it dim, as far as it fits; where
/// the command cannot apply fixes ([`Fixes::ByHand`]), `apply by hand` in place of the command.
fn footer(pen: &mut Pen<'_, '_>, frame: &Frame<'_, '_>, input: &Input<'_>, group: &[u32], counts: Counts) {
    let caps = frame.caps;
    pen.spaces(frame.gutter + 1);
    pen.put(Style::DIM, frame.glyphs.bottom, 2);
    let room = caps.width();
    let mut first = true;
    let offered = group.iter().any(|&at| !input.findings[at as usize].fix.is_empty());
    // The fix's id where the caller gave ids (D70): the command that applies this one fix.
    let id = group.iter().find(|&&at| !input.findings[at as usize].fix.is_empty()).and_then(|&at| input.ids.get(at as usize)).filter(|id| !id.is_empty());
    let words = match (input.fixes, id) {
        (Fixes::Command, Some(_)) if offered => Some("fix: blaze check --fix "),
        // One command fixes it only when it is the input's one fix.
        (Fixes::Command, None) if counts.fixable == 1 && offered => Some("fix: blaze check --fix"),
        (Fixes::ByHand, _) if offered => Some("apply by hand"),
        _ => None,
    };
    let id = id.filter(|_| input.fixes == Fixes::Command).copied().unwrap_or("");
    if let Some(words) = words
        && pen.cols + 1 + (words.len() + id.len()) as u32 <= room
    {
        pen.spaces(1);
        pen.put(Style::DIM, words, words.len() as u32);
        pen.put(Style::DIM, id, id.len() as u32);
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
        pen.safe(Style::DIM, rule);
    }
    pen.end();
}
