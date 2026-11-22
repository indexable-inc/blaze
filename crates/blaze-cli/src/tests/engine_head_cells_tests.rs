//! A HEAD CELL IS INJECTIVE OVER ONE ENTRY'S HEADS, AND EACH ENTRY IS A TASK OF ITS OWN (lane
//! RU3; engine_heads.rs `head_cell`, signature.rs `signature_name`). A child of main.rs's tests.
//!
//! THE CAUSE (found on `meters`, engine_scope_reads_tests.rs): `src/a.rs` holds `impl Pair` and
//! `impl crate::Measure for Pair`, two UNNAMED entries whose disambiguators are both 0 (each is
//! counted among the siblings of ITS head only, blaze-file-items digest.rs `sibling_keys`), and
//! `signature_name` hashed each entry's kind, name and disambiguator alone: the two impls were
//! one task name, so one head task held both impl headers, `(Impl, "", None)` each, under one
//! cell (engine_heads.rs `heads_of`: "two heads of its entry fall under one cell"), and every
//! body reading `impl Pair`'s header (`new`, `sum`, `measure`, `total`, `m`) was refused.
//!
//! The heads are modelled from the text here as engine_heads.rs `heads_of_crate` lists them from
//! the records (by_origin.rs `item_want`): an ADT, a trait, an impl header, a fn, const or
//! static, an alias each its own want; an impl's fn a `MemberFn` named by its impl and its own
//! name; a trait's members and an impl's consts and types no row (their container's head); an
//! `extern` block's foreign items each a row of the block's entry. Over the test's own frame.

use blaze_drive::Name;
use blaze_drive::cells::Cell;
use blaze_file_items::Outline;
use blaze_ids::ContentId;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_own::Alloc;
use blaze_surface::{Edition, Item, ItemIndex, ItemKind};

use crate::engine_heads::head_cell;
use crate::engine_tasks::WHOLE;
use crate::signature::{covering_entry, file_subject, is_signature_entry, signature_name, signature_name_before};

/// One file with every kind of head, and names shared across kinds and namespaces: a struct `T`
/// and a variant `T`; `fn a`, `const A` and `type A` in a trait (two namespaces); two inherent
/// impls of `T` (their headers alike: the disambiguator tells them); two impls of `Tr` (their
/// headers different: both unnamed, both disambiguator 0), each with `fn a`, `const A` and
/// `type A`; a module-level `fn a`; two `extern` blocks of different ABIs.
const TEXT: &str = "pub struct T {\n    pub a: u8,\n    pub b: u8,\n}\n\npub enum E {\n    A,\n    B,\n    T,\n}\n\npub trait Tr {\n    fn a(&self);\n    const A: u8;\n    type A;\n}\n\nimpl T {\n    pub fn a() {}\n    pub fn b() {}\n    pub const A: u8 = 0;\n}\n\nimpl T {\n    pub fn c() {}\n}\n\nimpl Tr for T {\n    fn a(&self) {}\n    const A: u8 = 1;\n    type A = u8;\n}\n\nimpl Tr for E {\n    fn a(&self) {}\n    const A: u8 = 2;\n    type A = u16;\n}\n\npub fn a() {}\n\npub type Alias = T;\n\nextern \"C\" {\n    fn ext();\n    static EXT: u8;\n}\n\nextern \"system\" {\n    fn ext_sys();\n}\n";

/// The two units that hold the file (a member's lib and its test harness): their crate prefixes.
const UNITS: [[u8; 8]; 2] = [[1, 1, 1, 1, 1, 1, 1, 1], [2, 2, 2, 2, 2, 2, 2, 2]];

/// How an entry's task is named: the rule ([`signature_name`]) or the plant.
type NameOf = fn(ContentId, &Outline<'_>, &Item, &str) -> Name;

/// Words in the test's frame.
type Words<'a> = blaze_own::String<Alloc<'a>>;

/// One head as its entry's task would list it: its entry's index and task name, its cell, and
/// its words (kind, item name, impl fn name, entry) for a failure.
struct Modelled<'a> {
    entry: ItemIndex,
    task: Name,
    cell: Cell,
    words: Words<'a>,
}

