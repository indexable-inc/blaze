//! A file's item tree as the store keeps it once cold (`crates/hir-def/src/item_tree/persist.rs`):
//! `<ItemTree as Persist>::read`, then `ItemTreeRef::decode`, of bytes from the store or another
//! machine. The input is one of `hir_shared`'s two kinds: a buffer as it came, or edits of the
//! plain columns of a valid tree.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `read` and `decode` never panic or hang;
//! - no allocation of them is larger than `blaze_fuzz::allowed` of the buffer's length;
//! - a tree decoded from any buffer is written as a buffer that decodes to an equal tree and is
//!   written again as the same bytes;
//! - a valid tree edited only in its columns (the per-item columns, the payload columns, the
//!   lists, the use trees, the constructor visibilities, the attribute owners) decodes to
//!   `Err`, or to a tree written as exactly the edited bytes.
//!
//! The decoded tree's macro calls and attributes hold `SyntaxContext`s from the bytes; they are
//! compared and dropped, never looked up.

#![forbid(unsafe_code)]

use std::{sync::OnceLock, time::Duration};

use blaze_format::{FormatError, Persist};
use blaze_fuzzer::Target;
use hir_def::item_tree::ItemTree;
use span::{Edition, EditionedFileId, FileId, SyntaxContext};

#[path = "hir_shared.rs"]
mod shared;

use shared::{Build, Valid, Words};

const TARGET: Target = Target {
    name: "hir_item_tree",
    check,
    seeds,
    dictionary: &[
        b"BLAZEV2\x00",
        b"\xff\xff\xff\xff",
        b"\xfe\xff\xff\xff",
        b"\xfd\xff\xff\xff",
        b"\xfc\xff\xff\xff",
        b"\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
        b"\x0f",
        b"\x10",
    ],
    max_len: 1 << 14,
    budget: Duration::from_secs(600),
};

fn decode(buffer: &[u8]) -> Result<ItemTree, FormatError> {
    ItemTree::read(buffer)?.decode()
}

fn check(data: &[u8]) {
    shared::check(data, valid(), decode);
}

fn seeds() -> Vec<Vec<u8>> {
    shared::inputs(valid())
}

/// `RawVisibilityId`s every tree has.
const PUB: u32 = u32::MAX;
const PRIV_IMPLICIT: u32 = u32::MAX - 1;
const PRIV_EXPLICIT: u32 = u32::MAX - 2;
const PUB_CRATE: u32 = u32::MAX - 3;
/// `NameId::NONE`, `PathId::NONE`, `AliasId::NONE`, `AttrOwner::ENABLED`.
const NONE: u32 = u32::MAX;
/// `IdRange::NONE`'s start: the items of a `mod m;`.
const OUTLINE: u32 = u32::MAX;

/// `ItemKind`s, in the order `mod_items!` lists them.
const CONST: u8 = 0;
const ENUM: u8 = 1;
const EXTERN_BLOCK: u8 = 2;
const EXTERN_CRATE: u8 = 3;
const FUNCTION: u8 = 4;
const IMPL: u8 = 5;
const MACRO2: u8 = 6;
const MACRO_CALL: u8 = 7;
const MACRO_RULES: u8 = 8;
const MOD: u8 = 9;
const STATIC: u8 = 10;
const STRUCT: u8 = 11;
const TRAIT: u8 = 12;
const TYPE_ALIAS: u8 = 13;
const UNION: u8 = 14;
const USE: u8 = 15;

/// An ast id for each of `kinds`, of the `ErasedFileAstIdKind` each item kind's syntax is
/// (`span::ast_id`: the kind in the top 5 bits), its hash the item's place plus one.
fn ast_ids(kinds: &[u8]) -> Vec<u32> {
    (1..).zip(kinds)
        .map(|(hash, &kind)| {
            let erased: u32 = match kind {
                CONST => 11,
                ENUM => 1,
                EXTERN_BLOCK => 15,
                EXTERN_CRATE => 4,
                FUNCTION => 12,
                IMPL => 17,
                MACRO2 => 5,
                MACRO_CALL => 13,
                MACRO_RULES => 6,
                MOD => 7,
                STATIC => 8,
                STRUCT => 2,
                TRAIT => 9,
                TYPE_ALIAS => 14,
                UNION => 3,
                USE => 16,
                _ => unreachable!("no item kind {kind}"),
            };
            hash | erased << 27
        })
        .collect()
}

/// `UseTree` kinds.
const SINGLE: u32 = 0;
const GLOB: u32 = 1;
const PREFIXED: u32 = 2;

