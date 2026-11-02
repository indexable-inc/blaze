use std::{cell::Cell, panic, sync::Arc as StdArc};

use blaze_bytes::Bytes;
use blaze_query::db::Durability;
use span::Edition;
use syntax::{FlatTree, SourceFile};
use triomphe::Arc;
use crate::{FileId, FileSet, VfsPath};

use super::{Trees, write_section};
use crate::{Db, EditionedFileId, SourceRoot, SourceRootId, reparse::WHOLE};

const EDITION: Edition = Edition::Edition2021;

/// Texts with bodies of every kind: the parser's test data that parses, and one with errors in
/// its bodies. Each text once: the store keeps a tree per text, so two files of one text (the
/// fixtures have two empty ones) are one entry.
fn texts() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../parser/test_data/parser/ok");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|it| it.unwrap().path())
        .filter(|it| it.extension().is_some_and(|it| it == "rs"))
        .collect();
    paths.sort();
    let mut seen = std::collections::HashSet::new();
    let mut texts: Vec<(String, String)> = paths
        .iter()
        .map(|it| {
            let name = it.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(it).unwrap())
        })
        .filter(|(_, text)| seen.insert(text.clone()))
        .take(40)
        .collect();
    let broken = "fn f() { let x = ; }\nfn g() -> u8 { 1 + }\nstruct S { a: u8 }\n";
    texts.push(("broken.rs".to_owned(), broken.to_owned()));
    texts
}

