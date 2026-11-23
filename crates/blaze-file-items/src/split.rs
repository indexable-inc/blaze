//! A file cut into its top-level items, after an edit from the cut before it, or on many
//! threads at once; and the outlines of the parts put back together (crate docs, "A file is
//! outlined item by item").
//!
//! # The scan, and why a cut is a place to resume it
//!
//! One scan ([`scan`]) makes every cut: a token scan outside top-level brace groups, whose ends
//! `blaze_lex::skip_body` finds. Its state between tokens is the bracket depth and a cut it may
//! make once it reads the next token ([`Pending`]). A cut is made only when that pending cut is
//! decided, and a pending cut is only ever set at depth 0; so right after a cut is made, the
//! state is the same however the scan got there: depth 0 (moved only by the tokens that decided
//! the cut, which are the text's), nothing pending, at a token boundary of the text. Two scans
//! of one text that both cut at `c` make the same cuts after `c`. And a scan started at a cut
//! `b` with that state, its first token lexed from `b`, makes the cuts a scan from the start
//! makes after `b`, when the cut at `b` is decided by tokens of the text after `b` that it
//! reads too: [`resplit`] and [`join`] rest on these two facts.

use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::ContentId;
use blaze_lex::{Cpu, DocStyle, FrontmatterAllowed, TokenKind, skip_body, tokenize};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{Attr, AttrSpan, ErrorOrigin, Item, ItemFlags, ItemIndex, Range, SyntaxError};
use core::simd::{Simd, cmp::SimdPartialEq};

use crate::Outline;

/// The domain of [`Part::digest`]: a part's bytes, alone.
pub const PART_DOMAIN: &str = "blaze v2 file part 1";

/// One top-level part of a file and the digest of its bytes ([`part_digest`]): with the edition,
/// the key of its outline ([`outline_item`](crate::outline_item)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub range: Range,
    pub digest: ContentId,
}

/// The digest of a part's bytes in [`PART_DOMAIN`]: computed on any thread, one per part.
pub fn part_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(PART_DOMAIN, bytes)
}

/// The parts of `text` its top-level items are in, in order, contiguous, covering it: each
/// part is one item or more, with the trivia, attributes and doc comments before them.
///
/// The scan lexes only outside top-level brace groups, whose ends `blaze_lex::skip_body` finds.
/// It cuts after a `;` outside brackets unless an inner attribute or doc comment follows (that
/// is an error entry, which the walk tells apart only after an item), and after a top-level
/// `}` when what follows can only start an item: an identifier other than `as` or `else`, an
/// outer attribute, an outer doc comment. Nowhere else: no expression or type at the top level
/// has a `;` outside brackets, or a `}` followed by such a token, inside one item. A brace
/// group with no end (an unclosed `{`) ends the cutting; the rest is one part.
pub fn split_top_level<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, Range> {
    let mut cuts: AVec<'a, u32> = Vec::new_in(alloc);
    scan(text, 0, |cut| {
        cuts.push(cut);
        true
    });
    ranges(&cuts, 0, offset(text.len()), alloc)
}

/// The parts from `from` to `to` with cuts `cuts` between.
fn ranges<'a>(cuts: &[u32], from: u32, to: u32, alloc: Alloc<'a>) -> AVec<'a, Range> {
    let mut parts: AVec<'a, Range> = Vec::with_capacity_in(cuts.len() + 1, alloc);
    let mut start = from;
    for &cut in cuts {
        parts.push(Range::new(start, cut));
        start = cut;
    }
    parts.push(Range::new(start, to));
    parts
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("a file under 4 GiB")
}

fn wide(at: u32) -> usize {
    usize::try_from(at).expect("a file offset fits usize")
}

/// How a scan ended: at the text's end, at a brace group with no end (no cut after it, ever),
/// or because `cut` said stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    Text,
    Unclosed,
    Stopped,
}

