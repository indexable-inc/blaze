//! The edge values of the byte, character, text, name and float draws (docs/v2/plan/design/test.md,
//! "End state: the search itself", piece S1b; decision D95): each table written once, and the
//! functions that choose from them.
//!
//! Everything that draws is behind blaze-sim's cargo feature `edge-first` (off by default, until
//! the harvest of the fifty crates' tests with it on is read). With the feature on, each draw of
//! src/draw.rs first draws a GATE, `Tape::one_in(EDGE_ONE_IN)`: its calm answer (false) is the
//! draw exactly as it was before the feature, so shrinking a gate leads from an edge to the plain
//! value. Past the gate, the draw takes an index among the table's entries the asked sizes allow
//! (entry 0 the simplest), then a length inside the asked sizes and the entry's own window, then
//! the entry's contents in one span. Every result keeps the plain draw's promises: a length in
//! `sizes`, `text_from` only its alphabet's characters, no surrogate, a finite float where one is
//! asked for, a float in its range.
//!
//! With the feature off only [`Leak`] and [`NAME_ALPHABET`] exist: the plain `name` draw reads
//! the alphabet, and `text_from` passes [`Leak::None`].

#![blaze::sans_io]

#[cfg(feature = "edge-first")]
use core::ops::RangeInclusive;

#[cfg(feature = "edge-first")]
use crate::tape::Tape;

#[cfg(test)]
mod tests;

/// A bug `text_from` can carry, for the control that proves its alphabet's oracle (rule
/// dst-not-review): passed in by value, and `None` everywhere outside that one test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leak {
    None,
    /// In the edge shape of first and last characters, the alphabet's last character is taken
    /// as the scalar value after it: an off-by-one past the alphabet's end.
    #[cfg(all(test, feature = "edge-first"))]
    PastLast,
}

/// The characters of a plain name (`Tape::name`'s draw past its gate, and the plain parts of
/// an edge name), the calm one first: ASCII letters, digits, `_` and `-`. No `.` and no space:
/// those are the edge shapes' own.
pub(crate) const NAME_ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-";

/// One draw in this many takes an edge shape (the owner's "about one case in four").
#[cfg(feature = "edge-first")]
const EDGE_ONE_IN: u64 = 4;

/// How far an edge shape's length goes past the least its window and the sizes allow: a shape
/// shows its point in a short value, which stays cheap to run and to read.
#[cfg(feature = "edge-first")]
const EDGE_SPAN: usize = 64;

/// The sizes an entry can take, `(least, most)`; `usize::MAX` for no top.
#[cfg(feature = "edge-first")]
type Window = (usize, usize);

/// From one up, with no top: the window of most shapes.
#[cfg(feature = "edge-first")]
const FROM_ONE: Window = (1, usize::MAX);

/// How many values a byte has.
#[cfg(feature = "edge-first")]
const BYTE_VALUES: u64 = 1 << u8::BITS;

// ---------------------------------------------------------------------------------------------
// The gate and the length.
// ---------------------------------------------------------------------------------------------

/// Which entry of a table of `N` the draw takes, or `None` for the plain draw: the gate first
/// (`one_in(EDGE_ONE_IN)`, calm: plain), then an index among the entries `allowed` (calm: the
/// first allowed). With no entry allowed nothing is drawn at all.
#[cfg(feature = "edge-first")]
fn edge<const N: usize>(tape: &Tape, allowed: &[bool; N], label: &'static str) -> Option<usize> {
    let count = allowed.iter().filter(|ok| **ok).count();
    if count == 0 || !tape.one_in(EDGE_ONE_IN, label) {
        return None;
    }
    let pick = tape.index(count, label);
    let at = allowed.iter().enumerate().filter(|(_, ok)| **ok).nth(pick).map(|(at, _)| at);
    Some(at.expect("`index` is below the count of allowed entries, so the entry exists"))
}

