//! [`ExpressionStoreSourceMap`] and [`BodySourceMap`] as [`blaze_format`] buffers, in the
//! encoding of [`crate::hir::persist`]. A source map is a buffer of its own, so that spans stay
//! out of what is hashed.
//!
//! Each map back from an id to its syntax ([`BackMap`]) is written as the columns it is in
//! memory ([`BackRec`]): the `u16` kinds, the starts and the ends copied as they are, and its
//! runs of ids in one file as [`FileRun`]s. The patterns of each binding are its CSR offsets and
//! patterns. Each map from syntax to an id ([`SourceTable`]) is two columns: the keys as
//! [`Src`]s in the table's order and the ids beside them. Everything else is in words.
//!
//! Every column is checked when the buffer is read: a kind its pointer type doesn't cast, a
//! range that ends before it starts, an id past its arena (each back map is dense over its
//! arena, so its length is the arena's), runs or CSR offsets out of order, keys out of order,
//! are each a [`FormatError::Invalid`], never a later panic. The ids of the side tables in the
//! words are checked the same way when they are decoded.

use std::mem::{offset_of, size_of};

use blaze_format::{FormatError, Part, Persist, Record, SchemaHash, Slice, Writer, open_as, schema_of};
use either::Either;
use hir_expand::{HirFileId, InFile, MacroCallId};
use blaze_query::db::FromId;
use syntax::{AstNode, AstPtr, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, ast};

use super::{HAS_EXPRS, at_of, bits, ensure, idx, packed_bits, packed_of, raw};
use crate::{
    expr_store::{
        BodySourceMap, ExprOrPatSource, ExprSource, ExpressionStoreDiagnostics,
        ExpressionStoreSourceMap, LabelSource, LifetimeSource, MacroCallPtr, PatSource, SelfParamPtr,
        TypeSource,
        source_map::{
            BackMap, BindingPats, ExpressionOnlySourceMap, FormatTemplate, IdTable, NO_SOURCE,
            SourceTable,
        },
    },
    hir::{
        BindingId, ExprId, ExprOrPatId, LabelId, PatId,
        persist::{Code, Column, Placed, Read, Words, WordsRec, id_of_bits},
    },
    type_ref::{LifetimeRefId, TypeRefId},
};

blaze_format::record! {
    /// A syntax pointer in a file: absent when `file_kind` is 0, and then all zeros.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub(crate) struct Src: 1 {
        /// The id of the file or of the macro call, as its bits.
        pub(crate) file: u64,
        /// 0 for no pointer, [`FILE`] or [`MACRO_FILE`].
        pub(crate) file_kind: u32,
        pub(crate) kind: u32,
        pub(crate) start: u32,
        pub(crate) end: u32,
    }
}

blaze_format::record! {
    /// A run of ids of a [`BackMap`] written in one file: the file and the run's first id.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct FileRun: 1 {
        /// The id of the file or of the macro call, as its bits.
        pub(crate) file: u64,
        /// [`FILE`] or [`MACRO_FILE`].
        pub(crate) file_kind: u32,
        pub(crate) start: u32,
    }
}

blaze_format::record! {
    /// A [`BackMap`]'s columns, as they are in memory: by id, the syntax kind ([`NO_SOURCE`]
    /// for an id without syntax, its range then zeros), the start and the end; and its runs.
    #[derive(Clone, Copy)]
    pub(crate) struct BackRec: 1 {
        pub(crate) kinds: Slice<u16>,
        pub(crate) starts: Slice<u32>,
        pub(crate) ends: Slice<u32>,
        pub(crate) runs: Slice<FileRun>,
    }
}

blaze_format::record! {
    /// The root of an [`ExpressionStoreSourceMap`]'s buffer. The back maps are dense, by id.
    /// `binding_starts` and `binding_pats` are the CSR of each binding's patterns, raw. Each
    /// [`SourceTable`] is its keys, in strictly increasing order of [`Src::key`], and the
    /// value of each beside it: an id raw (a packed one for expressions and patterns), a macro
    /// call's id as its bits.
    #[derive(Clone, Copy)]
    pub(crate) struct SourceMapRoot: 4 {
        pub(crate) expr_map_back: BackRec,
        pub(crate) pat_map_back: BackRec,
        pub(crate) label_map_back: BackRec,
        pub(crate) types_map_back: BackRec,
        pub(crate) lifetime_map_back: BackRec,
        pub(crate) binding_starts: Slice<u32>,
        pub(crate) binding_pats: Slice<u32>,
        pub(crate) expr_map: Slice<Src>,
        pub(crate) expr_map_ids: Slice<u32>,
        pub(crate) pat_map: Slice<Src>,
        pub(crate) pat_map_ids: Slice<u32>,
        pub(crate) label_map: Slice<Src>,
        pub(crate) label_map_ids: Slice<u32>,
        pub(crate) types_map: Slice<Src>,
        pub(crate) types_map_ids: Slice<u32>,
        pub(crate) lifetime_map: Slice<Src>,
        pub(crate) lifetime_map_ids: Slice<u32>,
        pub(crate) expansions: Slice<Src>,
        pub(crate) expansion_calls: Slice<u64>,
        pub(crate) words: WordsRec,
        /// [`HAS_EXPRS`].
        pub(crate) flags: u32,
    }
}

blaze_format::record! {
    /// The root of a [`BodySourceMap`]'s buffer.
    #[derive(Clone, Copy)]
    pub(crate) struct BodySourceMapRoot: 4 {
        pub(crate) store: SourceMapRoot,
        /// The pointer to `self`, when there is one.
        pub(crate) self_param: Slice<Src>,
    }
}

/// A [`Src`] or [`FileRun`] in a file.
const FILE: u32 = 1;
/// A [`Src`] or [`FileRun`] in the expansion of a macro call.
const MACRO_FILE: u32 = 2;

/// A run's row on disk: 16 bytes, where in memory it is a `u32` start and a 12-byte file.
const _: () = assert!(size_of::<FileRun>() == 16);
/// The kind, start and end of an id, as in [`BackMap`]: 10 bytes, where an `Option<Src>` slot
/// of the dense column this replaces was 24.
const _: () = assert!(size_of::<u16>() + 2 * size_of::<u32>() == 10 && size_of::<Src>() == 24);

fn syntax_kind(bits: u32) -> Option<SyntaxKind> {
    (bits < SyntaxKind::__LAST as u32).then(|| SyntaxKind::from(bits as u16))
}

fn file_bits(file: HirFileId) -> (u32, u64) {
    match file {
        HirFileId::FileId(file) => (FILE, bits(file)),
        HirFileId::MacroFile(call) => (MACRO_FILE, bits(call)),
    }
}

/// The file of `file_kind` and `file`, `None` when no file is.
fn file_of(file_kind: u32, file: u64) -> Option<HirFileId> {
    let id = id_of_bits(file)?;
    match file_kind {
        FILE => Some(HirFileId::FileId(FromId::from_id(id))),
        MACRO_FILE => Some(HirFileId::MacroFile(FromId::from_id(id))),
        _ => None,
    }
}

/// The pointer of type `N` of `kind` at `start..end`, `None` when `N` doesn't cast `kind`.
fn ptr_of<N: AstNode>(kind: SyntaxKind, start: u32, end: u32) -> Option<AstPtr<N>> {
    let range = TextRange::new(TextSize::from(start), TextSize::from(end));
    AstPtr::try_from_raw(SyntaxNodePtr::from_parts(kind, range))
}

/// Writes `table` in its order, which is by id.
fn put_table<K: Code, V: Code>(w: &mut Words, table: &IdTable<K, V>) {
    w.len(table.entries().len());
    for (k, v) in table.entries() {
        k.put(w);
        v.put(w);
    }
}

