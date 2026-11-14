//! How `items(file)` and `modules(crate)` are judged (end-state.md, piece 2 of the order):
//! against what the old front end sees of real files of this tree, and against the floor at
//! item grain for random edits.

use std::{collections::BTreeMap, format, path::Path, string::String, vec::Vec};

use blaze_report::{
    say,
    say::{Code, Words},
};
use blaze_sim::{Config, Tape};
use hir_expand::declarative::stored::index;
use span::AstIdMap;
use syntax::{Edition, Extensions, SourceFile, SyntaxKind};

use crate::{
    entry::{Entry, ItemId, Kind, UNSPLIT},
    items::{ITEMS, file_items, is_item},
    lines::{ItemLines, LINES},
    modules::{self, MODULES, Module, ModuleReads, Status},
    parse::Reading,
    table::{Key, Table},
    tests::{changed, id_of, items_of, rows},
};

fn kind_of(kind: SyntaxKind) -> Option<Kind> {
    Some(match kind {
        SyntaxKind::FN => Kind::Fn,
        SyntaxKind::CONST => Kind::Const,
        SyntaxKind::STATIC => Kind::Static,
        SyntaxKind::STRUCT => Kind::Struct,
        SyntaxKind::UNION => Kind::Union,
        SyntaxKind::ENUM => Kind::Enum,
        SyntaxKind::TRAIT => Kind::Trait,
        SyntaxKind::TYPE_ALIAS => Kind::TypeAlias,
        SyntaxKind::IMPL => Kind::Impl,
        SyntaxKind::MODULE => Kind::Mod,
        SyntaxKind::USE => Kind::Use,
        SyntaxKind::EXTERN_CRATE => Kind::ExternCrate,
        SyntaxKind::EXTERN_BLOCK => Kind::ExternBlock,
        SyntaxKind::MACRO_CALL => Kind::MacroCall,
        SyntaxKind::MACRO_RULES => Kind::MacroRules,
        SyntaxKind::MACRO_DEF => Kind::MacroDef,
        SyntaxKind::ASM_EXPR => Kind::Asm,
        _ => return None,
    })
}

/// Checks one real file; gives how many items were compared.
fn check_file(name: &str, text: &str) -> usize {
    let parsed = SourceFile::parse(text, Edition::CURRENT);
    let root = parsed.root();
    let made = file_items(root).unwrap_or_else(|what| panic!("{name}: {what}"));
    let (items, lines) = (rows(ITEMS, &made.items), rows(LINES, &made.lines));
    // Every entry but the file's and the variants', by where its node is.
    let mut mine: BTreeMap<(u32, u32), (Kind, bool, bool)> = BTreeMap::new();
    for (key, bytes) in items.iter().filter(|(key, _)| !ItemId(**key).is_list() && ItemId(**key) != ItemId::ROOT) {
        let entry = Entry::read(bytes).unwrap_or_else(|what| panic!("{name}: {what}"));
        let row = ItemLines::read(&lines[key]).unwrap_or_else(|what| panic!("{name}: {what}"));
        assert_eq!(
            entry.has_body(),
            items.bodies.contains_key(key),
            "{}",
            say!(Words(name), ": ", Words(&format!("{:?}", entry.kind)), " ", Code(entry.name)).flat()
        );
        if entry.kind != Kind::Variant {
            let old = mine.insert((row.start, row.len), (entry.kind, entry.is(UNSPLIT), entry.has_body()));
            assert!(old.is_none(), "{name}: two entries at byte {}", row.start);
        }
    }
    // The tree's own word on what is an item: every item-position node outside an unsplit
    // item is one entry of its kind, and there is no other entry.
    let mut unsplit_end = 0u32;
    let mut expected = 0;
    for node in root.descendants() {
        let (start, len): (u32, u32) = (node.text_range().start().into(), node.text_range().len().into());
        if start < unsplit_end || !is_item(node) {
            continue;
        }
        let Some((kind, unsplit, _)) = mine.get(&(start, len)) else {
            panic!("{name}: no entry for the {:?} at byte {start}", node.kind());
        };
        assert_eq!(Some(*kind), kind_of(node.kind()), "{name}: at byte {start}");
        if *unsplit {
            unsplit_end = start + len;
        }
        expected += 1;
    }
    assert_eq!(mine.len(), expected, "{name}: entries that are no item of the tree");
    // The old front end: every item it gives a row in its file index (the items outside
    // bodies) is an entry here with a body exactly when the old one has one, unless this one
    // is unsplit; and every entry outside a body is a node the old front end gives an id.
    let map = AstIdMap::from_view(root);
    let old = index::decode(&index::make(root, &map)).unwrap_or_else(|what| panic!("{name}: {what}"));
    let mut seen = 0;
    for (id, source) in old.rows() {
        let ptr = map.get_erased(id);
        let Some(kind) = kind_of(ptr.kind()) else { continue };
        let range = ptr.text_range();
        let at = (u32::from(range.start()), u32::from(range.len()));
        match mine.get(&at) {
            Some((mine_kind, unsplit, body)) => {
                assert_eq!(*mine_kind, kind, "{name}: at byte {}", at.0);
                assert!(*unsplit || *body == source.body.is_some(), "{name}: the body of the {kind:?} at byte {}", at.0);
                seen += 1;
            }
            // Inside an unsplit item the old index still has rows; here the macro makes them.
            None => assert!(
                mine.iter().any(|((start, len), (_, unsplit, _))| *unsplit && *start <= at.0 && at.0 + at.1 <= start + len),
                "{name}: the old front end's {kind:?} at byte {} has no entry",
                at.0
            ),
        }
    }
    let outside_bodies = root
        .descendants()
        .filter(|node| is_item(*node) && !node.ancestors().skip(1).any(|up| up.kind() == SyntaxKind::BLOCK_EXPR))
        .filter(|node| mine.contains_key(&(node.text_range().start().into(), node.text_range().len().into())))
        .count();
    assert_eq!(seen, outside_bodies, "{name}: entries outside bodies the old file index has no row for");
    expected
}

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).map(|e| e.expect("a dir entry").path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The refinement: over real code (every `.rs` file of `crates/syntax/src` and
/// `crates/blaze-record/src` of this tree, read now), `items(file)` has exactly the items the
/// tree has in item position, each of the kind the old front end's file index gives it, with a
/// body exactly where the old one has one.
#[test]
fn items_agree_with_the_old_front_end_on_real_files() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/");
    let mut files = Vec::new();
    rs_files(&crates.join("syntax/src"), &mut files);
    rs_files(&crates.join("blaze-record/src"), &mut files);
    assert!(files.len() > 20, "the corpus is there: {} files", files.len());
    let mut compared = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        compared += check_file(&path.display().to_string(), &text);
    }
    // A floor against a vacuous pass, not a count: the corpus is thousands of items.
    assert!(compared > 500, "the corpus has its items: {compared} compared");
}