/// Whether some size in `sizes` is inside `window`.
#[cfg(feature = "edge-first")]
fn fits(sizes: &RangeInclusive<usize>, (least, most): Window) -> bool {
    (*sizes.start()).max(least) <= (*sizes.end()).min(most)
}

/// A size in both `sizes` and `window` (which [`fits`] said meet), at most [`EDGE_SPAN`] past
/// the least of them; calm is that least.
#[cfg(feature = "edge-first")]
fn length(
    tape: &Tape,
    sizes: &RangeInclusive<usize>,
    (least, most): Window,
    label: &'static str,
) -> usize {
    let from = (*sizes.start()).max(least);
    let to = (*sizes.end()).min(most).min(from.saturating_add(EDGE_SPAN));
    tape.int_in(from..=to, label)
}

/// An entry of `table` whose window `sizes` meets, past the gate, and its length.
#[cfg(feature = "edge-first")]
fn shape<S: Copy, const N: usize>(
    tape: &Tape,
    table: &[S; N],
    window: fn(S) -> Window,
    sizes: &RangeInclusive<usize>,
    label: &'static str,
) -> Option<(usize, usize)> {
    let allowed: [bool; N] = core::array::from_fn(|at| fits(sizes, window(table[at])));
    let at = edge(tape, &allowed, label)?;
    Some((at, length(tape, sizes, window(table[at]), label)))
}

// ---------------------------------------------------------------------------------------------
// Bytes.
// ---------------------------------------------------------------------------------------------

/// The bytes a run is made of: zero, all ones, the last ASCII byte, the first byte past ASCII.
#[cfg(feature = "edge-first")]
pub(crate) const RUN_BYTES: [u8; 4] = [0x00, 0xFF, 0x7F, 0x80];

/// Bytes that break any valid UTF-8 they follow: a stray continuation byte (0x80, 0xBF), a byte
/// UTF-8 never holds (0xC0, 0xFF), and a lead byte with nothing after it (0xC2, 0xE2, 0xF0).
#[cfg(feature = "edge-first")]
pub(crate) const BREAKING_BYTES: [u8; 7] = [0x80, 0xBF, 0xC0, 0xC2, 0xE2, 0xF0, 0xFF];

/// The longest pattern [`ByteShape::Pattern`] repeats (its shortest is two).
#[cfg(feature = "edge-first")]
const PATTERN_LONGEST: usize = 4;

/// The shapes of an edge byte string.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ByteShape {
    /// No bytes.
    Empty,
    /// One byte, repeated.
    Repeated(u8),
    /// 0, 1, 2, .. 255, 0, ..
    Ascending,
    /// A short pattern (two to four drawn bytes), repeated.
    Pattern,
    /// Valid UTF-8 (characters of [`EDGE_CHARS`]), then one of [`BREAKING_BYTES`].
    Utf8ThenBreak,
    /// Drawn bytes with a run of one of [`RUN_BYTES`] at least half the length inside them.
    LongRun,
}

#[cfg(feature = "edge-first")]
impl ByteShape {
    fn window(self) -> Window {
        match self {
            ByteShape::Empty => (0, 0),
            ByteShape::Repeated(_) | ByteShape::Utf8ThenBreak => FROM_ONE,
            ByteShape::Ascending | ByteShape::Pattern => (2, usize::MAX),
            ByteShape::LongRun => (3, usize::MAX),
        }
    }
}

/// The byte table, the simplest first.
#[cfg(feature = "edge-first")]
pub(crate) const BYTE_SHAPES: [ByteShape; 9] = [
    ByteShape::Empty,
    ByteShape::Repeated(RUN_BYTES[0]),
    ByteShape::Repeated(RUN_BYTES[1]),
    ByteShape::Repeated(RUN_BYTES[2]),
    ByteShape::Repeated(RUN_BYTES[3]),
    ByteShape::Ascending,
    ByteShape::Pattern,
    ByteShape::Utf8ThenBreak,
    ByteShape::LongRun,
];

