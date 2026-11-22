//! Diagnostics as cargo 1.99 prints them, plain (no colour): the form annotate-snippets 0.12
//! renders, written here for the two shapes cargo's manifest and config reading uses: a title
//! with a span of one source marked, and a title with notes under it.
//!
//! ```text
//! error: invalid type: integer `1`, expected SemVer version
//!  --> Cargo.toml:3:11
//!   |
//! 3 | version = 1
//!   |           ^
//! ```
//!
//! A span over several lines is drawn with rustc's margin (`/`, `|`, `|_^`). Columns count
//! characters, and a tab is drawn as four spaces, as annotate-snippets draws them; a
//! character wider than one column is counted as one.

use std::fmt::Write as _;

/// How severe: the word before the title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Error,
    Warning,
    Note,
    Help,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Note => "note",
            Level::Help => "help",
        }
    }
}

/// `level: title`, then each note as `= level: text` under an empty gutter.
pub(crate) fn with_notes(level: Level, title: &str, notes: &[(Level, &str)]) -> String {
    let mut out = String::new();
    let _ = write!(out, "{}: {title}", level.word());
    if !notes.is_empty() {
        out.push_str("\n  |");
        for (level, text) in notes {
            let _ = write!(out, "\n  = {}: ", level.word());
            // A note's later lines line up under its first.
            let indent = 4 + level.word().len() + 2;
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    out.push('\n');
                    out.extend(core::iter::repeat_n(' ', indent));
                }
                out.push_str(line);
            }
        }
    }
    out
}

/// The line of `source` that holds byte `at`: its 0-based number and its byte range, without
/// the line break.
fn line_of(source: &str, at: usize) -> (usize, usize, usize) {
    let at = at.min(source.len());
    let start = source[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = source[start..].find('\n').map_or(source.len(), |i| start + i);
    let end = if source[..end].ends_with('\r') { end - 1 } else { end };
    let number = source[..start].bytes().filter(|&b| b == b'\n').count();
    (number, start, end)
}

/// The columns a piece of a line takes as drawn.
fn columns(text: &str) -> usize {
    text.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn push_line_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            c => out.push(c),
        }
    }
}

/// `level: title`, the place, and the source lines `span` covers with it marked.
pub(crate) fn with_span(level: Level, title: &str, path: &str, source: &str, span: core::ops::Range<usize>) -> String {
    let start = span.start.min(source.len());
    let end = span.end.clamp(start, source.len());
    let (first, first_start, first_end) = line_of(source, start);
    // The last line is the one its end is on: a span that takes a line's break ends on the
    // next line, at its column 0.
    // The end of a text that ends in a break has no line of its own: it is on the last line,
    // one column past its text (the break's).
    let at_eof = end == source.len() && end > start && source.ends_with('\n');
    let (last, last_start, last_end) = match at_eof {
        true => line_of(source, end - 1),
        false => line_of(source, end),
    };
    // annotate-snippets sizes the gutter by `lines()` of the text up to the end, one less
    // than the end's line when that text ends in a break.
    let width = digits(1 + source[..end].lines().count().saturating_sub(1));
    let gutter = |out: &mut String| {
        out.extend(core::iter::repeat_n(' ', width + 1));
        out.push('|');
    };
    let mut out = String::new();
    let start_col = columns(&source[first_start..start]);
    let _ = writeln!(out, "{}: {title}", level.word());
    out.extend(core::iter::repeat_n(' ', width));
    let _ = writeln!(out, "--> {path}:{}:{}", first + 1, source[first_start..start].chars().count() + 1);
    gutter(&mut out);
    if first == last {
        head(&mut out, first + 1, width);
        push_line_text(&mut out, &source[first_start..first_end]);
        out.push('\n');
        gutter(&mut out);
        out.push(' ');
        out.extend(core::iter::repeat_n(' ', start_col));
        let marked = columns(&source[start..end.min(first_end).max(start)]).max(1);
        out.extend(core::iter::repeat_n('^', marked));
        return trim_lines(out);
    }
    // Several lines: the first opens the margin, `/` when nothing but blanks is before the
    // span, else a `_` run to it; each line after it is in the margin; the last closes it.
    let opens_at_start = source[first_start..start].chars().all(char::is_whitespace);
    head(&mut out, first + 1, width);
    out.push_str(if opens_at_start { "/ " } else { "  " });
    push_line_text(&mut out, &source[first_start..first_end]);
    if !opens_at_start {
        out.push('\n');
        gutter(&mut out);
        out.push_str("  ");
        out.extend(core::iter::repeat_n('_', start_col + 1));
        out.push('^');
    }
    // The lines between: annotate-snippets' fold (`source_map.rs`), in 0-based numbers: up
    // to three after the first, short of trailing blank or bare-delimiter ones, and the one
    // before the last unless that is blank; a gap of one line is drawn, a longer one is `...`.
    let lines: Vec<&str> = source.lines().collect();
    let shows = |line: usize| lines.get(line).is_some_and(|text| !["", "{", "}", "(", ")", "[", "]"].contains(&text.trim()));
    let middle = (first + 4).min(last);
    let until = (first..middle).rev().find(|&line| shows(line)).map_or(first, |line| line + 1);
    let mut shown: Vec<usize> = (first + 1..until).collect();
    if middle < last - 1 && shows(last - 1) {
        shown.push(last - 1);
    }
    shown.push(last);
    shown.dedup();
    let mut previous = first;
    for number in shown {
        if number <= previous {
            continue;
        }
        match number - previous {
            1 => {}
            2 => {
                head(&mut out, previous + 2, width);
                out.push_str("| ");
                push_line_text(&mut out, lines.get(previous + 1).copied().unwrap_or(""));
            }
            _ => {
                out.push_str("\n...");
                out.extend(core::iter::repeat_n(' ', width));
                out.push('|');
            }
        }
        head(&mut out, number + 1, width);
        out.push_str("| ");
        push_line_text(&mut out, lines.get(number).copied().unwrap_or(""));
        previous = number;
    }
    out.push('\n');
    gutter(&mut out);
    out.push_str(" |");
    let end_col = columns(&source[last_start..end.min(last_end).max(last_start)]) + usize::from(at_eof);
    out.extend(core::iter::repeat_n('_', end_col.max(1)));
    out.push('^');
    trim_lines(out)
}