/// Reads a table [`put_table`] wrote, each id below `len`, the length of its arena.
fn get_table<T, V: Code>(
    r: &mut Read<'_>,
    len: usize,
) -> Result<IdTable<la_arena::Idx<T>, V>, FormatError> {
    let count = r.len()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let id = r.u32()?;
        if id as usize >= len {
            return Err(r.invalid("an id past its arena in a table"));
        }
        entries.push((idx(id), V::get(r)?));
    }
    IdTable::from_sorted(entries.into_boxed_slice())
        .ok_or_else(|| r.invalid("a table not sorted by id, or an id twice"))
}

impl Src {
    /// The order of a [`SourceTable`]'s keys: the file (files before macro files, each by its
    /// id), the range, the syntax kind.
    fn key(&self) -> (u32, u64, u32, u32, u32) {
        (self.file_kind, self.file, self.start, self.end, self.kind)
    }

    fn of<N: AstNode>(src: &InFile<AstPtr<N>>) -> Src {
        let (file_kind, file) = file_bits(src.file_id);
        let range = src.value.text_range();
        Src {
            file,
            file_kind,
            kind: u32::from(u16::from(src.value.kind())),
            start: range.start().into(),
            end: range.end().into(),
        }
    }

    /// The pointer this is, `None` for no pointer.
    fn get<N: AstNode>(&self) -> Result<Option<InFile<AstPtr<N>>>, &'static str> {
        if self.file_kind == 0 {
            return match *self == Src::default() {
                true => Ok(None),
                false => Err("an empty slot that is not zeros"),
            };
        }
        let file = match self.file_kind {
            FILE | MACRO_FILE => file_of(self.file_kind, self.file).ok_or("an id of 0 or past 32 bits")?,
            _ => return Err("a file kind out of range"),
        };
        let kind = syntax_kind(self.kind).ok_or("a syntax kind out of range")?;
        if self.start > self.end {
            return Err("a range that ends before it starts");
        }
        let ptr = ptr_of(kind, self.start, self.end)
            .ok_or("a syntax pointer of a kind its type does not cast")?;
        Ok(Some(InFile::new(file, ptr)))
    }
}

/// Where a [`BackMap`]'s columns were written.
#[derive(Clone, Copy)]
struct BackColumns {
    kinds: Column,
    starts: Column,
    ends: Column,
    runs: Column,
}

impl BackColumns {
    /// The columns of a map that has no ids.
    const EMPTY: BackColumns = {
        let none = Column { at: 0, len: 0 };
        BackColumns { kinds: none, starts: none, ends: none, runs: none }
    };

    fn write<T, N>(map: &BackMap<T, N>, out: &mut Writer) -> BackColumns {
        let runs: Vec<FileRun> = (map.run_starts.iter().zip(&map.run_files))
            .map(|(&start, &file)| {
                let (file_kind, file) = file_bits(file);
                FileRun { file, file_kind, start }
            })
            // alloc: the encode's one copy of the column, once per map.
            .collect();
        BackColumns {
            kinds: Column::push(out, &map.kinds),
            starts: Column::push(out, &map.starts),
            ends: Column::push(out, &map.ends),
            runs: Column::push(out, &runs),
        }
    }

    fn rec(self, at: u64) -> BackRec {
        let field = |offset: usize| at + offset as u64;
        BackRec {
            kinds: self.kinds.slice(field(offset_of!(BackRec, kinds))),
            starts: self.starts.slice(field(offset_of!(BackRec, starts))),
            ends: self.ends.slice(field(offset_of!(BackRec, ends))),
            runs: self.runs.slice(field(offset_of!(BackRec, runs))),
        }
    }
}

/// A [`BackMap`] read in place: its columns, checked against its rules by [`BackRef::check`].
#[derive(Clone, Copy)]
struct BackRef<'a> {
    kinds: &'a [u16],
    starts: &'a [u32],
    ends: &'a [u32],
    runs: &'a [FileRun],
}

impl<'a> BackRef<'a> {
    fn follow(rec: &'a BackRec, buffer: &'a [u8]) -> Result<BackRef<'a>, FormatError> {
        Ok(BackRef {
            kinds: rec.kinds.follow(buffer)?,
            starts: rec.starts.follow(buffer)?,
            ends: rec.ends.follow(buffer)?,
            runs: rec.runs.follow(buffer)?,
        })
    }

    /// The length of the arena this maps.
    fn len(&self) -> usize {
        self.kinds.len()
    }

    fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.starts.is_empty() && self.ends.is_empty() && self.runs.is_empty()
    }

    /// Checks the columns are a [`BackMap`] of pointers of type `N`, as
    /// [`crate::expr_store::source_map::BackPushes::freeze`] makes them: one length; each id
    /// without syntax all zeros, each other one of a kind `N` casts and a range that doesn't end
    /// before it starts; the runs canonical (the first at id 0, each other at an id with syntax,
    /// increasing, no two in a row of one file, none without syntax), none when no id has syntax.
    fn check<N: AstNode>(&self, rec: &BackRec, buffer: &[u8]) -> Result<(), FormatError> {
        let len = self.len();
        let one_length = self.starts.len() == len && self.ends.len() == len;
        ensure(one_length, rec, buffer, "a back map whose columns are not of one length")?;
        let mut first = None;
        for (slot, kind) in self.kinds.iter().enumerate() {
            let (start, end) = (self.starts[slot], self.ends[slot]);
            if *kind == NO_SOURCE {
                let zeros = start == 0 && end == 0;
                ensure(zeros, &self.starts[slot], buffer, "an id without syntax and a range")?;
                continue;
            }
            if first.is_none() {
                first = Some(slot);
            }
            ensure(start <= end, &self.ends[slot], buffer, "a range that ends before it starts")?;
            let casts =
                syntax_kind(u32::from(*kind)).and_then(|kind| ptr_of::<N>(kind, start, end)).is_some();
            ensure(casts, kind, buffer, "a syntax kind its pointer type does not cast")?;
        }
        let Some(first) = first else {
            return ensure(self.runs.is_empty(), rec, buffer, "file runs in a back map without syntax");
        };
        let Some(head) = self.runs.first() else {
            return Err(FormatError::Invalid { at: at_of(rec, buffer), what: "syntax without a file" });
        };
        ensure(head.start == 0, head, buffer, "a first file run that does not start at id 0")?;
        for run in self.runs {
            let file = file_of(run.file_kind, run.file).is_some();
            ensure(file, run, buffer, "a file run of a file kind out of range or an id of 0 or past 32 bits")?;
        }
        for pair in self.runs.windows(2) {
            let (before, run) = (&pair[0], &pair[1]);
            ensure(before.start < run.start, run, buffer, "file runs out of order")?;
            ensure((run.start as usize) < len, run, buffer, "a file run past its arena")?;
            let at_syntax = self.kinds[run.start as usize] != NO_SOURCE;
            ensure(at_syntax, run, buffer, "a file run that starts at an id without syntax")?;
            let other = (before.file_kind, before.file) != (run.file_kind, run.file);
            ensure(other, run, buffer, "two file runs in a row of one file")?;
        }
        match self.runs.get(1) {
            Some(second) => {
                ensure(first < second.start as usize, second, buffer, "a file run without syntax")
            }
            None => Ok(()),
        }
    }

    /// The syntax of `slot`, of the `N` [`BackRef::check`] checked the columns for.
    #[inline]
    fn get<N: AstNode>(&self, slot: u32) -> Option<InFile<AstPtr<N>>> {
        let at = slot as usize;
        let kind = *self.kinds.get(at)?;
        if kind == NO_SOURCE {
            return None;
        }
        let run = &self.runs[self.runs.partition_point(|run| run.start <= slot) - 1];
        let file = file_of(run.file_kind, run.file).expect("checked when the buffer was read");
        let ptr = ptr_of(SyntaxKind::from(kind), self.starts[at], self.ends[at])
            .expect("checked when the buffer was read");
        Some(InFile::new(file, ptr))
    }

    /// The map, each column one copy.
    fn decode<T, N>(&self) -> BackMap<T, N> {
        BackMap::from_columns(
            self.kinds.into(),
            self.starts.into(),
            self.ends.into(),
            // alloc: the decode's one copy of the column.
            self.runs.iter().map(|run| run.start).collect(),
            (self.runs.iter())
                .map(|run| file_of(run.file_kind, run.file).expect("checked when the buffer was read"))
                // alloc: the decode's one copy of the column.
                .collect(),
        )
    }
}

