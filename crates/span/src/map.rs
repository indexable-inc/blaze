//! A map that maps a span to every position in a file. Usually maps a span to some range of positions.
//! Allows bidirectional lookup.

use std::{
    alloc::{Allocator, AllocatorNightly, Global},
    cell::RefCell,
    fmt,
    hash::Hash,
    sync::OnceLock,
};

use blaze_format::Spans;
use blaze_ids::{ContentId, radix};
use blaze_mem::scratch::{self, Scratch};
use stdx::{always, itertools::Itertools};

use crate::{
    EditionedFileId, ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, Span, SyntaxContext, TextRange,
    TextSize,
};

/// Maps absolute text ranges for the corresponding file to the relevant span data.
///
/// Two columns: where each piece of text ends, and its span. The span at an offset is a
/// branch-free binary search of the ends alone (4 bytes each, 16 to a cache line), then one
/// load of a 20-byte span; in-order lookups ([`SpanMap::span_at_from`]) are `O(1)` amortized.
/// Mapping a span down to the text is a binary search of `by_anchor`.
///
/// The columns are in `A`: the global allocator for a map that is kept, a task's frame for one
/// that is a temporary of the task. `by_anchor` is always global (made on the first map down,
/// the IDE's path).
pub struct SpanMap<A: Allocator + Clone = Global> {
    /// Where each piece of text ends, in order; it begins where the one before ends, or at 0.
    ends: Vec<TextSize, A>,
    /// The span of each piece of text.
    spans: Vec<Span, A>,
    /// The pieces' numbers sorted by their spans' [`down_key`], then number: made when a span
    /// is first mapped down (4 bytes a piece), dropped by whatever changes the pieces. Not part
    /// of what the map is.
    by_anchor: OnceLock<Box<[u32]>>,
    /// Index of the matched macro arm on successful expansion for declarative macros.
    // FIXME: Does it make sense to have this here?
    pub matched_arm: Option<u32>,
    /// The `;` an expression's expansion ended in, which the expansion's text leaves out
    /// (rustc's `ParserAnyMacro::make` drops it): where `semicolon_in_expressions_from_macros`
    /// is linted.
    pub dropped_semicolon: Option<Span>,
    /// The spans of the identifiers the expansion's `${concat(..)}`s made, in the order the
    /// transcriber made them (`mbe::MadeIdents`): what the lexer lints skip. Empty (no
    /// allocation) for every other expansion.
    pub made_idents: Box<[Span]>,
    /// Where the macro fragments start and where they end, each in order. A fragment is one
    /// token tree to the macro it is passed on to, which the text does not show.
    fragment_starts: Vec<TextSize, A>,
    fragment_ends: Vec<TextSize, A>,
    /// Each fragment's range and the span of the invisible group it was (its metavariable's),
    /// which no piece of the text holds: an invisible delimiter has no text. Sorted by start,
    /// then end; of fragments with one range (a fragment passed on whole to another macro), the
    /// innermost first.
    fragment_spans: Vec<(TextRange, Span), A>,
}

thread_local! {
    /// The scratch of the radix sorts of a map's fragment offsets, kept by the worker.
    static OFFSET_SORT: RefCell<Scratch<TextSize>> = const { RefCell::new(Scratch::new()) };
    /// The scratch of the radix sort of a map's fragment spans, kept by the worker.
    static FRAGMENT_SORT: RefCell<Scratch<(TextRange, Span)>> =
        const { RefCell::new(Scratch::new()) };
    /// The pairs a [`RealSpanMap`] is made from in [`RealSpanMap::within`] and
    /// [`RealSpanMap::without`], kept by the worker.
    static ANCHOR_PAIRS: RefCell<Scratch<(TextSize, ErasedFileAstId)>> =
        const { RefCell::new(Scratch::new()) };
}

/// What [`SpanMap::fragment_spans`] is sorted by: where a fragment starts, then where it ends.
#[inline]
fn fragment_key(range: TextRange) -> u64 {
    radix::pack_32(u32::from(range.start()), u32::from(range.end()))
}

/// How many of the sorted `offsets` are `offset`.
fn count_of(offsets: &[TextSize], offset: TextSize) -> usize {
    let first = offsets.partition_point(|&it| it < offset);
    offsets[first..].partition_point(|&it| it == offset)
}

/// The first of `keys` that `below` refuses, `below` accepting a prefix, searched from `hint`:
/// forward in steps of 1, 2, 4, ... then a binary search of the last step, or a binary search
/// of what is before `hint` when the answer is there. `O(log d)` for an answer `d` away from
/// `hint`, and two compares when it is at `hint` or the one after.
#[inline]
fn search_from<T: Copy>(keys: &[T], hint: usize, below: impl Fn(T) -> bool) -> usize {
    let hint = hint.min(keys.len());
    if hint > 0 && !below(keys[hint - 1]) {
        return keys[..hint].partition_point(|&it| below(it));
    }
    // Every key before `lo` is accepted.
    let (mut lo, mut step) = (hint, 1);
    loop {
        let hi = lo + step;
        if hi > keys.len() {
            return lo + keys[lo..].partition_point(|&it| below(it));
        }
        if !below(keys[hi - 1]) {
            return lo + keys[lo..hi - 1].partition_point(|&it| below(it));
        }
        lo = hi;
        step *= 2;
    }
}

/// What [`SpanMap::by_anchor`] sorts pieces by: their span's anchor, then range.
#[inline]
fn down_key(span: &Span) -> [u32; 4] {
    [
        span.anchor.file_id.as_u32(),
        span.anchor.ast_id.into_raw(),
        u32::from(span.range.start()),
        u32::from(span.range.end()),
    ]
}

impl<A: Allocator + Clone> PartialEq for SpanMap<A> {
    fn eq(&self, other: &SpanMap<A>) -> bool {
        self.ends == other.ends
            && self.spans == other.spans
            && self.matched_arm == other.matched_arm
            && self.dropped_semicolon == other.dropped_semicolon
            && self.made_idents == other.made_idents
            && self.fragment_starts == other.fragment_starts
            && self.fragment_ends == other.fragment_ends
            && self.fragment_spans == other.fragment_spans
    }
}

impl<A: Allocator + Clone> Eq for SpanMap<A> {}

impl<A: Allocator + Clone> Hash for SpanMap<A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ends.hash(state);
        self.spans.hash(state);
        self.matched_arm.hash(state);
        self.dropped_semicolon.hash(state);
        self.made_idents.hash(state);
        self.fragment_starts.hash(state);
        self.fragment_ends.hash(state);
        self.fragment_spans.hash(state);
    }
}

impl<A: Allocator + Clone> fmt::Debug for SpanMap<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpanMap")
            // alloc: `Debug` output, the edge
            .field("spans", &self.iter().collect::<Vec<_>>())
            .field("matched_arm", &self.matched_arm)
            .field("dropped_semicolon", &self.dropped_semicolon)
            .field("made_idents", &self.made_idents)
            .field("fragment_starts", &self.fragment_starts)
            .field("fragment_ends", &self.fragment_ends)
            .field("fragment_spans", &self.fragment_spans)
            .finish()
    }
}

impl SpanMap {
    /// Creates a new empty [`SpanMap`].
    pub fn empty() -> Self {
        Self::with_capacity(0)
    }

    /// An empty [`SpanMap`] with room for `pieces` [`SpanMap::push`]es without reallocating.
    pub fn with_capacity(pieces: usize) -> Self {
        Self {
            ends: Vec::with_capacity(pieces),
            spans: Vec::with_capacity(pieces),
            by_anchor: OnceLock::new(),
            matched_arm: None,
            dropped_semicolon: None,
            // alloc: none; an empty box
            made_idents: Box::default(),
            // alloc: none; `Vec::new` allocates only when a fragment is pushed
            fragment_starts: Vec::new(),
            // alloc: as `fragment_starts`
            fragment_ends: Vec::new(),
            // alloc: as `fragment_starts`
            fragment_spans: Vec::new(),
        }
    }
}