/// The scan from `from`, 0 or a cut (module docs), giving each cut to `cut` in order until it
/// returns false.
pub(crate) fn scan(text: &str, from: usize, mut cut: impl FnMut(u32) -> bool) -> End {
    let cpu = Cpu::BASELINE;
    let bytes = text.as_bytes();
    let (mut pos, mut frontmatter) = match from {
        // A shebang line is the parser's to read: the scan starts past it.
        0 if bytes.starts_with(b"#!") && !bytes.starts_with(b"#![") => {
            (blaze_memchr::memchr(b'\n', bytes).map_or(bytes.len(), |at| at + 1), FrontmatterAllowed::Yes)
        }
        0 => (0, FrontmatterAllowed::Yes),
        from => (from, FrontmatterAllowed::No),
    };
    let mut depth = 0u32;
    let mut pending = Pending::None;
    'restart: while pos < bytes.len() {
        let mut at = pos;
        for token in tokenize(&text[pos..], frontmatter, cpu) {
            let start = at;
            at += wide(token.len);
            let kind = token.kind;
            if is_trivia(kind) {
                continue;
            }
            let (made, still) = match pending {
                Pending::None => (None, Pending::None),
                Pending::Semi(cut) | Pending::Brace(cut) if kind == TokenKind::Pound => (None, Pending::Pound(cut)),
                Pending::Pound(cut) => ((kind != TokenKind::Bang).then_some(cut), Pending::None),
                Pending::Semi(cut) => ((!is_doc(kind, DocStyle::Inner)).then_some(cut), Pending::None),
                Pending::Brace(cut) => {
                    let word = &text[start..at];
                    let item = kind == TokenKind::Ident && word != "as" && word != "else";
                    ((item || is_doc(kind, DocStyle::Outer)).then_some(cut), Pending::None)
                }
            };
            pending = still;
            if let Some(at) = made
                && !cut(at)
            {
                return End::Stopped;
            }
            match kind {
                TokenKind::OpenBrace if depth == 0 => {
                    let Ok(inside) = skip_body(cpu, &text[start..]) else { return End::Unclosed };
                    pos = start + inside + 1;
                    pending = Pending::Brace(offset(pos));
                    frontmatter = FrontmatterAllowed::No;
                    continue 'restart;
                }
                TokenKind::Semi if depth == 0 => pending = Pending::Semi(offset(at)),
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
                // An unbalanced closer is an error the parser reports; the scan stays at the top.
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        break;
    }
    End::Text
}

/// A cut the scan may make, once it reads the next token: after a `;`, after a top-level `}`,
/// or after either when a `#` followed (the cut is made unless a `!` follows the `#`).
#[derive(Clone, Copy)]
enum Pending {
    None,
    Semi(u32),
    Brace(u32),
    Pound(u32),
}

fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::LineComment { doc_style: None }
            | TokenKind::BlockComment { doc_style: None, .. }
    )
}

fn is_doc(kind: TokenKind, style: DocStyle) -> bool {
    match kind {
        TokenKind::LineComment { doc_style } | TokenKind::BlockComment { doc_style, .. } => doc_style == Some(style),
        _ => false,
    }
}

/// What [`resplit`] gives: the parts of the new text, and how much of it was scanned again.
pub struct Resplit<'a> {
    pub parts: AVec<'a, Part>,
    /// The bytes of the new text scanned and digested again: from the start of the last old
    /// part wholly before the first changed byte, to the first cut the scan made in the common
    /// suffix at an old cut (moved by the edit's length), or to the end.
    pub rescanned: Range,
    /// Whether the scan met an old cut in the common suffix; when not, it ran to the end (an
    /// edit that moved every later cut, an unclosed brace, or no old cut after the edit).
    pub synced: bool,
}

/// A planted bug for [`resplit_in`]'s test: when the scan meets no old cut, the old parts after
/// the edit are kept anyway, as a resplit that trusts the suffix would keep them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    KeepSuffixUnsynced,
}

/// The parts of `text`, an edit of `prev_text` whose parts were `prev`, equal to
/// [`split_top_level`] of `text` with each part's [`part_digest`]: the old parts before the
/// edit and after it keep their digests (those after move by the edit's length), and only the
/// bytes between are scanned and digested again.
///
/// The common prefix and suffix are found by a vector compare. The scan restarts at the start
/// `b` of the last old part that ends within the common prefix: that part's tokens decide the
/// cut at `b` and are unchanged (module docs). It stops at the first cut it makes in the common
/// suffix that is an old cut moved by the edit's length: both scans cut there, so they agree
/// after it. When it makes none, it runs to the end, which is the bound: never more than one
/// scan of the text after `b`, and [`Resplit::synced`] says so.
///
/// # Panics
///
/// When `prev` is not the parts of `prev_text` (they do not cover it in order).
pub fn resplit<'a>(prev_text: &str, prev: &[Part], text: &str, alloc: Alloc<'a>) -> Resplit<'a> {
    resplit_in(prev_text, prev, text, alloc, Plant::Clean)
}