/// Checks CSR `starts` over `pats` for a store of `pats_len` patterns: empty with no patterns,
/// or from 0, never decreasing, to `pats.len()`, each pattern in the store.
fn check_binding_pats(
    starts: &[u32],
    pats: &[u32],
    pats_len: usize,
    root: &SourceMapRoot,
    buffer: &[u8],
) -> Result<(), FormatError> {
    match (starts.first(), starts.last()) {
        (Some(first), Some(last)) => {
            ensure(*first == 0, first, buffer, "binding patterns that don't start at 0")?;
            let whole = *last as usize == pats.len();
            ensure(whole, last, buffer, "binding patterns that don't end at the last pattern")?;
        }
        _ => ensure(pats.is_empty(), root, buffer, "binding patterns without bindings")?,
    }
    starts.windows(2).try_for_each(|pair| {
        ensure(pair[0] <= pair[1], &pair[1], buffer, "binding patterns out of order")
    })?;
    pats.iter().try_for_each(|pat| {
        ensure((*pat as usize) < pats_len, pat, buffer, "a binding's pattern past its arena")
    })
}

/// A [`SourceTable`]'s keys and the word of each value, in the table's order.
fn table_columns<N: AstNode, V, W>(
    table: &SourceTable<InFile<AstPtr<N>>, V>,
    word: impl Fn(&V) -> W,
) -> (Vec<Src>, Vec<W>) {
    let (keys, words): (Vec<Src>, Vec<W>) =
        table.entries().iter().map(|(src, value)| (Src::of(src), word(value))).unzip();
    debug_assert!(
        keys.windows(2).all(|pair| pair[0].key() < pair[1].key()),
        "a source table not in the order of `Src::key`, which no reader accepts"
    );
    (keys, words)
}

/// Checks a [`SourceTable`]'s columns in place: a value for each key, `valid` for each value,
/// each key a pointer of its type, the keys in strictly increasing order of [`Src::key`].
fn check_table<N: AstNode, W>(
    keys: &[Src],
    values: &[W],
    valid: impl Fn(&W) -> bool,
    root: &SourceMapRoot,
    buffer: &[u8],
) -> Result<(), FormatError> {
    let paired = keys.len() == values.len();
    ensure(paired, root, buffer, "a source table that is not one value for each key")?;
    for src in keys {
        let found =
            src.get::<N>().map_err(|what| FormatError::Invalid { at: at_of(src, buffer), what })?;
        ensure(found.is_some(), src, buffer, "an empty key in a source table")?;
    }
    for value in values {
        ensure(valid(value), value, buffer, "a source table's id past its arena")?;
    }
    keys.windows(2).try_for_each(|pair| {
        let ordered = pair[0].key() < pair[1].key();
        ensure(ordered, &pair[1], buffer, "a source table out of order, or a key twice")
    })
}

/// The [`SourceTable`] of columns [`check_table`] accepted, in their order.
fn source_table<N: AstNode, W: Copy, V>(
    keys: &[Src],
    values: &[W],
    buffer: &[u8],
    value: impl Fn(W) -> V,
) -> Result<SourceTable<InFile<AstPtr<N>>, V>, FormatError> {
    let entries = (keys.iter().zip(values))
        .map(|(src, &word)| {
            let at = at_of(src, buffer);
            let found = src.get::<N>().map_err(|what| FormatError::Invalid { at, what })?;
            let found =
                found.ok_or(FormatError::Invalid { at, what: "an empty key in a source table" })?;
            Ok((found, value(word)))
        })
        .collect::<Result<Box<[_]>, FormatError>>()?;
    let at = keys.first().map_or(0, |src| at_of(src, buffer));
    SourceTable::from_sorted(entries)
        .ok_or(FormatError::Invalid { at, what: "a source table out of order, or a key twice" })
}

/// The word of `src`'s value in a [`SourceTable`]'s checked columns, by binary search.
fn table_get<N: AstNode, W: Copy>(keys: &[Src], values: &[W], src: &InFile<AstPtr<N>>) -> Option<W> {
    let at = keys.binary_search_by_key(&Src::of(src).key(), Src::key).ok()?;
    values.get(at).copied()
}

/// Whether the packed id `bits` is below its arena's length: `exprs` or `pats`.
fn packed_below(bits: &u32, exprs: usize, pats: usize) -> bool {
    match packed_of(*bits).unpack() {
        ExprOrPatId::ExprId(expr) => (raw(expr) as usize) < exprs,
        ExprOrPatId::PatId(pat) => (raw(pat) as usize) < pats,
    }
}

/// Where everything of a source map but its root was written.
struct SourceMapColumns {
    words: Placed,
    expr_map_back: BackColumns,
    pat_map_back: BackColumns,
    label_map_back: BackColumns,
    types_map_back: BackColumns,
    lifetime_map_back: BackColumns,
    binding_starts: Column,
    binding_pats: Column,
    expr_map: Column,
    expr_map_ids: Column,
    pat_map: Column,
    pat_map_ids: Column,
    label_map: Column,
    label_map_ids: Column,
    types_map: Column,
    types_map_ids: Column,
    lifetime_map: Column,
    lifetime_map_ids: Column,
    expansions: Column,
    expansion_calls: Column,
    flags: u32,
}

