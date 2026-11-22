//! blaze-file-items' interface: each item form on a fixture of real-looking Rust, the
//! disambiguator of equal impls, a file with syntax errors, and three properties over files
//! drawn from item snippets: an outline is a function of the text, a body edit changes only its
//! own item's hashes (and its ancestors' whole hashes), and text inserted at the top moves
//! nothing but the items' places, with the control its oracle must catch.

#![feature(allocator_api)]

use std::fmt::Write as _;

use blaze_file_items::{Assembled, Bodies, Outline, outline, outline_item, outline_with, split_top_level};
use blaze_format::Aligned;
use blaze_surface::{
    AttrKind, ContentId, Edition, FileItemsView, Item, ItemDetail, ItemIndex, Range, Rename,
    WrittenVis,
};
use blaze_sim::{Broke, Config, Tape};

fn parse(text: &str) -> Outline<'static> {
    parse_as::<blaze_file_items::ByTokens>(text, Edition::E2024)
}

/// The outline of `text` in `edition`, each entry fed to its digests as `F` says
/// (`outline_fed_by`; [`parse`] is `ByTokens` in 2024, what `outline` makes).
fn parse_as<F: blaze_file_items::Feed>(text: &str, edition: Edition) -> Outline<'static> {
    blaze_file_items::outline_fed_by::<F>(text, edition, blaze_own::testing::heap().alloc())
}

/// Keys from tokens: what moves an entry's digests and what does not.
#[path = "outline/token_keys.rs"]
mod token_keys;

/// The first entry in source order whose name is `name`.
fn named<'o>(outline: &'o Outline<'_>, text: &str, name: &str) -> (ItemIndex, &'o Item) {
    outline
        .items()
        .iter()
        .enumerate()
        .find(|(_, item)| item.name.is_some_and(|it| item.text(text, it) == name))
        .map(|(at, item)| (ItemIndex(u32::try_from(at).expect("a small fixture")), item))
        .unwrap_or_else(|| panic!("no entry is named `{name}`"))
}

/// The text of one of `item`'s optional ranges, `None` as "".
fn text_of<'t>(text: &'t str, item: &Item, range: Option<Range>) -> &'t str {
    range.map_or("", |range| item.text(text, range))
}

/// The kind's name, for a sequence compared at once.
fn kind_name(kind: ItemDetail) -> &'static str {
    match kind {
        ItemDetail::Fn { .. } => "fn",
        ItemDetail::Struct => "struct",
        ItemDetail::Enum => "enum",
        ItemDetail::Union => "union",
        ItemDetail::Trait => "trait",
        ItemDetail::TraitAlias => "trait alias",
        ItemDetail::Impl => "impl",
        ItemDetail::Const { .. } => "const",
        ItemDetail::Static { .. } => "static",
        ItemDetail::TypeAlias => "type",
        ItemDetail::Mod { .. } => "mod",
        ItemDetail::Use { .. } => "use",
        ItemDetail::ExternCrate { .. } => "extern crate",
        ItemDetail::ExternBlock { .. } => "extern block",
        ItemDetail::MacroRules { .. } => "macro_rules",
        ItemDetail::MacroDef => "macro",
        ItemDetail::MacroCall { .. } => "macro call",
        ItemDetail::Variant { .. } => "variant",
        ItemDetail::Field { .. } => "field",
        ItemDetail::Error => "error",
        ItemDetail::Unhandled { .. } => "unhandled",
    }
}