/// Entry `at`'s library key in `unit`: the unit's prefix, then the entry's index (unique in the
/// file, so two entries never share a key; the cell must tell them by their text alone).
fn key_of(unit: [u8; 8], at: ItemIndex) -> blaze_id::ItemKey {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&unit);
    bytes[8..12].copy_from_slice(&at.0.to_le_bytes());
    blaze_id::ItemKey(ContentId(bytes))
}

/// The name an entry's text gives it ("" for an unnamed one).
fn name_in<'t>(outline: &Outline<'_>, at: ItemIndex, text: &'t str) -> &'t str {
    let item = outline.item(at);
    item.name.map_or("", |range| item.text(text, range))
}

fn index(i: usize) -> ItemIndex {
    ItemIndex(u32::try_from(i).expect("an outline of a small text"))
}

/// Every head of `TEXT` under `unit`, its task named by `name_of` (the rule or the plant).
fn heads<'a>(outline: &Outline<'_>, unit: [u8; 8], name_of: NameOf, alloc: Alloc<'a>) -> blaze_own::Vec<Modelled<'a>, Alloc<'a>> {
    let file = file_subject("m/src/lib.rs");
    let mut heads = blaze_own::Vec::new_in(alloc);
    for (i, item) in outline.items().iter().enumerate() {
        let at = index(i);
        let name = name_in(outline, at, TEXT);
        let parent = item.parent.map(|up| (up, outline.item(up).kind()));
        let (want, item_name, member) = match (item.kind(), parent) {
            // An impl's fn: its own signature, named by its impl and its own name.
            (ItemKind::Function, Some((up, ItemKind::Impl))) => {
                let lib = key_of(unit, up);
                (Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, name)), name_in(outline, up, TEXT), Some(name))
            }
            // A trait's members, an impl's consts and types: their container's head.
            (_, Some((_, ItemKind::Trait | ItemKind::Impl))) => continue,
            (ItemKind::Struct | ItemKind::Enum | ItemKind::Union, _) => (Want::Adt(key_of(unit, at)), name, None),
            (ItemKind::Trait, _) => (Want::Members(key_of(unit, at)), name, None),
            (ItemKind::Impl, _) => (Want::Impl(key_of(unit, at)), name, None),
            (ItemKind::Function | ItemKind::Const | ItemKind::Static, _) => (Want::Fn(key_of(unit, at)), name, None),
            (ItemKind::TypeAlias | ItemKind::TraitAlias, _) => (Want::Head(key_of(unit, at)), name, None),
            _ => continue,
        };
        let Some(cover) = covering_entry(outline, at) else { continue };
        assert!(is_signature_entry(outline, outline.item(cover)), "a head's covering entry has a task: entry {}", at.0);
        let words = match member {
            Some(member) => blaze_own::text::format(alloc, format_args!("{:?} `{item_name}::{member}` at entry {}", item.kind(), at.0)),
            None => blaze_own::text::format(alloc, format_args!("{:?} `{item_name}` at entry {}", item.kind(), at.0)),
        };
        heads.push(Modelled { entry: cover, task: name_of(file, outline, outline.item(cover), TEXT), cell: head_cell(want, item_name, member, WHOLE), words });
    }
    heads
}

/// Each pair of heads under one (task, cell) in `heads`, in words: two heads of two entries
/// under one task name, or of one entry under one cell.
fn under_one_cell<'a>(heads: &[Modelled<'_>], alloc: Alloc<'a>) -> blaze_own::Vec<Words<'a>, Alloc<'a>> {
    let mut found = blaze_own::Vec::new_in(alloc);
    for (i, one) in heads.iter().enumerate() {
        for other in heads[i + 1..].iter() {
            if one.task == other.task && one.cell == other.cell {
                found.push(blaze_own::text::format(alloc, format_args!("{} and {} (entries {} and {})", one.words, other.words, one.entry.0, other.entry.0)));
            }
        }
    }
    found
}