impl<A: Allocator + Clone> SpanMap<A> {
    /// Finalizes the [`SpanMap`]: validates that the offsets are in order and sorts the
    /// fragments. The columns keep their capacity: a map that is kept is copied out at its
    /// length ([`SpanMap::copy_in`]).
    pub fn finish(&mut self) {
        always!(self.ends.iter().array_windows().all(|[a, b]| a < b), "spans are not in order");
        self.sort_fragments();
    }

    /// Sorts the fragment columns, each in the worker's scratch.
    fn sort_fragments(&mut self) {
        OFFSET_SORT.with_borrow_mut(|scratch| {
            let key = |it: &TextSize| u32::from(*it);
            radix::sort_copy_by_key_in(&mut self.fragment_starts, key, scratch.take());
            radix::sort_copy_by_key_in(&mut self.fragment_ends, key, scratch.take());
        });
        FRAGMENT_SORT.with_borrow_mut(|scratch| {
            radix::sort_copy_by_key_in(
                &mut self.fragment_spans,
                |&(range, _)| fragment_key(range),
                scratch.take(),
            )
        });
    }

    /// Empties the map, keeping each column's capacity: the worker's map, filled again.
    pub fn clear(&mut self) {
        self.ends.clear();
        self.spans.clear();
        self.by_anchor = OnceLock::new();
        self.matched_arm = None;
        self.dropped_semicolon = None;
        self.made_idents = Box::default();
        self.fragment_starts.clear();
        self.fragment_ends.clear();
        self.fragment_spans.clear();
    }

    /// Room for `pieces` more [`SpanMap::push`]es without reallocating.
    pub fn reserve(&mut self, pieces: usize) {
        self.ends.reserve(pieces);
        self.spans.reserve(pieces);
    }

    /// How many pieces the map has room for without reallocating.
    pub fn capacity(&self) -> usize {
        self.ends.capacity()
    }

    /// The same map in `alloc`, each column at its exact length (an empty one allocates
    /// nothing); `by_anchor` is made again on its first use.
    pub fn copy_in<B: Allocator + Clone>(&self, alloc: B) -> SpanMap<B> {
        fn copy<T: Copy, B: Allocator + Clone>(from: &[T], alloc: &B) -> Vec<T, B> {
            // The allocator handle is cloned: a frame's reference, or `Global`.
            // alloc: the map's column, its result, at its exact length
            let mut to = Vec::with_capacity_in(from.len(), alloc.clone());
            to.extend_from_slice(from);
            to
        }
        SpanMap {
            ends: copy(&self.ends, &alloc),
            spans: copy(&self.spans, &alloc),
            by_anchor: OnceLock::new(),
            matched_arm: self.matched_arm,
            dropped_semicolon: self.dropped_semicolon,
            // alloc: the made spans, when there are any (an empty box allocates nothing)
            made_idents: Box::from(&*self.made_idents),
            fragment_starts: copy(&self.fragment_starts, &alloc),
            fragment_ends: copy(&self.fragment_ends, &alloc),
            fragment_spans: copy(&self.fragment_spans, &alloc),
        }
    }

    /// Pushes a new span onto the [`SpanMap`].
    pub fn push(&mut self, offset: TextSize, span: Span) {
        if cfg!(debug_assertions)
            && let Some(&last_offset) = self.ends.last()
        {
            assert!(
                last_offset < offset,
                "last_offset({last_offset:?}) must be smaller than offset({offset:?})"
            );
        }
        self.ends.push(offset);
        self.spans.push(span);
        self.by_anchor = OnceLock::new();
    }

    /// The pieces sorted by [`down_key`], then number, made on first use: `O(n)` once (a stable
    /// radix sort of the numbers in order), then an acquire load per call.
    fn by_anchor(&self) -> &[u32] {
        self.by_anchor.get_or_init(|| {
            let mut pieces = (0..self.spans.len() as u32).collect::<Box<[u32]>>();
            radix::sort_by_key(&mut pieces, |&it| {
                let [file, ast_id, start, end] = down_key(&self.spans[it as usize]);
                (u128::from(file) << 96)
                    | (u128::from(ast_id) << 64)
                    | (u128::from(start) << 32)
                    | u128::from(end)
            });
            pieces
        })
    }

    /// The range of piece `idx` and its span's context.
    #[inline]
    fn range_of(&self, idx: u32) -> (TextRange, SyntaxContext) {
        let idx = idx as usize;
        (TextRange::new(self.start_of(idx), self.ends[idx]), self.spans[idx].ctx)
    }

    /// Where piece `idx` of the text begins: where the one before ends.
    #[inline]
    fn start_of(&self, idx: usize) -> TextSize {
        idx.checked_sub(1).map_or(TextSize::new(0), |prev| self.ends[prev])
    }

    /// Records that the tokens of `range` are one macro fragment, an invisible group of `span`.
    /// Of fragments with one range, push the innermost first (as a group closes).
    pub fn push_fragment(&mut self, range: TextRange, span: Span) {
        self.fragment_starts.push(range.start());
        self.fragment_ends.push(range.end());
        self.fragment_spans.push((range, span));
    }

    /// The span of the fragment whose range is `range`, exactly: the invisible group's, which
    /// is its metavariable's. Of fragments with that range, the innermost's. `None` when no
    /// fragment is exactly `range`. A binary search of a finished map's fragments.
    pub fn fragment_span(&self, range: TextRange) -> Option<Span> {
        let key = fragment_key(range);
        let first = self.fragment_spans.partition_point(|&(it, _)| fragment_key(it) < key);
        match self.fragment_spans.get(first) {
            Some(&(it, span)) if it == range => Some(span),
            _ => None,
        }
    }