fn kinds(outline: &Outline<'_>, parent: Option<ItemIndex>) -> Vec<&'static str> {
    outline.children(parent).map(|(_, item)| kind_name(item.detail)).collect()
}

const FORMS: &str = r#"//! A crate's root.
#![allow(dead_code)]

extern crate alloc as heap;
extern crate core;

use std::collections::{HashMap, BTreeMap};
pub(crate) use self::inner::Thing as Other;

/// Adds two numbers.
#[inline]
#[cfg(feature = "fast")]
pub fn add<T: core::ops::Add<Output = T>>(a: T, b: T) -> T where T: Copy {
    a + b
}

pub(super) fn declares() {
    struct Local;
    println!("{}", 1);
}

fn plain() -> u8 { 1 }

pub struct Point<T> where T: Clone {
    pub x: T,
    pub(in crate::shapes) y: T,
    #[cfg_attr(test, allow(unused))]
    z: T,
}

struct Pair(pub u8, u16);

pub enum Shape { Circle { r: f64 }, Square(f64), Empty = 3 }

union Bits { int: u32, float: f32 }

pub trait Area: Sized { fn area(&self) -> f64; const SIDES: u32; type Out; }

trait Both = Area + Clone;

impl<T: Clone> Area for Point<T> where T: Copy { fn area(&self) -> f64 { 0.0 } const SIDES: u32 = 0; type Out = (); }

impl Shape {}

const LIMIT: usize = 10;
static mut COUNT: u32 = 0;
type Map<K> = HashMap<K, u32>;

#[path = "other/place.rs"]
pub mod place;

mod inner {
    #![allow(unused)]
    pub struct Thing;
    pub(self) mod deeper { fn leaf() {} }
}

extern "C" {
    fn abs(x: i32) -> i32;
    static errno: i32;
}

#[macro_export]
macro_rules! square { ($x:expr) => { $x * $x }; }

pub macro twice($e:expr) { $e + $e }

thread_local! { static DEPTH: u32 = 0; }
"#;

/// Every item form of the fixture is listed with its kind, name, visibility and ranges, its
/// variants and fields as children, and nothing in a function body.
#[test]
fn each_item_form_has_its_kind_name_and_ranges() {
    let text = FORMS;
    let outline = parse(text);
    assert!(outline.errors().is_empty(), "the fixture parses: {:?}", outline.errors());
    assert_eq!(
        kinds(&outline, None),
        [
            "extern crate",
            "extern crate",
            "use",
            "use",
            "fn",
            "fn",
            "fn",
            "struct",
            "struct",
            "enum",
            "union",
            "trait",
            "trait alias",
            "impl",
            "impl",
            "const",
            "static",
            "type",
            "mod",
            "mod",
            "extern block",
            "macro_rules",
            "macro",
            "macro call",
        ]
    );
    let file_attrs: Vec<_> = outline.file_attrs().iter().map(|attr| (attr.inner, attr.kind)).collect();
    assert_eq!(file_attrs, [(true, AttrKind::Doc), (true, AttrKind::Other)]);

    let (_, heap) = named(&outline, text, "alloc");
    let ItemDetail::ExternCrate { rename: Rename::To(rename) } = heap.detail else {
        panic!("`extern crate alloc as heap` renames: {:?}", heap.detail)
    };
    assert_eq!(heap.text(text, rename), "heap");
    assert_eq!(named(&outline, text, "core").1.detail, ItemDetail::ExternCrate { rename: Rename::Same });

    let uses: Vec<_> = outline
        .children(None)
        .filter(|(_, item)| matches!(item.detail, ItemDetail::Use { .. }))
        .map(|(_, item)| item)
        .collect();
    let ItemDetail::Use { tree } = uses[0].detail else { unreachable!("filtered to uses") };
    assert_eq!(text_of(text, uses[0], tree), "std::collections::{HashMap, BTreeMap}");
    assert_eq!(uses[0].text(text, uses[0].signature), "use std::collections::{HashMap, BTreeMap}");
    assert_eq!(uses[0].name, None);
    assert_eq!(uses[1].visibility, WrittenVis::Crate);

    let (_, add) = named(&outline, text, "add");
    assert_eq!(add.detail, ItemDetail::Fn { body_holds_items: false });
    assert_eq!(add.visibility, WrittenVis::Pub);
    assert_eq!(
        add.text(text, add.signature),
        "/// Adds two numbers.\n#[inline]\n#[cfg(feature = \"fast\")]\npub fn add<T: core::ops::Add<Output = T>>(a: T, b: T) -> T where T: Copy"
    );
    assert_eq!(text_of(text, add, add.body), "{\n    a + b\n}");
    let add_attrs: Vec<_> = outline.attrs(add).iter().map(|attr| (attr.inner, attr.kind)).collect();
    assert_eq!(
        add_attrs,
        [(false, AttrKind::Doc), (false, AttrKind::Other), (false, AttrKind::Cfg)]
    );
    assert_eq!(add.text(text, outline.attrs(add)[2].range), "#[cfg(feature = \"fast\")]");

    let (_, declares) = named(&outline, text, "declares");
    assert_eq!(declares.detail, ItemDetail::Fn { body_holds_items: true });
    assert_eq!(declares.visibility, WrittenVis::Super);
    assert!(outline.items().iter().all(|item| item.name.is_none_or(|name| item.text(text, name) != "Local")));

    let (_, plain) = named(&outline, text, "plain");
    assert_eq!(plain.detail, ItemDetail::Fn { body_holds_items: false });
    assert_eq!(plain.visibility, WrittenVis::Private);
    assert_eq!(plain.range.of(text), "fn plain() -> u8 { 1 }");

    let (point_at, point) = named(&outline, text, "Point");
    assert_eq!(point.detail, ItemDetail::Struct);
    assert_eq!(point.text(text, point.signature), "pub struct Point<T> where T: Clone");
    let fields: Vec<_> = outline.children(Some(point_at)).map(|(_, field)| field).collect();
    let names: Vec<_> = fields.iter().map(|field| text_of(text, field, field.name)).collect();
    assert_eq!(names, ["x", "y", "z"]);
    assert_eq!(fields[0].visibility, WrittenVis::Pub);
    let ItemDetail::Field { ty, default: None } = fields[0].detail else { panic!("{:?}", fields[0].detail) };
    assert_eq!(text_of(text, fields[0], ty), "T");
    let WrittenVis::In(path) = fields[1].visibility else { panic!("{:?}", fields[1].visibility) };
    assert_eq!(fields[1].text(text, path), "crate::shapes");
    assert_eq!(outline.attrs(fields[2]).iter().map(|attr| attr.kind).collect::<Vec<_>>(), [AttrKind::CfgAttr]);
    assert!(fields.iter().all(|field| field.parent == Some(point_at)));

    let (pair_at, _) = named(&outline, text, "Pair");
    let fields: Vec<_> = outline.children(Some(pair_at)).map(|(_, field)| field).collect();
    let shapes: Vec<_> = fields
        .iter()
        .map(|field| {
            let ItemDetail::Field { ty, .. } = field.detail else { panic!("{:?}", field.detail) };
            (field.name, text_of(text, field, ty), field.visibility, field.disambiguator)
        })
        .collect();
    assert_eq!(shapes, [(None, "u8", WrittenVis::Pub, 0), (None, "u16", WrittenVis::Private, 1)]);

    let (shape_at, shape) = named(&outline, text, "Shape");
    assert_eq!(shape.detail, ItemDetail::Enum);
    assert_eq!(kinds(&outline, Some(shape_at)), ["variant", "variant", "variant"]);
    let (circle_at, _) = named(&outline, text, "Circle");
    assert_eq!(kinds(&outline, Some(circle_at)), ["field"]);
    let (square_at, _) = named(&outline, text, "Square");
    assert_eq!(kinds(&outline, Some(square_at)), ["field"]);
    let (_, empty) = named(&outline, text, "Empty");
    let ItemDetail::Variant { discriminant } = empty.detail else { panic!("{:?}", empty.detail) };
    assert_eq!(text_of(text, empty, discriminant), "3");

    let (bits_at, bits) = named(&outline, text, "Bits");
    assert_eq!(bits.detail, ItemDetail::Union);
    assert_eq!(kinds(&outline, Some(bits_at)), ["field", "field"]);

    let (area_at, area) = named(&outline, text, "Area");
    assert_eq!(area.detail, ItemDetail::Trait);
    assert_eq!(kinds(&outline, Some(area_at)), ["fn", "const", "type"]);
    let (_, area_fn) = outline.children(Some(area_at)).next().expect("the trait has items");
    assert_eq!(area_fn.body, None);
    assert_eq!(area_fn.text(text, area_fn.signature), "fn area(&self) -> f64");
    assert_eq!(named(&outline, text, "Both").1.detail, ItemDetail::TraitAlias);

    let impls: Vec<_> = outline.children(None).filter(|(_, item)| item.detail == ItemDetail::Impl).collect();
    let (impl_at, area_impl) = impls[0];
    assert_eq!(area_impl.name, None);
    assert_eq!(area_impl.text(text, area_impl.signature), "impl<T: Clone> Area for Point<T> where T: Copy");
    assert_eq!(kinds(&outline, Some(impl_at)), ["fn", "const", "type"]);
    let (_, impl_fn) = outline.children(Some(impl_at)).next().expect("the impl has items");
    assert_eq!(text_of(text, impl_fn, impl_fn.body), "{ 0.0 }");
    assert_eq!((impls[0].1.disambiguator, impls[1].1.disambiguator), (0, 0));

    let (_, limit) = named(&outline, text, "LIMIT");
    assert_eq!(limit.detail, ItemDetail::Const { body_holds_items: false });
    assert_eq!(limit.text(text, limit.signature), "const LIMIT: usize");
    assert_eq!(text_of(text, limit, limit.body), "10");
    let (_, count) = named(&outline, text, "COUNT");
    assert_eq!(count.detail, ItemDetail::Static { body_holds_items: false });
    assert_eq!(count.text(text, count.signature), "static mut COUNT: u32");
    let (_, map) = named(&outline, text, "Map");
    assert_eq!(map.detail, ItemDetail::TypeAlias);
    assert_eq!(map.text(text, map.signature), "type Map<K> = HashMap<K, u32>");
    assert_eq!(map.body, None);

    let (_, place) = named(&outline, text, "place");
    let ItemDetail::Mod { inline: false, path: Some(path) } = place.detail else { panic!("{:?}", place.detail) };
    assert_eq!(place.text(text, path), "\"other/place.rs\"");
    assert_eq!(place.visibility, WrittenVis::Pub);

    let (inner_at, inner) = named(&outline, text, "inner");
    assert_eq!(inner.detail, ItemDetail::Mod { inline: true, path: None });
    let inner_attrs: Vec<_> = outline.attrs(inner).iter().map(|attr| (attr.inner, attr.kind)).collect();
    assert_eq!(inner_attrs, [(true, AttrKind::Other)]);
    assert_eq!(kinds(&outline, Some(inner_at)), ["struct", "mod"]);
    let (deeper_at, deeper) = named(&outline, text, "deeper");
    assert_eq!(deeper.visibility, WrittenVis::SelfOnly);
    assert_eq!(deeper.parent, Some(inner_at));
    let (leaf_at, leaf) = named(&outline, text, "leaf");
    assert_eq!(leaf.parent, Some(deeper_at));
    assert_eq!(leaf.range.of(text), "fn leaf() {}");
    assert!(leaf_at.0 < inner.end.0, "a module's descendants come before its end");

    let (block_at, block) = outline
        .children(None)
        .find(|(_, item)| matches!(item.detail, ItemDetail::ExternBlock { .. }))
        .expect("the fixture has an extern block");
    let ItemDetail::ExternBlock { abi } = block.detail else { unreachable!("found as an extern block") };
    assert_eq!(text_of(text, block, abi), "extern \"C\"");
    assert_eq!(kinds(&outline, Some(block_at)), ["fn", "static"]);
    assert_eq!(named(&outline, text, "abs").1.body, None);

    assert_eq!(named(&outline, text, "square").1.detail, ItemDetail::MacroRules { exported: true });
    let (_, twice) = named(&outline, text, "twice");
    assert_eq!(twice.detail, ItemDetail::MacroDef);
    assert_eq!(twice.text(text, twice.signature), "pub macro twice($e:expr)");
    assert_eq!(text_of(text, twice, twice.body), "{ $e + $e }");

    let (_, call) = outline.children(None).last().expect("the fixture has items");
    let ItemDetail::MacroCall { path, input } = call.detail else { panic!("{:?}", call.detail) };
    assert_eq!(text_of(text, call, path), "thread_local");
    assert_eq!(text_of(text, call, input), "{ static DEPTH: u32 = 0; }");
    assert_eq!(call.text(text, call.signature), "thread_local!");
    assert_eq!(call.name, None);
}

/// A second `impl S {}` after the first is told apart by its disambiguator, 1; the first keeps
/// 0, and an impl with another header counts apart. A head is its TOKENS (lane K1; blaze-surface
/// file_items.rs, "Its tokens'"): `impl   S` is the same head as `impl S`, so written a second
/// time with other whitespace it is the second of its group, as a reformat must leave it.
#[test]
fn a_second_equal_impl_gets_disambiguator_one() {
    let one = "struct S;\nimpl S {}\nimpl Clone for S { fn clone(&self) -> S { S } }\n";
    let two = format!("{one}impl S {{}}\n");
    let spaced = parse(&format!("{one}impl   S {{}}\n"));
    let disambiguators = spaced.children(None).map(|(_, item)| item.disambiguator).collect::<Vec<_>>();
    assert_eq!(disambiguators, [0, 0, 0, 1], "a head with other whitespace is the same head: the second of its group");
    let (first, second) = (parse(one), parse(&two));
    let impls = |outline: &Outline<'_>| -> Vec<(u32, ContentId)> {
        outline
            .children(None)
            .filter(|(_, item)| item.detail == ItemDetail::Impl)
            .map(|(_, item)| (item.disambiguator, item.hash))
            .collect()
    };
    let (before, after) = (impls(&first), impls(&second));
    assert_eq!(before.iter().map(|it| it.0).collect::<Vec<_>>(), [0, 0]);
    assert_eq!(after.iter().map(|it| it.0).collect::<Vec<_>>(), [0, 0, 1]);
    assert_eq!(before[..], after[..2], "the first two impls keep their disambiguators and hashes");
    assert_eq!(after[0].1, after[2].1, "impls of equal bytes hash alike");
}