/// Each line without its trailing spaces, as annotate-snippets' buffer writes them.
fn trim_lines(out: String) -> String {
    let mut trimmed = String::with_capacity(out.len());
    for (i, line) in out.split('\n').enumerate() {
        if i > 0 {
            trimmed.push('\n');
        }
        trimmed.push_str(line.trim_end_matches(' '));
    }
    trimmed
}

/// A new source line's head: its number right-aligned in the gutter, then `| `; a number
/// wider than the gutter runs into its space (`10| `), as annotate-snippets draws it.
fn head(out: &mut String, number: usize, width: usize) {
    let len = digits(number);
    out.push('\n');
    out.extend(core::iter::repeat_n(' ', width.saturating_sub(len)));
    let _ = write!(out, "{number}");
    out.extend(core::iter::repeat_n(' ', (width + 1).saturating_sub(width.max(len))));
    out.push_str("| ");
}

fn digits(mut n: usize) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::{Level, with_notes, with_span};

    // Each expected text is annotate-snippets 0.12.16's plain rendering of the case, recorded
    // on 2026-11-16 from a differential run in which this renderer matched it on every span of
    // nine sources (tabs, inline tables, folds, blank and bare-delimiter lines, non-ASCII).

    #[test]
    fn spans() {
        let cases: [(&str, core::ops::Range<usize>, &str); 5] = [
            (
                "[package]\nname = \"a\"\nversion = 1\n",
                31..32,
                "error: t\n --> C.toml:3:11\n  |\n3 | version = 1\n  |           ^",
            ),
            (
                "a = 1\nb = [\n  1,\n  2,\n]\nc = 3\n",
                6..21,
                "error: t\n --> C.toml:2:1\n  |\n2 | / b = [\n3 | |   1,\n4 | |   2,\n  | |____^",
            ),
            (
                "x = { a = 1,\n  b = 2 }\n",
                4..22,
                "error: t\n --> C.toml:1:5\n  |\n1 |   x = { a = 1,\n  |  _____^\n2 | |   b = 2 }\n  | |_________^",
            ),
            (
                "1\n2\n3\n4\n5\n6\n7\n8\n9\n10 = 1\n11 = [\n2\n]\n",
                0..19,
                "error: t\n  --> C.toml:1:1\n   |\n 1 | / 1\n 2 | | 2\n 3 | | 3\n 4 | | 4\n...  |\n 9 | | 9\n10 | | 10 = 1\n   | |_^",
            ),
            ("1\n2\n3\n4\n5\n6\n7\n8\n9\n10 = 1\n11 = [\n2\n]\n", 18..18, "error: t\n --> C.toml:10:1\n  |\n10| 10 = 1\n  | ^"),
        ];
        for (source, span, expected) in cases {
            assert_eq!(with_span(Level::Error, "t", "C.toml", source, span.clone()), expected, "{source:?} {span:?}");
        }
    }

    #[test]
    fn notes() {
        assert_eq!(
            with_notes(Level::Warning, "w", &[(Level::Note, "one"), (Level::Note, "two\nlines")]),
            "warning: w\n  |\n  = note: one\n  = note: two\n          lines"
        );
        assert_eq!(with_notes(Level::Warning, "w", &[(Level::Help, "h")]), "warning: w\n  |\n  = help: h");
    }
}