/// An edge byte string of a size in `sizes`, written to `out`, and its entry of
/// [`BYTE_SHAPES`]; `None` (and nothing written) for the plain draw.
#[cfg(feature = "edge-first")]
pub(crate) fn bytes(
    tape: &Tape,
    sizes: &RangeInclusive<usize>,
    label: &'static str,
    out: &mut impl Extend<u8>,
) -> Option<usize> {
    let (at, len) = shape(tape, &BYTE_SHAPES, ByteShape::window, sizes, label)?;
    tape.span(label, |tape| write_bytes(tape, BYTE_SHAPES[at], len, label, out));
    Some(at)
}

/// `len` bytes of `shape` (`len` inside its window).
#[cfg(feature = "edge-first")]
fn write_bytes(
    tape: &Tape,
    shape: ByteShape,
    len: usize,
    label: &'static str,
    out: &mut impl Extend<u8>,
) {
    match shape {
        ByteShape::Empty => {}
        ByteShape::Repeated(byte) => out.extend(core::iter::repeat_n(byte, len)),
        ByteShape::Ascending => out.extend((0..=u8::MAX).cycle().take(len)),
        ByteShape::Pattern => {
            let long = 2 + tape.index(PATTERN_LONGEST - 1, label);
            let mut pattern = [0u8; PATTERN_LONGEST];
            for byte in &mut pattern[..long] {
                *byte = any_byte(tape, label);
            }
            out.extend(pattern[..long].iter().copied().cycle().take(len));
        }
        ByteShape::Utf8ThenBreak => {
            write_utf8(tape, len - 1, label, out);
            out.extend([*tape.one_of(&BREAKING_BYTES, label)]);
        }
        ByteShape::LongRun => {
            let run = len.div_ceil(2);
            let start = tape.index(len - run + 1, label);
            let value = *tape.one_of(&RUN_BYTES, label);
            for at in 0..len {
                let byte = match (start..start + run).contains(&at) {
                    true => value,
                    false => any_byte(tape, label),
                };
                out.extend([byte]);
            }
        }
    }
}

/// A uniform byte; calm is 0.
#[expect(
    blaze::as_cast,
    reason = "the cast is the operation: a draw below 256 taken as the byte of the same value, which every such draw is"
)]
#[cfg(feature = "edge-first")]
fn any_byte(tape: &Tape, label: &'static str) -> u8 {
    tape.below(BYTE_VALUES, label) as u8
}

/// Exactly `left` bytes of valid UTF-8, characters of [`EDGE_CHARS`] that fit what is left (an
/// ASCII entry always does). A work list, no recursion.
#[cfg(feature = "edge-first")]
fn write_utf8(tape: &Tape, mut left: usize, label: &'static str, out: &mut impl Extend<u8>) {
    while left > 0 {
        let fitting = EDGE_CHARS.iter().filter(|c| c.len_utf8() <= left).count();
        let pick = tape.index(fitting, label);
        let c = EDGE_CHARS
            .iter()
            .filter(|c| c.len_utf8() <= left)
            .nth(pick)
            .expect("an ASCII entry always fits, so `index` is below a count of at least one");
        let mut buf = [0u8; 4];
        out.extend(c.encode_utf8(&mut buf).bytes());
        left -= c.len_utf8();
    }
}

// ---------------------------------------------------------------------------------------------
// Characters.
// ---------------------------------------------------------------------------------------------

/// The characters tests break on, ASCII first: NUL, a tab, the ASCII line ends, DEL; the ends
/// of each UTF-8 length; the scalar values beside the surrogates (which are no `char` and never
/// drawn); the last scalar value; the byte-order mark; a combining mark (U+0301); a zero-width
/// joiner; the right-to-left override; the other line ends (NEL, the line separator); and two
/// whose case folding changes the length (`ß` uppercases to "SS", `İ` lowercases to "i̇").
#[cfg(feature = "edge-first")]
pub(crate) const EDGE_CHARS: [char; 21] = [
    '\0', '\t', '\n', '\r', '\u{7F}', '\u{80}', '\u{7FF}', '\u{800}', '\u{FFFF}', '\u{10000}',
    '\u{D7FF}', '\u{E000}', '\u{10FFFF}', '\u{FEFF}', '\u{301}', '\u{200D}', '\u{202E}', '\u{85}',
    '\u{2028}', '\u{DF}', '\u{130}',
];

