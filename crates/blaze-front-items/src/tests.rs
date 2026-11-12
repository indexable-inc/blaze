//! The directed cases of `items(file)`: each test states the rule the design's review requires
//! (end-state.md, review point 4) and fails when the rule does not hold.

use std::{collections::BTreeMap, format, string::String, vec::Vec};

use syntax::{Edition, Extensions, SourceFile};

use crate::{
    entry::{self, Body, CFG, CFG_ATTR, DECLARED, EXPORTED, Entry, Gate, ItemId, Kind, UNSPLIT, body_key, key_item},
    items::{FileItems, ITEMS, file_items},
    lines::{self, ItemLines, LINES},
    parse::{Reading, parse, tree},
    table::{Family, Key, Table},
};

pub(crate) fn items_of(text: &str) -> FileItems {
    let parsed = SourceFile::parse(text, Edition::CURRENT);
    file_items(parsed.root()).expect("a file's items")
}

/// The parts of a table by item: under each 16-byte key its part (an entry, a list, a line
/// row), which is what it derefs to, and apart the body's part of each item that has one.
pub(crate) struct Rows {
    heads: BTreeMap<Key, Vec<u8>>,
    pub(crate) bodies: BTreeMap<Key, Vec<u8>>,
}

impl std::ops::Deref for Rows {
    type Target = BTreeMap<Key, Vec<u8>>;

    fn deref(&self) -> &BTreeMap<Key, Vec<u8>> {
        &self.heads
    }
}

/// The rows of a table whose keys are items' ids and their bodies' keys.
pub(crate) fn rows(family: Family, bytes: &[u8]) -> Rows {
    let table = Table::read(family, bytes).expect("a table this crate wrote");
    let mut rows = Rows { heads: BTreeMap::new(), bodies: BTreeMap::new() };
    for row in table.iter() {
        let (key, part) = row.expect("a whole table");
        let (id, body) = key_item(key).unwrap_or_else(|| panic!("a key of {} bytes", key.len()));
        let into = if body { &mut rows.bodies } else { &mut rows.heads };
        assert!(into.insert(id.0, part.to_vec()).is_none());
    }
    rows
}

fn differing(a: &BTreeMap<Key, Vec<u8>>, b: &BTreeMap<Key, Vec<u8>>, out: &mut Vec<Key>) {
    out.extend(a.iter().filter(|(key, part)| b.get(*key) != Some(*part)).map(|(key, _)| *key));
    out.extend(b.keys().filter(|key| !a.contains_key(*key)).copied());
}

