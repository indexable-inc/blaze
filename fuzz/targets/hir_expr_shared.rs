//! The expression store's buffers, as `crates/hir-def/src/expr_store/persist.rs` lays them out
//! (`StoreColumns::write`, `SourceMapColumns::write`), for the four `expr_store` targets. It is a
//! module of each (`#[path = "hir_expr_shared.rs"] mod expr;`), beside `hir_shared`.
//!
//! [`store`] is a store of expressions, patterns and types whose every column holds something:
//! an expression root returning a tuple of a missing expression and `_`, a wild pattern, an
//! arm, a tuple type, hygiene, a block scope, an edition and a closure that inherits overflow
//! checks. [`source_map`] points each kind of id at syntax in a file and in a macro file. Their
//! side tables, bindings, labels, lifetimes, statements, paths, macro calls and record fields
//! are empty: those are words and names the decode reads as values, or columns no other column
//! here names.

#![allow(dead_code)]

use blaze_format::SchemaHash;
use span::{Edition, SyntaxContext};
use syntax::SyntaxKind;

use crate::shared::{Build, Col, Placed, Root, Valid, Words};

/// `StoreRoot::flags`.
const HAS_EXPRS: u32 = 1 << 0;
const HAS_TYPE_HYGIENE: u32 = 1 << 1;

/// `ExprKind`, `PatKind` and `TypeRefKind` numbers.
const EXPR_MISSING: u8 = 0;
const EXPR_RETURN: u8 = 12;
const EXPR_TUPLE: u8 = 26;
const EXPR_UNDERSCORE: u8 = 29;
const PAT_WILD: u8 = 2;
const TYPE_NEVER: u8 = 0;
const TYPE_PLACEHOLDER: u8 = 1;
const TYPE_TUPLE: u8 = 2;

/// A packed id's bit for a pattern.
const PAT_BIT: u32 = 1 << 31;

/// `Src::file_kind`s.
const FILE: u32 = 1;
const MACRO_FILE: u32 = 2;

/// Where a store's columns went, in `StoreRoot`'s order.
pub struct StoreCols {
    placed: Placed,
    columns: [Col; 32],
    side: u32,
    ty_side: u32,
    flags: u32,
}

/// Writes [`store`]'s columns, as `StoreColumns::write` writes a store's.
pub fn store(build: &mut Build) -> StoreCols {
    let ctx = SyntaxContext::root(Edition::CURRENT).into_u32();
    let mut w = Words::default();
    // The side tables (names, literals, `offset_of!`s, `asm!`s, macro definitions), then the
    // type side tables (bounds; the paths' mod paths, lang items, generic args, args, bindings,
    // binding bounds, names and use args; references, arrays, fn pointers, type params): all
    // empty.
    let side = w.at();
    (0..5).for_each(|_| w.u32(0));
    let ty_side = w.at();
    (0..13).for_each(|_| w.u32(0));
    let placed = w.place(build);
    build.column::<u8>(&[], false);
    let record_fields = build.column::<[u32; 3]>(&[], false);
    let field_pats = build.column::<[u32; 3]>(&[], false);
    let expr_kind = build.column(&[EXPR_MISSING, EXPR_UNDERSCORE, EXPR_TUPLE, EXPR_RETURN], true);
    // A tuple's words in `extra`; `return` names expression 2 plus one.
    let expr_data = build.column::<u32>(&[0, 0, 0, 3], true);
    let pat_kind = build.column(&[PAT_WILD], true);
    let pat_data = build.column::<u32>(&[0], true);
    let extra = build.column::<u32>(&[0, 2], true);
    let expr_lists = build.column::<u32>(&[0, 1], true);
    let pat_lists = build.column::<u32>(&[0], true);
    let stmt_kind = build.column::<u8>(&[], true);
    let stmt_data = build.column::<u32>(&[], true);
    // Pattern 0, no guard, expression 0.
    let arms = build.column::<[u32; 3]>(&[[0, 0, 0]], true);
    // `None`, then type 0.
    let type_lists = build.column::<u32>(&[0, 1], true);
    let bindings = build.column::<[u32; 2]>(&[], false);
    let labels = build.column::<[u32; 2]>(&[], false);
    let ty_kind = build.column(&[TYPE_NEVER, TYPE_PLACEHOLDER, TYPE_TUPLE], true);
    let ty_data = build.column::<u32>(&[0, 0, 0], true);
    let ty_extra = build.column::<u32>(&[0, 2], true);
    let ty_lists = build.column::<u32>(&[0, 1], true);
    let path_kind = build.column::<u8>(&[], true);
    let path_data = build.column::<u32>(&[], true);
    let path_extra = build.column::<u32>(&[], true);
    let path_slots = build.column::<u32>(&[], true);
    let lifetimes = build.column::<[u32; 2]>(&[], false);
    let binding_owners = build.column::<u32>(&[], true);
    let block_scopes = build.column::<u64>(&[1], true);
    let macro_calls = build.column::<u64>(&[], true);
    let ident_hygiene = build.column::<[u32; 2]>(&[[1, ctx], [PAT_BIT, ctx]], true);
    let block_editions = build.column::<[u32; 2]>(&[[3, Edition::CURRENT as u32]], true);
    let overflow_inheriting = build.column::<u32>(&[2], true);
    // The root, and where its expressions, patterns and bindings end.
    let expr_roots = build.column::<[u32; 4]>(&[[3, 4, 1, 0]], true);
    let type_hygiene = build.column::<[u32; 2]>(&[[0, ctx], [2, ctx]], true);
    StoreCols {
        placed,
        columns: [
            expr_kind,
            expr_data,
            pat_kind,
            pat_data,
            extra,
            expr_lists,
            pat_lists,
            stmt_kind,
            stmt_data,
            arms,
            record_fields,
            field_pats,
            type_lists,
            bindings,
            labels,
            ty_kind,
            ty_data,
            ty_extra,
            ty_lists,
            path_kind,
            path_data,
            path_extra,
            path_slots,
            lifetimes,
            binding_owners,
            block_scopes,
            macro_calls,
            ident_hygiene,
            block_editions,
            overflow_inheriting,
            expr_roots,
            type_hygiene,
        ],
        side,
        ty_side,
        flags: HAS_EXPRS | HAS_TYPE_HYGIENE,
    }
}