/// An edge character, or `None` for the plain draw.
#[cfg(feature = "edge-first")]
pub(crate) fn char(tape: &Tape, label: &'static str) -> Option<char> {
    edge(tape, &[true; EDGE_CHARS.len()], label).map(|at| EDGE_CHARS[at])
}

// ---------------------------------------------------------------------------------------------
// Text.
// ---------------------------------------------------------------------------------------------

/// One text in composed and decomposed form, as `[composed, base, combining mark]`: é, Å, ñ, ö.
#[cfg(feature = "edge-first")]
pub(crate) const COMPOSED: [[char; 3]; 4] = [
    ['\u{E9}', 'e', '\u{301}'],
    ['\u{C5}', 'A', '\u{30A}'],
    ['\u{F1}', 'n', '\u{303}'],
    ['\u{F6}', 'o', '\u{308}'],
];

/// The shapes of an edge text.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextShape {
    /// No characters.
    Empty,
    /// One character of `Tape::char`, repeated.
    Repeated,
    /// CR LF pairs (an odd length ends in a lone CR).
    CrLf,
    /// One of [`COMPOSED`] in its composed and decomposed forms, in turn.
    Composed,
}

#[cfg(feature = "edge-first")]
impl TextShape {
    fn window(self) -> Window {
        match self {
            TextShape::Empty => (0, 0),
            TextShape::Repeated => FROM_ONE,
            TextShape::CrLf => (2, usize::MAX),
            TextShape::Composed => (3, usize::MAX),
        }
    }
}

/// The text table, the simplest first.
#[cfg(feature = "edge-first")]
pub(crate) const TEXT_SHAPES: [TextShape; 4] =
    [TextShape::Empty, TextShape::Repeated, TextShape::CrLf, TextShape::Composed];

/// An edge text of a count of characters in `sizes`, written to `out`, and its entry of
/// [`TEXT_SHAPES`]; `None` (and nothing written) for the plain draw.
#[cfg(feature = "edge-first")]
pub(crate) fn text(
    tape: &Tape,
    sizes: &RangeInclusive<usize>,
    label: &'static str,
    out: &mut impl Extend<char>,
) -> Option<usize> {
    let (at, len) = shape(tape, &TEXT_SHAPES, TextShape::window, sizes, label)?;
    tape.span(label, |tape| match TEXT_SHAPES[at] {
        TextShape::Empty => {}
        TextShape::Repeated => {
            let c = tape.char(label);
            out.extend(core::iter::repeat_n(c, len));
        }
        TextShape::CrLf => out.extend(['\r', '\n'].into_iter().cycle().take(len)),
        TextShape::Composed => {
            let forms = *tape.one_of(&COMPOSED, label);
            out.extend(forms.into_iter().cycle().take(len));
        }
    });
    Some(at)
}

/// The shapes an alphabet allows, for `text_from`.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FromShape {
    /// No characters.
    Empty,
    /// One character of the alphabet, repeated.
    Repeated,
    /// Only the alphabet's first and last characters.
    Ends,
}

#[cfg(feature = "edge-first")]
impl FromShape {
    fn window(self) -> Window {
        match self {
            FromShape::Empty => (0, 0),
            FromShape::Repeated => FROM_ONE,
            FromShape::Ends => (2, usize::MAX),
        }
    }
}

/// The table of `text_from`, the simplest first.
#[cfg(feature = "edge-first")]
pub(crate) const FROM_SHAPES: [FromShape; 3] =
    [FromShape::Empty, FromShape::Repeated, FromShape::Ends];