/// A file with syntax errors around items that parse.
const BROKEN: &str = "fn before() {}\nfn broken() { let x = ; }\nstruct Missing { a: u8 b: u16 }\nfn after() {};\nfn last<T>(t: T) -> T { t }\n";

/// A file with syntax errors still lists the items around them, the stray `;` as an error
/// entry, and each error at a range of the file.
#[test]
fn a_syntax_error_in_one_item_leaves_the_others() {
    let text = BROKEN;
    let outline = parse(text);
    assert!(!outline.errors().is_empty(), "the fixture has syntax errors");
    for error in outline.errors() {
        assert!(error.range.end as usize <= text.len(), "{error:?} is a range of the file");
        assert!(!outline.message(error).is_empty());
    }
    for name in ["before", "broken", "after", "last"] {
        assert!(matches!(named(&outline, text, name).1.detail, ItemDetail::Fn { .. }), "`{name}` is listed");
    }
    let (missing_at, missing) = named(&outline, text, "Missing");
    assert_eq!(missing.detail, ItemDetail::Struct);
    let fields: Vec<_> =
        outline.children(Some(missing_at)).map(|(_, field)| text_of(text, field, field.name)).collect();
    assert_eq!(fields, ["a", "b"]);
    assert!(outline.items().iter().any(|item| item.detail == ItemDetail::Error), "the stray `;` is an entry");
}

/// One item of a drawn file: a kind and one of three names, so names repeat.
#[derive(Clone, Copy, Debug)]
enum Snip {
    Fn(u8),
    Struct(u8),
    Enum(u8),
    Impl(u8),
    Mod(u8),
    Call(u8),
    Use(u8),
    Const(u8),
}

fn snip(tape: &Tape) -> Snip {
    let name = u8::try_from(tape.below(3, "name")).expect("a draw below 3 fits a u8");
    match tape.below(8, "kind") {
        0 => Snip::Fn(name),
        1 => Snip::Struct(name),
        2 => Snip::Enum(name),
        3 => Snip::Impl(name),
        4 => Snip::Mod(name),
        5 => Snip::Call(name),
        6 => Snip::Use(name),
        _ => Snip::Const(name),
    }
}

/// The two bodies a function of a drawn file has: before and after the edit.
const BODY: &str = "x";
const EDITED_BODY: &str = "let y = x; y";

fn render(snip: Snip, body: &str, out: &mut String) {
    match snip {
        Snip::Fn(n) => writeln!(
            out,
            "/// Doubles.\n#[inline]\npub fn f{n}<T: Clone>(x: T) -> T where T: Copy {{ {body} }}"
        ),
        Snip::Struct(n) => writeln!(out, "#[derive(Clone)]\npub struct S{n}<T> {{ pub a: T, b: u8 }}"),
        Snip::Enum(n) => writeln!(out, "enum E{n} {{ A, B(u8), C {{ x: u16 }} }}"),
        Snip::Impl(n) => writeln!(out, "impl<T> S{n}<T> {{ fn get(&self) -> u8 {{ self.b }} }}"),
        Snip::Mod(n) => writeln!(out, "mod m{n} {{\n    pub fn inner<T>(x: T) -> T {{ {body} }}\n}}"),
        Snip::Call(n) => writeln!(out, "m{n}! {{ {n} }}"),
        Snip::Use(n) => writeln!(out, "use std::collections::HashMap as H{n};"),
        Snip::Const(n) => writeln!(out, "const C{n}: u8 = {n};"),
    }
    .expect("writing to a String does not fail");
}