/// The `StoreRoot` fields of `cols`.
pub fn store_root(root: &mut Root, cols: &StoreCols) {
    cols.columns.iter().for_each(|&column| root.slice(column));
    root.words(&cols.placed);
    [cols.side, cols.ty_side, cols.flags].into_iter().for_each(|word| root.u32(word));
}

/// The [`store`] alone, as `<ExpressionStore as Persist>::write` writes it.
pub fn store_buffer(schema: SchemaHash) -> Valid {
    let mut build = Build::new(schema);
    let cols = store(&mut build);
    let mut root = build.root();
    store_root(&mut root, &cols);
    build.finish(root)
}

/// A `Src` record, as its three words of eight bytes: the file's query ID, then the file kind
/// and the syntax kind, then the range.
fn src(file_kind: u32, file: u64, kind: SyntaxKind, start: u32, end: u32) -> [u64; 3] {
    let kind = u64::from(u16::from(kind));
    [file, u64::from(file_kind) | kind << 32, u64::from(start) | u64::from(end) << 32]
}

/// A `Src` in the one file of the source map.
fn in_file(kind: SyntaxKind, start: u32, end: u32) -> [u64; 3] {
    src(FILE, 1, kind, start, end)
}

/// `NO_SOURCE`: the kind of an id of a back map without syntax.
const NO_SOURCE: u16 = u16::MAX;

/// A `FileRun`, as its two words of eight bytes: the file's query ID, then the file kind and
/// the run's first id.
fn run(file_kind: u32, file: u64, start: u32) -> [u64; 2] {
    [file, u64::from(file_kind) | u64::from(start) << 32]
}

/// A `BackRec`'s columns: by id the syntax kind, start and end, then the runs of ids in one file.
struct Back {
    kinds: Col,
    starts: Col,
    ends: Col,
    runs: Col,
}

/// Writes a back map of `ids` (`None` for an id without syntax) and its `runs`, as
/// `BackColumns::write` does.
fn back(build: &mut Build, ids: &[Option<(SyntaxKind, u32, u32)>], runs: &[[u64; 2]]) -> Back {
    let kinds: Vec<u16> = ids.iter().map(|it| it.map_or(NO_SOURCE, |(kind, ..)| u16::from(kind))).collect();
    let starts: Vec<u32> = ids.iter().map(|it| it.map_or(0, |(_, start, _)| start)).collect();
    let ends: Vec<u32> = ids.iter().map(|it| it.map_or(0, |(.., end)| end)).collect();
    Back {
        kinds: build.column(&kinds, true),
        starts: build.column(&starts, true),
        ends: build.column(&ends, true),
        runs: build.column(runs, true),
    }
}