/// The invariant (the floor, at item grain): for a random file and a random edit classified
/// as comment, body, header, insert or remove, the entries whose bytes changed are exactly
/// what the classification says: none; that item; that item; the file and the new item; the
/// file and the removed item.
#[test]
fn an_edit_changes_exactly_the_entries_its_class_says() {
    blaze_sim::check(Config::cases(200), edit_case);
}

/// One case of [`an_edit_changes_exactly_the_entries_its_class_says`].
fn edit_case(tape: &Tape) {
    let count = 1 + tape.int_in(0usize..=11, "items past the first");
    let mut pieces: Vec<String> = Vec::new();
    for n in 0..count {
        let value = tape.int_in(0u32..=9, "value");
        pieces.push(match tape.int_in(0u8..=3, "piece") {
            0 => format!("pub fn item{n}(x: u32) -> u32 {{\n    x + {value} + 7777\n}}\n"),
            1 => format!("struct Item{n} {{ a: u32 }}\nimpl Item{n} {{\n    fn item{n}(&self) -> u32 {{ self.a + {value} + 7777 }}\n}}\n"),
            2 => format!("const ITEM{n}: u32 = {value};\nfn item{n}() -> u32 {{ fn inner() -> u32 {{ {value} }} inner() + 7777 }}\n"),
            _ => format!("mod m{n} {{\n    pub fn item{n}() -> u32 {{ {value} + 7777 }}\n}}\n"),
        });
    }
    let before = items_of(&pieces.concat());
    let target = tape.index(count, "target");
    let name = format!("item{target}");
    let id = id_of(&before, Kind::Fn, &name);
    let mut edited = pieces.clone();
    let mut expected: Vec<Key> = match tape.int_in(0u8..=4, "edit class") {
        0 => {
            edited[target] = format!("// a comment\n{}\n\n", edited[target].replace(" + ", "  /* c */ +  "));
            Vec::new()
        }
        1 => {
            // `7777` is written in the fn's own body and nowhere else.
            edited[target] = edited[target].replace("7777", "7778");
            vec![id.0]
        }
        2 => {
            edited[target] = edited[target].replace(&format!("fn {name}("), &format!("fn {name}<T>("));
            vec![id.0]
        }
        3 => {
            let at = tape.int_in(0usize..=count, "insert at");
            edited.insert(at, String::from("fn fresh(y: u8) -> u8 { y }\n"));
            Vec::new()
        }
        _ => {
            // The fn alone: what it is nested in stays.
            let piece = &edited[target];
            let start = piece.find(&format!("fn {name}(")).expect("the fn");
            let start = piece[..start].rfind("pub ").filter(|at| piece[*at..start].trim() == "pub").unwrap_or(start);
            let open = start + piece[start..].find('{').expect("its body");
            let (mut depth, mut end) = (0, open);
            for (at, c) in piece[open..].char_indices() {
                depth += i32::from(c == '{') - i32::from(c == '}');
                if depth == 0 {
                    end = open + at + 1;
                    break;
                }
            }
            edited[target] = format!("{}{}", &piece[..start], &piece[end..]);
            vec![id.0]
        }
    };
    let after = items_of(&edited.concat());
    let (a, b) = (rows(ITEMS, &before.items), rows(ITEMS, &after.items));
    let moved = changed(&a, &b);
    // What an insert or a remove also changes: the one container whose children they are, and
    // the entries that are in only one of the two files (the item, and what is nested in it).
    let only: Vec<Key> = moved.iter().copied().filter(|key| a.contains_key(key) != b.contains_key(key)).collect();
    let both: Vec<Key> = moved.iter().copied().filter(|key| a.contains_key(key) && b.contains_key(key)).collect();
    if only.is_empty() {
        expected.sort_unstable();
        assert_eq!(moved, expected, "an edit in place moves exactly its item");
    } else {
        assert_eq!(both.len(), 1, "an insert or a remove moves one container's entry");
        let container = Entry::read(&a[&both[0]]).expect("an entry");
        assert!(matches!(container.kind, Kind::File | Kind::Mod | Kind::Impl | Kind::Fn));
        for key in &only {
            let entry = Entry::read(a.get(key).or(b.get(key)).expect("in one")).expect("an entry");
            assert!(
                entry.name == name || entry.name == "fresh" || entry.name == "inner",
                "{}",
                say!(Code(entry.name), " came or went").flat()
            );
        }
        assert!(expected.iter().all(|key| only.contains(key)), "the removed item is gone");
    }
}