/// A drawn file, and which of its snippets is a function (or a module holding one) to edit.
#[derive(Debug)]
struct File {
    snips: Vec<Snip>,
    target: usize,
}

fn file(tape: &Tape) -> File {
    let mut snips = tape.vec(0..=8, "items", snip);
    let target = tape.index(snips.len() + 1, "target");
    let name = u8::try_from(tape.below(3, "target name")).expect("a draw below 3 fits a u8");
    snips.insert(target, if tape.coin("nested") { Snip::Mod(name) } else { Snip::Fn(name) });
    File { snips, target }
}

/// The file's text after `prefix`, the target's body edited or not, and where the target
/// starts.
fn text(file: &File, prefix: &str, edited: bool) -> (String, u32) {
    let mut out = String::from(prefix);
    let mut target_start = 0;
    for (at, &snip) in file.snips.iter().enumerate() {
        if at == file.target {
            target_start = u32::try_from(out.len()).expect("a small file");
        }
        render(snip, if edited && at == file.target { EDITED_BODY } else { BODY }, &mut out);
    }
    (out, target_start)
}

/// The invariant: an outline is a function of the text.
#[test]
fn an_outline_is_a_function_of_the_text() {
    assert_eq!(parse(FORMS), parse(FORMS));
    blaze_sim::check(Config::cases(200), |tape| {
        let (text, _) = text(&file(tape), "", false);
        match parse(&text) == parse(&text) {
            true => Ok(()),
            false => Err(Broke::Oracle(format!("two outlines of one text differ:\n{text}"))),
        }
    });
}

/// Whether the `FileItems` record of `text`'s outline answers what the outline answers: every
/// entry, its name's text, its attributes and children, the file's attributes, the errors and
/// their messages.
fn record_answers_as_outline(text: &str) -> Result<(), Broke> {
    let made = parse(text);
    let broke = |what: &str| Broke::Oracle(format!("{what}:\n{text}"));
    let bytes = made.to_file_items(text, ContentId([7; 32]), blaze_own::testing::heap().alloc()).map_err(|error| broke(&error.to_string()))?;
    let buffer = Aligned::copy_from(&bytes);
    let view = FileItemsView::read(&buffer).map_err(|error| broke(&error.to_string()))?;
    let entries = view.file() == ContentId([7; 32])
        && view.items().map(|entry| entry.item).eq(made.items().iter().copied())
        && view.items().all(|entry| entry.name == text_of(text, &entry, entry.item.name))
        && view.items().all(|entry| view.attrs(&entry).eq(made.attrs(&entry).iter().copied()));
    let parents = (0..made.items().len()).map(|at| Some(ItemIndex(u32::try_from(at).expect("a small file"))));
    let children = core::iter::once(None).chain(parents).all(|parent| {
        let (ours, theirs) = (view.children(parent), made.children(parent));
        ours.map(|(at, entry)| (at, entry.item)).eq(theirs.map(|(at, item)| (at, *item)))
    });
    let rest = view.file_attrs().eq(made.file_attrs().iter().copied())
        && view.flags() == made.flags()
        && view.own_flags() == made.own_flags()
        && view.errors().eq(made.errors().iter().copied())
        && made.errors().iter().all(|error| view.message(error) == made.message(error));
    match entries && children && rest {
        true => Ok(()),
        false => Err(broke("the record answers other than the outline")),
    }
}

/// The refinement: an outline's record answers what the outline answers, on the fixture of
/// every form, the file with syntax errors, and drawn files.
#[test]
fn the_record_answers_what_the_outline_answers() {
    for text in [FORMS, BROKEN] {
        record_answers_as_outline(text).expect("the record of a fixture answers as its outline");
    }
    blaze_sim::check(Config::cases(200), |tape| record_answers_as_outline(&text(&file(tape), "", false).0));
}