    /// The outermost fragments inside `range`, in text order, each with its span (of fragments
    /// with one range, the innermost's, as [`Self::fragment_span`]). A fragment that starts in
    /// `range` and ends past it is not one, nor is any fragment inside another of them. A
    /// binary search a fragment taken, then a scan of the fragments starting where it does.
    pub fn outermost_fragments_in(
        &self,
        range: TextRange,
    ) -> impl Iterator<Item = (TextRange, Span)> + '_ {
        let mut from = range.start();
        std::iter::from_fn(move || {
            let mut at = self.fragment_spans.partition_point(|&(it, _)| it.start() < from);
            while let Some(&(first, _)) = self.fragment_spans.get(at) {
                if first.start() >= range.end() {
                    return None;
                }
                let mut best: Option<(TextRange, Span)> = None;
                while let Some(&(it, span)) = self.fragment_spans.get(at)
                    && it.start() == first.start()
                {
                    if it.end() <= range.end() && best.is_none_or(|(b, _)| it.end() > b.end()) {
                        best = Some((it, span));
                    }
                    at += 1;
                }
                if let Some((it, span)) = best {
                    from = it.end().max(it.start() + TextSize::new(1));
                    return Some((it, span));
                }
            }
            None
        })
    }

    /// How many macro fragments start at `offset`.
    pub fn fragments_starting_at(&self, offset: TextSize) -> usize {
        count_of(&self.fragment_starts, offset)
    }

    /// How many macro fragments end at `offset`.
    pub fn fragments_ending_at(&self, offset: TextSize) -> usize {
        count_of(&self.fragment_ends, offset)
    }

    /// Returns all [`TextRange`]s that correspond to the given span, in text order.
    ///
    /// A binary search of the pieces sorted by span, then the run of equal ones: `O(log n)`
    /// plus one step a match.
    pub fn ranges_with_span_exact(
        &self,
        span: Span,
    ) -> impl Iterator<Item = (TextRange, SyntaxContext)> + '_ {
        let key = down_key(&span);
        let by_anchor = self.by_anchor();
        let first = by_anchor.partition_point(|&it| down_key(&self.spans[it as usize]) < key);
        by_anchor[first..]
            .iter()
            .take_while(move |&&it| down_key(&self.spans[it as usize]) == key)
            .map(move |&it| self.range_of(it))
    }

    /// Returns all [`TextRange`]s whose spans contain the given span, in text order.
    ///
    /// Two binary searches bound the pieces with the same anchor that start at or before the
    /// span; those that end at or after it are put back in text order: `O(log n + c log c)` for
    /// `c` such pieces, and nothing allocated once the worker's buffer has grown (it is lent
    /// until the iterator is dropped).
    pub fn ranges_with_span(
        &self,
        span: Span,
    ) -> impl Iterator<Item = (TextRange, SyntaxContext)> + '_ {
        let [file, ast_id, start, end] = down_key(&span);
        let by_anchor = self.by_anchor();
        let key = |it: &u32| down_key(&self.spans[*it as usize]);
        let first = by_anchor.partition_point(|it| key(it) < [file, ast_id, 0, 0]);
        let last = first
            + by_anchor[first..].partition_point(|it| key(it) <= [file, ast_id, start, u32::MAX]);
        let mut found = scratch::lend_u32s();
        found.extend(by_anchor[first..last].iter().copied().filter(|it| key(it)[3] >= end));
        // The pieces' numbers are distinct, so any sort gives the one order.
        found.sort_unstable();
        (0..found.len()).map(move |at| self.range_of(found[at]))
    }

    /// Returns the span at the given position.
    #[inline]
    pub fn span_at(&self, offset: TextSize) -> Span {
        let entry = self.ends.partition_point(|&it| it <= offset);
        self.spans[entry]
    }

    /// [`SpanMap::span_at`], searched from the piece `hint` names and storing the one found
    /// there: for offsets in increasing order (a walk of the tokens) each lookup is two
    /// compares when it lands in the same piece or the next, `O(log d)` for `d` pieces on.
    /// Any `hint` gives the same span.
    #[inline]
    pub fn span_at_from(&self, offset: TextSize, hint: &mut u32) -> Span {
        let entry = search_from(&self.ends, *hint as usize, |it| it <= offset);
        *hint = entry as u32;
        self.spans[entry]
    }

    /// Returns the spans associated with the given range.
    /// In other words, this will return all spans that correspond to all offsets within the given range.
    pub fn spans_for_range(&self, range: TextRange) -> impl Iterator<Item = Span> + '_ {
        let (start, end) = (range.start(), range.end());
        let start_entry = self.ends.partition_point(|&it| it <= start);
        let end_entry = self.ends[start_entry..].partition_point(|&it| it <= end); // FIXME: this might be wrong?
        self.spans[start_entry..][..end_entry].iter().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (TextSize, Span)> + '_ {
        self.ends.iter().copied().zip(self.spans.iter().copied())
    }

    /// Merges this span map with another span map, where `other` is inserted at (and replaces) `other_range`.
    ///
    /// The length of the replacement node needs to be `other_size`.
    pub fn merge<B: Allocator + Clone>(
        &mut self,
        other_range: TextRange,
        other_size: TextSize,
        other: &SpanMap<B>,
    ) where
        A: AllocatorNightly,
    {
        // I find the following diagram helpful to illustrate the bounds and why we use `<` or `<=`:
        // --------------------------------------------------------------------
        //   1   3   5   6   7   10    11          <-- offsets we store
        // 0-1 1-3 3-5 5-6 6-7 7-10 10-11          <-- ranges these offsets refer to
        //       3   ..      7                     <-- other_range
        //         3-5 5-6 6-7                     <-- ranges we replace (len = 7-3 = 4)
        //         ^^^^^^^^^^^ ^^^^^^^^^^
        //           remove       shift
        //   2   3   5   9                         <-- offsets we insert
        // 0-2 2-3 3-5 5-9                         <-- ranges we insert (other_size = 9-0 = 9)
        // ------------------------------------
        //   1   3
        // 0-1 1-3                                 <-- these remain intact
        //           5   6   8   12
        //         3-5 5-6 6-8 8-12                <-- we shift these by other_range.start() and insert them
        //                             15    16
        //                          12-15 15-16    <-- we shift these by other_size-other_range.len() = 9-4 = 5
        // ------------------------------------
        //   1   3   5   6   8   12    15    16    <-- final offsets we store
        // 0-1 1-3 3-5 5-6 6-8 8-12 12-15 15-16    <-- final ranges

        // The ends are sorted, so what stays before, what is replaced and what is shifted are
        // three runs, and the result is the first, `other`'s moved, then the third shifted:
        // in order already, so it is spliced in place, not sorted.
        let before = self.ends.partition_point(|&it| it <= other_range.start());
        let after = self.ends.partition_point(|&it| it <= other_range.end());
        for offset in &mut self.ends[after..] {
            *offset += other_size;
            *offset -= other_range.len();
        }
        self.ends.splice(before..after, other.ends.iter().map(|&it| it + other_range.start()));
        self.spans.splice(before..after, other.spans.iter().copied());
        self.by_anchor = OnceLock::new();

        // A fragment that starts where the replaced text ends comes after it, one that ends
        // where it starts comes before it.
        let shift = |offset: &mut TextSize| *offset = *offset + other_size - other_range.len();
        self.fragment_starts.retain_mut(|start| {
            if other_range.contains(*start) {
                return false;
            }
            if *start >= other_range.end() {
                shift(start);
            }
            true
        });
        self.fragment_ends.retain_mut(|end| {
            if other_range.start() < *end && *end <= other_range.end() {
                return false;
            }
            if *end > other_range.end() {
                shift(end);
            }
            true
        });
        // A fragment keeps its span while both its start and its end stay, by the two rules
        // above: one that holds the replaced text or is apart from it.
        self.fragment_spans.retain_mut(|(range, _)| {
            let (mut start, mut end) = (range.start(), range.end());
            let end_replaced = other_range.start() < end && end <= other_range.end();
            if other_range.contains(start) || end_replaced {
                return false;
            }
            if start >= other_range.end() {
                shift(&mut start);
            }
            if end > other_range.end() {
                shift(&mut end);
            }
            *range = TextRange::new(start, end);
            true
        });
        let start = other_range.start();
        self.fragment_starts.extend(other.fragment_starts.iter().map(|&offset| offset + start));
        self.fragment_ends.extend(other.fragment_ends.iter().map(|&offset| offset + start));
        let moved = other.fragment_spans.iter().map(|&(range, span)| (range + start, span));
        self.fragment_spans.extend(moved);
        self.sort_fragments();

        // An eager call's merged expansion keeps the identifiers both made.
        if !other.made_idents.is_empty() {
            let both = self.made_idents.iter().chain(&other.made_idents).copied();
            // alloc: the merged map's made spans, only when the merged-in map made any
            self.made_idents = both.collect();
        }

        // Matched arm info is no longer correct once we have multiple macros.
        self.matched_arm = None;
    }
}

/// The anchors of a real file's spans: the items whose ast ids the spans of the text from where
/// each begins up to the next one are relative to.
///
/// What it is (what is compared and hashed) is each anchor with the length of its text, not
/// where that text is: no position in the file. The positions lookups take are summed from
/// the lengths when the map is made.
///
/// The anchor of an offset is a branch-free binary search of the starts alone (4 bytes per
/// anchor, 16 to a cache line: a file's few thousand anchors stay in L1 while its tokens are
/// mapped), then one load of the anchor's id. 12 bytes per anchor.
pub struct RealSpanMap {
    file_id: EditionedFileId,
    /// Each anchor's raw ast id and the length of its text, in the order of the text; the first
    /// begins at 0. With `file_id`, what the map is: compared as one `[u32]`, hashed with one
    /// `write`.
    anchors: Box<[[u32; 2]]>,
    /// Where each anchor's text begins: the lengths before it, summed.
    starts: Box<[u32]>,
}

impl fmt::Debug for RealSpanMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let anchors = self
            .anchors
            .iter()
            .map(|&[ast_id, len]| (ErasedFileAstId::from_raw(ast_id), TextSize::new(len)));
        f.debug_struct("RealSpanMap")
            .field("file_id", &self.file_id)
            // alloc: `Debug` output, the edge
            .field("anchors", &anchors.collect::<Vec<_>>())
            .field("starts", &self.starts)
            .finish()
    }
}

impl PartialEq for RealSpanMap {
    fn eq(&self, other: &RealSpanMap) -> bool {
        self.file_id == other.file_id && self.anchors.as_flattened() == other.anchors.as_flattened()
    }
}

impl Eq for RealSpanMap {}

impl Hash for RealSpanMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.file_id.hash(state);
        self.anchors.as_flattened().hash(state);
    }
}