/// An edge text of `alphabet`'s characters (`count` of them, at least one), of a count in
/// `sizes`, written to `out`, and its entry of [`FROM_SHAPES`]; `None` (and nothing written)
/// for the plain draw. `leak` is [`Leak::None`] outside the control.
#[cfg(feature = "edge-first")]
pub(crate) fn text_from(
    tape: &Tape,
    alphabet: &str,
    count: usize,
    sizes: &RangeInclusive<usize>,
    label: &'static str,
    out: &mut impl Extend<char>,
    leak: Leak,
) -> Option<usize> {
    let (at, len) = shape(tape, &FROM_SHAPES, FromShape::window, sizes, label)?;
    tape.span(label, |tape| match FROM_SHAPES[at] {
        FromShape::Empty => {}
        FromShape::Repeated => {
            let c = nth(alphabet, tape.index(count, label));
            out.extend(core::iter::repeat_n(c, len));
        }
        FromShape::Ends => {
            let first = nth(alphabet, 0);
            let last = planted_last(nth(alphabet, count - 1), leak);
            for _ in 0..len {
                let c = match tape.coin(label) {
                    true => last,
                    false => first,
                };
                out.extend([c]);
            }
        }
    });
    Some(at)
}

/// The `at`th character of `text`, which has more than `at`.
#[cfg(feature = "edge-first")]
fn nth(text: &str, at: usize) -> char {
    text.chars().nth(at).expect("an index below the text's count of characters")
}

/// `last` as `leak` leaves it.
#[cfg(feature = "edge-first")]
fn planted_last(last: char, leak: Leak) -> char {
    match leak {
        Leak::None => last,
        #[cfg(test)]
        Leak::PastLast => u32::from(last)
            .checked_add(1)
            .and_then(char::from_u32)
            .expect("the control's alphabets end in a character with a next scalar value"),
    }
}

// ---------------------------------------------------------------------------------------------
// Names.
// ---------------------------------------------------------------------------------------------

/// A character of one, two, three and four UTF-8 bytes: `a`, `é`, `€`, `𝄞`.
#[cfg(feature = "edge-first")]
const WIDE: [char; 4] = ['a', '\u{E9}', '\u{20AC}', '\u{1D11E}'];

/// The shapes of an edge name (a file's or an item's).
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameShape {
    /// `.`
    Dot,
    /// `..`
    DotDot,
    /// The character, then a plain name.
    Leading(char),
    /// A plain name, then the character.
    Trailing(char),
    /// Plain names on both sides of the character.
    Inside(char),
    /// A name of exactly this many UTF-8 bytes (plain ASCII, then wider characters).
    Bytes(usize),
}

#[cfg(feature = "edge-first")]
impl NameShape {
    fn window(self) -> Window {
        match self {
            NameShape::Dot => (1, 1),
            NameShape::DotDot => (2, 2),
            NameShape::Leading(_) | NameShape::Trailing(_) => (2, usize::MAX),
            NameShape::Inside(_) => (3, usize::MAX),
            // A character is one to four bytes.
            NameShape::Bytes(bytes) => (bytes.div_ceil(4), bytes),
        }
    }
}

/// The name table, the simplest first: the two dot names, a leading or trailing space or dot,
/// a separator of either kind and a NUL inside, and a name of 255 bytes (the most most file
/// systems keep) and of 256 (one past).
#[cfg(feature = "edge-first")]
pub(crate) const NAME_SHAPES: [NameShape; 11] = [
    NameShape::Dot,
    NameShape::DotDot,
    NameShape::Leading(' '),
    NameShape::Trailing(' '),
    NameShape::Leading('.'),
    NameShape::Trailing('.'),
    NameShape::Inside('/'),
    NameShape::Inside('\\'),
    NameShape::Inside('\0'),
    NameShape::Bytes(255),
    NameShape::Bytes(256),
];