impl SourceMapColumns {
    fn write(map: &ExpressionStoreSourceMap, out: &mut Writer) -> SourceMapColumns {
        let ExpressionStoreSourceMap {
            expr_only,
            types_map_back,
            types_map,
            lifetime_map_back,
            lifetime_map,
        } = map;
        let mut w = Words::new();
        let (types_map, types_map_ids) = table_columns(types_map, |&ty| raw(ty));
        let (lifetime_map, lifetime_map_ids) = table_columns(lifetime_map, |&lifetime| raw(lifetime));
        let (mut expr_map_back, mut pat_map_back, mut label_map_back) =
            (BackColumns::EMPTY, BackColumns::EMPTY, BackColumns::EMPTY);
        // alloc: empty unless the map has expressions; then the column's one copy.
        let (mut binding_starts, mut binding_pats) = (Column { at: 0, len: 0 }, Vec::new());
        // alloc: empty unless the map has expressions; then each is the column's one copy.
        let (mut expr_keys, mut expr_ids, mut pat_keys, mut pat_ids) = (vec![], vec![], vec![], vec![]);
        // alloc: empty unless the map has expressions; then each is the column's one copy.
        let (mut label_keys, mut label_ids, mut call_keys, mut calls) = (vec![], vec![], vec![], vec![]);
        if let Some(it) = expr_only.as_deref() {
            let ExpressionOnlySourceMap {
                expr_map,
                expr_map_back: exprs_back,
                pat_map,
                pat_map_back: pats_back,
                label_map,
                label_map_back: labels_back,
                binding_definitions,
                field_map_back,
                pat_field_map_back,
                template_map,
                expansions,
                diagnostics,
            } = it;
            put_table(&mut w, field_map_back);
            put_table(&mut w, pat_field_map_back);
            put_template(&mut w, template_map.as_deref());
            w.put(diagnostics);
            expr_map_back = BackColumns::write(exprs_back, out);
            pat_map_back = BackColumns::write(pats_back, out);
            label_map_back = BackColumns::write(labels_back, out);
            binding_starts = Column::push(out, &binding_definitions.starts);
            // alloc: the encode's one copy of the column, once per map.
            binding_pats = binding_definitions.pats.iter().map(|&pat| raw(pat)).collect();
            (expr_keys, expr_ids) = table_columns(expr_map, |&id| packed_bits(id));
            (pat_keys, pat_ids) = table_columns(pat_map, |&id| packed_bits(id));
            (label_keys, label_ids) = table_columns(label_map, |&label| raw(label));
            (call_keys, calls) = table_columns(expansions, |&call| bits(call));
        }
        let words = w.place(out);
        SourceMapColumns {
            words,
            expr_map_back,
            pat_map_back,
            label_map_back,
            types_map_back: BackColumns::write(types_map_back, out),
            lifetime_map_back: BackColumns::write(lifetime_map_back, out),
            binding_starts,
            binding_pats: Column::push(out, &binding_pats),
            expr_map: Column::push(out, &expr_keys),
            expr_map_ids: Column::push(out, &expr_ids),
            pat_map: Column::push(out, &pat_keys),
            pat_map_ids: Column::push(out, &pat_ids),
            label_map: Column::push(out, &label_keys),
            label_map_ids: Column::push(out, &label_ids),
            types_map: Column::push(out, &types_map),
            types_map_ids: Column::push(out, &types_map_ids),
            lifetime_map: Column::push(out, &lifetime_map),
            lifetime_map_ids: Column::push(out, &lifetime_map_ids),
            expansions: Column::push(out, &call_keys),
            expansion_calls: Column::push(out, &calls),
            flags: if expr_only.is_some() { HAS_EXPRS } else { 0 },
        }
    }

    fn rec(&self, at: u64) -> SourceMapRoot {
        let field = |offset: usize| at + offset as u64;
        SourceMapRoot {
            expr_map_back: self.expr_map_back.rec(field(offset_of!(SourceMapRoot, expr_map_back))),
            pat_map_back: self.pat_map_back.rec(field(offset_of!(SourceMapRoot, pat_map_back))),
            label_map_back: (self.label_map_back)
                .rec(field(offset_of!(SourceMapRoot, label_map_back))),
            types_map_back: (self.types_map_back)
                .rec(field(offset_of!(SourceMapRoot, types_map_back))),
            lifetime_map_back: (self.lifetime_map_back)
                .rec(field(offset_of!(SourceMapRoot, lifetime_map_back))),
            binding_starts: (self.binding_starts)
                .slice(field(offset_of!(SourceMapRoot, binding_starts))),
            binding_pats: self.binding_pats.slice(field(offset_of!(SourceMapRoot, binding_pats))),
            expr_map: self.expr_map.slice(field(offset_of!(SourceMapRoot, expr_map))),
            expr_map_ids: self.expr_map_ids.slice(field(offset_of!(SourceMapRoot, expr_map_ids))),
            pat_map: self.pat_map.slice(field(offset_of!(SourceMapRoot, pat_map))),
            pat_map_ids: self.pat_map_ids.slice(field(offset_of!(SourceMapRoot, pat_map_ids))),
            label_map: self.label_map.slice(field(offset_of!(SourceMapRoot, label_map))),
            label_map_ids: (self.label_map_ids)
                .slice(field(offset_of!(SourceMapRoot, label_map_ids))),
            types_map: self.types_map.slice(field(offset_of!(SourceMapRoot, types_map))),
            types_map_ids: (self.types_map_ids)
                .slice(field(offset_of!(SourceMapRoot, types_map_ids))),
            lifetime_map: self.lifetime_map.slice(field(offset_of!(SourceMapRoot, lifetime_map))),
            lifetime_map_ids: (self.lifetime_map_ids)
                .slice(field(offset_of!(SourceMapRoot, lifetime_map_ids))),
            expansions: self.expansions.slice(field(offset_of!(SourceMapRoot, expansions))),
            expansion_calls: (self.expansion_calls)
                .slice(field(offset_of!(SourceMapRoot, expansion_calls))),
            words: self.words.rec(field(offset_of!(SourceMapRoot, words))),
            flags: self.flags,
        }
    }
}

fn put_template(w: &mut Words, template: Option<&FormatTemplate>) {
    match template {
        None => w.u32(0),
        Some(template) => {
            let FormatTemplate {
                format_args_to_captures,
                asm_to_captures,
                implicit_capture_to_source,
            } = template;
            w.u32(1);
            put_table(w, format_args_to_captures);
            put_table(w, asm_to_captures);
            put_table(w, implicit_capture_to_source);
        }
    }
}

/// Reads a template [`put_template`] wrote, for a store of `exprs` expressions.
fn get_template(r: &mut Read<'_>, exprs: usize) -> Result<Option<Box<FormatTemplate>>, FormatError> {
    Ok(match r.tag(2, "a template map that is neither 0 nor 1")? {
        0 => None,
        // alloc: once per decoded map with templates, as `finish` boxes it.
        _ => Some(Box::new(FormatTemplate {
            format_args_to_captures: get_table(r, exprs)?,
            asm_to_captures: get_table(r, exprs)?,
            implicit_capture_to_source: get_table(r, exprs)?,
        })),
    })
}

/// An [`ExpressionStoreSourceMap`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct ExpressionStoreSourceMapRef<'a> {
    buffer: &'a [u8],
    root: &'a SourceMapRoot,
    expr_map_back: BackRef<'a>,
    pat_map_back: BackRef<'a>,
    label_map_back: BackRef<'a>,
    types_map_back: BackRef<'a>,
    lifetime_map_back: BackRef<'a>,
    binding_starts: &'a [u32],
    binding_pats: &'a [u32],
    expr_map: &'a [Src],
    expr_map_ids: &'a [u32],
    pat_map: &'a [Src],
    pat_map_ids: &'a [u32],
    label_map: &'a [Src],
    label_map_ids: &'a [u32],
    types_map: &'a [Src],
    types_map_ids: &'a [u32],
    lifetime_map: &'a [Src],
    lifetime_map_ids: &'a [u32],
    expansions: &'a [Src],
    expansion_calls: &'a [u64],
}

