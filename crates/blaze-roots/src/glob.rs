//! [`Glob`]: a package's declared file pattern, matched against a [`RelPath`] by segments.
//!
//! The language, and nothing more: a segment `**` matches any number of whole segments (none
//! included), except a trailing one, which matches one at least (`src/**` is what is inside
//! `src`, as gitignore and cargo's `include` read it); in any other segment `*` matches any run
//! of characters and `?` one character, never a `/`; every other character matches itself. A pattern is in the canonical form of a
//! [`RelPath`] (no leading `/`, no empty, `.` or `..` segment), so it cannot reach outside its
//! root either.

#![blaze::sans_io]

use crate::path::RelPath;

/// Why text is not a [`Glob`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlobError {
    /// Empty: a pattern that matches nothing is not declared, it is left out.
    Empty,
    /// Not in a [`RelPath`]'s canonical form (a leading `/`, an empty, `.` or `..` segment).
    NotRelative,
    /// `**` inside a segment with other characters (`a**`): ambiguous, refused.
    MixedDoubleStar,
}

/// A checked pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Glob<'g>(&'g str);

impl<'g> Glob<'g> {
    pub fn new(text: &'g str) -> Result<Glob<'g>, GlobError> {
        if text.is_empty() {
            return Err(GlobError::Empty);
        }
        if RelPath::new(text, u32::MAX).is_err() {
            return Err(GlobError::NotRelative);
        }
        if text.split('/').any(|segment| segment != "**" && segment.contains("**")) {
            return Err(GlobError::MixedDoubleStar);
        }
        Ok(Glob(text))
    }

    pub fn as_str(self) -> &'g str {
        self.0
    }

    /// Whether the file `path` matches.
    pub fn matches(self, path: RelPath<'_>) -> bool {
        let mut pattern = [""; MOST_SEGMENTS];
        let mut segments = [""; MOST_SEGMENTS];
        let (Some(mut p), Some(s)) = (fill(self.0, &mut pattern), fill(path.as_str(), &mut segments)) else {
            return false;
        };
        // A trailing `**` matches what is INSIDE (`src/**` is every file beneath `src`, not the
        // file `src`): it is read as `*/**`, one segment at least.
        if pattern[p - 1] == "**" {
            if p == MOST_SEGMENTS {
                return false;
            }
            pattern[p - 1] = "*";
            pattern[p] = "**";
            p += 1;
        }
        segments_match(&pattern[..p], &segments[..s])
    }

    /// Whether some file beneath the directory `dir` could match: what a listing of `dir` may
    /// show (a directory the pattern cannot reach is not listed).
    pub fn reaches(self, dir: RelPath<'_>) -> bool {
        let mut pattern = [""; MOST_SEGMENTS];
        let mut segments = [""; MOST_SEGMENTS];
        let (Some(p), Some(s)) = (fill(self.0, &mut pattern), fill(dir.as_str(), &mut segments)) else {
            return false;
        };
        for (at, segment) in segments[..s].iter().enumerate() {
            match pattern[..p].get(at) {
                Some(&"**") => return true,
                Some(pat) if segment_matches(pat.as_bytes(), segment.as_bytes()) => {}
                Some(_) | None => return false,
            }
        }
        // Every segment of `dir` matched one of the pattern's: a file beneath it must still
        // match the rest, which needs the pattern to go on past `dir`.
        p > s
    }
}

/// Segments a pattern or path may have for a match: a deeper path matches no glob, so a read of
/// it is refused as not declared, by name (two arrays of these are on the stack: 8 KiB).
const MOST_SEGMENTS: usize = 256;

/// `text`'s segments into `out`, how many; `None` past [`MOST_SEGMENTS`].
fn fill<'t>(text: &'t str, out: &mut [&'t str; MOST_SEGMENTS]) -> Option<usize> {
    let mut n = 0;
    for segment in text.split('/').filter(|segment| !segment.is_empty()) {
        *out.get_mut(n)? = segment;
        n += 1;
    }
    Some(n)
}

/// Segment lists matched, `**` taking any number; iterative, with one point to go back to (the
/// last `**`), which is enough for a language whose only multi-segment token is `**`.
fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    let (mut p, mut s) = (0, 0);
    let mut back: Option<(usize, usize)> = None;
    while s < path.len() {
        if p < pattern.len() && pattern[p] == "**" {
            back = Some((p, s));
            p += 1;
        } else if p < pattern.len() && segment_matches(pattern[p].as_bytes(), path[s].as_bytes()) {
            p += 1;
            s += 1;
        } else if let Some((bp, bs)) = back {
            back = Some((bp, bs + 1));
            p = bp + 1;
            s = bs + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|segment| *segment == "**")
}

/// One segment against one pattern segment: `*` any run of characters, `?` one character.
fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut back: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            back = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == b'?' {
            p += 1;
            t += char_len(text[t]);
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((bp, bt)) = back {
            let next = bt + char_len(text[bt]);
            back = Some((bp, next));
            p = bp + 1;
            t = next;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|b| *b == b'*')
}

/// The length of the UTF-8 character whose first byte is `first` (a path is checked UTF-8).
fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