impl fmt::Display for RealSpanMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "RealSpanMap({:?}):", self.file_id)?;
        for (start, &[ast_id, _]) in self.starts.iter().zip(&self.anchors) {
            writeln!(f, "{}: {:#?}", start, ErasedFileAstId::from_raw(ast_id))?;
        }
        Ok(())
    }
}

impl RealSpanMap {
    /// Creates a real file span map that returns absolute ranges (relative ranges to the root ast id).
    pub fn absolute(file_id: EditionedFileId) -> Self {
        RealSpanMap {
            file_id,
            anchors: Box::from([[ROOT_ERASED_FILE_AST_ID.into_raw(), !0]]),
            starts: Box::from([0]),
        }
    }

    /// The map of a file of `end` bytes whose anchors begin at `pairs`' offsets, in order,
    /// the first at 0.
    pub fn from_file(
        file_id: EditionedFileId,
        pairs: &[(TextSize, ErasedFileAstId)],
        end: TextSize,
    ) -> Self {
        always!(
            pairs.first().is_some_and(|it| it.0 == TextSize::new(0))
                && pairs.windows(2).all(|it| it[0].0 <= it[1].0)
                && pairs.last().is_some_and(|it| it.0 <= end),
            "the anchors of a file begin at 0, in order, within it"
        );
        let anchors = pairs
            .iter()
            .enumerate()
            .map(|(i, &(start, ast_id))| {
                let next = pairs.get(i + 1).map_or(end, |it| it.0);
                [ast_id.into_raw(), u32::from(next.checked_sub(start).unwrap_or_default())]
            })
            // alloc: the map's two columns, its result, once per file at their exact length
            .collect();
        // alloc: as `anchors`
        let starts = pairs.iter().map(|&(start, _)| u32::from(start)).collect();
        RealSpanMap { file_id, anchors, starts }
    }

    /// Each anchor's raw ast id and the length of its text, in the order of the text, and where
    /// each begins.
    pub fn anchors(&self) -> (&[[u32; 2]], &[u32]) {
        (&self.anchors, &self.starts)
    }

    /// Where the file ends: past the last anchor's text.
    fn end(&self) -> TextSize {
        let last = self.anchors.len() - 1;
        TextSize::new(self.starts[last].checked_add(self.anchors[last][1]).unwrap_or(!0))
    }

    /// The number of the anchor of what begins at `offset`: the last to begin at or before it.
    #[inline]
    fn index_at(&self, offset: TextSize) -> usize {
        let offset = u32::from(offset);
        let after = self.starts.partition_point(|&it| it <= offset);
        after.checked_sub(1).unwrap_or_else(|| panic!("no anchor begins at or before {offset}"))
    }

    /// The anchor that the spans of what begins at `offset` are relative to, and where it begins.
    #[inline]
    pub fn anchor_at(&self, offset: TextSize) -> (TextSize, ErasedFileAstId) {
        let idx = self.index_at(offset);
        (TextSize::new(self.starts[idx]), ErasedFileAstId::from_raw(self.anchors[idx][0]))
    }

    /// The map of `range` alone, as a file of its own: the spans it gives what is in `range`,
    /// moved to begin at 0, are those this map gives it where it is. `range` has to begin with
    /// an anchor ([`RealSpanMap::anchor_at`]).
    pub fn within(&self, range: TextRange) -> RealSpanMap {
        let (start, end) = (u32::from(range.start()), u32::from(range.end()));
        let from = self.starts.partition_point(|&it| it < start);
        let to = self.starts.partition_point(|&it| it < end);
        ANCHOR_PAIRS.with_borrow_mut(|scratch| {
            let pairs = scratch.take();
            pairs.extend(self.starts[from..to].iter().zip(&self.anchors[from..to]).map(
                |(&offset, &[ast_id, _])| {
                    (TextSize::new(offset - start), ErasedFileAstId::from_raw(ast_id))
                },
            ));
            debug_assert_eq!(pairs.first().map(|it| it.0), Some(TextSize::new(0)));
            RealSpanMap::from_file(self.file_id, pairs, range.len())
        })
    }

    /// The map of this map's text with each range of `elided` cut out (sorted, disjoint, past
    /// the text's start and within it): what [`RealSpanMap::from_file`] makes of the text left.
    /// An anchor that begins in a cut range is dropped; the others move back by the lengths
    /// cut before them, and each loses from its length what is cut inside its text. A span
    /// keeps its offset from its anchor where nothing between the two is cut, as when the
    /// bodies cut are of items that are their own anchors.
    pub fn without(&self, elided: &[TextRange]) -> RealSpanMap {
        let end = self.end();
        debug_assert!(
            elided.windows(2).all(|it| it[0].end() <= it[1].start())
                && elided.first().is_none_or(|it| it.start() > TextSize::new(0))
                && elided.last().is_none_or(|it| it.end() <= end),
            "ranges cut are sorted, disjoint, past the text's start and within it"
        );
        let mut cuts = elided.iter().peekable();
        let mut cut = 0u32;
        let total = elided.iter().fold(0, |total, it| total + u32::from(it.len()));
        ANCHOR_PAIRS.with_borrow_mut(|scratch| {
            let pairs = scratch.take();
            for (&start, &[ast_id, _]) in self.starts.iter().zip(&self.anchors) {
                while let Some(range) = cuts.next_if(|it| u32::from(it.end()) <= start) {
                    cut += u32::from(range.len());
                }
                if cuts.peek().is_some_and(|it| u32::from(it.start()) <= start) {
                    continue;
                }
                pairs.push((TextSize::new(start - cut), ErasedFileAstId::from_raw(ast_id)));
            }
            RealSpanMap::from_file(self.file_id, pairs, end - TextSize::new(total))
        })
    }

    /// The span [`RealSpanMap::absolute`]'s map gives `range`, with no map made: relative to the
    /// file's root.
    #[cfg(feature = "query")]
    pub fn absolute_span(file_id: EditionedFileId, range: TextRange) -> Span {
        Span {
            range,
            anchor: crate::SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContext::root(file_id.edition()),
        }
    }

    #[cfg(feature = "query")]
    pub fn span_for_range(&self, range: TextRange) -> Span {
        let end = self.end();
        assert!(range.end() <= end, "range {range:?} goes beyond the end of the file {end:?}");
        let (offset, ast_id) = self.anchor_at(range.start());
        Span {
            range: range - offset,
            anchor: crate::SpanAnchor { file_id: self.file_id, ast_id },
            ctx: SyntaxContext::root(self.file_id.edition()),
        }
    }

    /// [`RealSpanMap::span_for_range`], searched from the anchor `hint` names and storing the
    /// one found there: for ranges in increasing order (a walk of the tokens) each lookup is
    /// two compares while it stays in an anchor or moves to the next, `O(log d)` for `d`
    /// anchors on. Any `hint` gives the same span.
    #[cfg(feature = "query")]
    #[inline]
    pub fn span_for_range_from(&self, range: TextRange, hint: &mut u32) -> Span {
        let end = self.end();
        assert!(range.end() <= end, "range {range:?} goes beyond the end of the file {end:?}");
        let offset = u32::from(range.start());
        let after = search_from(&self.starts, *hint as usize + 1, |it| it <= offset);
        let idx = after
            .checked_sub(1)
            .unwrap_or_else(|| panic!("no anchor begins at or before {offset}"));
        *hint = idx as u32;
        Span {
            range: range - TextSize::new(self.starts[idx]),
            anchor: crate::SpanAnchor {
                file_id: self.file_id,
                ast_id: ErasedFileAstId::from_raw(self.anchors[idx][0]),
            },
            ctx: SyntaxContext::root(self.file_id.edition()),
        }
    }

    /// `ranges`, of this map's text at or after where anchor `ast_id` begins, as the side table
    /// of the anchor's spans (docs/v2/plan/lanes/format.md section 5, "Spans"), each relative to where the
    /// anchor begins. `anchor` is the content id of the anchor's item, which names the table;
    /// the span of the `i`th range is the table's `i`th, `SpanRef { anchor, index: i }`.
    ///
    /// # Panics
    ///
    /// When `ast_id` is not an anchor that begins at or before the first range, or a range
    /// begins before it or ends past the end of the text.
    pub fn write_spans(
        &self,
        anchor: ContentId,
        ast_id: ErasedFileAstId,
        ranges: impl IntoIterator<Item = TextRange>,
    ) -> Spans {
        let mut spans = Spans::new(anchor);
        let mut found = None;
        let end = self.end();
        for range in ranges {
            let begins = *found.get_or_insert_with(|| self.start_of(ast_id, range.start()));
            assert!(
                begins <= range.start() && range.end() <= end,
                "{range:?} is not in the text after {ast_id:?} begins at {begins:?} in {:?}",
                self.file_id
            );
            spans.push(u32::from(range.start() - begins), u32::from(range.len()));
        }
        spans
    }