impl<'a> ExpressionStoreSourceMapRef<'a> {
    /// The source map of `root`, in `buffer`, its columns checked.
    fn new(
        root: &'a SourceMapRoot,
        buffer: &'a [u8],
    ) -> Result<ExpressionStoreSourceMapRef<'a>, FormatError> {
        let map = ExpressionStoreSourceMapRef {
            buffer,
            root,
            expr_map_back: BackRef::follow(&root.expr_map_back, buffer)?,
            pat_map_back: BackRef::follow(&root.pat_map_back, buffer)?,
            label_map_back: BackRef::follow(&root.label_map_back, buffer)?,
            types_map_back: BackRef::follow(&root.types_map_back, buffer)?,
            lifetime_map_back: BackRef::follow(&root.lifetime_map_back, buffer)?,
            binding_starts: root.binding_starts.follow(buffer)?,
            binding_pats: root.binding_pats.follow(buffer)?,
            expr_map: root.expr_map.follow(buffer)?,
            expr_map_ids: root.expr_map_ids.follow(buffer)?,
            pat_map: root.pat_map.follow(buffer)?,
            pat_map_ids: root.pat_map_ids.follow(buffer)?,
            label_map: root.label_map.follow(buffer)?,
            label_map_ids: root.label_map_ids.follow(buffer)?,
            types_map: root.types_map.follow(buffer)?,
            types_map_ids: root.types_map_ids.follow(buffer)?,
            lifetime_map: root.lifetime_map.follow(buffer)?,
            lifetime_map_ids: root.lifetime_map_ids.follow(buffer)?,
            expansions: root.expansions.follow(buffer)?,
            expansion_calls: root.expansion_calls.follow(buffer)?,
        };
        ensure(root.flags & !HAS_EXPRS == 0, root, buffer, "source map flags out of range")?;
        if root.flags & HAS_EXPRS == 0 {
            let empty = map.expr_map_back.is_empty()
                && map.pat_map_back.is_empty()
                && map.label_map_back.is_empty()
                && map.binding_starts.is_empty()
                && map.binding_pats.is_empty()
                && map.expr_map.is_empty()
                && map.expr_map_ids.is_empty()
                && map.pat_map.is_empty()
                && map.pat_map_ids.is_empty()
                && map.label_map.is_empty()
                && map.label_map_ids.is_empty()
                && map.expansions.is_empty()
                && map.expansion_calls.is_empty();
            ensure(empty, root, buffer, "expression maps in a source map without expressions")?;
        }
        map.expr_map_back.check::<Either<ast::Expr, ast::Pat>>(&root.expr_map_back, buffer)?;
        map.pat_map_back.check::<Either<ast::Expr, ast::Pat>>(&root.pat_map_back, buffer)?;
        (map.label_map_back)
            .check::<Either<ast::Label, ast::BlockExpr>>(&root.label_map_back, buffer)?;
        map.types_map_back.check::<ast::Type>(&root.types_map_back, buffer)?;
        map.lifetime_map_back.check::<ast::Lifetime>(&root.lifetime_map_back, buffer)?;
        let (exprs, pats) = (map.expr_map_back.len(), map.pat_map_back.len());
        let (labels, types, lifetimes) =
            (map.label_map_back.len(), map.types_map_back.len(), map.lifetime_map_back.len());
        check_binding_pats(map.binding_starts, map.binding_pats, pats, root, buffer)?;
        let packed = |bits: &u32| packed_below(bits, exprs, pats);
        check_table::<ast::Expr, _>(map.expr_map, map.expr_map_ids, packed, root, buffer)?;
        check_table::<ast::Pat, _>(map.pat_map, map.pat_map_ids, packed, root, buffer)?;
        check_table::<Either<ast::Label, ast::BlockExpr>, _>(
            map.label_map,
            map.label_map_ids,
            |&label| (label as usize) < labels,
            root,
            buffer,
        )?;
        let ty = |&ty: &u32| (ty as usize) < types;
        check_table::<ast::Type, _>(map.types_map, map.types_map_ids, ty, root, buffer)?;
        let lifetime = |&lifetime: &u32| (lifetime as usize) < lifetimes;
        check_table::<ast::Lifetime, _>(map.lifetime_map, map.lifetime_map_ids, lifetime, root, buffer)?;
        let call = |&call: &u64| id_of_bits(call).is_some();
        check_table::<ast::MacroCall, _>(map.expansions, map.expansion_calls, call, root, buffer)?;
        Ok(map)
    }

    /// As [`ExpressionStoreSourceMap::node_expr`], by binary search over the keys in place.
    pub fn node_expr(self, src: ExprSource) -> Option<ExprOrPatId> {
        table_get(self.expr_map, self.expr_map_ids, &src).map(|bits| packed_of(bits).unpack())
    }

    pub fn node_pat(self, src: PatSource) -> Option<ExprOrPatId> {
        table_get(self.pat_map, self.pat_map_ids, &src).map(|bits| packed_of(bits).unpack())
    }

    pub fn node_label(self, src: LabelSource) -> Option<LabelId> {
        table_get(self.label_map, self.label_map_ids, &src).map(idx)
    }

    pub fn node_type(self, src: TypeSource) -> Option<TypeRefId> {
        table_get(self.types_map, self.types_map_ids, &src).map(idx)
    }

    pub fn node_lifetime(self, src: LifetimeSource) -> Option<LifetimeRefId> {
        table_get(self.lifetime_map, self.lifetime_map_ids, &src).map(idx)
    }

    /// As [`ExpressionStoreSourceMap::expansion`].
    pub fn expansion(self, src: InFile<MacroCallPtr>) -> Option<MacroCallId> {
        let call = table_get(self.expansions, self.expansion_calls, &src)?;
        id_of_bits(call).map(<MacroCallId as FromId>::from_id)
    }

    pub fn has_exprs(self) -> bool {
        self.root.flags & HAS_EXPRS != 0
    }

    /// As [`ExpressionStoreSourceMap::expr_syntax`], `None` for a synthetic expression.
    pub fn expr_syntax(self, expr: ExprId) -> Option<ExprOrPatSource> {
        self.expr_map_back.get(raw(expr))
    }

    pub fn pat_syntax(self, pat: PatId) -> Option<ExprOrPatSource> {
        self.pat_map_back.get(raw(pat))
    }

    pub fn label_syntax(self, label: LabelId) -> Option<LabelSource> {
        self.label_map_back.get(raw(label))
    }

    pub fn type_syntax(self, ty: TypeRefId) -> Option<TypeSource> {
        self.types_map_back.get(raw(ty))
    }

    pub fn lifetime_syntax(self, lifetime: LifetimeRefId) -> Option<LifetimeSource> {
        self.lifetime_map_back.get(raw(lifetime))
    }

    /// As [`ExpressionStoreSourceMap::patterns_for_binding`], read in place; empty for a
    /// binding past the store's.
    pub fn patterns_for_binding(self, binding: BindingId) -> impl ExactSizeIterator<Item = PatId> + 'a {
        let at = raw(binding) as usize;
        let pats: &'a [u32] = match (self.binding_starts.get(at), self.binding_starts.get(at + 1)) {
            (Some(&start), Some(&end)) => &self.binding_pats[start as usize..end as usize],
            _ => &[],
        };
        pats.iter().map(|&pat| idx(pat))
    }

    /// The source map, back as the value that was written.
    pub fn decode(self) -> Result<ExpressionStoreSourceMap, FormatError> {
        let buffer = self.buffer;
        let mut r = Read::new(&self.root.words, buffer);
        let types_map = source_table(self.types_map, self.types_map_ids, buffer, idx)?;
        let lifetime_map = source_table(self.lifetime_map, self.lifetime_map_ids, buffer, idx)?;
        let (exprs, pats) = (self.expr_map_back.len(), self.pat_map_back.len());
        let expr_only = match self.has_exprs() {
            false => None,
            // alloc: once per decoded map, as `finish` boxes it.
            true => Some(Box::new(ExpressionOnlySourceMap {
                expr_map: source_table(self.expr_map, self.expr_map_ids, buffer, packed_of)?,
                expr_map_back: self.expr_map_back.decode(),
                pat_map: source_table(self.pat_map, self.pat_map_ids, buffer, packed_of)?,
                pat_map_back: self.pat_map_back.decode(),
                label_map: source_table(self.label_map, self.label_map_ids, buffer, idx)?,
                label_map_back: self.label_map_back.decode(),
                binding_definitions: BindingPats {
                    starts: self.binding_starts.into(),
                    // alloc: the decode's one copy of the column.
                    pats: self.binding_pats.iter().map(|&pat| idx(pat)).collect(),
                },
                field_map_back: get_table(&mut r, exprs)?,
                pat_field_map_back: get_table(&mut r, pats)?,
                template_map: get_template(&mut r, exprs)?,
                expansions: source_table(self.expansions, self.expansion_calls, buffer, |call| {
                    <MacroCallId as FromId>::from_id(
                        id_of_bits(call).expect("checked when the buffer was read"),
                    )
                })?,
                diagnostics: r.get()?,
            })),
        };
        Ok(ExpressionStoreSourceMap {
            expr_only,
            types_map_back: self.types_map_back.decode(),
            types_map,
            lifetime_map_back: self.lifetime_map_back.decode(),
            lifetime_map,
        })
    }
}