pub(crate) fn resplit_in<'a>(prev_text: &str, prev: &[Part], text: &str, alloc: Alloc<'a>, plant: Plant) -> Resplit<'a> {
    assert!(
        prev.first().is_some_and(|part| part.range.start == 0)
            && prev.last().is_some_and(|part| wide(part.range.end) == prev_text.len())
            && prev.windows(2).all(|pair| pair[0].range.end == pair[1].range.start),
        "the previous parts do not cover the previous text in order"
    );
    let (old, new) = (prev_text.as_bytes(), text.as_bytes());
    let prefix = common_prefix(old, new);
    let suffix = common_suffix(&old[prefix..], &new[prefix..]);
    let (old_end, new_end) = (offset(old.len() - suffix), offset(new.len() - suffix));
    let prefix = offset(prefix);
    let mut parts: AVec<'a, Part> = Vec::with_capacity_in(prev.len() + 4, alloc);
    // The old parts wholly before the first change, but the last of them, are kept.
    let before = prev.partition_point(|part| part.range.end <= prefix);
    let from = match before {
        0 => 0,
        before => prev[before - 1].range.start,
    };
    parts.extend_from_slice(&prev[..before.saturating_sub(1)]);
    // A cut in the new text at `cut`, past the change, is the old cut at `cut - grown`.
    let grown = |cut: u32| -> Option<u32> { (cut + old_end).checked_sub(new_end) };
    let mut cuts: AVec<'a, u32> = Vec::new_in(alloc);
    let mut synced = None;
    let end = scan(text, wide(from), |cut| {
        cuts.push(cut);
        if cut >= new_end
            && let Some(old_cut) = grown(cut)
            && let Ok(at) = prev.binary_search_by_key(&old_cut, |part| part.range.start)
        {
            synced = Some(at);
            return false;
        }
        true
    });
    let planted = match (synced, plant, end) {
        (None, Plant::KeepSuffixUnsynced, _) => {
            let at = prev.partition_point(|part| part.range.start < old_end.max(1));
            (at < prev.len()).then_some(at)
        }
        _ => None,
    };
    let kept = synced.or(planted);
    let rescanned_end = match kept {
        Some(at) => prev[at].range.start + new_end - old_end,
        None => offset(new.len()),
    };
    if synced.is_some() {
        cuts.pop();
    } else if planted.is_some() {
        cuts.retain(|cut| *cut < rescanned_end);
    }
    let rescanned = ranges(&cuts, from, rescanned_end, alloc);
    let mut digests: AVec<'a, ContentId> = Vec::with_capacity_in(rescanned.len(), alloc);
    digests.resize(rescanned.len(), ContentId([0; 32]));
    crate::part_digests::<crate::SideBySide>(new, &rescanned, &mut digests);
    parts.extend(rescanned.iter().zip(&digests).map(|(range, digest)| Part { range: *range, digest: *digest }));
    if let Some(at) = kept {
        for part in &prev[at..] {
            let range = Range::new(part.range.start + new_end - old_end, part.range.end + new_end - old_end);
            parts.push(Part { range, digest: part.digest });
        }
    }
    Resplit { parts, rescanned: Range::new(from, rescanned_end), synced: synced.is_some() }
}

/// The number of leading bytes `a` and `b` share, 32 at a time.
pub fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let mut at = 0;
    while at + 32 <= len {
        let differ = Simd::<u8, 32>::from_slice(&a[at..at + 32])
            .simd_ne(Simd::<u8, 32>::from_slice(&b[at..at + 32]))
            .to_bitmask();
        if differ != 0 {
            return at + wide(differ.trailing_zeros());
        }
        at += 32;
    }
    while at < len && a[at] == b[at] {
        at += 1;
    }
    at
}

/// The number of trailing bytes `a` and `b` share, 32 at a time.
pub fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let (a, b) = (&a[a.len() - len..], &b[b.len() - len..]);
    let mut shared = 0;
    while shared + 32 <= len {
        let from = len - shared - 32;
        let differ = Simd::<u8, 32>::from_slice(&a[from..from + 32])
            .simd_ne(Simd::<u8, 32>::from_slice(&b[from..from + 32]))
            .to_bitmask();
        if differ != 0 {
            // Lane 31 is the last byte: the shared run is the lanes above the highest that
            // differs.
            return shared + wide(differ.leading_zeros()) - 32;
        }
        shared += 32;
    }
    while shared < len && a[len - shared - 1] == b[len - shared - 1] {
        shared += 1;
    }
    shared
}