    /// Where anchor `ast_id` begins, at or before `offset`.
    fn start_of(&self, ast_id: ErasedFileAstId, offset: TextSize) -> TextSize {
        let last = self.index_at(offset);
        let at = (0..=last).rev().find(|&it| self.anchors[it][0] == ast_id.into_raw());
        let at = at.unwrap_or_else(|| {
            panic!("{ast_id:?} is no anchor at or before {offset:?} in {:?}", self.file_id)
        });
        TextSize::new(self.starts[at])
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::{ContentId, rng::SplitMix64};

    use super::*;
    use crate::FileId;

    fn file() -> EditionedFileId {
        EditionedFileId::current_edition(FileId::from_raw(0))
    }

    fn at(offset: u32) -> TextSize {
        TextSize::new(offset)
    }

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(at(start), at(end))
    }

    fn id(raw: u32) -> ErasedFileAstId {
        ErasedFileAstId::from_raw(raw)
    }

    /// Anchors at 0, `first` and `first + 15`, in a text that ends 15 past the last.
    fn map(first: u32) -> RealSpanMap {
        let pairs = [(at(0), ROOT_ERASED_FILE_AST_ID), (at(first), id(1)), (at(first + 15), id(2))];
        RealSpanMap::from_file(file(), &pairs, at(first + 30))
    }

    fn of(pairs: &[(u32, u32)], end: u32) -> RealSpanMap {
        let pairs = pairs.iter().map(|&(start, raw)| (at(start), id(raw))).collect::<Box<[_]>>();
        RealSpanMap::from_file(file(), &pairs, at(end))
    }

    /// Anchors before a cut stay; those after move back by it; one in a cut is dropped; the
    /// anchor holding a cut loses its length. With nothing cut, the map is the same.
    #[test]
    fn a_map_without_ranges_is_the_map_of_the_text_left() {
        let map = map(10);
        let root = ROOT_ERASED_FILE_AST_ID.into_raw();
        assert_eq!(map.without(&[]), map);
        assert_eq!(map.without(&[range(12, 20)]), of(&[(0, root), (10, 1), (17, 2)], 32));
        assert_eq!(map.without(&[range(20, 30)]), of(&[(0, root), (10, 1)], 30));
        assert_eq!(
            map.without(&[range(2, 5), range(12, 20)]),
            of(&[(0, root), (7, 1), (14, 2)], 29)
        );
        // A cut that ends where an anchor begins keeps it, moved to where the cut began.
        assert_eq!(map.without(&[range(5, 10)]), of(&[(0, root), (5, 1), (20, 2)], 35));
        // A cut that begins where an anchor begins drops it.
        assert_eq!(map.without(&[range(25, 27)]), of(&[(0, root), (10, 1)], 38));
        let cut = map.without(&[range(12, 20)]);
        assert_eq!(cut.anchor_at(at(16)), (at(10), id(1)));
        assert_eq!(cut.anchor_at(at(17)), (at(17), id(2)));
        assert_eq!(cut.end(), at(32));
    }

    /// Against a model: cutting the ranges from the pairs' text and placing each anchor by
    /// what is left before it.
    #[test]
    fn without_agrees_with_cutting_the_pairs() {
        let mut rng = SplitMix64::new(5);
        for n in [1usize, 2, 3, 17, 100] {
            let (pairs, end) = anchors(&mut rng, n, 4);
            let map = RealSpanMap::from_file(file(), &pairs, at(end));
            let mut elided = Vec::new();
            let mut from = 1;
            while from < end {
                let start = from + rng.below(12) as u32;
                let stop = start + rng.below(9) as u32;
                if stop > end {
                    break;
                }
                elided.push(range(start, stop));
                from = stop + rng.below(4) as u32;
            }
            let removed_before = |offset: u32| {
                elided
                    .iter()
                    .filter(|it| u32::from(it.end()) <= offset)
                    .fold(0, |cut, it| cut + u32::from(it.len()))
            };
            let kept = pairs
                .iter()
                .filter(|(start, _)| !elided.iter().any(|it| it.start() <= *start && *start < it.end()))
                .map(|&(start, ast_id)| (start - at(removed_before(u32::from(start))), ast_id))
                .collect::<Box<[_]>>();
            let want = RealSpanMap::from_file(file(), &kept, at(end - removed_before(end)));
            assert_eq!(map.without(&elided), want, "{n} anchors, cut {elided:?}");
            assert_eq!(map.without(&elided).starts, want.starts);
        }
    }

    /// The slices lent are the pairs the map was made of: each id with its length, and starts.
    #[test]
    fn anchors_are_lent_as_the_pairs_they_were_made_of() {
        let map = map(10);
        let (ids, starts) = map.anchors();
        let root = ROOT_ERASED_FILE_AST_ID.into_raw();
        assert_eq!(ids, [[root, 10], [1, 15], [2, 15]]);
        assert_eq!(starts, [0, 10, 25]);
    }

    #[test]
    fn an_offset_has_the_last_anchor_at_or_before_it() {
        let map = map(10);
        assert_eq!(map.anchor_at(at(0)), (at(0), ROOT_ERASED_FILE_AST_ID));
        assert_eq!(map.anchor_at(at(9)), (at(0), ROOT_ERASED_FILE_AST_ID));
        assert_eq!(map.anchor_at(at(10)), (at(10), id(1)));
        assert_eq!(map.anchor_at(at(24)), (at(10), id(1)));
        assert_eq!(map.anchor_at(at(25)), (at(25), id(2)));
        assert_eq!(map.end(), at(40));

        // An item at the very start of the file is the anchor there, not the root.
        let pairs = [(at(0), ROOT_ERASED_FILE_AST_ID), (at(0), id(1))];
        let map = RealSpanMap::from_file(file(), &pairs, at(5));
        assert_eq!(map.anchor_at(at(0)), (at(0), id(1)));
    }

    #[test]
    fn what_is_kept_is_lengths() {
        // The text before the first item grew: only the root's length is another.
        let (map, moved) = (map(10), map(12));
        assert_ne!(map, moved);
        assert_eq!(map.anchors[1..], moved.anchors[1..]);
        assert_eq!(map.within(range(10, 40)), moved.within(range(12, 42)));
        let within = map.within(range(10, 40));
        assert_eq!(within.anchor_at(at(0)), (at(0), id(1)));
        assert_eq!(within.anchor_at(at(15)), (at(15), id(2)));
        assert_eq!(within.end(), at(30));
    }

    #[test]
    fn spans_are_written_relative_to_their_anchor() {
        let anchor = ContentId::of("blaze v2 test 1", b"an item");
        let spans = map(10).write_spans(anchor, id(1), [range(12, 15), range(10, 40), range(30, 31)]);
        assert_eq!(spans.anchor, anchor);
        let spans: Vec<_> = spans.spans.iter().map(|it| (it.start, it.len)).collect();
        assert_eq!(spans, [(2, 3), (0, 30), (20, 1)]);
    }

    #[test]
    #[should_panic]
    fn a_span_before_its_anchor_is_refused() {
        map(10).write_spans(ContentId::default(), id(2), [range(12, 15)]);
    }

    /// `n` anchors with gaps of 0 to 7 bytes, and a text 5 past the last; ids from `ids`.
    fn anchors(
        rng: &mut SplitMix64,
        n: usize,
        ids: u32,
    ) -> (Box<[(TextSize, ErasedFileAstId)]>, u32) {
        let mut start = 0;
        let pairs = (0..n)
            .map(|i| {
                if i > 0 {
                    start += rng.below(8) as u32;
                }
                (at(start), id(rng.below(u64::from(ids)) as u32))
            })
            .collect();
        (pairs, start + 5)
    }

