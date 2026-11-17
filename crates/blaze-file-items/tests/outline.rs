//! blaze-file-items' interface: each item form on a fixture of real-looking Rust, the
//! disambiguator of equal impls, a file with syntax errors, and three properties over files
//! drawn from item snippets: an outline is a function of the text, a body edit changes only its
//! own item's hashes (and its ancestors' whole hashes), and text inserted at the top moves
//! nothing but the items' places, with the control its oracle must catch.

#![feature(allocator_api)]

use std::alloc::Global;
use std::fmt::Write as _;

use blaze_file_items::{
    AttrKind, ContentId, Edition, Item, ItemIndex, ItemKind, Outline, Range, Rename, Visibility,
    outline,
};
use blaze_sim::{Broke, Config, Tape};

fn parse(text: &str) -> Outline<Global> {
    outline(text, Edition::Edition2024, Global)
}

/// The first entry in source order whose name is `name`.
fn named<'o>(outline: &'o Outline<Global>, text: &str, name: &str) -> (ItemIndex, &'o Item) {
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
fn kind_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Fn { .. } => "fn",
        ItemKind::Struct => "struct",
        ItemKind::Enum => "enum",
        ItemKind::Union => "union",
        ItemKind::Trait => "trait",
        ItemKind::TraitAlias => "trait alias",
        ItemKind::Impl => "impl",
        ItemKind::Const { .. } => "const",
        ItemKind::Static { .. } => "static",
        ItemKind::TypeAlias => "type",
        ItemKind::Mod { .. } => "mod",
        ItemKind::Use { .. } => "use",
        ItemKind::ExternCrate { .. } => "extern crate",
        ItemKind::ExternBlock { .. } => "extern block",
        ItemKind::MacroRules { .. } => "macro_rules",
        ItemKind::MacroDef => "macro",
        ItemKind::MacroCall { .. } => "macro call",
        ItemKind::Variant { .. } => "variant",
        ItemKind::Field { .. } => "field",
        ItemKind::Error => "error",
        ItemKind::Unhandled { .. } => "unhandled",
    }
}