impl ExpressionStoreSourceMap {
    /// As [`ExpressionStore::columns`](crate::expr_store::ExpressionStore::columns), for
    /// [`ExpressionStoreSourceMap`]'s schema.
    pub fn columns() -> blaze_format::tiers::BufferColumns {
        blaze_format::tiers::BufferColumns::of::<SourceMapRoot>(<ExpressionStoreSourceMap as Persist>::SCHEMA)
    }
}

impl Persist for ExpressionStoreSourceMap {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("ExpressionStoreSourceMap"),
        Part::U64(4),
        Part::Schema(<SourceMapRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = ExpressionStoreSourceMapRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let columns = SourceMapColumns::write(self, out);
        let at = out.next::<SourceMapRoot>();
        let written = out.push(&columns.rec(at));
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<ExpressionStoreSourceMapRef<'_>, FormatError> {
        let root = open_as::<SourceMapRoot>(buffer, Self::SCHEMA)?;
        ExpressionStoreSourceMapRef::new(root, buffer)
    }
}

/// A [`BodySourceMap`] read in place from its buffer.
#[derive(Clone, Copy)]
pub struct BodySourceMapRef<'a> {
    store: ExpressionStoreSourceMapRef<'a>,
    self_param: Option<&'a Src>,
}

impl<'a> BodySourceMapRef<'a> {
    pub fn store(self) -> ExpressionStoreSourceMapRef<'a> {
        self.store
    }

    /// As [`BodySourceMap::self_param_syntax`].
    pub fn self_param(self) -> Option<InFile<SelfParamPtr>> {
        self.self_param?.get().ok()?
    }

    /// The source map, back as the value that was written.
    pub fn decode(self) -> Result<BodySourceMap, FormatError> {
        Ok(BodySourceMap { self_param: self.self_param(), store: self.store.decode()? })
    }
}

impl BodySourceMap {
    /// As [`ExpressionStore::columns`](crate::expr_store::ExpressionStore::columns), for
    /// [`BodySourceMap`]'s schema.
    pub fn columns() -> blaze_format::tiers::BufferColumns {
        blaze_format::tiers::BufferColumns::of::<BodySourceMapRoot>(<BodySourceMap as Persist>::SCHEMA)
    }
}

impl Persist for BodySourceMap {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("BodySourceMap"),
        Part::U64(4),
        Part::Schema(<BodySourceMapRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = BodySourceMapRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let BodySourceMap { self_param, store } = self;
        let columns = SourceMapColumns::write(store, out);
        let self_param = self_param.as_ref().map(Src::of);
        let self_param = Column::push(out, self_param.as_slice());
        let at = out.next::<BodySourceMapRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = BodySourceMapRoot {
            store: columns.rec(field(offset_of!(BodySourceMapRoot, store))),
            self_param: self_param.slice(field(offset_of!(BodySourceMapRoot, self_param))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<BodySourceMapRef<'_>, FormatError> {
        let root = open_as::<BodySourceMapRoot>(buffer, Self::SCHEMA)?;
        let store = ExpressionStoreSourceMapRef::new(&root.store, buffer)?;
        let self_params = root.self_param.follow(buffer)?;
        ensure(self_params.len() <= 1, root, buffer, "more than one `self` parameter")?;
        let self_param = self_params.first();
        if let Some(src) = self_param {
            let found = src
                .get::<ast::SelfParam>()
                .map_err(|what| FormatError::Invalid { at: at_of(src, buffer), what })?;
            ensure(found.is_some(), src, buffer, "an empty `self` parameter")?;
        }
        Ok(BodySourceMapRef { store, self_param })
    }
}

impl Code for SyntaxNodePtr {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(u16::from(self.kind())));
        self.text_range().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let kind = r.u32()?;
        let kind = syntax_kind(kind).ok_or_else(|| r.invalid("a syntax kind out of range"))?;
        Ok(SyntaxNodePtr::from_parts(kind, r.get()?))
    }
}

impl<N: AstNode> Code for AstPtr<N> {
    fn put(&self, w: &mut Words) {
        self.syntax_node_ptr().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let raw = r.get()?;
        AstPtr::try_from_raw(raw)
            .ok_or_else(|| r.invalid("a syntax pointer of a kind its type does not cast"))
    }
}

impl Code for ExpressionStoreDiagnostics {
    fn put(&self, w: &mut Words) {
        use ExpressionStoreDiagnostics::*;
        match self {
            InactiveCode { node, cfg } => {
                w.u32(0);
                node.put(w);
                cfg.put(w);
            }
            UnresolvedMacroCall { node, path } => {
                w.u32(1);
                node.put(w);
                path.put(w);
            }
            UnreachableLabel { node, name } => {
                w.u32(2);
                node.put(w);
                name.put(w);
            }
            AwaitOutsideOfAsync { node, location } => {
                w.u32(3);
                node.put(w);
                location.put(w);
            }
            UndeclaredLabel { node, name } => {
                w.u32(4);
                node.put(w);
                name.put(w);
            }
            PatternArgInExternFn { node } => {
                w.u32(5);
                node.put(w);
            }
            FruInDestructuringAssignment { node } => {
                w.u32(6);
                node.put(w);
            }
            MissingBody { node, kind } => {
                w.u32(7);
                node.put(w);
                kind.put(w);
            }
            IntLiteralTooLarge { node } => {
                w.u32(8);
                node.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        use ExpressionStoreDiagnostics::*;
        Ok(match r.tag(9, "a diagnostic kind out of range")? {
            0 => InactiveCode { node: r.get()?, cfg: r.get()? },
            1 => UnresolvedMacroCall { node: r.get()?, path: r.get()? },
            2 => UnreachableLabel { node: r.get()?, name: r.get()? },
            3 => AwaitOutsideOfAsync { node: r.get()?, location: r.get()? },
            4 => UndeclaredLabel { node: r.get()?, name: r.get()? },
            5 => PatternArgInExternFn { node: r.get()? },
            6 => FruInDestructuringAssignment { node: r.get()? },
            7 => MissingBody { node: r.get()?, kind: r.get()? },
            _ => IntLiteralTooLarge { node: r.get()? },
        })
    }
}

/// Writes `map` into a buffer, reads it back in place and decoded, and checks both are what was
/// written, field by field, and that the decoded map writes the same bytes.
#[cfg(test)]
pub(super) fn assert_round_trip(map: &ExpressionStoreSourceMap) {
    let bytes = blaze_format::to_bytes(map);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
    tests::assert_reads(read, map);
    let decoded = read.decode().expect("a source map just written");
    tests::assert_maps_eq(&decoded, map);
    assert_eq!(decoded, *map);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a source map written again as other bytes");
}

/// [`assert_round_trip`] for a body's source map.
#[cfg(test)]
pub(super) fn assert_body_round_trip(map: &BodySourceMap) {
    let bytes = blaze_format::to_bytes(map);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = BodySourceMap::read(&buffer).expect("a source map just written");
    tests::assert_reads(read.store(), &map.store);
    assert_eq!(read.self_param(), map.self_param);
    let decoded = read.decode().expect("a source map just written");
    tests::assert_maps_eq(&decoded.store, &map.store);
    assert_eq!(decoded, *map);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "a source map written again as other bytes");
}

#[cfg(test)]
mod tests {
    use blaze_format::Aligned;
    use hir_expand::name::Name;
    use smallvec::SmallVec;