/// Each entry's flags are its own tokens', in a body the skeleton leaves unlexed too (`m`'s,
/// whose bytes hold a `#` and `unsafe` but no call or item keyword), whichever way the file is
/// outlined, and the record rolls them up.
#[test]
fn each_entry_has_the_flags_of_its_own_tokens() {
    const TEXT: &str = "#![no_std]
mod a {
    pub unsafe fn f() {}
    fn g() { let s = \"unsafe { asm!() }\"; /* extern \"C\" {} */ }
}
extern \"C\" { fn h(); }
#[derive(Clone)] struct S { x: u8 }
fn k() { unsafe { core::arch::asm!(\"nop\") } }
fn m() { #[allow(unused)] let y = unsafe { 1 }; }
fn n() { let r#unsafe = 1; let unsafe_code = r#\"unsafe\"#; }
";
    use blaze_surface::ItemFlags as F;
    let want = [
        ("a", F::NONE),
        ("f", F::UNSAFE),
        ("g", F::NONE),
        ("h", F::NONE),
        ("S", F::MACRO_CALL),
        ("x", F::NONE),
        ("k", F::UNSAFE | F::ASM),
        ("m", F::UNSAFE),
        ("n", F::NONE),
    ];
    for bodies in [None, Some(Bodies::Skipped), Some(Bodies::Parsed)] {
        let made = match bodies {
            None => parse(TEXT),
            Some(bodies) => outline_with(TEXT, Edition::E2024, bodies, blaze_own::testing::heap().alloc()),
        };
        for (name, flags) in want {
            assert_eq!(named(&made, TEXT, name).1.flags, flags, "{name}'s flags, outlined by {bodies:?}");
        }
        let block = made.items().iter().find(|item| matches!(item.detail, ItemDetail::ExternBlock { .. }));
        assert_eq!(block.map(|item| item.flags), Some(F::FOREIGN), "the extern block's flags, by {bodies:?}");
        assert_eq!(made.own_flags(), F::NONE);
        assert_eq!(made.flags(), F::UNSAFE | F::FOREIGN | F::ASM | F::MACRO_CALL);
    }
    let made = parse(TEXT);
    let bytes = made.to_file_items(TEXT, ContentId([7; 32]), blaze_own::testing::heap().alloc()).expect("the outline's record");
    let buffer = Aligned::copy_from(&bytes);
    let view = FileItemsView::read(&buffer).expect("a record just written");
    let flagged: Vec<_> = view.flagged().map(|(at, flags)| (view.item(at).expect("an entry").name.to_owned(), flags)).collect();
    let want = want.iter().filter(|(_, flags)| !flags.is_empty()).map(|(name, flags)| (name.to_string(), *flags));
    let mut want: Vec<_> = want.collect();
    want.insert(1, (String::new(), F::FOREIGN));
    assert_eq!(flagged, want, "the record's flagged entries, in order");
    assert_eq!(blaze_surface::flags_of_files([&view]), made.flags());
    let clean = parse("fn f() { let x = \"unsafe\"; }");
    assert!(clean.flags().is_empty() && clean.items().iter().all(|item| item.flags.is_empty()));
}

/// Whether the outline that skips bodies equals the one that parses them, apart from the
/// syntax errors the parse of a skipped body finds, which are not the skipping outline's.
fn skipping_equals_parsing(what: &str, text: &str) -> Result<(), String> {
    let skipped = outline_with(text, Edition::E2024, Bodies::Skipped, blaze_own::testing::heap().alloc());
    let parsed = outline_with(text, Edition::E2024, Bodies::Parsed, blaze_own::testing::heap().alloc());
    let first = |ours: &[Item], theirs: &[Item]| ours.iter().zip(theirs).position(|(a, b)| a != b);
    if skipped.items() != parsed.items() {
        let at = first(skipped.items(), parsed.items());
        return Err(format!(
            "{what}: skipping bodies gives other entries than parsing them: {} against {}, first at {at:?}: {:?} against {:?}",
            skipped.items().len(),
            parsed.items().len(),
            at.and_then(|at| skipped.items().get(at)),
            at.and_then(|at| parsed.items().get(at)),
        ));
    }
    let attrs = |o: &Outline<'_>| o.items().iter().map(|item| o.attrs(item).to_vec()).collect::<Vec<_>>();
    if attrs(&skipped) != attrs(&parsed) || skipped.file_attrs() != parsed.file_attrs() {
        return Err(format!("{what}: skipping bodies gives other attributes than parsing them"));
    }
    // The flags of a body left unlexed are read from its tokens all the same.
    if skipped.flags() != parsed.flags() || skipped.own_flags() != parsed.own_flags() {
        return Err(format!("{what}: skipping bodies gives other file flags than parsing them"));
    }
    if parsed.errors().is_empty() && skipped != parsed {
        return Err(format!("{what}: with no syntax errors, the outlines still differ: {skipped:?} against {parsed:?}"));
    }
    // An error by where it is, whose and what it says: its message's place in the messages
    // moves when an earlier error is not the skipping outline's.
    let said = |o: &Outline<'_>| {
        o.errors().iter().map(|error| (error.range, error.origin, o.message(error).to_owned())).collect::<Vec<_>>()
    };
    let theirs = said(&parsed);
    if !said(&skipped).iter().all(|error| theirs.contains(error)) {
        return Err(format!("{what}: skipping bodies finds an error parsing them does not: {:?}", skipped.errors()));
    }
    Ok(())
}

/// The refinement: skipping `fn` bodies changes no entry, attribute or hash, on the fixtures,
/// drawn files, and every file of library/core.
#[test]
fn skipping_bodies_outlines_as_parsing_them() {
    for (what, text) in [("FORMS", FORMS), ("BROKEN", BROKEN)] {
        skipping_equals_parsing(what, text).unwrap_or_else(|why| panic!("{why}"));
        digests_are_the_definition(what, text).unwrap_or_else(|why| panic!("{why}"));
    }
    blaze_sim::check(Config::cases(200), |tape| {
        let (text, _) = text(&file(tape), "", false);
        skipping_equals_parsing("a drawn file", &text).map_err(Broke::Oracle)?;
        digests_are_the_definition("a drawn file", &text).map_err(Broke::Oracle)
    });
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library/core");
    let mut stack = vec![root.clone()];
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                skipping_equals_parsing(&path.display().to_string(), &text).unwrap_or_else(|why| panic!("{why}"));
                digests_are_the_definition(&path.display().to_string(), &text).unwrap_or_else(|why| panic!("{why}"));
                files += 1;
            }
        }
    }
    assert!(files > 250,"library/core under {} holds only {files} .rs files", root.display());
}

/// Whether the digests hashed side by side equal those hashed one by one (the whole outline,
/// sibling keys and disambiguators too), and each entry's digest and signature digest equal
/// the crate docs' definition ("Identity") computed here with one streaming hasher per entry,
/// from the deepest entry up, sharing no code with the crate's.
fn digests_are_the_definition(what: &str, text: &str) -> Result<(), String> {
    use blaze_file_items::{OneByOne, SideBySide, outline_hashed_by};
    let side = outline_hashed_by::<SideBySide>(text, Edition::E2024, Bodies::Skipped, blaze_own::testing::heap().alloc());
    let one = outline_hashed_by::<OneByOne>(text, Edition::E2024, Bodies::Skipped, blaze_own::testing::heap().alloc());
    if side != one {
        return Err(format!("{what}: the digests side by side are not those one by one"));
    }
    let bytes = text.as_bytes();
    let wide = |offset: u32| usize::try_from(offset).expect("a small file");
    // The text's tokens, lexed here.
    let lexed = parser::LexedStr::new(parser::Edition::Edition2024, text);
    let items = side.items();
    let mut want = vec![(ContentId([0; 32]), ContentId([0; 32])); items.len()];
    for at in (0..items.len()).rev() {
        let item = &items[at];
        let (start, end) = (item.range.start, item.range.end);
        // The crate docs' exception: an entry that calls a macro is fed by its bytes.
        let by_bytes = item.flags.contains(blaze_surface::ItemFlags::MACRO_CALL);
        let run = |digest: &mut blaze_ids::Hasher, from: u32, to: u32| {
            if by_bytes {
                digest.update(&[0]).part(&bytes[wide(from)..wide(to)]);
            } else {
                defined_tokens(digest, bytes, &lexed, wide(from), wide(to), wide(end));
            }
        };
        let signature_end = start + item.signature.end;
        let mut digest = blaze_ids::Hasher::new(blaze_file_items::SIGNATURE_DOMAIN);
        // Edition 2024's byte.
        digest.update(&[3]);
        let mut fed = start;
        let mut signature = None;
        let index = ItemIndex(u32::try_from(at).expect("a small outline"));
        for (child, inner) in side.children(Some(index)) {
            if signature.is_none() && signature_end <= inner.range.start {
                run(&mut digest, fed, signature_end);
                let cut = digest.finish();
                signature = Some(cut);
                digest = blaze_ids::Hasher::new(blaze_file_items::ITEM_DOMAIN);
                digest.id(&cut);
                fed = signature_end;
            }
            run(&mut digest, fed, inner.range.start);
            digest.update(&[1]).id(&want[child.at()].0);
            fed = inner.range.end;
        }
        let signature = match signature {
            Some(signature) => signature,
            None => {
                run(&mut digest, fed, signature_end);
                fed = signature_end;
                let cut = digest.finish();
                digest = blaze_ids::Hasher::new(blaze_file_items::ITEM_DOMAIN);
                digest.id(&cut);
                cut
            }
        };
        run(&mut digest, fed, end);
        want[at] = (digest.finish(), signature);
    }
    for (at, (item, (hash, signature))) in items.iter().zip(&want).enumerate() {
        if item.hash != *hash || item.signature_hash != *signature {
            return Err(format!("{what}: entry {at}'s digests are not the definition's"));
        }
    }
    let parts = split_top_level(text, blaze_own::testing::heap().alloc());
    let mut digests = vec![ContentId([0; 32]); parts.len()];
    blaze_file_items::part_digests::<SideBySide>(bytes, &parts, &mut digests);
    if !parts.iter().zip(&digests).all(|(range, digest)| blaze_file_items::part_digest(&bytes[range.to_usize()]) == *digest) {
        return Err(format!("{what}: the parts' digests side by side are not each part's"));
    }
    Ok(())
}

/// The crate docs' token run of `from..to` in an entry ending at `limit`, written from the docs
/// alone: the tag 2, the count (LEB128), per token the kind as a u16 with the top bit for joined,
/// and a lexeme's length (LEB128) and bytes unless it is punctuation. Joined: punctuation but no
/// delimiter, written right against punctuation or a lifetime that ends by `limit`. A token is
/// any lexed one but whitespace and a plain comment.
fn defined_tokens(digest: &mut blaze_ids::Hasher, bytes: &[u8], lexed: &parser::LexedStr<'_>, from: usize, to: usize, limit: usize) {
    use parser::SyntaxKind::{L_BRACK, L_CURLY, L_PAREN, LIFETIME_IDENT, R_BRACK, R_CURLY, R_PAREN};
    let token = |at: usize| (!lexed.kind(at).is_trivia()).then(|| (lexed.kind(at), lexed.text_range(at).start, lexed.text_range(at).end));
    let inside = |at: usize| token(at).filter(|&(_, start, end)| from <= start && end <= to);
    let leb = |digest: &mut blaze_ids::Hasher, mut value: usize| loop {
        let low = u8::try_from(value & 0x7F).expect("seven bits");
        value >>= 7;
        if value == 0 {
            digest.update(&[low]);
            break;
        }
        digest.update(&[low | 0x80]);
    };
    let left = |kind: parser::SyntaxKind| kind.is_punct() && !matches!(kind, L_PAREN | R_PAREN | L_CURLY | R_CURLY | L_BRACK | R_BRACK);
    digest.update(&[2]);
    leb(digest, (0..lexed.len()).filter(|&at| inside(at).is_some()).count());
    for (at, (kind, start, end)) in (0..lexed.len()).filter_map(|at| inside(at).map(|it| (at, it))) {
        // The raw next token: a token right against this one, or trivia, which parts them.
        let joined = (at + 1 < lexed.len()).then(|| token(at + 1)).flatten().is_some_and(|(next, next_start, next_end)| {
            left(kind) && next_start == end && next_end <= limit && (left(next) || next == LIFETIME_IDENT)
        });
        digest.update(&((kind as u16) | (u16::from(joined) << 15)).to_le_bytes());
        if !kind.is_punct() {
            leb(digest, end - start);
            digest.update(&bytes[start..end]);
        }
    }
}

/// Whether the outline made part by part (`outline`: `split_top_level`, each part walked into
/// the file's one outline), and the parts each outlined alone and put together by `Assembled`,
/// equal the walk of the whole file, and the parts cover the file in order.
fn parts_equal_whole(what: &str, text: &str) -> Result<usize, String> {
    let parts = split_top_level(text, blaze_own::testing::heap().alloc());
    let mut from = 0;
    for part in parts.iter() {
        if part.start != from || part.end < part.start {
            return Err(format!("{what}: the parts do not cover the file in order: {:?}", &parts[..]));
        }
        from = part.end;
    }
    if usize::try_from(from).expect("a small file") != text.len() {
        return Err(format!("{what}: the parts end at {from}, the file at {}", text.len()));
    }
    let whole = outline_with(text, Edition::E2024, Bodies::Skipped, blaze_own::testing::heap().alloc());
    let assembled = outline(text, Edition::E2024, blaze_own::testing::heap().alloc());
    if whole != assembled {
        let at = whole.items().iter().zip(assembled.items()).position(|(a, b)| a != b);
        return Err(format!(
            "{what}: {} parts outline other than the whole file: {} entries against {}, first at {at:?}: {:?} against {:?}; errors {:?} against {:?}",
            parts.len(),
            assembled.items().len(),
            whole.items().len(),
            at.and_then(|at| assembled.items().get(at)),
            at.and_then(|at| whole.items().get(at)),
            assembled.errors(),
            whole.errors(),
        ));
    }
    // The incremental path: each part outlined alone, put together by `Assembled`.
    let alone: Vec<Outline<'static>> =
        parts.iter().map(|part| outline_item(&text[part.to_usize()], Edition::E2024, blaze_own::testing::heap().alloc())).collect();
    let put = Assembled::new(parts.iter().zip(&alone).map(|(part, it)| (part.start, it)), blaze_own::testing::heap().alloc())
        .to_outline(blaze_own::testing::heap().alloc());
    if put != whole {
        return Err(format!("{what}: {} parts outlined alone and assembled are not the whole file's outline", parts.len()));
    }
    Ok(parts.len())
}

/// The composition: a file outlined part by part equals the whole file's walk, on the
/// fixtures, drawn files, and every file of library/.
#[test]
fn a_file_outlined_by_parts_equals_its_whole_walk() {
    for (what, text) in [("FORMS", FORMS), ("BROKEN", BROKEN)] {
        parts_equal_whole(what, text).unwrap_or_else(|why| panic!("{why}"));
    }
    blaze_sim::check(Config::cases(200), |tape| {
        let (text, _) = text(&file(tape), "", false);
        parts_equal_whole("a drawn file", &text).map(|_| ()).map_err(Broke::Oracle)
    });
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library");
    let mut stack = vec![root.clone()];
    let (mut files, mut parts) = (0usize, 0usize);
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                parts += parts_equal_whole(&path.display().to_string(), &text).unwrap_or_else(|why| panic!("{why}"));
                files += 1;
            }
        }
    }
    assert!(files > 3000, "library/ under {} holds only {files} .rs files", root.display());
    assert!(parts > 10 * files, "library/'s {files} files split into only {parts} parts");
}