    /// The columns give what a search of the pairs they were made from gives (the layout
    /// before kept `(ast id, length)` pairs and searched the same starts).
    #[test]
    fn anchors_agree_with_a_search_of_the_pairs() {
        use std::hash::BuildHasher;
        let mut rng = SplitMix64::new(3);
        for n in [1usize, 2, 3, 15, 16, 17, 100, 1000] {
            let (pairs, end) = anchors(&mut rng, n, 1 << 20);
            let map = RealSpanMap::from_file(file(), &pairs, at(end));
            assert_eq!(map.end(), at(end));
            for offset in 0..=end {
                let want = *pairs.iter().rev().find(|it| it.0 <= at(offset)).expect("an anchor");
                assert_eq!(map.anchor_at(at(offset)), want, "{n} anchors, at {offset}");
            }
            for &(start, ast_id) in pairs.iter().step_by(7) {
                let range = TextRange::new(start, at(end));
                let filtered = pairs
                    .iter()
                    .filter(|it| range.contains(it.0))
                    .map(|&(offset, ast_id)| (offset - range.start(), ast_id))
                    .collect::<Box<[_]>>();
                let want = RealSpanMap::from_file(file(), &filtered, range.len());
                let within = map.within(range);
                assert_eq!(within, want, "{n} anchors, within {range:?}");
                let hash = |it: &RealSpanMap| blaze_hash::FixedState.hash_one(it);
                assert_eq!(hash(&within), hash(&want));
                assert_eq!(map.start_of(ast_id, start), want_start(&pairs, ast_id, start));
            }
        }

        fn want_start(
            pairs: &[(TextSize, ErasedFileAstId)],
            ast_id: ErasedFileAstId,
            offset: TextSize,
        ) -> TextSize {
            pairs.iter().rev().filter(|it| it.0 <= offset).find(|it| it.1 == ast_id).unwrap().0
        }
    }

    /// From any hint, the search finds what a binary search of the whole does.
    #[test]
    fn a_search_from_any_hint_is_a_binary_search() {
        let mut rng = SplitMix64::new(8);
        for len in 0..40 {
            let mut keys = (0..len).map(|_| rng.below(16) as u32).collect::<Vec<_>>();
            keys.sort_unstable();
            for offset in 0..=16 {
                let want = keys.partition_point(|&it| it <= offset);
                for hint in 0..len + 3 {
                    let found = search_from(&keys, hint, |it| it <= offset);
                    assert_eq!(found, want, "{keys:?}, {offset} from {hint}");
                }
            }
        }
    }

    /// The cursor gives the spans [`RealSpanMap::span_for_range`] does, walking in order, from
    /// any hint and in no order.
    #[cfg(feature = "query")]
    #[test]
    fn a_cursor_gives_the_spans_a_search_does() {
        let mut rng = SplitMix64::new(7);
        for n in [1usize, 2, 3, 17, 100, 1000] {
            let (pairs, end) = anchors(&mut rng, n, 1 << 20);
            let map = RealSpanMap::from_file(file(), &pairs, at(end));
            let mut hint = 0;
            for offset in 0..end {
                let range = range(offset, offset + 1);
                let want = map.span_for_range(range);
                assert_eq!(map.span_for_range_from(range, &mut hint), want, "{n}, at {offset}");
                let from = rng.below(n as u64 + 2) as u32;
                let found = map.span_for_range_from(range, &mut { from });
                assert_eq!(found, want, "{n} anchors, from {from}, at {offset}");
            }
            let mut hint = u32::MAX;
            for _ in 0..200 {
                let offset = rng.below(u64::from(end)) as u32;
                let range = range(offset, end);
                let found = map.span_for_range_from(range, &mut hint);
                assert_eq!(found, map.span_for_range(range), "{n} anchors, {range:?}");
            }
        }
    }

    /// Two maps are equal exactly when their anchors' ids and lengths are, and equal maps hash
    /// the same.
    #[test]
    fn real_maps_are_equal_when_their_lengths_and_ids_are() {
        use std::hash::BuildHasher;
        let mut rng = SplitMix64::new(4);
        let maps: Vec<_> = (0..200)
            .map(|_| {
                let n = 1 + rng.below(4) as usize;
                let (pairs, end) = anchors(&mut rng, n, 2);
                let lengths: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .map(|(i, &(start, ast_id))| {
                        (ast_id, pairs.get(i + 1).map_or(at(end), |it| it.0) - start)
                    })
                    .collect();
                (RealSpanMap::from_file(file(), &pairs, at(end)), lengths)
            })
            .collect();
        for (a, a_lengths) in &maps {
            for (b, b_lengths) in &maps {
                assert_eq!(a == b, a_lengths == b_lengths);
                if a == b {
                    let hash = |it: &RealSpanMap| blaze_hash::FixedState.hash_one(it);
                    assert_eq!(hash(a), hash(b));
                }
            }
        }
    }

    /// The expansion map as it was kept before its ends and spans were columns: one
    /// `(end, span)` pair each, with its lookups and merge as they were.
    #[cfg(feature = "query")]
    #[derive(Default)]
    struct Pairs(Vec<(TextSize, Span)>);

    #[cfg(feature = "query")]
    impl Pairs {
        fn span_at(&self, offset: TextSize) -> Span {
            let entry = self.0.partition_point(|&(it, _)| it <= offset);
            self.0[entry].1
        }

        fn spans_for_range(&self, range: TextRange) -> Vec<Span> {
            let (start, end) = (range.start(), range.end());
            let start_entry = self.0.partition_point(|&(it, _)| it <= start);
            let end_entry = self.0[start_entry..].partition_point(|&(it, _)| it <= end);
            self.0[start_entry..][..end_entry].iter().map(|&(_, s)| s).collect()
        }

        fn ranges_with(&self, span: Span, exact: bool) -> Vec<(TextRange, SyntaxContext)> {
            let found = self.0.iter().enumerate().filter(|(_, (_, s))| match exact {
                true => s.eq_ignoring_ctx(span),
                false => s.anchor == span.anchor && s.range.contains_range(span.range),
            });
            found
                .map(|(idx, &(end, s))| {
                    let start = idx.checked_sub(1).map_or(TextSize::new(0), |prev| self.0[prev].0);
                    (TextRange::new(start, end), s.ctx)
                })
                .collect()
        }

        fn merge(&mut self, other_range: TextRange, other_size: TextSize, other: &Pairs) {
            self.0.retain_mut(|(offset, _)| {
                if other_range.start() < *offset && *offset <= other_range.end() {
                    false
                } else {
                    if *offset > other_range.end() {
                        *offset += other_size;
                        *offset -= other_range.len();
                    }
                    true
                }
            });
            self.0.extend(other.0.iter().map(|&(offset, span)| (offset + other_range.start(), span)));
            self.0.sort_unstable_by_key(|&(offset, _)| offset);
        }
    }

    /// Looking up a file's spans, an expansion's spans and the ranges of a span allocates
    /// nothing, once the expansion's index by anchor is made (on its first downward lookup).
    #[cfg(feature = "query")]
    #[test]
    fn lookups_allocate_nothing() {
        let mut rng = SplitMix64::new(11);
        let (pairs, end) = anchors(&mut rng, 300, 1 << 20);
        let real = RealSpanMap::from_file(file(), &pairs, at(end));
        let (expansion, _) = spans(&mut rng, 500);
        let last = expansion.iter().last().map_or(0, |(end, _)| u32::from(end));
        let _ = expansion.ranges_with_span_exact(expansion.span_at(at(0))).count();
        blaze_alloc_count::assert_at_most(0, "span map lookups", || {
            let mut hint = 0;
            for offset in 0..end {
                let range = range(offset, offset + 1);
                std::hint::black_box((real.span_for_range_from(range, &mut hint), real.span_for_range(range)));
            }
            let mut hint = 0;
            for offset in 0..last {
                let span = expansion.span_at_from(at(offset), &mut hint);
                std::hint::black_box(expansion.span_at(at(offset)));
                std::hint::black_box(expansion.ranges_with_span_exact(span).count());
            }
        });
    }

