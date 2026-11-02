use blaze_ids::rng::SplitMix64 as Rng;
use text_size::TextSize;

use super::{Kernel, LineIndex, char_width, display_width, simd};

/// Texts weighted to where lines and columns could go wrong: newlines of each kind, a byte
/// order mark (at the start or not), tabs, controls, wide and multibyte chars, and lines long
/// enough to cross a 512-byte rank block with no newline in it.
fn random_text(rng: &mut Rng) -> String {
    const PIECES: &[&str] = &[
        "\n", "\n", "\n\n", "\r\n", "\r", "\r\r\n", "\t", "a", "fn f() {}", " ", "é", "名", "🦀",
        "\u{feff}", "\u{202a}", "\u{2066}", "\0", "\x7f", "\u{85}", "\u{300}", "\u{200d}",
    ];
    let mut text = String::new();
    if rng.index(4) == 0 {
        text.push('\u{feff}');
    }
    let pieces = rng.index(300);
    for _ in 0..pieces {
        match rng.index(60) {
            0 => text.push_str(&"x".repeat(rng.index(1200))),
            _ => text.push_str(PIECES[rng.index(PIECES.len())]),
        }
    }
    text
}

fn texts() -> Vec<String> {
    let mut rng = Rng::new(5);
    let mut texts: Vec<String> = (0..250).map(|_| random_text(&mut rng)).collect();
    texts.extend(
        [
            "",
            "\n",
            "a\n",
            "\u{feff}",
            "\u{feff}\n",
            "\r\n\r\n",
            &"\n".repeat(64),
            &"\n".repeat(513),
            &format!("{}\n{}", "y".repeat(4096), "z".repeat(5000)),
        ]
        .map(str::to_owned),
    );
    texts.push(include_str!("../lib.rs").to_owned());
    texts.push(include_str!("../lines.rs").to_owned());
    texts
}

/// Every kernel this CPU runs gives the scalar kernel's words, for every length and alignment.
#[test]
fn every_kernel_gives_the_scalar_bits() {
    let mut rng = Rng::new(7);
    let mut inputs: Vec<Vec<u8>> = (0..300)
        .map(|len| {
            (0..len)
                .map(|_| match rng.index(4) {
                    0 => b'\n',
                    _ => rng.index(256) as u8,
                })
                .collect()
        })
        .collect();
    inputs.push(vec![b'\n'; 4096 + 17]);
    inputs.push((0..100_000).map(|at| if at % 61 == 0 { b'\n' } else { b'q' }).collect());
    let available: Vec<Kernel> = Kernel::ALL.into_iter().filter(|it| it.available()).collect();
    assert!(available.contains(&Kernel::Scalar));
    for input in &inputs {
        for skip in [0, 1, 31, 63] {
            let bytes = &input[skip.min(input.len())..];
            let mut scalar = vec![0u64; bytes.len().div_ceil(64)];
            simd::fill(Kernel::Scalar, bytes, &mut scalar);
            for (at, &byte) in bytes.iter().enumerate() {
                assert_eq!(scalar[at / 64] >> (at % 64) & 1 == 1, byte == b'\n');
            }
            for &kernel in &available {
                let mut got = vec![!0u64; bytes.len().div_ceil(64)];
                simd::fill(kernel, bytes, &mut got);
                assert_eq!(got, scalar, "{kernel:?} on {} bytes", bytes.len());
            }
        }
    }
}

#[test]
fn every_kernel_builds_the_same_index() {
    for text in texts() {
        let scalar = LineIndex::with_kernel(&text, Kernel::Scalar).expect("scalar runs anywhere");
        for kernel in Kernel::ALL {
            if let Some(index) = LineIndex::with_kernel(&text, kernel) {
                assert_eq!(index, scalar, "{kernel:?} on {} bytes", text.len());
            }
        }
        assert_eq!(LineIndex::new(&text), scalar);
    }
}

#[test]
fn the_best_kernel_is_available() {
    assert!(Kernel::best().available());
}

/// A file as rustc's `SourceFile::new` has it: the leading byte order mark dropped and each
/// `\r\n` read as `\n`, with where each byte of the text as read went, and where its lines
/// start.
struct RustcFile {
    normal: String,
    /// By offset of the text as read (and its end): the offset in `normal`. A dropped byte
    /// goes where the next kept one does.
    map: Vec<usize>,
    starts: Vec<usize>,
}