/// The invariant: editing one function's body changes that entry's whole hash and no other
/// entry's hashes or disambiguators, but the whole hash of the module it is in.
#[test]
fn a_body_edit_changes_only_its_own_items_hashes() {
    blaze_sim::check(Config::cases(200), |tape| {
        let file = file(tape);
        let (before_text, start) = text(&file, "", false);
        let (after_text, _) = text(&file, "", true);
        let (before, after) = (parse(&before_text), parse(&after_text));
        if before.items().len() != after.items().len() {
            return Err(Broke::Oracle(format!("a body edit changed the number of entries:\n{after_text}")));
        }
        let Some((top, _)) = before.children(None).find(|(_, item)| item.range.start == start) else {
            return Err(Broke::Harness(format!("no entry starts at the target, {start}:\n{before_text}")));
        };
        let (edited, ancestors) = match file.snips[file.target] {
            Snip::Mod(_) => match before.children(Some(top)).next() {
                Some((inner, _)) => (inner, vec![top]),
                None => return Err(Broke::Harness(format!("the target module lists no fn:\n{before_text}"))),
            },
            _ => (top, Vec::new()),
        };
        for (at, (old, new)) in before.items().iter().zip(after.items()).enumerate() {
            let at = ItemIndex(u32::try_from(at).expect("a small file"));
            let whole_changes = at == edited || ancestors.contains(&at);
            if (old.hash != new.hash) != whole_changes
                || old.signature_hash != new.signature_hash
                || old.disambiguator != new.disambiguator
            {
                return Err(Broke::Oracle(format!(
                    "editing the body of entry {} changed entry {}: {old:?} became {new:?}",
                    edited.0, at.0
                )));
            }
        }
        Ok(())
    });
}