    /// A map of `n` spans over pieces of 1 to 4 bytes, with the same as [`Pairs`].
    #[cfg(feature = "query")]
    fn spans(rng: &mut SplitMix64, n: usize) -> (SpanMap, Pairs) {
        let ctx = SyntaxContext::root(crate::Edition::CURRENT);
        let (mut map, mut pairs) = (SpanMap::empty(), Pairs::default());
        let mut end = 0;
        for _ in 0..n {
            end += 1 + rng.below(4) as u32;
            let ast_id = id(rng.below(3) as u32);
            let anchor = crate::SpanAnchor { file_id: file(), ast_id };
            let start = rng.below(8) as u32;
            let span = Span { range: range(start, start + rng.below(4) as u32), anchor, ctx };
            map.push(at(end), span);
            pairs.0.push((at(end), span));
        }
        map.finish();
        (map, pairs)
    }

    #[cfg(feature = "query")]
    fn check_same(map: &SpanMap, pairs: &Pairs, rng: &mut SplitMix64) {
        assert_eq!(map.iter().collect::<Vec<_>>(), pairs.0);
        let Some(&(last, _)) = pairs.0.last() else { return };
        let mut hint = 0;
        for offset in 0..u32::from(last) {
            let want = pairs.span_at(at(offset));
            assert_eq!(map.span_at(at(offset)), want, "at {offset}");
            assert_eq!(map.span_at_from(at(offset), &mut hint), want, "in order, at {offset}");
            let from = rng.below(pairs.0.len() as u64 + 2) as u32;
            assert_eq!(map.span_at_from(at(offset), &mut { from }), want, "from {from}, at {offset}");
        }
        let mut hint = u32::MAX;
        for _ in 0..50 {
            let offset = at(rng.below(u64::from(u32::from(last))) as u32);
            assert_eq!(map.span_at_from(offset, &mut hint), pairs.span_at(offset), "at {offset:?}");
        }
        let ctx = SyntaxContext::root(crate::Edition::CURRENT);
        for _ in 0..20 {
            let anchor = crate::SpanAnchor { file_id: file(), ast_id: id(rng.below(4) as u32) };
            let start = rng.below(9) as u32;
            let span = Span { range: range(start, start + rng.below(5) as u32), anchor, ctx };
            let exact = map.ranges_with_span_exact(span).collect::<Vec<_>>();
            assert_eq!(exact, pairs.ranges_with(span, true), "{span:?}");
            let containing = map.ranges_with_span(span).collect::<Vec<_>>();
            assert_eq!(containing, pairs.ranges_with(span, false), "{span:?}");
        }
        for _ in 0..50 {
            let start = rng.below(u64::from(u32::from(last))) as u32;
            let range = range(start, start + rng.below(12) as u32);
            assert_eq!(map.spans_for_range(range).collect::<Vec<_>>(), pairs.spans_for_range(range));
        }
        for &(_, span) in pairs.0.iter().step_by(5) {
            let exact = map.ranges_with_span_exact(span).collect::<Vec<_>>();
            assert_eq!(exact, pairs.ranges_with(span, true));
            let containing = map.ranges_with_span(span).collect::<Vec<_>>();
            assert_eq!(containing, pairs.ranges_with(span, false));
        }
    }

    /// The columns give the pairs' answers, before and after merging another map in.
    #[cfg(feature = "query")]
    #[test]
    fn expansion_maps_agree_with_the_pairs() {
        let mut rng = SplitMix64::new(5);
        for n in [1usize, 2, 16, 17, 300] {
            let (mut map, mut pairs) = spans(&mut rng, n);
            check_same(&map, &pairs, &mut rng);
            for _ in 0..8 {
                let other_n = 1 + rng.below(20) as usize;
                let (other, other_pairs) = spans(&mut rng, other_n);
                let last = u32::from(pairs.0.last().expect("a span").0);
                let start = rng.below(u64::from(last + 1)) as u32;
                let end = start + rng.below(u64::from(last - start + 1)) as u32;
                let size = other_pairs.0.last().expect("a span").0;
                map.merge(range(start, end), size, &other);
                pairs.merge(range(start, end), size, &other_pairs);
                check_same(&map, &pairs, &mut rng);
            }
        }
    }

    /// Two expansion maps are equal exactly when their pairs are, and equal ones hash the same.
    #[cfg(feature = "query")]
    #[test]
    fn expansion_maps_are_equal_when_their_pairs_are() {
        use std::hash::BuildHasher;
        let mut rng = SplitMix64::new(6);
        let maps: Vec<_> = (0..100)
            .map(|_| {
                let n = rng.below(3) as usize;
                spans(&mut rng, n)
            })
            .collect();
        for (a, a_pairs) in &maps {
            for (b, b_pairs) in &maps {
                assert_eq!(a == b, a_pairs.0 == b_pairs.0);
                if a == b {
                    let hash = |it: &SpanMap| blaze_hash::FixedState.hash_one(it);
                    assert_eq!(hash(a), hash(b));
                }
            }
        }
    }

    /// A metavariable's span, `n` past the start of anchor 7's text.
    #[cfg(feature = "query")]
    fn group(n: u32) -> Span {
        let anchor = crate::SpanAnchor { file_id: file(), ast_id: id(7) };
        Span { range: range(n, n + 2), anchor, ctx: SyntaxContext::root(crate::Edition::CURRENT) }
    }

    /// A fragment's span is found by its exact range only; of two with one range, the innermost
    /// (pushed first, as groups close) is found; pushing out of order changes nothing once
    /// finished; and the spans are part of what the map is.
    #[cfg(feature = "query")]
    #[test]
    fn a_fragment_keeps_its_groups_span() {
        let (mut map, _) = spans(&mut SplitMix64::new(9), 20);
        map.push_fragment(range(10, 14), group(1));
        map.push_fragment(range(3, 8), group(2));
        map.push_fragment(range(3, 8), group(3));
        map.push_fragment(range(3, 20), group(4));
        let plain = map.fragment_spans.is_sorted_by_key(|&(it, _)| fragment_key(it));
        assert!(!plain, "pushed as groups close, not in order");
        map.finish();
        assert_eq!(map.fragment_span(range(10, 14)), Some(group(1)));
        assert_eq!(map.fragment_span(range(3, 8)), Some(group(2)), "the innermost");
        assert_eq!(map.fragment_span(range(3, 20)), Some(group(4)));
        let missing = [range(3, 7), range(3, 9), range(4, 8), range(10, 15), range(0, 0), range(30, 40)];
        for missing in missing {
            assert_eq!(map.fragment_span(missing), None, "{missing:?}");
        }
        assert_eq!(map.fragments_starting_at(at(3)), 3);
        assert_eq!(map.fragments_ending_at(at(8)), 2);

        let (mut other, _) = spans(&mut SplitMix64::new(9), 20);
        other.push_fragment(range(10, 14), group(1));
        other.push_fragment(range(3, 8), group(2));
        other.push_fragment(range(3, 8), group(5));
        other.push_fragment(range(3, 20), group(4));
        other.finish();
        assert!(map != other, "the spans are part of the map");
        assert_eq!(SpanMap::empty().fragment_span(range(0, 0)), None);
    }

    /// The outermost fragments inside a range, in order: one inside another is not, nor is one
    /// running past the range's end; of two with one range, the innermost's span.
    #[cfg(feature = "query")]
    #[test]
    fn the_outermost_fragments_in_a_range() {
        let (mut map, _) = spans(&mut SplitMix64::new(9), 30);
        map.push_fragment(range(4, 6), group(1));
        map.push_fragment(range(3, 8), group(2));
        map.push_fragment(range(3, 8), group(3));
        map.push_fragment(range(10, 14), group(4));
        map.push_fragment(range(16, 18), group(6));
        map.push_fragment(range(15, 20), group(5));
        map.finish();
        let outermost = |r| map.outermost_fragments_in(r).collect::<Vec<_>>();
        assert_eq!(outermost(range(0, 30)), [
            (range(3, 8), group(2)),
            (range(10, 14), group(4)),
            (range(15, 20), group(5)),
        ]);
        assert_eq!(outermost(range(0, 15)), [(range(3, 8), group(2)), (range(10, 14), group(4))]);
        assert_eq!(outermost(range(4, 13)), [(range(4, 6), group(1))]);
        assert_eq!(outermost(range(16, 30)), [(range(16, 18), group(6))]);
        assert!(outermost(range(21, 30)).is_empty());
        assert_eq!(SpanMap::empty().outermost_fragments_in(range(0, 10)).count(), 0);
    }