/// An item tree's columns, each record as its fields' words, in `Root`'s order.
#[derive(Default)]
struct Columns {
    top_level: Vec<u32>,
    kinds: Vec<u8>,
    data: Vec<u32>,
    ast_ids: Vec<u32>,
    children: Vec<u32>,
    /// name, visibility
    consts: Vec<[u32; 2]>,
    enums: Vec<[u32; 2]>,
    /// children start, len
    extern_blocks: Vec<[u32; 2]>,
    /// name, alias, visibility
    extern_crates: Vec<[u32; 3]>,
    functions: Vec<[u32; 2]>,
    /// `is_trait_impl`
    impls: Vec<u8>,
    macro2s: Vec<[u32; 2]>,
    /// path, syntax context, expansion kind
    macro_calls: Vec<[u32; 3]>,
    /// name
    macro_rules: Vec<u32>,
    /// name, visibility, items start, len
    mods: Vec<[u32; 4]>,
    statics: Vec<[u32; 2]>,
    /// name, visibility, constructor visibilities start, len
    structs: Vec<[u32; 4]>,
    traits: Vec<[u32; 2]>,
    type_aliases: Vec<[u32; 2]>,
    unions: Vec<[u32; 2]>,
    /// visibility, use tree
    uses: Vec<[u32; 2]>,
    /// kind, path, alias, list start, len
    use_trees: Vec<[u32; 5]>,
    use_tree_lists: Vec<u32>,
    ctor_vis: Vec<u32>,
    /// owner, attributes start, len, cfg
    attr_owners: Vec<[u32; 4]>,
}

/// The tree of `columns` and `words`, its tables (names, paths, visibilities, attributes, cfgs)
/// beginning at the words `tables` names, laid out as `<ItemTree as Persist>::write` lays one
/// out. Every column is one the decode copies byte for byte.
fn tree(words: &Words, tables: [u32; 5], c: &Columns) -> Valid {
    let mut build = Build::new(ItemTree::SCHEMA);
    let placed = words.place(&mut build);
    let columns = [
        build.column(&c.top_level, true),
        build.column(&c.kinds, true),
        build.column(&c.data, true),
        build.column(&c.ast_ids, true),
        build.column(&c.children, true),
        build.column(&c.consts, true),
        build.column(&c.enums, true),
        build.column(&c.extern_blocks, true),
        build.column(&c.extern_crates, true),
        build.column(&c.functions, true),
        build.column(&c.impls, true),
        build.column(&c.macro2s, true),
        build.column(&c.macro_calls, true),
        build.column(&c.macro_rules, true),
        build.column(&c.mods, true),
        build.column(&c.statics, true),
        build.column(&c.structs, true),
        build.column(&c.traits, true),
        build.column(&c.type_aliases, true),
        build.column(&c.unions, true),
        build.column(&c.uses, true),
        build.column(&c.use_trees, true),
        build.column(&c.use_tree_lists, true),
        build.column(&c.ctor_vis, true),
        build.column(&c.attr_owners, true),
    ];
    let mut root = build.root();
    columns.into_iter().for_each(|column| root.slice(column));
    root.words(&placed);
    tables.into_iter().for_each(|at| root.u32(at));
    build.finish(root)
}

fn ctx() -> u32 {
    SyntaxContext::root(Edition::CURRENT).into_u32()
}

/// A span's words: its range, its anchor's file and ast id, its context.
fn span(w: &mut Words, start: u32, end: u32) {
    for word in [start, end, EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT).as_u32(), 0, ctx()] {
        w.u32(word);
    }
}

/// A plain path of one segment.
fn plain(w: &mut Words, segment: &str) {
    w.u32(0);
    w.u32(1);
    w.name(segment);
}

/// `N` empty tables from `w.at()` on, and where each begins.
fn empty_tables<const N: usize>(w: &mut Words) -> [u32; N] {
    std::array::from_fn(|_| {
        let at = w.at();
        w.u32(0);
        at
    })
}

/// A function in an inline module, a struct with a constructor visibility, a single `use`, a
/// macro call, a trait impl and `const _`; a visibility `pub(super)`, an attribute and a `cfg`
/// that disables the struct.
fn items() -> Valid {
    let mut w = Words::default();
    let names = w.at();
    w.u32(3);
    ["f", "m", "S"].into_iter().for_each(|name| w.name(name));
    let paths = w.at();
    w.u32(1);
    plain(&mut w, "a");
    let vis = w.at();
    // `Module(ModPath { Super(1), [] }, Explicit)`.
    [1, 0, 1, 1, 0, 0].into_iter().for_each(|word| w.u32(word));
    let attrs = w.at();
    // `#[inline]`: its path, no input, its context.
    w.u32(1);
    plain(&mut w, "inline");
    w.u32(0);
    w.u32(ctx());
    let cfgs = w.at();
    // `Atom(Flag(test))`.
    w.u32(1);
    w.u32(1);
    w.u32(0);
    w.name("test");
    let kinds = vec![FUNCTION, MOD, STRUCT, USE, MACRO_CALL, IMPL, CONST];
    let columns = Columns {
        top_level: vec![1, 2, 3, 4, 5, 6],
        ast_ids: ast_ids(&kinds),
        kinds,
        data: vec![0; 7],
        children: vec![0],
        consts: vec![[NONE, PRIV_EXPLICIT]],
        functions: vec![[0, PUB]],
        impls: vec![1],
        macro_calls: vec![[0, ctx(), 1]],
        mods: vec![[1, 0, 0, 1]],
        structs: vec![[2, PUB_CRATE, 0, 1]],
        uses: vec![[PRIV_IMPLICIT, 0]],
        use_trees: vec![[SINGLE, 0, u32::MAX - 1, 0, 0]],
        ctor_vis: vec![PRIV_IMPLICIT],
        attr_owners: vec![[0, 0, 1, NONE], [2, 0, 0, 0]],
        ..Columns::default()
    };
    tree(&w, [names, paths, vis, attrs, cfgs], &columns)
}