    use super::{
        super::tests::{bodies, offset_in, with_word},
        *,
    };
    use crate::{
        expr_store::{Body, ExpressionStoreBuilder, HygieneId, source_map::tests::src},
        hir::{Binding, BindingAnnotation, Expr, Label, Pat},
        type_ref::{LifetimeRef, TypeRef},
    };

    /// Checks what `read` reads in place against `map`: every id of every arena (and one past
    /// it), with syntax or without, and every key of every table.
    pub(super) fn assert_reads(read: ExpressionStoreSourceMapRef<'_>, map: &ExpressionStoreSourceMap) {
        assert_eq!(read.has_exprs(), map.expr_only.is_some());
        if let Some(it) = map.expr_only.as_deref() {
            for id in (0..it.expr_map_back.len() as u32 + 1).map(idx) {
                assert_eq!(read.expr_syntax(id), it.expr_map_back.get(id), "{id:?}");
            }
            for id in (0..it.pat_map_back.len() as u32 + 1).map(idx) {
                assert_eq!(read.pat_syntax(id), it.pat_map_back.get(id), "{id:?}");
            }
            for id in (0..it.label_map_back.len() as u32 + 1).map(idx) {
                assert_eq!(read.label_syntax(id), it.label_map_back.get(id), "{id:?}");
            }
            let bindings = it.binding_definitions.starts.len() as u32;
            for binding in (0..bindings + 1).map(idx) {
                let pats: Vec<PatId> = read.patterns_for_binding(binding).collect();
                assert_eq!(pats, it.binding_definitions.get(binding), "{binding:?}");
            }
            for &(src, id) in it.expr_map.entries() {
                assert_eq!(read.node_expr(src), Some(id.unpack()));
            }
            for &(src, id) in it.pat_map.entries() {
                assert_eq!(read.node_pat(src), Some(id.unpack()));
            }
            for &(src, label) in it.label_map.entries() {
                assert_eq!(read.node_label(src), Some(label));
            }
            for &(src, call) in it.expansions.entries() {
                assert_eq!(read.expansion(src), Some(call));
            }
        }
        for id in (0..map.types_map_back.len() as u32 + 1).map(idx) {
            assert_eq!(read.type_syntax(id), map.types_map_back.get(id), "{id:?}");
        }
        for id in (0..map.lifetime_map_back.len() as u32 + 1).map(idx) {
            assert_eq!(read.lifetime_syntax(id), map.lifetime_map_back.get(id), "{id:?}");
        }
        for &(src, ty) in map.types_map.entries() {
            assert_eq!(read.node_type(src), Some(ty));
        }
        for &(src, lifetime) in map.lifetime_map.entries() {
            assert_eq!(read.node_lifetime(src), Some(lifetime));
        }
    }

    /// Every field of two source maps equal, the ones their `PartialEq` skips too.
    pub(super) fn assert_maps_eq(a: &ExpressionStoreSourceMap, b: &ExpressionStoreSourceMap) {
        assert_eq!(a.types_map_back, b.types_map_back);
        assert_eq!(a.types_map, b.types_map);
        assert_eq!(a.lifetime_map_back, b.lifetime_map_back);
        assert_eq!(a.lifetime_map, b.lifetime_map);
        assert_eq!(a.expr_only.is_some(), b.expr_only.is_some());
        if let (Some(a), Some(b)) = (a.expr_only.as_deref(), b.expr_only.as_deref()) {
            assert_eq!(a.expr_map, b.expr_map);
            assert_eq!(a.expr_map_back, b.expr_map_back);
            assert_eq!(a.pat_map, b.pat_map);
            assert_eq!(a.pat_map_back, b.pat_map_back);
            assert_eq!(a.label_map, b.label_map);
            assert_eq!(a.label_map_back, b.label_map_back);
            assert_eq!(a.binding_definitions, b.binding_definitions);
            assert_eq!(a.field_map_back, b.field_map_back);
            assert_eq!(a.pat_field_map_back, b.pat_field_map_back);
            assert_eq!(a.template_map, b.template_map);
            assert_eq!(a.expansions, b.expansions);
            assert_eq!(a.diagnostics, b.diagnostics);
        }
    }

    /// A source map lowered by hand: ids pushed twice, ids without syntax, runs in a file, a
    /// macro file and the file again, bindings with several patterns and with none.
    fn by_hand() -> ExpressionStoreSourceMap {
        let mut builder = ExpressionStoreBuilder::new();
        builder.inference_roots = Some(SmallVec::new());
        let exprs: Vec<ExprId> = (0..6).map(|_| builder.alloc_expr(Expr::Missing)).collect();
        let pats: Vec<PatId> = (0..4).map(|_| builder.alloc_pat(Pat::Wild)).collect();
        let labels: Vec<LabelId> =
            (0..2).map(|_| builder.labels.alloc(Label { name: Name::missing() })).collect();
        let types: Vec<TypeRefId> = (0..3).map(|_| builder.alloc_type_ref(TypeRef::Never)).collect();
        let lifetime = builder.lifetimes.alloc(LifetimeRef::Static);
        let bindings: Vec<BindingId> = (0..3)
            .map(|_| {
                builder.bindings.alloc(Binding {
                    name: Name::missing(),
                    mode: BindingAnnotation::Unannotated,
                    problems: None,
                    hygiene: HygieneId::ROOT,
                })
            })
            .collect();
        let (call, path, wild) = (SyntaxKind::CALL_EXPR, SyntaxKind::PATH_EXPR, SyntaxKind::WILDCARD_PAT);
        builder.expr_map_back.insert(exprs[0], src(0, false, call, 0, 4));
        builder.expr_map_back.insert(exprs[2], src(1, true, path, 0, 1));
        builder.expr_map_back.insert(exprs[3], src(1, true, path, 2, 3));
        builder.expr_map_back.insert(exprs[4], src(0, false, path, 5, 6));
        builder.expr_map_back.insert(exprs[0], src(0, false, call, 0, 7));
        builder.pat_map_back.insert(pats[1], src(0, false, wild, 8, 9));
        builder.pat_map_back.insert(pats[3], src(0, false, path, 10, 12));
        builder.label_map_back.insert(labels[1], src(0, false, SyntaxKind::LABEL, 13, 15));
        builder.types_map_back.insert(types[0], src(2, true, SyntaxKind::NEVER_TYPE, 1, 2));
        builder.types_map_back.insert(types[2], src(0, false, SyntaxKind::NEVER_TYPE, 16, 17));
        builder.lifetime_map_back.insert(lifetime, src(0, false, SyntaxKind::LIFETIME, 18, 20));
        builder.expr_map.push((src(0, false, call, 0, 7), exprs[0].into()));
        builder.expr_map.push((src(0, false, path, 5, 6), pats[2].into()));
        builder.pat_map.push((src(0, false, wild, 8, 9), pats[1].into()));
        builder.label_map.push((src(0, false, SyntaxKind::LABEL, 13, 15), labels[1]));
        builder.types_map.push((src(0, false, SyntaxKind::NEVER_TYPE, 16, 17), types[2]));
        builder.lifetime_map.push((src(0, false, SyntaxKind::LIFETIME, 18, 20), lifetime));
        builder.binding_definitions.extend([
            (bindings[2], pats[0]),
            (bindings[0], pats[1]),
            (bindings[2], pats[3]),
        ]);
        builder.finish().1
    }