/// An edge name of a count of characters in `sizes`, written to `out`, and its entry of
/// [`NAME_SHAPES`]; `None` (and nothing written) for the plain draw.
#[cfg(feature = "edge-first")]
pub(crate) fn name(
    tape: &Tape,
    sizes: &RangeInclusive<usize>,
    label: &'static str,
    out: &mut impl Extend<char>,
) -> Option<usize> {
    let (at, len) = shape(tape, &NAME_SHAPES, NameShape::window, sizes, label)?;
    tape.span(label, |tape| match NAME_SHAPES[at] {
        NameShape::Dot => out.extend(['.']),
        NameShape::DotDot => out.extend(['.', '.']),
        NameShape::Leading(c) => {
            out.extend([c]);
            write_plain(tape, len - 1, label, out);
        }
        NameShape::Trailing(c) => {
            write_plain(tape, len - 1, label, out);
            out.extend([c]);
        }
        NameShape::Inside(c) => {
            let before = (len - 1) / 2;
            write_plain(tape, before, label, out);
            out.extend([c]);
            write_plain(tape, len - 1 - before, label, out);
        }
        NameShape::Bytes(bytes) => write_sized(tape, len, bytes, label, out),
    });
    Some(at)
}

/// `count` characters of [`NAME_ALPHABET`]; calm is `a`s.
#[cfg(feature = "edge-first")]
fn write_plain(tape: &Tape, count: usize, label: &'static str, out: &mut impl Extend<char>) {
    for _ in 0..count {
        out.extend([nth(NAME_ALPHABET, tape.index(NAME_ALPHABET.len(), label))]);
    }
}

/// `chars` characters in exactly `bytes` UTF-8 bytes, for `bytes.div_ceil(4) <= chars <=
/// bytes`: plain ASCII, then four-byte characters, then one of two or three bytes for the rest.
/// With `extra = bytes - chars` (at most `3 * chars`), `extra / 3` four-byte characters and one
/// more when `extra % 3` is not 0 are never more than `chars`, and the total is
/// `chars + extra = bytes`.
#[cfg(feature = "edge-first")]
fn write_sized(
    tape: &Tape,
    chars: usize,
    bytes: usize,
    label: &'static str,
    out: &mut impl Extend<char>,
) {
    let extra = bytes - chars;
    let four = extra / 3;
    let rest = extra % 3;
    write_plain(tape, chars - four - usize::from(rest > 0), label, out);
    out.extend(core::iter::repeat_n(WIDE[3], four));
    if rest > 0 {
        out.extend([WIDE[rest]]);
    }
}

// ---------------------------------------------------------------------------------------------
// Floats.
// ---------------------------------------------------------------------------------------------

/// 2^63: `i64::MAX as f64` rounds up to it, the first value `as i64` saturates on.
#[cfg(feature = "edge-first")]
const TWO_63: f64 = f64::from_bits(0x43E0_0000_0000_0000);
/// -2^63: `i64::MIN`, exactly.
#[cfg(feature = "edge-first")]
const NEGATIVE_TWO_63: f64 = f64::from_bits(0xC3E0_0000_0000_0000);
/// 2^64: `u64::MAX as f64` rounds up to it.
#[cfg(feature = "edge-first")]
const TWO_64: f64 = f64::from_bits(0x43F0_0000_0000_0000);

/// The value an edge float is at or beside, in either width.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Base {
    /// The least positive normal value (`MIN_POSITIVE`); the greatest subnormal is below it.
    LeastNormal,
    /// The first power of two past which not every whole number is exact: 2^53 for `f64`,
    /// 2^24 for `f32`.
    Whole,
    /// A value that exists in both widths (a power of two or its negation).
    Value(f64),
}

/// An edge float, described once for both widths.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EdgeFloat {
    At(Base),
    /// The next value toward negative infinity.
    Below(Base),
    /// The next value toward positive infinity.
    Above(Base),
    /// A NaN with a payload of 1, of either sign, quiet or signalling.
    Nan { negative: bool, signalling: bool },
}