/// Each pair of distinct entries with a task under one task name, in words.
fn entries_under_one_name<'a>(outline: &Outline<'_>, name_of: NameOf, alloc: Alloc<'a>) -> blaze_own::Vec<Words<'a>, Alloc<'a>> {
    let file = file_subject("m/src/lib.rs");
    let mut named: blaze_own::Vec<(Name, ItemIndex), Alloc<'a>> = blaze_own::Vec::new_in(alloc);
    for (i, item) in outline.items().iter().enumerate() {
        if is_signature_entry(outline, item) {
            named.push((name_of(file, outline, item, TEXT), index(i)));
        }
    }
    let shown = |at: ItemIndex| (outline.item(at).kind(), name_in(outline, at, TEXT), at.0);
    let mut found = blaze_own::Vec::new_in(alloc);
    for (i, one) in named.iter().enumerate() {
        for other in named[i + 1..].iter() {
            if one.0 == other.0 {
                let ((k1, n1, e1), (k2, n2, e2)) = (shown(one.1), shown(other.1));
                found.push(blaze_own::text::format(alloc, format_args!("{k1:?} `{n1}` (entry {e1}) and {k2:?} `{n2}` (entry {e2})")));
            }
        }
    }
    found
}

/// Each line of `lines`, printed under `what`.
fn print(what: &str, lines: &[Words<'_>]) {
    println!("{what}: {}", lines.len());
    for line in lines.iter() {
        println!("  {line}");
    }
}

/// THE RULE: every entry of `TEXT` with a task has a task name of its own, and within each
/// unit every head of one entry has a cell of its own (no two heads of a task under one cell);
/// one head's cells in two units differ (the unit is in the cell, lane RU1).
/// CONTROL (rule 23): with the old name (`signature_name_before`) the two impls of `Tr` (both
/// unnamed, both disambiguator 0) are one task name, their two fns `a` another, and the two
/// impl headers fall under one cell: the very collision `meters` printed; the test names them.
#[test]
fn engine_head_cells_are_one_a_head_and_entries_one_a_task() {
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let outline = blaze_file_items::outline(TEXT, Edition::E2021, alloc);

    let shared = entries_under_one_name(&outline, signature_name, alloc);
    print("entries under one task name", &shared);
    assert!(shared.is_empty(), "every entry with a task has a name of its own (printed above)");
    let lib = heads(&outline, UNITS[0], signature_name, alloc);
    let test = heads(&outline, UNITS[1], signature_name, alloc);
    for (unit, listed) in [(UNITS[0], &lib), (UNITS[1], &test)] {
        let collided = under_one_cell(listed, alloc);
        println!("unit {unit:?}: {} heads", listed.len());
        for one in listed.iter() {
            println!("  {}", one.words);
        }
        print("  under one cell", &collided);
        // 3 ADT and trait heads, 4 impl headers, 5 impl fns, a fn and an alias outside the
        // `extern` blocks (and their 3 foreign items).
        assert!(listed.len() >= 14, "unit {unit:?}: the text's heads are all modelled ({}, printed above)", listed.len());
        assert!(collided.is_empty(), "unit {unit:?}: no two heads of one entry under one cell (printed above)");
    }
    assert_eq!(lib.len(), test.len(), "both units list every head");
    for (one, other) in lib.iter().zip(test.iter()) {
        assert_eq!(one.words, other.words, "both units list the heads in one order");
        assert_ne!(one.cell, other.cell, "{}: one head's cells in two units differ", one.words);
        assert!(one.task == other.task, "{}: one entry is one task in both units", one.words);
    }

    // CONTROL: the old task name.
    let before = entries_under_one_name(&outline, signature_name_before, alloc);
    print("before: entries under one task name", &before);
    assert!(before.iter().any(|pair| pair.matches("Impl ``").count() == 2), "control: the two impls of `Tr` were one task name (printed above)");
    assert!(before.iter().any(|pair| pair.matches("Function `a`").count() == 2), "control: their fns `a` were one task name (printed above)");
    let collided = under_one_cell(&heads(&outline, UNITS[0], signature_name_before, alloc), alloc);
    print("before: under one cell", &collided);
    assert!(collided.iter().any(|pair| pair.matches("Impl ``").count() == 2), "control: the two impl headers fell under one cell (the `meters` failure; printed above)");
}