/// A bug planted in what the oracle reads: each hash mixed with the item's place in the file,
/// as a builder that hashed file positions would make it.
#[derive(Clone, Copy, Debug)]
enum Plant {
    None,
    HashOfFilePlace,
}

/// What must not move when text is inserted above an item: its hashes and disambiguator.
fn identities(outline: &Outline<'_>, plant: Plant) -> Vec<(ContentId, ContentId, u32)> {
    outline
        .items()
        .iter()
        .map(|item| {
            let hash = match plant {
                Plant::None => item.hash,
                Plant::HashOfFilePlace => ContentId::of(
                    "blaze-file-items test: a hash of the file place",
                    &[&item.hash.0[..], &item.range.start.to_le_bytes()[..]].concat(),
                ),
            };
            (hash, item.signature_hash, item.disambiguator)
        })
        .collect()
}

const PREFIXES: [&str; 5] = ["\n", "\n\n\n", "// a comment\n", "/* a block comment */\n", "   \n"];

/// Text inserted at the top of a file changes no entry's hashes or disambiguator, and moves
/// each entry by the text's length and nothing else.
fn insert_at_top_moves_only_places(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let file = file(tape);
    let prefix = *tape.pick(&PREFIXES, "prefix").expect("the prefixes are not empty");
    let (before_text, _) = text(&file, "", false);
    let (after_text, _) = text(&file, prefix, false);
    let (before, after) = (parse(&before_text), parse(&after_text));
    let (old, new) = (identities(&before, plant), identities(&after, plant));
    if old.len() != new.len() {
        return Err(Broke::Oracle(format!("inserting {prefix:?} changed the number of entries")));
    }
    if let Some(at) = old.iter().zip(&new).position(|(old, new)| old != new) {
        return Err(Broke::Oracle(format!(
            "inserting {prefix:?} at the top changed entry {at}'s identity: {:?} became {:?}",
            old[at], new[at]
        )));
    }
    let moved = u32::try_from(prefix.len()).expect("a short prefix");
    for (at, (old, new)) in before.items().iter().zip(after.items()).enumerate() {
        if (Item { range: old.range.shifted(moved), ..*old }) != *new {
            return Err(Broke::Oracle(format!(
                "inserting {prefix:?} moved entry {at} other than by its length: {old:?} became {new:?}"
            )));
        }
    }
    Ok(())
}

/// The invariant: a blank line or a comment inserted at the top of a file changes no entry's
/// hash, signature hash or disambiguator, and every item-relative range stays as it was.
#[test]
fn inserting_at_the_top_changes_no_hash_or_disambiguator() {
    blaze_sim::check(Config::cases(200), |tape| insert_at_top_moves_only_places(tape, Plant::None));
}

/// A hash that holds the item's place in the file moves when text is inserted above it; the
/// oracle must say so.
#[test]
fn the_oracle_catches_a_hash_of_file_positions() {
    blaze_sim::control(
        Config::cases(200),
        Plant::None,
        Plant::HashOfFilePlace,
        "changed entry",
        insert_at_top_moves_only_places,
    );
}

/// The surface is a digest of what a file's entries name, not of their types
/// (docs/v2/plan/design/read-addressed.md, "End state: what an edit costs", piece 2; the list of
/// what the name solver reads is in src/names_digest.rs). A parameter's type, a field's, a
/// where clause, a body and a comment name nothing: the surface stays equal, while the signature
/// digest of `fn f(x: u8)` against `fn f(x: u16)` moves. Each edit that can change a name moves
/// it: a rename; `pub`; an attribute (the Reference, "Attributes": `cfg` can remove the item,
/// `derive` adds items); a variant (a name in the type and value namespaces: the Reference,
/// "Enumerations"); `struct S;` to `struct S {}` (the Reference, "Structs": a unit or tuple
/// struct also defines a constant or constructor in the value namespace, a braced one does not);
/// a member; a `use`; a `macro_rules!` body (its expansion's items). Under a derive or an
/// attribute macro the whole item is the macro's input, so a field's type there, or a body,
/// moves it. A wrong fix fails here either way: the old surface (signature digests) moves on the
/// type edits; a digest of the name alone stays on the attribute, form and macro edits.
///
/// Prints `names digest: 6 edits kept the surface, 11 moved it`.
#[test]
fn a_type_edit_keeps_the_surface_and_a_name_edit_moves_it() {
    let (before, after) = (parse("fn f(x: u8) {}\n"), parse("fn f(x: u16) {}\n"));
    assert_eq!(before.surface(), after.surface(), "a parameter's type moved the surface");
    assert_ne!(
        before.items()[0].signature_hash,
        after.items()[0].signature_hash,
        "a parameter's type left the signature digest equal"
    );
    let keeps: [(&str, &str, &str); 5] = [
        ("a comment and a blank line above an item", "struct S;\nfn f() {}\n", "struct S;\n\n// a note\nfn f() {}\n"),
        ("a body edit", "fn f() -> u8 { 1 }\n", "fn f() -> u8 { 2 + 3 }\n"),
        ("a field's type", "struct S { a: u8 }\n", "struct S { a: u16 }\n"),
        ("a where clause", "fn f<T>(t: T) {}\n", "fn f<T>(t: T) where T: Copy {}\n"),
        (
            "a body under a tool's attribute on an impl's member",
            "struct S;\nimpl S {\n    #[tracing::instrument]\n    fn a(&self) { x() }\n}\n",
            "struct S;\nimpl S {\n    #[tracing::instrument]\n    fn a(&self) { y() }\n}\n",
        ),
    ];
    let moves: [(&str, &str, &str); 11] = [
        ("renaming `f`", "fn f(x: u8) {}\n", "fn g(x: u8) {}\n"),
        ("adding `pub`", "fn f(x: u8) {}\n", "pub fn f(x: u8) {}\n"),
        ("adding `#[derive(Clone)]` to a struct", "struct S { a: u8 }\n", "#[derive(Clone)]\nstruct S { a: u8 }\n"),
        ("adding a variant to an enum", "enum E { A, B }\n", "enum E { A, B, C }\n"),
        ("turning `struct S;` into `struct S {}`", "struct S;\n", "struct S {}\n"),
        (
            "adding a method to an impl",
            "struct S;\nimpl S { fn a(&self) {} }\n",
            "struct S;\nimpl S { fn a(&self) {} fn b(&self) {} }\n",
        ),
        ("adding a `use`", "fn f() {}\n", "use core::mem;\nfn f() {}\n"),
        (
            "changing a `macro_rules!` body",
            "macro_rules! m { () => { struct A; }; }\n",
            "macro_rules! m { () => { struct B; }; }\n",
        ),
        ("adding `#[cfg(test)]` to an item", "fn f() {}\n", "#[cfg(test)]\nfn f() {}\n"),
        (
            "a field's type under a derive",
            "#[derive(Clone)]\nstruct S { a: u8 }\n",
            "#[derive(Clone)]\nstruct S { a: u16 }\n",
        ),
        ("a body under an attribute macro", "#[tokio::main]\nfn f() { a() }\n", "#[tokio::main]\nfn f() { b() }\n"),
    ];
    for (what, old, new) in keeps {
        assert_eq!(parse(old).surface(), parse(new).surface(), "{what} moved the surface:\n{old}\nto\n{new}");
    }
    for (what, old, new) in moves {
        assert_ne!(parse(old).surface(), parse(new).surface(), "{what} left the surface equal:\n{old}\nto\n{new}");
    }
    println!("names digest: {} edits kept the surface, {} moved it", keeps.len() + 1, moves.len());
}