/// The floats added to draw.rs's twelve special values. Not repeated from there: -0.0 and the
/// least subnormal (the spec lists them; draw.rs's `SPECIAL_F64` already holds them). Here: the
/// greatest subnormal; 1.0's two neighbours; the whole-number limit with its two neighbours
/// (2^53 - 1, 2^53, 2^53 + 2 for `f64`); the `i64` conversion ends (2^63, the value below it,
/// -2^63 and the value below it) and the `u64` one (2^64 and the value below it); NaNs of both
/// signs, quiet and signalling, with a payload.
#[cfg(feature = "edge-first")]
pub(crate) const EDGE_FLOATS: [EdgeFloat; 16] = [
    EdgeFloat::Below(Base::LeastNormal),
    EdgeFloat::Below(Base::Value(1.0)),
    EdgeFloat::Above(Base::Value(1.0)),
    EdgeFloat::Below(Base::Whole),
    EdgeFloat::At(Base::Whole),
    EdgeFloat::Above(Base::Whole),
    EdgeFloat::At(Base::Value(TWO_63)),
    EdgeFloat::Below(Base::Value(TWO_63)),
    EdgeFloat::At(Base::Value(NEGATIVE_TWO_63)),
    EdgeFloat::Below(Base::Value(NEGATIVE_TWO_63)),
    EdgeFloat::At(Base::Value(TWO_64)),
    EdgeFloat::Below(Base::Value(TWO_64)),
    EdgeFloat::Nan { negative: false, signalling: false },
    EdgeFloat::Nan { negative: true, signalling: false },
    EdgeFloat::Nan { negative: false, signalling: true },
    EdgeFloat::Nan { negative: true, signalling: true },
];

/// A float width: [`EDGE_FLOATS`] read in it.
#[cfg(feature = "edge-first")]
pub(crate) trait Width: Copy + PartialOrd {
    const ZERO: Self;
    const NEGATIVE_ZERO: Self;
    #[cfg(feature = "edge-first")]
    fn base(base: Base) -> Self;
    fn nan(negative: bool, signalling: bool) -> Self;
    fn up(self) -> Self;
    fn down(self) -> Self;
    fn finite(self) -> bool;

    /// `edge` in this width.
    #[cfg(feature = "edge-first")]
    fn of(edge: EdgeFloat) -> Self {
        match edge {
            EdgeFloat::At(base) => Self::base(base),
            EdgeFloat::Below(base) => Self::base(base).down(),
            EdgeFloat::Above(base) => Self::base(base).up(),
            EdgeFloat::Nan { negative, signalling } => Self::nan(negative, signalling),
        }
    }
}

#[cfg(feature = "edge-first")]
impl Width for f64 {
    const ZERO: f64 = 0.0;
    const NEGATIVE_ZERO: f64 = -0.0;

    #[cfg(feature = "edge-first")]
    fn base(base: Base) -> f64 {
        match base {
            Base::LeastNormal => f64::MIN_POSITIVE,
            // 2^53.
            Base::Whole => f64::from_bits(0x4340_0000_0000_0000),
            Base::Value(x) => x,
        }
    }

    fn nan(negative: bool, signalling: bool) -> f64 {
        f64::from_bits(
            (u64::from(negative) << 63)
                | 0x7FF0_0000_0000_0000
                | (u64::from(!signalling) << 51)
                | 1,
        )
    }

    fn up(self) -> f64 {
        self.next_up()
    }

    fn down(self) -> f64 {
        self.next_down()
    }

    fn finite(self) -> bool {
        self.is_finite()
    }
}

#[cfg(feature = "edge-first")]
impl Width for f32 {
    const ZERO: f32 = 0.0;
    const NEGATIVE_ZERO: f32 = -0.0;

    /// The `f64` table read as `f32`: each `Base::Value` exists in both widths, and the
    /// conversion refuses one that does not, naming it.
    #[cfg(feature = "edge-first")]
    fn base(base: Base) -> f32 {
        match base {
            Base::LeastNormal => f32::MIN_POSITIVE,
            // 2^24.
            Base::Whole => f32::from_bits(0x4B80_0000),
            Base::Value(x) => narrow(x),
        }
    }