fn kinds(outline: &Outline<Global>, parent: Option<ItemIndex>) -> Vec<&'static str> {
    outline.children(parent).map(|(_, item)| kind_name(item.kind)).collect()
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
    let ItemKind::ExternCrate { rename: Some(Rename::Name(rename)) } = heap.kind else {
        panic!("`extern crate alloc as heap` renames: {:?}", heap.kind)
    };
    assert_eq!(heap.text(text, rename), "heap");
    assert_eq!(named(&outline, text, "core").1.kind, ItemKind::ExternCrate { rename: None });

    let uses: Vec<_> = outline
        .children(None)
        .filter(|(_, item)| matches!(item.kind, ItemKind::Use { .. }))
        .map(|(_, item)| item)
        .collect();
    let ItemKind::Use { tree } = uses[0].kind else { unreachable!("filtered to uses") };
    assert_eq!(text_of(text, uses[0], tree), "std::collections::{HashMap, BTreeMap}");
    assert_eq!(uses[0].text(text, uses[0].signature), "use std::collections::{HashMap, BTreeMap}");
    assert_eq!(uses[0].name, None);
    assert_eq!(uses[1].visibility, Visibility::Crate);

    let (_, add) = named(&outline, text, "add");
    assert_eq!(add.kind, ItemKind::Fn { body_holds_items: false });
    assert_eq!(add.visibility, Visibility::Pub);
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
    assert_eq!(declares.kind, ItemKind::Fn { body_holds_items: true });
    assert_eq!(declares.visibility, Visibility::Super);
    assert!(outline.items().iter().all(|item| item.name.is_none_or(|name| item.text(text, name) != "Local")));

    let (_, plain) = named(&outline, text, "plain");
    assert_eq!(plain.kind, ItemKind::Fn { body_holds_items: false });
    assert_eq!(plain.visibility, Visibility::Private);
    assert_eq!(plain.range.of(text), "fn plain() -> u8 { 1 }");

    let (point_at, point) = named(&outline, text, "Point");
    assert_eq!(point.kind, ItemKind::Struct);
    assert_eq!(point.text(text, point.signature), "pub struct Point<T> where T: Clone");
    let fields: Vec<_> = outline.children(Some(point_at)).map(|(_, field)| field).collect();
    let names: Vec<_> = fields.iter().map(|field| text_of(text, field, field.name)).collect();
    assert_eq!(names, ["x", "y", "z"]);
    assert_eq!(fields[0].visibility, Visibility::Pub);
    let ItemKind::Field { ty, default: None } = fields[0].kind else { panic!("{:?}", fields[0].kind) };
    assert_eq!(text_of(text, fields[0], ty), "T");
    let Visibility::In { path } = fields[1].visibility else { panic!("{:?}", fields[1].visibility) };
    assert_eq!(fields[1].text(text, path), "crate::shapes");
    assert_eq!(outline.attrs(fields[2]).iter().map(|attr| attr.kind).collect::<Vec<_>>(), [AttrKind::CfgAttr]);
    assert!(fields.iter().all(|field| field.parent == Some(point_at)));

    let (pair_at, _) = named(&outline, text, "Pair");
    let fields: Vec<_> = outline.children(Some(pair_at)).map(|(_, field)| field).collect();
    let shapes: Vec<_> = fields
        .iter()
        .map(|field| {
            let ItemKind::Field { ty, .. } = field.kind else { panic!("{:?}", field.kind) };
            (field.name, text_of(text, field, ty), field.visibility, field.disambiguator)
        })
        .collect();
    assert_eq!(shapes, [(None, "u8", Visibility::Pub, 0), (None, "u16", Visibility::Private, 1)]);

    let (shape_at, shape) = named(&outline, text, "Shape");
    assert_eq!(shape.kind, ItemKind::Enum);
    assert_eq!(kinds(&outline, Some(shape_at)), ["variant", "variant", "variant"]);
    let (circle_at, _) = named(&outline, text, "Circle");
    assert_eq!(kinds(&outline, Some(circle_at)), ["field"]);
    let (square_at, _) = named(&outline, text, "Square");
    assert_eq!(kinds(&outline, Some(square_at)), ["field"]);
    let (_, empty) = named(&outline, text, "Empty");
    let ItemKind::Variant { discriminant } = empty.kind else { panic!("{:?}", empty.kind) };
    assert_eq!(text_of(text, empty, discriminant), "3");

    let (bits_at, bits) = named(&outline, text, "Bits");
    assert_eq!(bits.kind, ItemKind::Union);
    assert_eq!(kinds(&outline, Some(bits_at)), ["field", "field"]);

    let (area_at, area) = named(&outline, text, "Area");
    assert_eq!(area.kind, ItemKind::Trait);
    assert_eq!(kinds(&outline, Some(area_at)), ["fn", "const", "type"]);
    let (_, area_fn) = outline.children(Some(area_at)).next().expect("the trait has items");
    assert_eq!(area_fn.body, None);
    assert_eq!(area_fn.text(text, area_fn.signature), "fn area(&self) -> f64");
    assert_eq!(named(&outline, text, "Both").1.kind, ItemKind::TraitAlias);

    let impls: Vec<_> = outline.children(None).filter(|(_, item)| item.kind == ItemKind::Impl).collect();
    let (impl_at, area_impl) = impls[0];
    assert_eq!(area_impl.name, None);
    assert_eq!(area_impl.text(text, area_impl.signature), "impl<T: Clone> Area for Point<T> where T: Copy");
    assert_eq!(kinds(&outline, Some(impl_at)), ["fn", "const", "type"]);
    let (_, impl_fn) = outline.children(Some(impl_at)).next().expect("the impl has items");
    assert_eq!(text_of(text, impl_fn, impl_fn.body), "{ 0.0 }");
    assert_eq!((impls[0].1.disambiguator, impls[1].1.disambiguator), (0, 0));

    let (_, limit) = named(&outline, text, "LIMIT");
    assert_eq!(limit.kind, ItemKind::Const { body_holds_items: false });
    assert_eq!(limit.text(text, limit.signature), "const LIMIT: usize");
    assert_eq!(text_of(text, limit, limit.body), "10");
    let (_, count) = named(&outline, text, "COUNT");
    assert_eq!(count.kind, ItemKind::Static { body_holds_items: false });
    assert_eq!(count.text(text, count.signature), "static mut COUNT: u32");
    let (_, map) = named(&outline, text, "Map");
    assert_eq!(map.kind, ItemKind::TypeAlias);
    assert_eq!(map.text(text, map.signature), "type Map<K> = HashMap<K, u32>");
    assert_eq!(map.body, None);

    let (_, place) = named(&outline, text, "place");
    let ItemKind::Mod { inline: false, path: Some(path) } = place.kind else { panic!("{:?}", place.kind) };
    assert_eq!(place.text(text, path), "\"other/place.rs\"");
    assert_eq!(place.visibility, Visibility::Pub);

    let (inner_at, inner) = named(&outline, text, "inner");
    assert_eq!(inner.kind, ItemKind::Mod { inline: true, path: None });
    let inner_attrs: Vec<_> = outline.attrs(inner).iter().map(|attr| (attr.inner, attr.kind)).collect();
    assert_eq!(inner_attrs, [(true, AttrKind::Other)]);
    assert_eq!(kinds(&outline, Some(inner_at)), ["struct", "mod"]);
    let (deeper_at, deeper) = named(&outline, text, "deeper");
    assert_eq!(deeper.visibility, Visibility::SelfOnly);
    assert_eq!(deeper.parent, Some(inner_at));
    let (leaf_at, leaf) = named(&outline, text, "leaf");
    assert_eq!(leaf.parent, Some(deeper_at));
    assert_eq!(leaf.range.of(text), "fn leaf() {}");
    assert!(leaf_at.0 < inner.end.0, "a module's descendants come before its end");

    let (block_at, block) = outline
        .children(None)
        .find(|(_, item)| matches!(item.kind, ItemKind::ExternBlock { .. }))
        .expect("the fixture has an extern block");
    let ItemKind::ExternBlock { abi } = block.kind else { unreachable!("found as an extern block") };
    assert_eq!(text_of(text, block, abi), "extern \"C\"");
    assert_eq!(kinds(&outline, Some(block_at)), ["fn", "static"]);
    assert_eq!(named(&outline, text, "abs").1.body, None);

    assert_eq!(named(&outline, text, "square").1.kind, ItemKind::MacroRules { exported: true });
    let (_, twice) = named(&outline, text, "twice");
    assert_eq!(twice.kind, ItemKind::MacroDef);
    assert_eq!(twice.text(text, twice.signature), "pub macro twice($e:expr)");
    assert_eq!(text_of(text, twice, twice.body), "{ $e + $e }");

    let (_, call) = outline.children(None).last().expect("the fixture has items");
    let ItemKind::MacroCall { path, input } = call.kind else { panic!("{:?}", call.kind) };
    assert_eq!(text_of(text, call, path), "thread_local");
    assert_eq!(text_of(text, call, input), "{ static DEPTH: u32 = 0; }");
    assert_eq!(call.text(text, call.signature), "thread_local!");
    assert_eq!(call.name, None);
}