/// Where each of `chunks` speculative scans of `text` starts ([`speculate`]): 0, then for each
/// even share of the text, the first line after it that starts with a byte that is no
/// whitespace and no closing bracket (where a top-level item starts in formatted code). A wrong
/// guess costs time, never the answer: [`join`] checks each.
pub fn chunk_starts<'a>(text: &str, chunks: usize, alloc: Alloc<'a>) -> AVec<'a, u32> {
    let bytes = text.as_bytes();
    let mut starts: AVec<'a, u32> = Vec::with_capacity_in(chunks.max(1), alloc);
    starts.push(0);
    for share in 1..chunks {
        let mut at = bytes.len() / chunks * share;
        let found = loop {
            let Some(line) = blaze_memchr::memchr(b'\n', &bytes[at..]) else { break None };
            at += line + 1;
            match bytes.get(at) {
                Some(byte) if !byte.is_ascii_whitespace() && !matches!(byte, b'}' | b')' | b']') => break Some(at),
                Some(_) => {}
                None => break None,
            }
        };
        match found {
            Some(at) if offset(at) > *starts.last().expect("0 is first") => starts.push(offset(at)),
            _ => {}
        }
    }
    starts
}

/// One chunk's speculative scan: from its start, as if a cut were there, to its first cut at
/// or past `until`.
pub struct Speculated<'a> {
    start: u32,
    cuts: AVec<'a, u32>,
    end: End,
}

/// The speculative scan of the chunk of `text` from `start` (one of [`chunk_starts`]) to
/// `until` (the next one, or the text's length): run on any thread, one per chunk, and given to
/// [`join`] in order.
pub fn speculate<'a>(text: &str, start: u32, until: u32, alloc: Alloc<'a>) -> Speculated<'a> {
    let mut cuts: AVec<'a, u32> = Vec::new_in(alloc);
    let end = scan(text, wide(start), |cut| {
        cuts.push(cut);
        cut < until
    });
    Speculated { start, cuts, end }
}

/// The parts of `text` from its chunks' speculative scans, in order: equal to
/// [`split_top_level`]. The first chunk's scan is the true one. Each later chunk's is checked at
/// its seam: the true scan resumes at the last true cut and runs until it makes a cut that a
/// later chunk's scan made too, whose cuts after it are then taken (module docs); a chunk whose
/// guess was wrong is scanned again by the true scan. The second number is how many bytes the
/// true scan went over again.
pub fn join<'a>(text: &str, chunks: &[Speculated<'_>], alloc: Alloc<'a>) -> (AVec<'a, Range>, usize) {
    let mut cuts: AVec<'a, u32> = Vec::new_in(alloc);
    let mut again = 0usize;
    let Some(first) = chunks.first() else { return (ranges(&cuts, 0, offset(text.len()), alloc), 0) };
    assert_eq!(first.start, 0, "the first chunk starts the text");
    cuts.extend_from_slice(&first.cuts);
    let mut end = first.end;
    let mut next = 1;
    while end == End::Stopped && next < chunks.len() {
        let last = cuts.last().copied().unwrap_or(0);
        // A chunk wholly before the true scan is passed over.
        let covers = |chunk: &Speculated<'_>| chunk.end != End::Stopped || chunk.cuts.last().is_some_and(|cut| *cut > last);
        if !covers(&chunks[next]) {
            next += 1;
            continue;
        }
        let mut synced = chunks[next].cuts.binary_search(&last).ok().map(|at| (next, at));
        if synced.is_none() {
            let mut look = next;
            let resumed = wide(last);
            end = scan(text, resumed, |cut| {
                cuts.push(cut);
                again = wide(cut) - resumed;
                while look < chunks.len() && chunks[look].end == End::Stopped && chunks[look].cuts.last().is_some_and(|last| *last < cut) {
                    look += 1;
                }
                match chunks.get(look).map(|chunk| chunk.cuts.binary_search(&cut)) {
                    Some(Ok(at)) => {
                        synced = Some((look, at));
                        false
                    }
                    _ => true,
                }
            });
            if synced.is_none() {
                again = text.len() - resumed;
                break;
            }
        }
        let (chunk, at) = synced.expect("synced above");
        cuts.extend_from_slice(&chunks[chunk].cuts[at + 1..]);
        end = chunks[chunk].end;
        next = chunk + 1;
    }
    // The last chunk's scan stopped at its first cut past `until`, the text's end: it ran out.
    if end == End::Stopped && next >= chunks.len() {
        let resumed = wide(cuts.last().copied().unwrap_or(0));
        scan(text, resumed, |cut| {
            cuts.push(cut);
            true
        });
        again += text.len() - resumed;
    }
    (ranges(&cuts, 0, offset(text.len()), alloc), again)
}