/// Where a source map's columns went, in `SourceMapRoot`'s order.
pub struct SourceMapCols {
    placed: Placed,
    backs: [Back; 5],
    columns: [Col; 14],
    flags: u32,
}

/// Writes [`source_map`]'s columns, as `SourceMapColumns::write` writes a source map's.
pub fn source_map(build: &mut Build) -> SourceMapCols {
    let mut w = Words::default();
    // No field maps, template or diagnostics.
    (0..4).for_each(|_| w.u32(0));
    // Expression 3 is in a macro file, so the map has a second run.
    let expr_map_back = back(
        build,
        &[
            Some((SyntaxKind::PATH_EXPR, 0, 3)),
            None,
            Some((SyntaxKind::TUPLE_EXPR, 4, 9)),
            Some((SyntaxKind::PATH_EXPR, 0, 1)),
        ],
        &[run(FILE, 1, 0), run(MACRO_FILE, 7, 3)],
    );
    let pat_map_back = back(build, &[Some((SyntaxKind::WILDCARD_PAT, 10, 11))], &[run(FILE, 1, 0)]);
    let label_map_back = back(build, &[Some((SyntaxKind::LABEL, 12, 15))], &[run(FILE, 1, 0)]);
    // One binding, of pattern 0.
    let binding_starts = build.column::<u32>(&[0, 1], true);
    // The words go after the expression maps' first columns, as the writer places them.
    let placed = w.place(build);
    let types_map_back = back(build, &[Some((SyntaxKind::NEVER_TYPE, 16, 17))], &[run(FILE, 1, 0)]);
    let lifetime_map_back = back(build, &[Some((SyntaxKind::LIFETIME, 18, 20))], &[run(FILE, 1, 0)]);
    let binding_pats = build.column::<u32>(&[0], true);
    // Each table's keys in increasing order of (file kind, file, start, end, kind).
    let expr_map = build.column(
        &[
            in_file(SyntaxKind::PATH_EXPR, 0, 3),
            in_file(SyntaxKind::TUPLE_EXPR, 4, 9),
            src(MACRO_FILE, 7, SyntaxKind::PATH_EXPR, 0, 1),
        ],
        true,
    );
    let expr_map_ids = build.column::<u32>(&[0, 2, 3], true);
    let pat_map = build.column(&[in_file(SyntaxKind::WILDCARD_PAT, 10, 11)], true);
    let pat_map_ids = build.column::<u32>(&[PAT_BIT], true);
    let label_map = build.column(&[in_file(SyntaxKind::LABEL, 12, 15)], true);
    let label_map_ids = build.column::<u32>(&[0], true);
    let types_map = build.column(&[in_file(SyntaxKind::NEVER_TYPE, 16, 17)], true);
    let types_map_ids = build.column::<u32>(&[0], true);
    let lifetime_map = build.column(&[in_file(SyntaxKind::LIFETIME, 18, 20)], true);
    let lifetime_map_ids = build.column::<u32>(&[0], true);
    let expansions = build.column(&[in_file(SyntaxKind::MACRO_CALL, 21, 25)], true);
    let expansion_calls = build.column::<u64>(&[5], true);
    SourceMapCols {
        placed,
        backs: [expr_map_back, pat_map_back, label_map_back, types_map_back, lifetime_map_back],
        columns: [
            binding_starts,
            binding_pats,
            expr_map,
            expr_map_ids,
            pat_map,
            pat_map_ids,
            label_map,
            label_map_ids,
            types_map,
            types_map_ids,
            lifetime_map,
            lifetime_map_ids,
            expansions,
            expansion_calls,
        ],
        flags: HAS_EXPRS,
    }
}

/// The `SourceMapRoot` fields of `cols`.
pub fn source_map_root(root: &mut Root, cols: &SourceMapCols) {
    for back in &cols.backs {
        [back.kinds, back.starts, back.ends, back.runs].into_iter().for_each(|column| root.slice(column));
    }
    cols.columns.iter().for_each(|&column| root.slice(column));
    root.words(&cols.placed);
    root.u32(cols.flags);
}

/// The `self` parameter's syntax, for a body's source map.
pub fn self_param() -> [u64; 3] {
    in_file(SyntaxKind::SELF_PARAM, 26, 30)
}