/// `texts` as the files of one library source root of `db`.
fn library(db: &mut Db, texts: &[(String, String)]) -> Vec<EditionedFileId> {
    let mut set = FileSet::default();
    let mut ids = Vec::new();
    for (n, (name, text)) in texts.iter().enumerate() {
        let file = FileId::from_raw(n as u32);
        db.set_file_text_with_durability(file, text, Durability::HIGH);
        db.set_file_source_root_with_durability(file, SourceRootId(0), Durability::HIGH);
        set.insert(file, VfsPath::new_virtual_path(format!("/lib/{name}")));
        ids.push(EditionedFileId::new(db, file, EDITION));
    }
    let root = Arc::new(SourceRoot::new_library(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::HIGH);
    ids
}

fn tree(db: &Db, file: EditionedFileId) -> StdArc<FlatTree> {
    file.parse(db).syntax_node().flat_tree().expect("a flat tree").clone()
}

/// Every element a walk of the whole tree reaches, every body parsed, as kind and range.
fn elements(parse: &syntax::Parse<SourceFile>) -> Vec<(syntax::SyntaxKind, syntax::TextRange)> {
    parse.syntax_node().descendants_with_tokens().map(|it| (it.kind(), it.text_range())).collect()
}

/// The section `db` writes, read back as an image's would be.
fn stored(db: &Db) -> Trees {
    let mut out = Vec::new();
    write_section(db, &mut out);
    Trees::open(Bytes::from(StdArc::<[u8]>::from(out))).expect("the section just written")
}

fn whole_parses() -> usize {
    WHOLE.with(Cell::get)
}

/// A restart: library files parsed as outlines and some bodies walked, their trees stored and
/// the database dropped, then a new database over the same files reads every file's outline
/// from the store, with no whole parse, the bodies walked before parsed already, and each
/// file's parse the one a fresh parse makes (the same tree walked, the same errors).
#[test]
fn a_restart_reads_library_trees_back_without_parsing() {
    let texts = texts();
    let section = {
        let mut db = Db::for_tests();
        let files = library(&mut db, &texts);
        for &file in &files {
            assert!(file.parse(&db).is_outline(), "a library file's parse is an outline");
            let tree = tree(&db, file);
            for at in tree.lazies().step_by(2) {
                let _ = tree.part(at);
            }
        }
        stored(&db)
    };
    assert_eq!(section.len(), texts.len(), "every library file's outline stored");

    let mut db = Db::for_tests();
    db.set_trees(Some(StdArc::new(section)));
    let files = library(&mut db, &texts);
    let before = whole_parses();
    for (&file, (name, text)) in files.iter().zip(&texts) {
        let tree = tree(&db, file);
        assert!(tree.is_read_in_place(), "{name}: read from the store");
        let walked: Vec<u32> = tree.lazies().step_by(2).collect();
        assert!(walked.iter().all(|&at| tree.is_parsed(at)), "{name}: walked bodies stored");
        assert!(
            tree.lazies().filter(|at| !walked.contains(at)).all(|at| !tree.is_parsed(at)),
            "{name}: the rest as lazy as a fresh outline's"
        );
        assert_eq!(whole_parses(), before, "{name}: no lex and no parse of the file");
        let fresh = SourceFile::parse_outline(text.as_str(), EDITION);
        let parse = file.parse(&db);
        assert_eq!(parse, &fresh, "{name}");
        assert_eq!(parse.errors_with_bodies(), fresh.errors_with_bodies(), "{name}");
        assert_eq!(elements(parse), elements(&fresh), "{name}");
    }
    assert_eq!(db.trees().and_then(|it| it.failed()), None);
}

/// A file the store has no entry for is parsed as any other, and stored with the next section;
/// a stored entry no file read this time is carried into it as it was.
#[test]
fn a_missing_entry_is_parsed_and_stored_and_an_unread_one_is_kept() {
    let texts = texts();
    let (first, rest) = texts.split_at(texts.len() / 2);
    let section = {
        let mut db = Db::for_tests();
        let files = library(&mut db, first);
        files.iter().for_each(|&file| _ = file.parse(&db));
        stored(&db)
    };
    let mut db = Db::for_tests();
    db.set_trees(Some(StdArc::new(section)));
    let files = library(&mut db, &texts);
    let before = whole_parses();
    let (read, missing) = files.split_at(first.len());
    for &file in missing {
        assert!(!tree(&db, file).is_read_in_place(), "not stored: parsed");
    }
    assert_eq!(whole_parses(), before + rest.len(), "a whole parse per missing file");
    let _ = tree(&db, read[0]);
    let next = stored(&db);
    assert_eq!(next.len(), texts.len(), "the new parses stored, the unread entries kept");
    let mut again = Db::for_tests();
    again.set_trees(Some(StdArc::new(next)));
    let files = library(&mut again, &texts);
    let before = whole_parses();
    assert!(files.iter().all(|&file| tree(&again, file).is_read_in_place()));
    assert_eq!(whole_parses(), before, "every file read from the store");
}

/// A section of another parser version is refused whole, naming the versions.
#[test]
fn a_section_of_another_parser_version_is_refused() {
    let mut db = Db::for_tests();
    let files = library(&mut db, &texts()[..3]);
    files.iter().for_each(|&file| _ = file.parse(&db));
    let mut out = Vec::new();
    write_section(&db, &mut out);
    out[4..8].copy_from_slice(&(syntax::image::TREE_VERSION + 1).to_le_bytes());
    let refused = Trees::open(Bytes::from(StdArc::<[u8]>::from(out))).err().expect("refused");
    assert!(refused.0.contains("parser version"), "{refused}");
}

/// An entry whose record is not its text's parse stops the parse with an error naming the
/// file, never a parse of the file in its place, and is recorded so the next section carries
/// none of the section's entries.
#[test]
fn a_stored_tree_that_is_not_its_files_is_an_error_naming_the_file() {
    let texts = texts();
    let mut db = Db::for_tests();
    let files = library(&mut db, &texts[..2]);
    files.iter().for_each(|&file| _ = file.parse(&db));
    let mut out = Vec::new();
    write_section(&db, &mut out);
    // The first record's version, past the section's head.
    let at = super::HEAD + 8;
    out[at..at + 4].copy_from_slice(&(syntax::image::TREE_VERSION + 1).to_le_bytes());
    let trees = StdArc::new(Trees::open(Bytes::from(StdArc::<[u8]>::from(out))).unwrap());
    let mut db = Db::for_tests();
    db.set_trees(Some(StdArc::clone(&trees)));
    let files = library(&mut db, &texts[..2]);
    let before = whole_parses();
    let failed: Vec<String> = files
        .iter()
        .filter_map(|&file| {
            let ran = panic::catch_unwind(panic::AssertUnwindSafe(|| _ = file.parse(&db)));
            let payload = ran.err()?;
            let message = payload.downcast_ref::<String>().cloned();
            Some(message.unwrap_or_else(|| "a panic of no message".to_owned()))
        })
        .collect();
    assert_eq!(whole_parses(), before, "no file parsed in place of its stored tree");
    assert_eq!(failed.len(), 1, "the one corrupt entry: {failed:?}");
    assert!(failed[0].contains("another parser version"), "{}", failed[0]);
    assert!(failed[0].contains("/lib/"), "names the file: {}", failed[0]);
    assert!(trees.failed().is_some());
    let next = stored(&db);
    assert!(next.len() < 2, "a failed section's entries are not carried on");
}

/// Reading a stored parse allocates its tree's few owned parts (the lazy nodes' list and
/// slots, the kept hashes, the handles), not its columns: well under the 6.5 bytes an element
/// a copy of them would be.
#[test]
fn a_stored_parse_reads_in_place() {
    let texts = texts();
    let section = {
        let mut db = Db::for_tests();
        let files = library(&mut db, &texts);
        files.iter().for_each(|&file| _ = file.parse(&db));
        stored(&db)
    };
    let (name, text) = texts.iter().max_by_key(|it| it.1.len()).unwrap();
    let text = syntax::Text::from(text.as_str());
    let id = blaze_ids::ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes());
    let (read, used) = blaze_alloc_count::measure(|| section.map(id, &text, EDITION, true));
    let read = read.unwrap().expect("stored");
    let tree = read.syntax_node().flat_tree().unwrap().clone();
    assert!(tree.is_read_in_place(), "{name}");
    assert!(used.allocations <= 8, "{name}: {used:?}");
    let bound = 512 + 64 * tree.lazies().count() + text.len() / 16;
    assert!((used.bytes as usize) <= bound, "{name}: {used:?}, {} elements", tree.len());
}