/// The items one of whose parts (its entry, its body) differs between two tables, an item in
/// only one of them included.
pub(crate) fn changed(a: &Rows, b: &Rows) -> Vec<Key> {
    let mut out: Vec<Key> = Vec::new();
    differing(&a.heads, &b.heads, &mut out);
    differing(&a.bodies, &b.bodies, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

/// The body's part of the item `id` (its syntax record), none where it has no body.
pub(crate) fn body_of(items: &FileItems, id: ItemId) -> Option<Vec<u8>> {
    let table = Table::read(ITEMS, &items.items).expect("an items table");
    let part = table.get(&body_key(id)).expect("a whole table")?;
    Body::read(part).expect("a body");
    Some(part.to_vec())
}

/// The line row of the item `id`.
pub(crate) fn lines_of(items: &FileItems, id: ItemId) -> ItemLines<'_> {
    let table = Table::read(LINES, &items.lines).expect("a line table");
    ItemLines::read(table.get(&id.0).expect("a whole table").expect("the item's row")).expect("a row")
}

/// The id of the one item of `kind` named `name`.
pub(crate) fn id_of(items: &FileItems, kind: Kind, name: &str) -> ItemId {
    let table = rows(ITEMS, &items.items);
    let mut found = table.iter().filter(|(key, _)| !ItemId(**key).is_list()).filter(|(_, bytes)| {
        let entry = Entry::read(bytes).expect("an entry");
        entry.kind == kind && entry.name == name
    });
    let (key, _) = found.next().unwrap_or_else(|| panic!("no {kind:?} named `{name}`"));
    assert!(found.next().is_none(), "two {kind:?} named `{name}`");
    ItemId(*key)
}

fn with_entry<T>(items: &FileItems, id: ItemId, read: impl FnOnce(Entry<'_>) -> T) -> T {
    let table = Table::read(ITEMS, &items.items).expect("an items table");
    let bytes = table.get(&id.0).expect("a whole table").unwrap_or_else(|| panic!("no entry under {id:?}"));
    read(Entry::read(bytes).expect("an entry"))
}

fn list_of(items: &FileItems, key: ItemId) -> Vec<ItemId> {
    let table = Table::read(ITEMS, &items.items).expect("an items table");
    let list = table.get(&key.0).expect("a whole table").expect("a list key is always there");
    entry::list(list).expect("a list").collect()
}

/// Fifty items of several kinds, each with a body where its kind has one.
fn fifty() -> String {
    let mut text = String::from("//! A file.\nuse std::fmt;\n\n");
    for n in 0..12 {
        text.push_str(&format!("/// Doc of f{n}.\npub fn f{n}(x: u32) -> u32 {{\n    x + {n}\n}}\n\n"));
        text.push_str(&format!("struct S{n} {{ a: u32 }}\n\nimpl S{n} {{\n    fn get(&self) -> u32 {{ self.a + {n} }}\n}}\n\n"));
        text.push_str(&format!("const C{n}: u32 = {n} * 2;\n\n"));
    }
    text.push_str("mod inner { pub fn deep() -> u8 { 7 } }\nmod declared;\n");
    text
}

/// The rule: ids are content-hashed, never ordinal. Inserting a fn before fifty items changes
/// no other item's id and no other item's entry; only the file's own entry (its list of
/// children) and the new item's are new.
#[test]
fn inserting_an_item_changes_no_other_items_id_or_entry() {
    let before = items_of(&fifty());
    let after = items_of(&fifty().replace("use std::fmt;\n", "use std::fmt;\nfn fresh() -> u8 { 0 }\n"));
    let (a, b) = (rows(ITEMS, &before.items), rows(ITEMS, &after.items));
    assert!(a.len() > 50, "the file has its fifty items: {}", a.len());
    let fresh = id_of(&after, Kind::Fn, "fresh");
    let mut expected = vec![ItemId::ROOT.0, fresh.0];
    expected.sort_unstable();
    assert_eq!(changed(&a, &b), expected, "only the file's entry and the new item's differ");
    // And removing it is the same edit read backwards.
    assert_eq!(changed(&b, &a), expected);
}

/// The rule: a comment or whitespace edit anywhere changes no entry, only the line part.
#[test]
fn a_comment_or_whitespace_edit_changes_no_entry() {
    let plain = "fn a(x: u32) -> u32 { x + 1 }\nstruct S { f: u8 }\nimpl S { fn m(&self) -> u8 { self.f } }\nmacro_rules! m { ($x:expr) => { $x + 1 }; }\n";
    let edited = "// a comment above\n\nfn a( x : u32 /* in the header */ ) -> u32 {\n    // in the body\n    x  +  1\n}\nstruct S { f: u8 } // after\nimpl S {\n\n    fn m( &self ) -> u8 { /* here */ self.f }\n}\nmacro_rules! m { ( $x:expr ) => { $x + 1 } ; }\n";
    let (a, b) = (items_of(plain), items_of(edited));
    assert_eq!(a.items, b.items, "every entry is byte-equal");
    assert_ne!(a.lines, b.lines, "the line part moved");
}

/// The rule: a body edit changes that item's entry alone, and in it the body hash alone.
#[test]
fn a_body_edit_changes_that_items_body_hash_only() {
    let before = items_of(&fifty());
    let after = items_of(&fifty().replace("x + 3\n", "x + 3 + 1\n"));
    let f3 = id_of(&before, Kind::Fn, "f3");
    assert_eq!(changed(&rows(ITEMS, &before.items), &rows(ITEMS, &after.items)), vec![f3.0]);
    let (a, b) = (rows(ITEMS, &before.items), rows(ITEMS, &after.items));
    assert_eq!(a.get(&f3.0), b.get(&f3.0), "the entry is the same bytes: the body is another part");
    let body = body_of(&before, f3);
    assert!(body.is_some() && body_of(&after, f3).is_some() && body_of(&after, f3) != body, "the body hash moved");
}

/// The rule: a header edit changes that item's header hash and leaves its body hash.
#[test]
fn a_header_edit_changes_the_header_hash_and_not_the_body() {
    let before = items_of(&fifty());
    let after = items_of(&fifty().replace("pub fn f3(x: u32) -> u32", "pub fn f3(x: u32) -> u64"));
    let f3 = id_of(&before, Kind::Fn, "f3");
    assert_eq!(changed(&rows(ITEMS, &before.items), &rows(ITEMS, &after.items)), vec![f3.0]);
    let header = with_entry(&before, f3, |entry| entry.header);
    with_entry(&after, f3, |entry| assert_ne!(entry.header, header));
    let (a, b) = (rows(ITEMS, &before.items), rows(ITEMS, &after.items));
    assert_eq!(a.bodies, b.bodies, "no body's part moved, the edited item's included");
}

const NESTED: &str = "
fn outer() -> u8 {
    struct Local;
    impl Local { fn m(&self) -> u8 { BODY } }
    #[macro_export]
    macro_rules! exported { () => { 1 } }
    Local.m()
}
const _: () = {
    impl Trait for Other { fn t(&self) {} }
};
";

/// The rule: items nested in bodies are taken out as their own entries, keyed under their
/// container; an `impl` in a fn body or in `const _: () = { .. }` is in the file's list of
/// impls, a `#[macro_export] macro_rules!` in a body in its list of exports; the container's
/// body hash covers their presence and not their content.
#[test]
fn items_in_bodies_are_taken_out_and_the_container_covers_only_their_presence() {
    let before = items_of(&NESTED.replace("BODY", "1"));
    let after = items_of(&NESTED.replace("BODY", "2"));
    let outer = id_of(&before, Kind::Fn, "outer");
    let local = id_of(&before, Kind::Struct, "Local");
    let method = id_of(&before, Kind::Fn, "m");
    let exported = id_of(&before, Kind::MacroRules, "exported");
    let anon = id_of(&before, Kind::Const, "_");
    let impls = list_of(&before, ItemId::IMPLS);
    assert_eq!(impls.len(), 2, "both impls are global for coherence");
    with_entry(&before, impls[0], |entry| {
        assert_eq!((entry.kind, entry.container), (Kind::Impl, outer));
        assert_eq!(entry.children().collect::<Vec<_>>(), vec![method]);
    });
    with_entry(&before, impls[1], |entry| assert_eq!((entry.kind, entry.container), (Kind::Impl, anon)));
    with_entry(&before, local, |entry| assert_eq!(entry.container, outer));
    with_entry(&before, exported, |entry| {
        assert!(entry.is(EXPORTED) && entry.is(UNSPLIT));
        assert_eq!(entry.container, outer);
    });
    assert_eq!(list_of(&before, ItemId::EXPORTS), vec![exported]);
    with_entry(&before, outer, |entry| {
        assert_eq!(entry.children().collect::<Vec<_>>(), vec![local, impls[0], exported]);
    });
    // The nested method's body edit reaches the method alone: not the impl, not `outer`.
    assert_eq!(changed(&rows(ITEMS, &before.items), &rows(ITEMS, &after.items)), vec![method.0]);
    // Taking a nested item away changes the container's body hash: it covered its presence.
    let without = items_of(&NESTED.replace("BODY", "1").replace("struct Local;", ""));
    assert_ne!(body_of(&without, outer), body_of(&before, outer));
}

/// The rule: an item under an attribute that is not built in is not split: its whole token
/// stream is one hash, the macro's input, and it is marked so. Built-in attributes, `cfg`, and
/// a `cfg_attr` that writes only built-in attributes leave the item split, with their gates.
#[test]
fn an_item_under_an_attribute_macro_is_one_hash() {
    let text = |body: &str| {
        format!(
            "#[async_trait]\nimpl T for S {{ async fn f(&self) -> u8 {{ {body} }} }}\n\
             #[tokio::test]\nasync fn t() {{}}\n\
             #[cfg_attr(feature = \"x\", instrument)]\nfn maybe() {{}}\n\
             #[cfg_attr(test, allow(dead_code))]\nfn split() {{}}\n\
             #[cfg(unix)]\n#[inline]\nfn gated() {{}}\n"
        )
    };
    let (before, after) = (items_of(&text("1")), items_of(&text("2")));
    let table = rows(ITEMS, &before.items);
    let unsplit: Vec<Key> = table
        .iter()
        .filter(|(key, _)| !ItemId(**key).is_list())
        .filter(|(_, bytes)| Entry::read(bytes).expect("an entry").is(UNSPLIT))
        .map(|(key, _)| *key)
        .collect();
    assert_eq!(unsplit.len(), 3, "the impl, `t` and `maybe`");
    assert!(list_of(&before, ItemId::IMPLS).is_empty(), "an unsplit impl is the macro's to make");
    for key in &unsplit {
        with_entry(&before, ItemId(*key), |entry| {
            assert!(!entry.has_body() && entry.children().len() == 0);
            assert!(entry.gates().any(|(class, _)| matches!(class, Gate::Macro | Gate::CfgAttr)));
        });
    }
    // The body edit inside the unsplit impl moves the impl's one hash, and nothing else.
    let moved = changed(&table, &rows(ITEMS, &after.items));
    assert_eq!(moved.len(), 1);
    with_entry(&before, ItemId(moved[0]), |entry| assert_eq!(entry.kind, Kind::Impl));
    with_entry(&before, id_of(&before, Kind::Fn, "split"), |entry| {
        assert!(entry.is(CFG_ATTR) && !entry.is(UNSPLIT) && entry.has_body());
    });
    with_entry(&before, id_of(&before, Kind::Fn, "gated"), |entry| {
        assert!(entry.is(CFG) && !entry.is(UNSPLIT));
        let gates: Vec<(Gate, String)> = entry.gates().map(|(class, tokens)| (class, tokens.text())).collect();
        assert_eq!(gates, vec![(Gate::Cfg, String::from("cfg ( unix )"))]);
    });
}

/// The rule: `mod` declarations, `use` trees, macro calls and macro definitions in item
/// position are entries; a declared module is in the file's list of them.
#[test]
fn mods_uses_and_macros_in_item_position_are_entries() {
    let items = items_of(
        "mod declared;\nmod inline { fn x() {} }\nuse std::fmt::{self, Debug};\nextern crate alloc;\n\
         thread_local! { static A: u8 = 1; }\nmacro_rules! rules { () => {} }\n\
         extern \"C\" { fn c_fn(x: u8); }\nfn f() { call!(); }\n",
    );
    let declared = id_of(&items, Kind::Mod, "declared");
    with_entry(&items, declared, |entry| assert!(entry.is(DECLARED)));
    assert_eq!(list_of(&items, ItemId::MODS), vec![declared]);
    let inline = id_of(&items, Kind::Mod, "inline");
    with_entry(&items, inline, |entry| assert!(!entry.is(DECLARED) && entry.children().len() == 1));
    with_entry(&items, id_of(&items, Kind::Fn, "x"), |entry| assert_eq!(entry.container, inline));
    id_of(&items, Kind::Use, "");
    id_of(&items, Kind::ExternCrate, "alloc");
    with_entry(&items, id_of(&items, Kind::MacroCall, "thread_local"), |entry| assert!(entry.is(UNSPLIT)));
    with_entry(&items, id_of(&items, Kind::MacroRules, "rules"), |entry| assert!(!entry.is(EXPORTED)));
    let block = id_of(&items, Kind::ExternBlock, "\"C\"");
    with_entry(&items, id_of(&items, Kind::Fn, "c_fn"), |entry| assert_eq!(entry.container, block));
    // A macro call in expression position is its body's, not an entry.
    let calls = rows(ITEMS, &items.items)
        .iter()
        .filter(|(key, _)| !ItemId(**key).is_list())
        .filter(|(_, bytes)| Entry::read(bytes).expect("an entry").kind == Kind::MacroCall)
        .count();
    assert_eq!(calls, 1);
}

/// The rule: a macro's input is hashed with its spacing, since the macro can see it, and
/// without its whitespace otherwise.
#[test]
fn a_macros_input_keeps_spacing_and_drops_whitespace() {
    let header = |text: &str| {
        let items = items_of(text);
        with_entry(&items, id_of(&items, Kind::MacroCall, "m"), |entry| entry.header)
    };
    assert_eq!(header("m!(a == b);"), header("m!( a  ==  b ) ;"));
    assert_ne!(header("m!(a == b);"), header("m!(a = = b);"));
}

/// The rule: a variant's discriminant is a body of its own, under the enum: editing it moves
/// the variant's entry and not the enum's.
#[test]
fn a_discriminant_is_its_variants_body() {
    let before = items_of("enum E { A = 1 + 1, B(u8), C }");
    let after = items_of("enum E { A = 1 + 2, B(u8), C }");
    let a = id_of(&before, Kind::Variant, "A");
    with_entry(&before, a, |entry| assert_eq!(entry.container, id_of(&before, Kind::Enum, "E")));
    assert_eq!(lines_of(&before, a).body_span, (0, 3));
    assert_eq!(changed(&rows(ITEMS, &before.items), &rows(ITEMS, &after.items)), vec![a.0]);
}

/// The rule: positions are in the line part, by item, and the body's span is in the item's
/// own tokens: its first token and count find the body's bytes through the row's tokens.
#[test]
fn the_line_part_places_an_items_tokens() {
    let text = "// above\n\nfn f(x: u8) -> u8 {\n    x\n}\n";
    let items = items_of(text);
    let f = id_of(&items, Kind::Fn, "f");
    let table = Table::read(LINES, &items.lines).expect("a line table");
    let row = lines_of(&items, f);
    assert_eq!((row.start as usize, row.len as usize), (text.find("fn").unwrap(), "fn f(x: u8) -> u8 {\n    x\n}".len()));
    let tokens: Vec<u32> = row.tokens().collect();
    let (first, count) = row.body_span;
    with_entry(&items, f, |entry| assert!(entry.has_body()));
    assert_eq!(count, 3, "the braces and `x`");
    assert_eq!(tokens[first as usize] as usize, text.find('{').unwrap());
    let starts = table.get(&lines::LINE_STARTS).expect("a whole table").expect("the line starts");
    assert_eq!(lines::line_col(starts, tokens[first as usize + 1]).expect("line starts"), (3, 4));
}

/// The rule: bytes that are no table are refused naming the table asked for, never read as
/// empty. (The layout is the kernel's, and a table's meaning is its step's code: the kernel's
/// key of the value covers it.)
#[test]
fn bytes_that_are_no_table_are_refused_by_name() {
    let items = items_of("fn f() {}");
    let error = Table::read(ITEMS, &items.items[..8]).expect_err("eight bytes are no table");
    assert_eq!(error.family, "items", "{error}");
    let mut other = items.items.clone();
    other[0] ^= 1;
    assert!(Table::read(ITEMS, &other).is_err(), "another magic is refused");
}

/// The rule: `parse` is the tree as its stored record: read back over the same text it is the
/// same tree, and gives the same items; under another edition it is refused.
#[test]
fn a_tree_record_reads_back_to_the_same_items() {
    let text = fifty();
    let reading = Reading { edition: Edition::CURRENT, extensions: Extensions::NONE };
    let record = parse(&text, reading);
    assert_eq!(record, parse(&text, reading), "the record is a function of the text");
    let back = tree(&record, &text, reading).expect("its own record");
    assert_eq!(file_items(back.root()).expect("items"), items_of(&text));
    let other = Reading { edition: Edition::Edition2015, extensions: Extensions::NONE };
    assert!(tree(&record, &text, other).is_ok(), "a full parse's record names no edition");
}

/// The rule (end-state.md review point 4, "each entry holds the key of the body's syntax
/// record"): a body's part is its own tokens, so what reads a body reads that part and not the
/// file's tree; it holds no position, so it is byte-equal after an edit of another body, a
/// comment inside it, or an edit of its own header that moves it; and its tokens are placed
/// by the line row: the `n`th is the item's own token `body_span.0 + n`.
#[test]
fn a_bodys_record_is_its_tokens_and_moves_only_with_them() {
    let text = "fn g() {}\nfn f(x: u8) -> u8 {\n    let y = x;\n    fn inner() {}\n    y + 1\n}\n";
    let items = items_of(text);
    let f = id_of(&items, Kind::Fn, "f");
    let record = body_of(&items, f).expect("f has a body");
    let body = Body::read(&record).expect("a body");
    assert_eq!(body.kind, syntax::SyntaxKind::BLOCK_EXPR as u16);
    let inner = id_of(&items, Kind::Fn, "inner");
    let mut texts: Vec<String> = Vec::new();
    for piece in body.pieces() {
        match piece {
            entry::Piece::Token { text, .. } => texts.push(String::from(text)),
            entry::Piece::Child(id) => {
                assert_eq!(id, inner, "the item taken out stands where it was");
                texts.push(String::from("<inner>"));
            }
        }
    }
    assert_eq!(texts, ["{", "let", "y", "=", "x", ";", "<inner>", "y", "+", "1", "}"]);
    // Placed by the line row: each token's start is its own token's in the row.
    let row = lines_of(&items, f);
    assert_eq!(row.body_span.1, body.tokens());
    let starts: Vec<u32> = row.tokens().skip(row.body_span.0 as usize).take(body.tokens() as usize).collect();
    let tokens: Vec<&str> = body
        .pieces()
        .filter_map(|piece| match piece {
            entry::Piece::Token { text, .. } => Some(text),
            entry::Piece::Child(_) => None,
        })
        .collect();
    for (start, token) in starts.iter().zip(&tokens) {
        assert!(text[*start as usize..].starts_with(token), "`{token}` at byte {start}");
    }
    // Byte-equal under edits that are not of its tokens.
    for edited in [
        text.replace("fn g() {}", "fn g() { let other = 1; }"),
        text.replace("let y = x;", "let y = /* a comment */ x; // and one more"),
        text.replace("fn f(x: u8) -> u8", "fn f(x: u8, more: &mut [u64]) -> u8"),
        text.replace("fn inner() {}", "fn inner() { 3 }"),
    ] {
        let after = items_of(&edited);
        assert_eq!(body_of(&after, f).as_deref(), Some(record.as_slice()), "after `{edited}`");
    }
    // And moved by an edit of them.
    let after = items_of(&text.replace("y + 1", "y + 2"));
    assert_ne!(body_of(&after, f).as_deref(), Some(record.as_slice()));
}

/// The rule (the known limit of index tie-breaks, closed for what is in an impl): an `impl`
/// written before others of the same self type, trait and gates moves those impls' ids (their
/// index among equals) and no id of an item in them, since an impl's items are scoped by what
/// names the impl, not by its id; their bodies' parts stay byte-equal. An impl of the same
/// type under another `cfg` is named apart, so it moves nothing.
#[test]
fn an_impl_inserted_before_its_equals_moves_no_item_in_them() {
    let text = "struct S;\nimpl S {\n    fn a(&self) -> u8 { 1 }\n}\nimpl S {\n    fn b(&self) -> u8 { 2 }\n}\n";
    let before = items_of(text);
    let (a, b) = (id_of(&before, Kind::Fn, "a"), id_of(&before, Kind::Fn, "b"));
    let impls = list_of(&before, ItemId::IMPLS);
    let edited = text.replace("struct S;\n", "struct S;\nimpl S {\n    fn c(&self) -> u8 { 3 }\n}\n");
    let after = items_of(&edited);
    assert_eq!(id_of(&after, Kind::Fn, "a"), a, "a method's id is not its impl's index");
    assert_eq!(id_of(&after, Kind::Fn, "b"), b);
    assert_eq!(body_of(&after, a), body_of(&before, a));
    assert_eq!(body_of(&after, b), body_of(&before, b));
    let moved = list_of(&after, ItemId::IMPLS);
    assert_eq!(moved.len(), 3);
    // The index is the tie-break left: the new impl took the first id, each impl after it
    // the next one's, and the last a new one.
    assert_eq!(moved[..2], impls[..]);
    assert!(!impls.contains(&moved[2]));
    with_entry(&after, a, |entry| assert_eq!(entry.container, moved[1], "a is in the second impl now written"));

    // Under another `cfg`, the new impl is named apart: no impl id moves either.
    let gated = text.replace("struct S;\n", "struct S;\n#[cfg(unix)]\nimpl S {\n    fn c(&self) -> u8 { 3 }\n}\n");
    let after = items_of(&gated);
    let moved = list_of(&after, ItemId::IMPLS);
    assert_eq!(moved[1..], impls[..], "the impls written after it keep their ids");
    assert_eq!((id_of(&after, Kind::Fn, "a"), id_of(&after, Kind::Fn, "b")), (a, b));

    // Two methods of one name in two impls of one name are told apart by index, as rustc
    // refuses them as duplicates unless a `cfg` names them apart, which it does here.
    let twice = "struct S;\n#[cfg(unix)]\nimpl S { fn new() {} }\n#[cfg(windows)]\nimpl S { fn new() {} }\n";
    let items = items_of(twice);
    let news: Vec<ItemId> = rows(ITEMS, &items.items)
        .iter()
        .filter(|(key, _)| !ItemId(**key).is_list())
        .filter(|(_, bytes)| Entry::read(bytes).expect("an entry").name == "new")
        .map(|(key, _)| ItemId(*key))
        .collect();
    assert_eq!(news.len(), 2, "two entries, two ids");
}