/// `text` lexed and written as a token record, every token made (an expansion's form, with no
/// source): what `outline_token_record` reads.
fn token_record_of(text: &str) -> Aligned {
    let alloc = blaze_own::testing::heap().alloc();
    let stream = blaze_expand::lex(text, Edition::E2024, alloc).expect("the fixture lexes");
    let origins = vec![blaze_expand::TokenOrigin::Made(blaze_expand::Transparency::Transparent); stream.len()];
    let record = blaze_expand::token_record(&stream, &origins, &blaze_expand::OriginMaps::NONE, alloc)
        .expect("a lexed fixture makes a token record");
    Aligned::copy_from(&record)
}

/// Each named entry as its name and its parent's name, in order; then, for every entry, its
/// parent's index and its descendants' end.
type Shape = (Vec<(String, Option<String>)>, Vec<(Option<u32>, u32)>);

fn text_shape(text: &str) -> (Shape, bool) {
    let outline = parse(text);
    let items = outline.items();
    let name = |item: &Item| item.name.map(|it| item.text(text, it).to_owned());
    let named = items
        .iter()
        .filter(|item| item.name.is_some())
        .map(|item| (name(item).expect("named"), item.parent.and_then(|p| name(&items[p.at()]))))
        .collect();
    let nesting = items.iter().map(|item| (item.parent.map(|p| p.0), item.end.0)).collect();
    ((named, nesting), outline.errors().is_empty())
}

fn token_shape(text: &str) -> Shape {
    let bytes = token_record_of(text);
    let tokens = blaze_tokens::Tokens::open(&bytes).expect("the record opens");
    let outline = blaze_file_items::outline_token_record(tokens, blaze_own::testing::heap().alloc())
        .expect("a whole record has an outline");
    let entries = outline.entries();
    let name = |entry: &blaze_file_items::TokenEntry| outline.name(entry, tokens).map(str::to_owned);
    let named = entries
        .iter()
        .filter(|entry| entry.name.is_some())
        .map(|entry| (name(entry).expect("named"), entry.parent.and_then(|p| name(&entries[p as usize]))))
        .collect();
    let nesting = entries.iter().map(|entry| (entry.parent, entry.end)).collect();
    (named, nesting)
}

/// For every fixture of these tests, the outline of its token record names the same items
/// with the same nesting as the outline of its text: every entry's parent and extent in
/// entries where the text parses, the named entries and their parents' names where it does not
/// (the text outline lists an unreadable stretch as an error entry; the token outline lists
/// what the grammar made). A token outline that took a name from a rendered text, counted an
/// item inside a body, or lost a list's nesting fails. Prints "token outline: N fixtures, M
/// named entries as the text outline's".
#[test]
fn a_token_outline_names_the_items_of_the_text_outline() {
    let fixtures = [
        FORMS,
        BROKEN,
        "fn k() { unsafe { core::arch::asm!(\"nop\") } }\nfn m() { #[allow(unused)] let y = unsafe { 1 }; }\nfn n() { let r#unsafe = 1; let unsafe_code = r#\"unsafe\"#; }\n",
        "mod a { /// one\n pub struct S { pub x: u8, y: (u16, u32) } enum E { A(u8), B { c: u8 } } }\nimpl a::S { fn new() -> Self { loop {} } }\ntrait T { type U; const C: u8; fn f(&self); }\nextern \"C\" { fn g(); static H: u8; }\nmacro_rules! m { () => {} }\nm!();\n",
    ];
    let mut named = 0;
    for text in fixtures {
        let ((text_named, text_nesting), parses) = text_shape(text);
        let (token_named, token_nesting) = token_shape(text);
        assert_eq!(token_named, text_named, "the named entries of:\n{text}");
        if parses {
            assert_eq!(token_nesting, text_nesting, "the nesting of:\n{text}");
        }
        named += text_named.len();
    }
    println!("token outline: {} fixtures, {named} named entries as the text outline's", fixtures.len());
}

/// Each entry's token digest in order.
fn token_digests(text: &str) -> Vec<ContentId> {
    let bytes = token_record_of(text);
    let tokens = blaze_tokens::Tokens::open(&bytes).expect("the record opens");
    let outline = blaze_file_items::outline_token_record(tokens, blaze_own::testing::heap().alloc())
        .expect("a whole record has an outline");
    outline.entries().iter().map(|entry| entry.digest).collect()
}

/// An entry's token digest is of its tokens: spaces and comments of the text they came from
/// leave every digest equal, a changed token changes its own entry's digest (and its
/// ancestors') and no other's, even when the change moves the other entry's tokens. Prints
/// "token digests: spaces and comments kept 4 digests, an edit to `f` moved 1 of 2".
#[test]
fn a_token_digest_is_of_its_entrys_tokens_alone() {
    let tight = "fn f() { a + b } // c\nstruct S { x: u8, y: Vec<u8> }\n";
    let loose = "fn  f( ) {\n    a+b\n}\n/* other */ struct S{x:u8,y:Vec< u8 > }";
    let (a, b) = (token_digests(tight), token_digests(loose));
    assert_eq!(a.len(), 4, "f, S and its two fields");
    assert_eq!(a, b, "spaces and comments moved a digest");
    let before = token_digests("fn f() { 1 }\nfn g() { 2 }\n");
    let after = token_digests("fn f() { 1 + 1 }\nfn g() { 2 }\n");
    assert_ne!(before[0], after[0], "an edit to `f` left its digest");
    assert_eq!(before[1], after[1], "an edit to `f` moved `g`'s digest");
    println!("token digests: spaces and comments kept {} digests, an edit to `f` moved 1 of 2", a.len());
}