    fn nan(negative: bool, signalling: bool) -> f32 {
        f32::from_bits(
            (u32::from(negative) << 31) | 0x7F80_0000 | (u32::from(!signalling) << 22) | 1,
        )
    }

    fn up(self) -> f32 {
        self.next_up()
    }

    fn down(self) -> f32 {
        self.next_down()
    }

    fn finite(self) -> bool {
        self.is_finite()
    }
}

/// `x` as the `f32` of the same value; a value `f32` does not hold exactly is refused, naming it
/// (a mistake in [`EDGE_FLOATS`], never a draw's).
#[cfg(feature = "edge-first")]
#[expect(
    blaze::as_cast,
    reason = "the cast is the operation, and the assert after it refuses any value it would change"
)]
fn narrow(x: f64) -> f32 {
    let narrowed = x as f32;
    assert!(
        f64::from(narrowed).to_bits() == x.to_bits(),
        "the edge float {x} has no exact `f32`: EDGE_FLOATS holds only values both widths hold"
    );
    narrowed
}

/// An edge float of any kind, or `None` for the plain draw.
#[cfg(feature = "edge-first")]
pub(crate) fn float<W: Width>(tape: &Tape, label: &'static str) -> Option<W> {
    edge(tape, &[true; EDGE_FLOATS.len()], label).map(|at| W::of(EDGE_FLOATS[at]))
}

/// An edge float that is finite (no NaN, no infinity), or `None` for the plain draw.
#[cfg(feature = "edge-first")]
pub(crate) fn float_finite<W: Width>(tape: &Tape, label: &'static str) -> Option<W> {
    let allowed: [bool; EDGE_FLOATS.len()] =
        core::array::from_fn(|at| W::of(EDGE_FLOATS[at]).finite());
    edge(tape, &allowed, label).map(|at| W::of(EDGE_FLOATS[at]))
}

/// The edges of a range.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeEdge {
    Start,
    End,
    /// The value after the start (a range of more than one value).
    AboveStart,
    /// The value before the end (a range of more than one value).
    BelowEnd,
    /// 0.0, when the range holds zero.
    Zero,
    /// -0.0, when the range holds zero.
    NegativeZero,
}

/// The range table, the simplest first.
#[cfg(feature = "edge-first")]
pub(crate) const RANGE_EDGES: [RangeEdge; 6] = [
    RangeEdge::Start,
    RangeEdge::End,
    RangeEdge::AboveStart,
    RangeEdge::BelowEnd,
    RangeEdge::Zero,
    RangeEdge::NegativeZero,
];

/// An edge of the finite range `lo..=hi` (`lo <= hi`, which the caller asserted), or `None`
/// for the plain draw. Every edge is inside the range: `lo`'s next value is at most `hi` when
/// `lo < hi`, and the zeros are offered only when `lo <= 0 <= hi`.
#[cfg(feature = "edge-first")]
pub(crate) fn float_in<W: Width>(tape: &Tape, lo: W, hi: W, label: &'static str) -> Option<W> {
    let wide = lo < hi;
    let holds_zero = lo <= W::ZERO && W::ZERO <= hi;
    let allowed = RANGE_EDGES.map(|edge| match edge {
        RangeEdge::Start | RangeEdge::End => true,
        RangeEdge::AboveStart | RangeEdge::BelowEnd => wide,
        RangeEdge::Zero | RangeEdge::NegativeZero => holds_zero,
    });
    edge(tape, &allowed, label).map(|at| match RANGE_EDGES[at] {
        RangeEdge::Start => lo,
        RangeEdge::End => hi,
        RangeEdge::AboveStart => lo.up(),
        RangeEdge::BelowEnd => hi.down(),
        RangeEdge::Zero => W::ZERO,
        RangeEdge::NegativeZero => W::NEGATIVE_ZERO,
    })
}