/// A function with `#[custom(x)]`: an attribute whose input is a token tree, its top a visible
/// subtree the builder kept by dropping its own invisible one.
fn token_tree() -> Valid {
    let mut w = Words::default();
    let names = w.at();
    w.u32(1);
    w.name("g");
    let paths = w.at();
    w.u32(0);
    let vis = w.at();
    w.u32(0);
    let attrs = w.at();
    w.u32(1);
    plain(&mut w, "custom");
    // `Some(TokenTree(..))`: it dropped a top of its own, then two tokens.
    [1, 1, 1, 2].into_iter().for_each(|word| w.u32(word));
    // `(`, holding the one token after it.
    w.u32(0);
    span(&mut w, 6, 7);
    span(&mut w, 8, 9);
    w.u32(0);
    w.u32(1);
    // `x`, not raw.
    w.u32(3);
    w.name("x");
    span(&mut w, 7, 8);
    w.u32(0);
    w.u32(ctx());
    let cfgs = w.at();
    w.u32(0);
    let columns = Columns {
        top_level: vec![0],
        kinds: vec![FUNCTION],
        data: vec![0],
        ast_ids: ast_ids(&[FUNCTION]),
        functions: vec![[0, PUB]],
        attr_owners: vec![[0, 0, 1, NONE]],
        ..Columns::default()
    };
    tree(&w, [names, paths, vis, attrs, cfgs], &columns)
}

/// Every other kind: a static in an extern block, an extern crate renamed, a trait, a type
/// alias, a union, an enum, both kinds of macro, an outline module and a `use a::{b, *}`.
fn every_kind() -> Valid {
    let mut w = Words::default();
    let names = w.at();
    let all = ["ex", "St", "Tr", "Ty", "U", "E", "mr", "m2", "core", "alias"];
    w.u32(all.len() as u32);
    all.into_iter().for_each(|name| w.name(name));
    let paths = w.at();
    w.u32(1);
    plain(&mut w, "ex");
    let [vis, attrs, cfgs] = empty_tables::<3>(&mut w);
    let kinds = vec![
        STATIC,
        EXTERN_BLOCK,
        EXTERN_CRATE,
        TRAIT,
        TYPE_ALIAS,
        UNION,
        ENUM,
        MACRO_RULES,
        MACRO2,
        MOD,
        USE,
    ];
    let columns = Columns {
        top_level: (1..=10).collect(),
        ast_ids: ast_ids(&kinds),
        kinds,
        data: vec![0; 11],
        children: vec![0],
        enums: vec![[5, PUB]],
        extern_blocks: vec![[0, 1]],
        extern_crates: vec![[8, 9, PRIV_IMPLICIT]],
        macro2s: vec![[7, PUB]],
        macro_rules: vec![6],
        mods: vec![[0, PUB, OUTLINE, 0]],
        statics: vec![[1, PUB]],
        traits: vec![[2, PUB]],
        type_aliases: vec![[3, PUB_CRATE]],
        unions: vec![[4, PRIV_EXPLICIT]],
        uses: vec![[PUB, 2]],
        use_trees: vec![[SINGLE, 0, NONE, 0, 0], [GLOB, NONE, NONE, 0, 0], [PREFIXED, 0, NONE, 0, 2]],
        use_tree_lists: vec![0, 1],
        ..Columns::default()
    };
    tree(&w, [names, paths, vis, attrs, cfgs], &columns)
}

/// The empty tree, through the crate's own writer, and the hand-built ones.
fn valid() -> &'static [Valid] {
    static VALID: OnceLock<Vec<Valid>> = OnceLock::new();
    VALID.get_or_init(|| {
        let empty = Valid { bytes: blaze_format::to_bytes(&ItemTree::default()), columns: Vec::new() };
        vec![empty, items(), token_tree(), every_kind()]
    })
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_format::Aligned;

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    /// The hand-built trees are laid out as the writer lays them out, so the column edits keep
    /// reaching `decode`'s checks.
    #[test]
    fn valid_trees_round_trip() {
        for (at, valid) in super::valid().iter().enumerate() {
            let tree = super::decode(&Aligned::copy_from(&valid.bytes))
                .unwrap_or_else(|error| panic!("valid tree {at} does not decode: {error}"));
            assert!(blaze_format::to_bytes(&tree) == valid.bytes, "valid tree {at} is written as other bytes");
        }
    }
}
