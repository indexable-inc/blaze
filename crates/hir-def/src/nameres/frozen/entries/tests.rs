//! Tests of macro definitions kept as entries (`hir_expand::declarative::stored`, through
//! [`super::set`]): each with its control, the cause removed or the edit that must miss.

use base_db::{Crate, Db, EditionedFileId};
use hir_expand::{HirFileId, InFile, fixture};
use span::{ErasedFileAstId, FileAstId};
use syntax::{AstPtr, SyntaxKind, ast};

use super::{Counts, Memory, set};
use crate::{nameres::frozen::tests::crate_named, test_db::TestDBExt};

const MACROS: usize = 4;

/// A crate of [`MACROS`] macros, the third written as `third`, and a dependent.
fn text(third: &str) -> String {
    // alloc: a test's fixture
    format!(
        r#"
//- /main.rs crate:main deps:lib
fn main() {{}}
//- /lib.rs crate:lib
#[macro_export]
macro_rules! first {{ () => {{ 1 }}; ($x:expr) => {{ $x + 1 }} }}
#[macro_export]
macro_rules! second {{ ($($x:tt)*) => {{ [$($x),*] }} }}
#[macro_export]
macro_rules! third {{ {third} }}
#[macro_export]
macro_rules! fourth {{ ($a:ident, $b:literal) => {{ let $a = $b; }} }}
pub fn f() {{}}
"#
    )
}

const THIRD: &str = "() => { \"three\" }";
const THIRD_EDITED: &str = "() => { \"three\"; 3 }";

fn lib_file(db: &Db) -> (Crate, EditionedFileId) {
    let lib = crate_named(db, "lib");
    let data = lib.data(db);
    (lib, EditionedFileId::new_with_extensions(db, data.root_file_id, data.edition, lib.extensions(db)))
}

/// The ast ids of the macros of the lib, in the file's order: from a parse, so asked of a
/// database other than the one whose parses are counted. An ast id is made of what the item
/// is, so it is the same in every database of the same text.
fn macro_ids(db: &Db) -> Vec<u32> {
    let (_, file) = lib_file(db);
    let map = HirFileId::FileId(file).ast_id_map(db);
    map.ptrs()
        .filter(|ptr| ptr.kind() == SyntaxKind::MACRO_RULES)
        .map(|ptr| {
            let ptr = AstPtr::<ast::Macro>::try_from_raw(ptr).expect("a macro_rules is a macro");
            map.ast_id_for_ptr(ptr).erase().into_raw()
        })
        // alloc: a test's list
        .collect()
}

/// The expander of each macro of `ids`, as its `Debug` text: equal for equal expanders of two
/// databases of the same text.
fn expanders(db: &Db, ids: &[u32]) -> Vec<String> {
    let (lib, file) = lib_file(db);
    ids.iter()
        .map(|&raw| {
            let id = FileAstId::<ast::Macro>::from_erased_unchecked(ErasedFileAstId::from_raw(raw));
            let id = InFile::new(HirFileId::FileId(file), id);
            // alloc: a test's text
            format!("{:?}", id.decl_macro_expander(db, lib))
        })
        // alloc: a test's list
        .collect()
}

fn parsed(ran: &[String]) -> Vec<&String> {
    // alloc: a test's list
    ran.iter().filter(|it| it.contains("parse") || it.contains("outline")).collect()
}

/// The count: a database whose macro definitions are kept makes each expander with no parse of
/// the defining file, and they are the expanders a database makes from the source.
#[test]
fn a_kept_macro_definition_is_expanded_with_no_parse_of_its_file() {
    let text = text(THIRD);
    let from_source = fixture::db(&text).into_db();
    let ids = macro_ids(&from_source);
    assert_eq!(ids.len(), MACROS);
    let want = expanders(&from_source, &ids);

    let store = Memory::new();
    let mut writing = fixture::db(&text).into_db();
    set(&mut writing, store.clone());
    assert_eq!(expanders(&writing, &ids), want);
    let made = store.take();
    // The file's index and the tokens of each.
    assert_eq!((made.made, made.hits, made.again), (1 + MACROS as u64, 0, 0), "{made:?}");

    let mut reading = fixture::db(&text).into_db();
    set(&mut reading, store.clone());
    // alloc: a test's list
    let mut got = Vec::new();
    let ran = reading.executed_in_read_order(|| got = expanders(&reading, &ids));
    assert_eq!(got, want);
    assert_eq!(parsed(&ran), Vec::<&String>::new(), "all that ran: {ran:?}");
    let read = store.take();
    assert_eq!(read, Counts { hits: 1 + MACROS as u64, misses: 0, made: 0, again: 0 });
}

/// The control: the cause removed. The same asks of a database with no entry store parse the
/// defining file.
#[test]
fn without_kept_definitions_the_same_asks_parse_the_file() {
    let text = text(THIRD);
    let ids = macro_ids(&fixture::db(&text).into_db());
    let db = fixture::db(&text).into_db();
    let ran = db.executed_in_read_order(|| _ = expanders(&db, &ids));
    assert!(!parsed(&ran).is_empty(), "{ran:?}");
}

/// Rule 65 by counts: one macro of N edited. The file's index misses (the file's content is
/// new, and its one parse makes it again); the tokens of the N-1 others are found under
/// their own keys and not made again, and the edited one's are a miss and made: the control,
/// since a key that left the definition's hash out would find the old tokens for it.
#[test]
fn one_macro_of_n_edited_finds_the_tokens_of_the_others() {
    let store = Memory::new();
    let before = text(THIRD);
    let mut writing = fixture::db(&before).into_db();
    set(&mut writing, store.clone());
    let ids = macro_ids(&fixture::db(&before).into_db());
    _ = expanders(&writing, &ids);
    _ = store.take();

    let after = text(THIRD_EDITED);
    let ids = macro_ids(&fixture::db(&after).into_db());
    let want = expanders(&fixture::db(&after).into_db(), &ids);
    let mut edited = fixture::db(&after).into_db();
    set(&mut edited, store.clone());
    assert_eq!(expanders(&edited, &ids), want);
    let counts = store.take();
    let n = MACROS as u64;
    assert_eq!(
        counts,
        Counts { hits: n - 1, misses: 2, made: 2, again: 0 },
        "N-1 tokens found; the file's index and the edited macro's tokens missed and made"
    );
}