/// A second `impl S {}` after the first is told apart by its disambiguator, 1, whatever its
/// whitespace; the first keeps 0, and an impl with another header counts apart.
#[test]
fn a_second_equal_impl_gets_disambiguator_one() {
    let one = "struct S;\nimpl S {}\nimpl Clone for S { fn clone(&self) -> S { S } }\n";
    let two = format!("{one}impl   S {{}}\n");
    let (first, second) = (parse(one), parse(&two));
    let impls = |outline: &Outline<Global>| -> Vec<(u32, ContentId)> {
        outline
            .children(None)
            .filter(|(_, item)| item.kind == ItemKind::Impl)
            .map(|(_, item)| (item.disambiguator, item.hash))
            .collect()
    };
    let (before, after) = (impls(&first), impls(&second));
    assert_eq!(before.iter().map(|it| it.0).collect::<Vec<_>>(), [0, 0]);
    assert_eq!(after.iter().map(|it| it.0).collect::<Vec<_>>(), [0, 0, 1]);
    assert_eq!(before[..], after[..2], "the first two impls keep their disambiguators and hashes");
    assert_eq!(after[0].1, after[2].1, "equal impls apart from whitespace hash alike");
}

/// A file with syntax errors still lists the items around them, the stray `;` as an error
/// entry, and each error at a range of the file.
#[test]
fn a_syntax_error_in_one_item_leaves_the_others() {
    let text = "fn before() {}\nfn broken() { let x = ; }\nstruct Missing { a: u8 b: u16 }\nfn after() {};\nfn last<T>(t: T) -> T { t }\n";
    let outline = parse(text);
    assert!(!outline.errors().is_empty(), "the fixture has syntax errors");
    for error in outline.errors() {
        assert!(error.range.end as usize <= text.len(), "{error:?} is a range of the file");
        assert!(!outline.message(error).is_empty());
    }
    for name in ["before", "broken", "after", "last"] {
        assert!(matches!(named(&outline, text, name).1.kind, ItemKind::Fn { .. }), "`{name}` is listed");
    }
    let (missing_at, missing) = named(&outline, text, "Missing");
    assert_eq!(missing.kind, ItemKind::Struct);
    let fields: Vec<_> =
        outline.children(Some(missing_at)).map(|(_, field)| text_of(text, field, field.name)).collect();
    assert_eq!(fields, ["a", "b"]);
    assert!(outline.items().iter().any(|item| item.kind == ItemKind::Error), "the stray `;` is an entry");
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
fn identities(outline: &Outline<Global>, plant: Plant) -> Vec<(ContentId, ContentId, u32)> {
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