impl RustcFile {
    fn new(text: &str) -> RustcFile {
        let bytes = text.as_bytes();
        let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
        let mut normal = Vec::with_capacity(bytes.len());
        let mut map = vec![0; bytes.len() + 1];
        for at in bom..bytes.len() {
            map[at] = normal.len();
            if !(bytes[at] == b'\r' && bytes.get(at + 1) == Some(&b'\n')) {
                normal.push(bytes[at]);
            }
        }
        map[bytes.len()] = normal.len();
        let normal = String::from_utf8(normal).expect("dropping ASCII bytes keeps UTF-8");
        // `analyze_source_file`: a start after each `\n`, and the last dropped when it is the
        // end (so an empty text has none).
        let mut starts: Vec<usize> = std::iter::once(0)
            .chain(normal.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(at, _)| at + 1))
            .collect();
        if starts.last() == Some(&normal.len()) {
            starts.pop();
        }
        RustcFile { normal, map, starts }
    }

    /// `lookup_char_pos` of byte `at` of the text as read: its line from 1 (0 where
    /// `lookup_line` finds none), then its columns from 1 in chars, display width (rustc's
    /// `col_display`: the widths of that many chars of the line's text, which has no `\n`)
    /// and bytes.
    fn place(&self, at: usize) -> (u32, u32, u32, u32) {
        let at = self.map[at];
        let line = self.starts.partition_point(|&start| start <= at);
        let Some(start) = line.checked_sub(1).map(|it| self.starts[it]) else {
            let chars = self.normal[..at].chars().count() as u32;
            return (0, chars + 1, chars + 1, at as u32 + 1);
        };
        let written = &self.normal[start..at];
        let chars = written.chars().count();
        let line_text = self.normal[start..].split('\n').next().unwrap();
        let display: usize = line_text.chars().take(chars).map(char_width).sum();
        (line as u32, chars as u32 + 1, display as u32 + 1, written.len() as u32 + 1)
    }
}

/// Every char boundary of every text is placed as rustc places it.
#[test]
fn places_are_rustcs() {
    for text in texts() {
        let index = LineIndex::new(&text);
        let rustc = RustcFile::new(&text);
        for at in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
            let place = index.place(TextSize::new(at as u32)).expect("an offset in the text");
            let got = (
                place.line(),
                place.char_column(&text).expect("a char boundary"),
                place.display_column(&text).expect("a char boundary"),
                place.byte_column(&text).expect("a char boundary"),
            );
            assert_eq!(got, rustc.place(at), "at {at} of {:?}", &text[..at.min(60)]);
            assert_eq!(index.line(TextSize::new(at as u32)), Some(place.line().saturating_sub(1)));
        }
        assert_eq!(index.place(TextSize::new(text.len() as u32 + 1)), None);
        assert_eq!(index.line(TextSize::new(text.len() as u32 + 1)), None);
    }
}

/// Each line's start and end are where splitting the text at `\n` puts them.
#[test]
fn lines_are_where_newlines_split_the_text() {
    for text in texts() {
        let index = LineIndex::new(&text);
        let mut start = 0;
        let mut lines: Vec<&str> = text.split('\n').collect();
        if text.ends_with('\n') {
            // rustc's `analyze_source_file` drops the line start at the end.
            lines.pop();
        }
        assert_eq!(index.line_count() as usize, lines.len());
        for (line, written) in lines.iter().enumerate() {
            let line = line as u32;
            assert_eq!(index.line_start(line), Some(TextSize::new(start as u32)), "line {line}");
            assert_eq!(index.line_end(line), Some(TextSize::new((start + written.len()) as u32)));
            start += written.len() + 1;
        }
        assert_eq!(index.line_start(index.line_count()), None);
        assert_eq!(index.line_end(index.line_count()), None);
        assert_eq!(index.len(), TextSize::new(text.len() as u32));
    }
}

/// The new index against what hir-expand's `line_and_column` and hir-ty-mir's computed from a
/// `Vec` of line starts: the same place everywhere but where the old one was not rustc's. At
/// the `\n` of a `\r\n` rustc counts the `\r` as no column; at the end of a text that ends with
/// `\n` it has no new line; in a text with nothing after its byte order mark it says line 0.
#[test]
fn places_match_the_old_line_starts() {
    for text in texts() {
        let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if body.is_empty() {
            let place = LineIndex::new(&text).place(TextSize::new(text.len() as u32)).unwrap();
            assert_eq!((place.line(), place.char_column(&text)), (0, Some(1)));
            continue;
        }
        let starts: Vec<u32> = std::iter::once(0)
            .chain(text.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(at, _)| at as u32 + 1))
            .collect();
        let index = LineIndex::new(&text);
        for at in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
            let line = starts.partition_point(|&start| start as usize <= at);
            let mut written = &text[starts[line - 1] as usize..at];
            if line == 1 {
                written = written.strip_prefix('\u{feff}').unwrap_or(written);
            }
            let old = (line as u32, written.chars().count() as u32 + 1);
            let place = index.place(TextSize::new(at as u32)).unwrap();
            let new = (place.line(), place.char_column(&text).unwrap());
            let bytes = text.as_bytes();
            if at == text.len() && text.ends_with('\n') {
                assert_eq!(new.0, old.0 - 1, "the end, past a last \\n");
                continue;
            }
            let at_crlf = at > 0 && bytes[at - 1] == b'\r' && bytes.get(at) == Some(&b'\n');
            match at_crlf {
                false => assert_eq!(new, old, "at {at}"),
                true => assert_eq!(new, (old.0, old.1 - 1), "at the \\n of a \\r\\n, {at}"),
            }
        }
    }
}

