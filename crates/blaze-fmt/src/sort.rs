// Follows `src/tools/rustfmt/src/sort.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use itertools::EitherOrBoth;
use itertools::Itertools;

/// Iterator which breaks an identifier into various [VersionChunk]s.
struct VersionChunkIter<'a> {
    ident: &'a str,
    start: usize,
}

impl<'a> VersionChunkIter<'a> {
    pub(crate) fn new(ident: &'a str) -> Self {
        Self { ident, start: 0 }
    }

    fn parse_numeric_chunk(
        &mut self,
        mut chars: std::str::CharIndices<'a>,
    ) -> Option<VersionChunk<'a>> {
        let mut end = self.start;
        let mut is_end_of_chunk = false;

        while let Some((idx, c)) = chars.next() {
            end = self.start + idx;

            if c.is_ascii_digit() {
                continue;
            }

            is_end_of_chunk = true;
            break;
        }

        let source = if is_end_of_chunk {
            let value = &self.ident[self.start..end];
            self.start = end;
            value
        } else {
            let value = &self.ident[self.start..];
            self.start = self.ident.len();
            value
        };

        let zeros = source.chars().take_while(|c| *c == '0').count();
        let value = source.parse::<usize>().ok()?;

        Some(VersionChunk::Number {
            value,
            zeros,
            source,
        })
    }

    fn parse_str_chunk(
        &mut self,
        mut chars: std::str::CharIndices<'a>,
    ) -> Option<VersionChunk<'a>> {
        let mut end = self.start;
        let mut is_end_of_chunk = false;

        while let Some((idx, c)) = chars.next() {
            end = self.start + idx;

            if c == '_' {
                is_end_of_chunk = true;
                break;
            }

            if !c.is_ascii_digit() {
                continue;
            }

            is_end_of_chunk = true;
            break;
        }

        let source = if is_end_of_chunk {
            let value = &self.ident[self.start..end];
            self.start = end;
            value
        } else {
            let value = &self.ident[self.start..];
            self.start = self.ident.len();
            value
        };

        Some(VersionChunk::Str(source))
    }
}

impl<'a> Iterator for VersionChunkIter<'a> {
    type Item = VersionChunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut chars = self.ident[self.start..].char_indices();
        let (_, next) = chars.next()?;

        if next == '_' {
            self.start = self.start + next.len_utf8();
            return Some(VersionChunk::Underscore);
        }

        if next.is_ascii_digit() {
            return self.parse_numeric_chunk(chars);
        }

        self.parse_str_chunk(chars)
    }
}

/// Represents a chunk in the version-sort algorithm
#[derive(Debug, PartialEq, Eq)]
enum VersionChunk<'a> {
    /// A single `_` in an identifier. Underscores are sorted before all other characters.
    Underscore,
    /// A &str chunk in the version sort.
    Str(&'a str),
    /// A numeric chunk in the version sort. Keeps track of the numeric value and leading zeros.
    Number {
        value: usize,
        zeros: usize,
        source: &'a str,
    },
}

/// Determine which side of the version-sort comparison had more leading zeros.
#[derive(Debug, PartialEq, Eq)]
enum MoreLeadingZeros {
    Left,
    Right,
    Equal,
}

/// Compare two identifiers based on the version sorting algorithm described in [the style guide]
///
/// [the style guide]: https://doc.rust-lang.org/nightly/style-guide/#sorting
pub(crate) fn version_sort(a: &str, b: &str) -> std::cmp::Ordering {
    let iter_a = VersionChunkIter::new(a);
    let iter_b = VersionChunkIter::new(b);
    let mut more_leading_zeros = MoreLeadingZeros::Equal;

    for either_or_both in iter_a.zip_longest(iter_b) {
        match either_or_both {
            EitherOrBoth::Left(_) => return std::cmp::Ordering::Greater,
            EitherOrBoth::Right(_) => return std::cmp::Ordering::Less,
            EitherOrBoth::Both(a, b) => match (a, b) {
                (VersionChunk::Underscore, VersionChunk::Underscore) => {
                    continue;
                }
                (VersionChunk::Underscore, _) => return std::cmp::Ordering::Less,
                (_, VersionChunk::Underscore) => return std::cmp::Ordering::Greater,
                (VersionChunk::Str(ca), VersionChunk::Str(cb))
                | (VersionChunk::Str(ca), VersionChunk::Number { source: cb, .. })
                | (VersionChunk::Number { source: ca, .. }, VersionChunk::Str(cb)) => {
                    match ca.cmp(&cb) {
                        std::cmp::Ordering::Equal => {
                            continue;
                        }
                        order @ _ => return order,
                    }
                }
                (
                    VersionChunk::Number {
                        value: va,
                        zeros: lza,
                        ..
                    },
                    VersionChunk::Number {
                        value: vb,
                        zeros: lzb,
                        ..
                    },
                ) => match va.cmp(&vb) {
                    std::cmp::Ordering::Equal => {
                        if lza == lzb {
                            continue;
                        }

                        if more_leading_zeros == MoreLeadingZeros::Equal && lza > lzb {
                            more_leading_zeros = MoreLeadingZeros::Left;
                        } else if more_leading_zeros == MoreLeadingZeros::Equal && lza < lzb {
                            more_leading_zeros = MoreLeadingZeros::Right;
                        }
                        continue;
                    }
                    order @ _ => return order,
                },
            },
        }
    }

    match more_leading_zeros {
        MoreLeadingZeros::Equal => std::cmp::Ordering::Equal,
        MoreLeadingZeros::Left => std::cmp::Ordering::Less,
        MoreLeadingZeros::Right => std::cmp::Ordering::Greater,
    }
}