/// One part of an [`Assembled`] file: where it starts, and what comes before it.
#[derive(Clone, Copy)]
struct Placed<'o, 'a> {
    /// The file offset the part starts at.
    base: u32,
    /// The file's entries and attributes before the part's.
    items: u32,
    attrs: u32,
    outline: &'o Outline<'a>,
}

/// A file's outline as its parts' outlines ([`split_top_level`],
/// [`outline_item`](crate::outline_item)), each kept in its own offsets and borrowed, not
/// copied: equal, entry by entry, to the outline of the whole file ([`Assembled::to_outline`]).
/// What putting the parts together changes is computed when an entry is read: its file range,
/// parent and end, and an error's range, move by its part's base; its attributes' span by the
/// attributes before. The one change that is not a move, a top-level disambiguator counted
/// over every part's sibling keys, is held in a side table of the entries where it differs
/// from the part's own count, which is empty unless one key is written in two parts.
///
/// Making one costs one step a part and one table probe a top-level entry, the parts' sibling
/// keys; no entry, attribute or message is copied. An entry is read by a binary search over
/// the parts.
pub struct Assembled<'o, 'a> {
    parts: AVec<'a, Placed<'o, 'a>>,
    /// By file entry index, in order: each top-level entry whose disambiguator in the file is
    /// not its part's.
    disambiguators: AVec<'a, (ItemIndex, u32)>,
    /// The file's entries and attributes.
    items: u32,
    attrs: u32,
    flags: ItemFlags,
}

impl<'o, 'a> Assembled<'o, 'a> {
    /// The file whose parts' outlines are `parts`, each with the file offset its part starts
    /// at, in order.
    ///
    /// # Panics
    ///
    /// When a part but the first holds file attributes or file flags, which
    /// [`split_top_level`] never cuts for: a file's own attribute is before its first item.
    pub fn new(parts: impl Iterator<Item = (u32, &'o Outline<'a>)>, alloc: Alloc<'a>) -> Assembled<'o, 'a> {
        let count = |len: usize| u32::try_from(len).expect("an outline's entries are counted in u32");
        let mut placed: AVec<'a, Placed<'o, 'a>> = Vec::new_in(alloc);
        let mut disambiguators: AVec<'a, (ItemIndex, u32)> = Vec::new_in(alloc);
        let mut siblings: Map<ContentId, u32, BuildKeyHasher, Alloc<'a>> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: alloc, capacity: None });
        let (mut items, mut attrs, mut flags) = (0u32, 0u32, ItemFlags::NONE);
        for (index, (base, outline)) in parts.enumerate() {
            assert!(
                index == 0 || (outline.file_attrs.start == outline.file_attrs.end && outline.own_flags.is_empty()),
                "a part but the first holds file attributes or file flags"
            );
            for &(at, key) in &outline.keys {
                let seen = siblings.entry(key).or_insert(0);
                let own = outline.items[at.at()].disambiguator;
                if *seen != own {
                    disambiguators.push((ItemIndex(at.0 + items), *seen));
                }
                *seen += 1;
            }
            placed.push(Placed { base, items, attrs, outline });
            items += count(outline.items.len());
            attrs += count(outline.attrs.len());
            flags |= outline.flags;
        }
        Assembled { parts: placed, disambiguators, items, attrs, flags }
    }

    /// How many entries the file has.
    pub fn len(&self) -> usize {
        usize::try_from(self.items).expect("a u32 fits usize")
    }

    /// Whether the file has no entry.
    pub fn is_empty(&self) -> bool {
        self.items == 0
    }

    /// Every flag the file's tokens set: its parts' together, without an entry read.
    pub fn flags(&self) -> ItemFlags {
        self.flags
    }