    /// What a merge keeps of the fragments' spans, by the rules of its fragment starts and ends
    /// taken one at a time: the moved fragments of the map merged in, and of this map's those
    /// whose start and end both stay, shifted past the replaced text.
    #[cfg(feature = "query")]
    #[test]
    fn a_merge_keeps_the_fragments_spans_by_their_starts_and_ends() {
        let mut rng = SplitMix64::new(12);
        let fragments = |rng: &mut SplitMix64, map: &mut SpanMap, len: u32, n: u32| {
            let mut pushed = Vec::new();
            for i in 0..n {
                let start = rng.below(u64::from(len)) as u32;
                let end = start + 1 + rng.below(u64::from(len - start)) as u32;
                map.push_fragment(range(start, end), group(i));
                pushed.push((range(start, end), group(i)));
            }
            pushed
        };
        for _ in 0..200 {
            let (n, other_n) = (1 + rng.below(30) as usize, 1 + rng.below(10) as usize);
            let (mine_n, theirs_n) = (rng.below(6) as u32, rng.below(4) as u32);
            let (mut map, pairs) = spans(&mut rng, n);
            let len = u32::from(pairs.0.last().expect("a span").0);
            let mine = fragments(&mut rng, &mut map, len, mine_n);
            map.finish();
            let (mut other, other_pairs) = spans(&mut rng, other_n);
            let size = other_pairs.0.last().expect("a span").0;
            let theirs = fragments(&mut rng, &mut other, u32::from(size), theirs_n);
            other.finish();
            let start = rng.below(u64::from(len + 1)) as u32;
            let end = start + rng.below(u64::from(len - start + 1)) as u32;
            let replaced = range(start, end);
            map.merge(replaced, size, &other);

            let shift = |offset: TextSize| match offset > replaced.end() {
                true => offset + size - replaced.len(),
                false => offset,
            };
            let mut want: Vec<(TextRange, Span)> = mine
                .iter()
                .filter(|(it, _)| {
                    let start_stays = !replaced.contains(it.start());
                    let end_stays = !(replaced.start() < it.end() && it.end() <= replaced.end());
                    start_stays && end_stays
                })
                .map(|&(it, span)| {
                    let start = match it.start() >= replaced.end() {
                        true => it.start() + size - replaced.len(),
                        false => it.start(),
                    };
                    (TextRange::new(start, shift(it.end())), span)
                })
                .chain(theirs.iter().map(|&(it, span)| (it + replaced.start(), span)))
                .collect();
            want.sort_by_key(|&(it, _)| (it.start(), it.end()));
            for &(it, _) in &want {
                let innermost = want.iter().find(|(other, _)| *other == it).map(|&(_, span)| span);
                assert_eq!(map.fragment_span(it), innermost, "{it:?} after replacing {replaced:?}");
            }
            let ranges: Vec<TextRange> = map.fragment_spans.iter().map(|&(it, _)| it).collect();
            assert_eq!(ranges, want.iter().map(|&(it, _)| it).collect::<Vec<_>>());
            for &(it, _) in &map.fragment_spans {
                assert!(map.fragments_starting_at(it.start()) > 0, "{it:?}: its start is kept");
                assert!(map.fragments_ending_at(it.end()) > 0, "{it:?}: its end is kept");
            }
        }
    }

    /// Mapping a span down allocates nothing once the map's index and the worker's buffer are
    /// made.
    #[cfg(feature = "query")]
    #[test]
    fn ranges_with_span_allocates_nothing_warm() {
        let mut rng = SplitMix64::new(7);
        let (map, pairs) = spans(&mut rng, 200);
        let span = pairs.0[pairs.0.len() / 2].1;
        let want = map.ranges_with_span(span).collect::<Vec<_>>();
        let got = blaze_alloc_count::assert_at_most(0, "a warm ranges_with_span", || {
            map.ranges_with_span(span).count()
        });
        assert_eq!(got, want.len());
        assert_eq!(want, pairs.ranges_with(span, false));
    }

    /// A finished map copied out has its columns at their exact lengths and is the same map,
    /// and filling and finishing a warm map allocates nothing (the fragment sorts are the
    /// worker's).
    #[cfg(feature = "query")]
    #[test]
    fn a_copied_map_is_exact_and_finishing_allocates_nothing_warm() {
        let span = |at: u32| Span {
            range: TextRange::at(TextSize::new(at), TextSize::new(1)),
            anchor: crate::SpanAnchor { file_id: file(), ast_id: ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContext::root(crate::Edition::CURRENT),
        };
        // Nine fragments, at 1, 8, .., 57, pushed in reverse so that `finish` has to sort them.
        let fill = |map: &mut SpanMap| {
            map.clear();
            for at in 1..=64u32 {
                map.push(TextSize::new(at), span(at));
            }
            for at in (1..60u32).step_by(7).rev() {
                map.push_fragment(range(at, at + 3), span(at));
            }
        };
        let mut map = SpanMap::with_capacity(64);
        fill(&mut map);
        map.finish();
        blaze_alloc_count::assert_at_most(0, "filling and finishing a warm map", || {
            fill(&mut map);
            map.finish();
        });
        assert_eq!(map.fragment_span(range(57, 60)), Some(span(57)));
        let copy = map.copy_in(Global);
        assert!(copy == map);
        assert_eq!((copy.ends.capacity(), copy.fragment_spans.capacity()), (64, 9));
        assert_eq!(SpanMap::empty().copy_in(Global).capacity(), 0);
    }

    /// The made identifiers' spans are part of what the map is, are copied out with it, are
    /// appended by a merge after the map's own, and are emptied by `clear`.
    #[cfg(feature = "query")]
    #[test]
    fn made_idents_are_kept_copied_and_merged() {
        let mut rng = SplitMix64::new(13);
        let (mut map, _) = spans(&mut rng, 10);
        let (mut other, other_pairs) = spans(&mut rng, 5);
        let (mut plain, _) = spans(&mut SplitMix64::new(13), 10);
        map.made_idents = Box::new([group(1)]);
        assert!(map != plain, "the made spans are part of the map");
        plain.made_idents = Box::new([group(1)]);
        assert!(map == plain);
        assert_eq!(map.copy_in(Global).made_idents[..], [group(1)]);
        other.made_idents = Box::new([group(2), group(3)]);
        let size = other_pairs.0.last().expect("a span").0;
        map.merge(range(0, 0), size, &other);
        assert_eq!(map.made_idents[..], [group(1), group(2), group(3)]);
        map.merge(range(0, 0), at(0), &SpanMap::empty());
        assert_eq!(map.made_idents.len(), 3, "merging a map that made none keeps them");
        map.clear();
        assert!(map.made_idents.is_empty());
    }

    /// `absolute_span` is the absolute map's span, allocating nothing; `from_file` allocates its
    /// two columns alone, and `within` and `without` nothing more once the worker's pairs are
    /// made.
    #[cfg(feature = "query")]
    #[test]
    fn absolute_spans_need_no_map() {
        let map = RealSpanMap::absolute(file());
        for range in [TextRange::empty(at(0)), range(3, 90)] {
            let span = blaze_alloc_count::assert_at_most(0, "an absolute span", || {
                RealSpanMap::absolute_span(file(), range)
            });
            assert_eq!(span, map.span_for_range(range));
        }
        let pairs = [(at(0), ROOT_ERASED_FILE_AST_ID), (at(9), id(1))];
        blaze_alloc_count::assert_at_most(2, "a map of two anchors", || {
            RealSpanMap::from_file(file(), &pairs, at(20))
        });
        let map = self::map(10);
        let _ = (map.within(range(10, 40)), map.without(&[range(12, 20)]));
        blaze_alloc_count::assert_at_most(4, "a warm within and without", || {
            (map.within(range(10, 40)), map.without(&[range(12, 20)]))
        });
    }
}