/// Rows of a text that is part of a file are counted by its newlines alone.
#[test]
fn rows_are_newlines_before() {
    for text in texts() {
        let index = LineIndex::new(&text);
        let (mut row, mut start) = (0u32, 0u32);
        for at in 0..=text.len() {
            assert_eq!(index.row(TextSize::new(at as u32)), Some((row, TextSize::new(start))), "at {at}");
            if text.as_bytes().get(at) == Some(&b'\n') {
                (row, start) = (row + 1, at as u32 + 1);
            }
        }
        assert_eq!(index.row(TextSize::new(text.len() as u32 + 1)), None);
    }
}

#[test]
fn rustcs_columns_by_example() {
    let place_of = |text: &str, at: u32| {
        let place = LineIndex::new(text).place(TextSize::new(at)).unwrap();
        (
            place.line(),
            place.char_column(text).unwrap(),
            place.display_column(text).unwrap(),
            place.byte_column(text).unwrap(),
        )
    };
    assert_eq!(place_of("\u{feff}fn f() {}", 3), (1, 1, 1, 1));
    assert_eq!(place_of("\u{feff}fn f() {}", 0), (1, 1, 1, 1));
    assert_eq!(place_of("\u{feff}fn f() {}", 5), (1, 3, 3, 3));
    assert_eq!(place_of("a\r\nb", 1), (1, 2, 2, 2));
    assert_eq!(place_of("a\r\nb", 2), (1, 2, 2, 2));
    assert_eq!(place_of("a\r\nb", 3), (2, 1, 1, 1));
    assert_eq!(place_of("a\rb", 2), (1, 3, 3, 3));
    assert_eq!(place_of("\tx", 1), (1, 2, 5, 2));
    assert_eq!(place_of("名x", 3), (1, 2, 3, 4));
    assert_eq!(place_of("🦀x", 4), (1, 2, 3, 5));
    assert_eq!(place_of("a\nb", 2), (2, 1, 1, 1));
    assert_eq!(place_of("a\n", 2), (1, 3, 2, 3));
    assert_eq!(place_of("a\r\n", 3), (1, 3, 2, 3));
    assert_eq!(place_of("\t\n", 2), (1, 3, 5, 3));
    assert_eq!(place_of("\n", 1), (1, 2, 1, 2));
    assert_eq!(place_of("\u{feff}\n", 4), (1, 2, 1, 2));
    assert_eq!(place_of("", 0), (0, 1, 1, 1));
    assert_eq!(place_of("\u{feff}", 3), (0, 1, 1, 1));
    assert_eq!(LineIndex::new("a\n").line_count(), 1);
    assert_eq!(LineIndex::new("a\nb").line_count(), 2);
    assert_eq!(LineIndex::new("").line_count(), 1);
}

/// A place off a char boundary has no columns; one on it does.
#[test]
fn columns_need_a_char_boundary() {
    let text = "名x";
    let place = LineIndex::new(text).place(TextSize::new(1)).unwrap();
    assert_eq!(place.char_column(text), None);
    assert_eq!(place.line(), 1);
}

#[test]
fn ascii_display_width_is_char_widths() {
    let mut rng = Rng::new(9);
    for _ in 0..2000 {
        let text: String = (0..rng.index(100))
            .map(|_| char::from(rng.index(128) as u8))
            .filter(|&ch| ch != '\n')
            .collect();
        let slow: usize = text.chars().map(char_width).sum();
        assert_eq!(display_width(&text), slow as u32, "{text:?}");
    }
}

#[test]
fn the_index_costs_an_eighth_of_the_text() {
    let text = "x".repeat(1 << 20);
    let index = LineIndex::new(&text);
    assert_eq!(index.heap_bytes(), (1 << 20) / 8 + ((1 << 20) / 512 + 1) * 4);
}

/// Building an index allocates its two buffers, the bitmap and the rank table, each once at its
/// exact length.
#[test]
fn an_index_is_two_allocations() {
    for text in ["a", "fn main() {}\n", "é\tx\r\n".repeat(700).as_str()] {
        // Feature detection is read once first, so only the build is measured.
        let _ = Kernel::best();
        let (_, used) = blaze_alloc_count::measure(|| LineIndex::new(text));
        assert_eq!(used.allocations, 2, "{} bytes: the bitmap and the rank table", text.len());
    }
}

/// Every lookup, and every column of a place, allocates nothing.
#[test]
fn lookups_allocate_nothing() {
    let text = "fn main() {\n\tlet é = '\u{4e00}';\r\n}\n".repeat(60);
    let index = LineIndex::new(&text);
    let places: Vec<TextSize> = (0..=text.len())
        .filter(|&at| text.is_char_boundary(at))
        .map(|at| TextSize::new(at as u32))
        .collect();
    blaze_alloc_count::assert_at_most(0, "line index lookups", || {
        for &at in &places {
            let place = index.place(at).expect("in the text");
            std::hint::black_box((
                place.line(),
                place.char_column(&text),
                place.display_column(&text),
                place.byte_column(&text),
                index.line(at),
                index.row(at),
            ));
        }
        for line in 0..index.line_count() {
            std::hint::black_box((index.line_start(line), index.line_end(line)));
        }
    });
}