    /// The part holding entry `at`.
    fn part_of(&self, at: ItemIndex) -> &Placed<'o, 'a> {
        let after = self.parts.partition_point(|part| part.items <= at.0);
        &self.parts[after.checked_sub(1).expect("an entry of the file is in a part")]
    }

    /// The entry at `at`, as the whole file's outline has it.
    pub fn item(&self, at: ItemIndex) -> Item {
        let part = self.part_of(at);
        self.placed(part, &part.outline.items[at.at() - wide(part.items)])
    }

    /// `item`, an entry of `part`, moved to the file.
    fn placed(&self, part: &Placed<'o, 'a>, item: &Item) -> Item {
        let mut item = *item;
        let at = part.items;
        item.range = item.range.shifted(part.base);
        item.parent = item.parent.map(|parent| ItemIndex(parent.0 + at));
        item.end = ItemIndex(item.end.0 + at);
        item.attrs = AttrSpan { start: item.attrs.start + part.attrs, end: item.attrs.end + part.attrs };
        item
    }

    /// Every entry, in the file's order, as the whole file's outline has them.
    pub fn items(&self) -> impl Iterator<Item = Item> + '_ {
        let mut fixes = self.disambiguators.iter().peekable();
        self.parts.iter().flat_map(|part| part.outline.items.iter().map(move |item| (part, item))).enumerate().map(
            move |(at, (part, item))| {
                let mut item = self.placed(part, item);
                if let Some(&&(fixed, disambiguator)) = fixes.peek()
                    && fixed.at() == at
                {
                    item.disambiguator = disambiguator;
                    fixes.next();
                }
                item
            },
        )
    }

    /// The attributes and doc comments of entry `at`: its part's, borrowed (an entry's are
    /// relative to its start, so none moves).
    pub fn attrs(&self, at: ItemIndex) -> &'o [Attr] {
        let part = self.part_of(at);
        let span = part.outline.items[at.at() - wide(part.items)].attrs;
        &part.outline.attrs[span.to_usize()]
    }

    /// The syntax errors as the whole file's outline has them: the parser's in the order found,
    /// then the lexer's, each with its message.
    pub fn errors(&self) -> impl Iterator<Item = (SyntaxError, &'o str)> + '_ {
        [ErrorOrigin::Parser, ErrorOrigin::Lexer].into_iter().flat_map(move |origin| {
            self.parts.iter().flat_map(move |part| {
                let outline: &'o Outline<'a> = part.outline;
                outline.errors.iter().filter(move |error| error.origin == origin).map(move |error| {
                    let moved = SyntaxError { range: error.range.shifted(part.base), ..*error };
                    (moved, outline.message(error))
                })
            })
        })
    }

    /// The outline of the whole file, copied out: equal to the whole file's walk.
    pub fn to_outline(&self, alloc: Alloc<'a>) -> Outline<'a> {
        let count = |len: usize| u32::try_from(len).expect("an outline's entries are counted in u32");
        let mut out = Outline {
            items: Vec::with_capacity_in(self.len(), alloc),
            attrs: Vec::with_capacity_in(wide(self.attrs), alloc),
            file_attrs: self.parts.first().map_or(AttrSpan::default(), |part| part.outline.file_attrs),
            errors: Vec::new_in(alloc),
            messages: Vec::new_in(alloc),
            keys: Vec::new_in(alloc),
            names: Vec::with_capacity_in(self.len(), alloc),
            file_names: self
                .parts
                .first()
                .map_or_else(|| crate::names_digest::file_names_digest("", &[]), |part| part.outline.file_names),
            own_flags: self.parts.first().map_or(ItemFlags::NONE, |part| part.outline.own_flags),
            flags: self.flags,
            extensions: self.parts.first().map_or(parser::Extensions::NONE, |part| part.outline.extensions),
        };
        out.items.extend(self.items());
        for part in &self.parts {
            out.attrs.extend_from_slice(&part.outline.attrs);
            // A names digest holds no position and no parent's place: a part's are the file's.
            out.names.extend_from_slice(&part.outline.names);
            out.keys.extend(part.outline.keys.iter().map(|&(at, key)| (ItemIndex(at.0 + part.items), key)));
        }
        for (error, message) in self.errors() {
            let start = count(out.messages.len());
            out.messages.extend_from_slice(message.as_bytes());
            let end = count(out.messages.len());
            out.errors.push(SyntaxError { message: Range::new(start, end), ..error });
        }
        out
    }
}

#[cfg(test)]
mod tests;