/// A crate's files and cfg in memory, with every read counted by name.
#[derive(Default)]
struct Files {
    files: BTreeMap<&'static str, &'static str>,
    asked: Vec<String>,
}

impl ModuleReads for Files {
    type Error = String;

    fn part(&mut self, file: &str, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let items = items_of(self.files.get(file).ok_or_else(|| say!("no file ", Code(file)).flat())?).items;
        let table = Table::read(ITEMS, &items).map_err(|error| format!("{error}"))?;
        Ok(table.get(key).map_err(|error| format!("{error}"))?.map(<[u8]>::to_vec))
    }

    fn exists(&mut self, file: &str) -> Result<bool, String> {
        self.asked.push(String::from(file));
        Ok(self.files.contains_key(file))
    }

    fn cfg(&mut self, predicate: &str) -> Result<bool, String> {
        Ok(predicate != "off")
    }

    fn refused(what: String) -> String {
        what
    }
}

/// The rule: `modules(crate)` follows `mod name;` by rustc's rules (`#[path]`, inline modules,
/// a file that owns its directory or does not, `cfg`), and reads the existence of BOTH
/// `name.rs` and `name/mod.rs`, so creating the other one reaches it.
#[test]
fn modules_follow_declarations_and_read_both_candidates() {
    let mut world = Files::default();
    world.files.extend([
        ("src/lib.rs", "mod a;\nmod b;\n#[cfg(off)]\nmod gone;\nmod inl { mod d; }\n#[path = \"x/y.rs\"]\nmod e;\n#[cfg_attr(on, path = \"alt.rs\")]\nmod f;\nmod missing;\n"),
        ("src/a.rs", "mod sub;\n"),
        ("src/a/sub.rs", ""),
        ("src/b/mod.rs", "mod under;\n"),
        ("src/b/under.rs", ""),
        ("src/inl/d.rs", ""),
        ("src/x/y.rs", "mod z;\n"),
        ("src/x/z.rs", ""),
        ("src/alt.rs", ""),
    ]);
    let reading = Reading { edition: Edition::CURRENT, extensions: Extensions::NONE };
    let read = |world: &mut Files| {
        let bytes = modules::modules("src/lib.rs", reading, world).expect("modules");
        let table = Table::read(MODULES, &bytes).expect("a modules table");
        let mut out: Vec<(String, Status, String)> = table
            .iter()
            .map(|row| Module::read(row.expect("a whole table").1).expect("a module"))
            .map(|module| (String::from(module.path), module.status, String::from(module.file)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    let found = |path: &str, file: &str| (String::from(path), Status::Found, String::from(file));
    let first = read(&mut world);
    assert_eq!(
        first,
        vec![
            found("", "src/lib.rs"),
            found("a", "src/a.rs"),
            found("a::sub", "src/a/sub.rs"),
            found("b", "src/b/mod.rs"),
            found("b::under", "src/b/under.rs"),
            found("e", "src/x/y.rs"),
            found("e::z", "src/x/z.rs"),
            found("f", "src/alt.rs"),
            found("inl::d", "src/inl/d.rs"),
            (String::from("missing"), Status::Missing, String::from("src/missing.rs")),
        ]
    );
    for both in ["src/a.rs", "src/a/mod.rs", "src/b.rs", "src/b/mod.rs"] {
        assert!(world.asked.iter().any(|asked| asked == both), "{}", say!(Code(both), " was read, there or not").flat());
    }
    world.files.insert("src/a/mod.rs", "");
    let second = read(&mut world);
    assert!(second.contains(&(String::from("a"), Status::Ambiguous, String::from("src/a.rs"))));
}
