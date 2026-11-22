//! Canonical decomposition, Normalization Form D (UAX #15): each character replaced by its full
//! canonical decomposition (Hangul syllables by the algorithm of Unicode section 3.12), then each
//! run of non-starters put in canonical order, a stable sort by combining class.
//!
//! [`nfd_chars`] gives the characters of a text in NFD and allocates nothing: a run of
//! non-starters is given class by class, lowest first, each class's characters in their order
//! (a stable sort, done by reading the run once per class in it and once more to find the
//! next class). Runs are a few characters, so that is a few reads of a few characters.

use crate::{tables, value_in};

/// `Canonical_Combining_Class` of `c`; 0 is a starter.
pub fn combining_class(c: char) -> u8 {
    if u32::from(c) < 0x300 {
        return 0;
    }
    value_in(tables::COMBINING_CLASS, c, 0)
}

const S_BASE: u32 = 0xac00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11a7;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const S_COUNT: u32 = 19 * V_COUNT * T_COUNT;

/// One character's full canonical decomposition.
#[derive(Clone, Copy)]
enum Parts {
    /// A Hangul syllable: L V or L V T, every one a starter.
    Hangul { jamo: [u32; 3], len: usize },
    /// A row of the table: its characters in `NFD_CHARS`.
    Table(&'static [u32]),
    /// A character that does not decompose.
    Itself(char),
}

impl Parts {
    fn of(c: char) -> Parts {
        let cp = u32::from(c);
        if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
            let index = cp - S_BASE;
            let l = L_BASE + index / (V_COUNT * T_COUNT);
            let v = V_BASE + index % (V_COUNT * T_COUNT) / T_COUNT;
            return match index % T_COUNT {
                0 => Parts::Hangul { jamo: [l, v, 0], len: 2 },
                t => Parts::Hangul { jamo: [l, v, T_BASE + t], len: 3 },
            };
        }
        let at = tables::NFD.partition_point(|&(key, _, _)| key < cp);
        match tables::NFD.get(at) {
            Some(&(key, start, len)) if key == cp => {
                let start = usize::from(start);
                Parts::Table(&tables::NFD_CHARS[start..start + usize::from(len)])
            }
            _ => Parts::Itself(c),
        }
    }

    fn len(self) -> usize {
        match self {
            Parts::Hangul { len, .. } => len,
            Parts::Table(parts) => parts.len(),
            Parts::Itself(_) => 1,
        }
    }

    /// The `i`-th, `i` below [`Parts::len`].
    fn get(self, i: usize) -> char {
        match self {
            Parts::Hangul { jamo, .. } => char::from_u32(jamo[i]).expect("a Hangul jamo"),
            Parts::Table(parts) => char::from_u32(parts[i]).expect("a decomposition is characters"),
            Parts::Itself(c) => c,
        }
    }
}

/// A place in a text's decomposition: the byte where a character of the text starts, and which
/// of its parts. A place past the text's last part is `(len, 0)`, so two places are equal when
/// they are one place.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Place {
    at: usize,
    part: usize,
}

/// What [`Nfd`] is giving.
#[derive(Clone, Copy)]
enum State {
    /// Characters as they decompose, until a non-starter.
    Starters,
    /// The run of non-starters from `start` to `end`, by class: the characters of `class` after
    /// `scan` are still to give, then those of the next class up.
    Run { start: Place, end: Place, class: u8, scan: Place },
}

/// The characters of a text in NFD: [`nfd_chars`].
#[derive(Clone)]
pub struct Nfd<'a> {
    text: &'a str,
    /// The first place not yet in a run or given.
    next: Place,
    state: State,
}

/// The characters of `s` in NFD, in order; nothing is allocated. Appending them to a text that
/// already holds some leaves those as they are: a non-starter of `s` is never moved in front of
/// them.
pub fn nfd_chars(s: &str) -> Nfd<'_> {
    Nfd { text: s, next: Place { at: 0, part: 0 }, state: State::Starters }
}

impl Nfd<'_> {
    /// The part at `place` and the place after it; `None` past the last.
    fn part(&self, place: Place) -> Option<(char, Place)> {
        let c = self.text[place.at..].chars().next()?;
        let parts = Parts::of(c);
        let after = match place.part + 1 < parts.len() {
            true => Place { at: place.at, part: place.part + 1 },
            false => Place { at: place.at + c.len_utf8(), part: 0 },
        };
        Some((parts.get(place.part), after))
    }

    /// The part at `place`, which is inside a run this iterator found.
    fn in_run(&self, place: Place) -> (char, Place) {
        self.part(place).expect("a run is inside the text")
    }
}

impl Iterator for Nfd<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        loop {
            match self.state {
                State::Starters => {
                    let (c, after) = self.part(self.next)?;
                    let class = combining_class(c);
                    if class == 0 {
                        self.next = after;
                        return Some(c);
                    }
                    // The run of non-starters from here to the next starter or the text's end.
                    let start = self.next;
                    let (mut end, mut lowest) = (after, class);
                    while let Some((c, after)) = self.part(end) {
                        match combining_class(c) {
                            0 => break,
                            class => lowest = lowest.min(class),
                        }
                        end = after;
                    }
                    self.next = end;
                    self.state = State::Run { start, end, class: lowest, scan: start };
                }
                State::Run { start, end, class, scan } => {
                    let mut at = scan;
                    while at != end {
                        let (c, after) = self.in_run(at);
                        at = after;
                        if combining_class(c) == class {
                            self.state = State::Run { start, end, class, scan: at };
                            return Some(c);
                        }
                    }
                    // Every character of `class` given: the next class up, or the run is done.
                    let mut higher: Option<u8> = None;
                    let mut at = start;
                    while at != end {
                        let (c, after) = self.in_run(at);
                        at = after;
                        let it = combining_class(c);
                        if it > class && higher.is_none_or(|higher| it < higher) {
                            higher = Some(it);
                        }
                    }
                    self.state = match higher {
                        Some(class) => State::Run { start, end, class, scan: start },
                        None => State::Starters,
                    };
                }
            }
        }
    }
}

/// Appends `s` in NFD to `out`. What `out` held before is left as it is, and a non-starter of `s`
/// is never moved in front of it.
///
/// Deprecated: it grows a global-heap `String`. Use [`nfd_chars`] and append its characters to
/// the caller's own text (`out.extend(nfd_chars(s))`). Its caller today: blaze-fetch's
/// `tree::fold`.
#[expect(blaze::implicit_allocator, reason = "deprecated; goes with its last caller, blaze-fetch's tree::fold")]
pub fn nfd(s: &str, out: &mut alloc::string::String) {
    out.extend(nfd_chars(s));
}