    #[test]
    fn a_source_map_by_hand_round_trips() {
        let map = by_hand();
        let expr_only = map.expr_only.as_deref().expect("a map with expressions");
        assert_eq!(expr_only.expr_map_back.run_files.len(), 3, "file, macro file, file");
        assert_eq!(map.types_map_back.run_files.len(), 2);
        assert_round_trip(&map);
        assert_round_trip(&ExpressionStoreSourceMap::default());
    }

    #[test]
    fn bodies_and_signatures_round_trip() {
        bodies(
            r#"
macro_rules! m { ($e:expr) => { $e + 1 }; }
fn f(a: u32, (b, c): (u32, u32)) -> u32 {
    let d = m!(a);
    let e = |x: u32| x + b;
    match c { 0 | 1 => d, n => e(n) }
}
"#,
        );
    }

    /// `bytes` with the `u16` at `at` replaced by `value`.
    fn with_u16(bytes: &[u8], at: usize, value: u16) -> Aligned {
        let mut bytes = bytes.to_vec();
        bytes[at..at + size_of::<u16>()].copy_from_slice(&value.to_le_bytes());
        Aligned::copy_from(&bytes)
    }

    fn refused(buffer: &Aligned) -> bool {
        ExpressionStoreSourceMap::read(buffer).is_err()
    }

    #[test]
    fn a_corrupted_back_map_is_refused() {
        let bytes = blaze_format::to_bytes(&by_hand());
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
        let exprs = read.expr_map_back;
        let slot = exprs.kinds.iter().position(|&kind| kind != NO_SOURCE).expect("syntax");
        let none = exprs.kinds.iter().position(|&kind| kind == NO_SOURCE).expect("no syntax");
        let kind = offset_in(exprs.kinds, &buffer);
        let starts = offset_in(exprs.starts, &buffer);
        let u32_at = |column: usize, slot: usize| column + slot * size_of::<u32>();

        let lifetime = u16::from(SyntaxKind::LIFETIME);
        assert!(refused(&with_u16(&bytes, kind + 2 * slot, lifetime)), "a kind its type doesn't cast");
        let last = SyntaxKind::__LAST as u16;
        assert!(refused(&with_u16(&bytes, kind + 2 * slot, last)), "a kind out of range");
        assert!(refused(&with_u16(&bytes, kind + 2 * slot, NO_SOURCE)), "no syntax with a range");
        assert!(refused(&with_word(&bytes, u32_at(starts, none), 1)), "a range for no syntax");
        let end = exprs.ends[slot];
        assert!(refused(&with_word(&bytes, u32_at(starts, slot), end + 1)), "an end before its start");

        let len_at = at_of(&read.root.expr_map_back.ends, &buffer) as usize + offset_of!(Slice<u32>, len);
        assert!(refused(&with_word(&bytes, len_at, exprs.len() as u32 - 1)), "columns of two lengths");

        let runs = offset_in(exprs.runs, &buffer);
        let run = |at: usize, field: usize| runs + at * size_of::<FileRun>() + field;
        assert!(refused(&with_word(&bytes, run(0, offset_of!(FileRun, start)), 1)), "a first run past 0");
        assert!(refused(&with_word(&bytes, run(0, offset_of!(FileRun, file_kind)), 7)), "a file kind");
        assert!(refused(&with_word(&bytes, run(0, offset_of!(FileRun, file)), 0)), "an id of 0");
        assert!(refused(&with_word(&bytes, run(0, offset_of!(FileRun, file)) + 4, 1)), "an id past 32 bits");
        let second = offset_of!(FileRun, start);
        assert!(refused(&with_word(&bytes, run(1, second), exprs.runs[2].start)), "runs out of order");
        let empty = exprs.runs[1].start - 1;
        assert_eq!(exprs.kinds[empty as usize], NO_SOURCE, "an id without syntax before the macro's");
        assert!(refused(&with_word(&bytes, run(1, second), empty)), "a run at an id without syntax");
        let mut same = bytes.clone();
        let (from, to) = (run(0, 0), run(1, 0));
        same.copy_within(from..from + size_of::<FileRun>(), to);
        let start = run(1, offset_of!(FileRun, start));
        same[start..start + 4].copy_from_slice(&exprs.runs[1].start.to_le_bytes());
        assert!(refused(&Aligned::copy_from(&same)), "two runs in a row of one file");
    }

    #[test]
    fn a_corrupted_id_or_binding_list_is_refused() {
        let bytes = blaze_format::to_bytes(&by_hand());
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
        let (exprs, pats) = (read.expr_map_back.len() as u32, read.pat_map_back.len() as u32);

        let expr_id = offset_in(read.expr_map_ids, &buffer);
        assert!(refused(&with_word(&bytes, expr_id, exprs)), "an expression past its arena");
        let pat_id = offset_in(read.pat_map_ids, &buffer);
        let past = packed_bits(idx::<Pat<'static>>(pats).into());
        assert!(refused(&with_word(&bytes, pat_id, past)), "a pattern past its arena");
        let label = offset_in(read.label_map_ids, &buffer);
        assert!(refused(&with_word(&bytes, label, read.label_map_back.len() as u32)));
        let ty = offset_in(read.types_map_ids, &buffer);
        assert!(refused(&with_word(&bytes, ty, read.types_map_back.len() as u32)));

        let binding_pat = offset_in(read.binding_pats, &buffer);
        assert!(refused(&with_word(&bytes, binding_pat, pats)), "a binding's pattern past its arena");
        let starts = offset_in(read.binding_starts, &buffer);
        assert!(read.binding_starts.len() >= 3, "a map of more than one binding");
        let word = |at: usize| starts + at * size_of::<u32>();
        assert!(refused(&with_word(&bytes, word(0), 1)), "binding patterns not from 0");
        assert!(refused(&with_word(&bytes, word(1), u32::MAX)), "binding patterns out of order");
        let last = read.binding_starts.len() - 1;
        assert!(refused(&with_word(&bytes, word(last), read.binding_pats.len() as u32 - 1)));
    }

    #[test]
    fn a_corrupted_source_table_is_refused() {
        let (db, defs) = bodies("fn f() -> u32 { let a = 1; a + 2 }");
        let (_, source_map) = Body::with_source_map(&db, defs[0]);
        let bytes = blaze_format::to_bytes(&source_map.store);
        let buffer = Aligned::copy_from(&bytes);
        let read = ExpressionStoreSourceMap::read(&buffer).expect("a source map just written");
        assert!(read.expr_map.len() >= 2, "a body of more than one expression");
        let (keys, size) = (offset_in(read.expr_map, &buffer), size_of::<Src>());

        let mut swapped = bytes.clone();
        let (first, second) = swapped[keys..keys + 2 * size].split_at_mut(size);
        first.swap_with_slice(second);
        assert!(refused(&Aligned::copy_from(&swapped)), "a source table out of order");

        let mut twice = bytes.clone();
        twice.copy_within(keys..keys + size, keys + size);
        assert!(refused(&Aligned::copy_from(&twice)), "a key twice in a source table");

        assert!(refused(&with_word(&bytes, keys + offset_of!(Src, kind), u32::MAX)), "a key's kind");
        assert!(refused(&with_word(&bytes, keys + offset_of!(Src, file_kind), 7)), "a key's file");
        assert!(refused(&with_word(&bytes, keys + offset_of!(Src, file), 0)), "a key's id");

        let expr_map = &source_map.store.expr_only.as_deref().expect("a body's source map").expr_map;
        let reversed: Box<[_]> = expr_map.entries().iter().rev().copied().collect();
        assert!(SourceTable::from_sorted(reversed).is_none(), "decoding re-sorts nothing");
    }
}
