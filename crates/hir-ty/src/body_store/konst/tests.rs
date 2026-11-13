//! The control of a constant an entry (AGENTS.md `granular-caching`, test-the-cause): what the
//! key this replaced does on the edit the count test makes. The count test itself (of N
//! constants, an edit of one finds the other N - 1 with none evaluated) evaluates constants,
//! which needs MIR, and hir-ty's own test database builds none: it is hir-ty-mir's
//! `tests::consteval::items`. This evaluates nothing.

use base_db::Db;
use blaze_ids::ContentId;
use hir_def::{ConstId, DefWithBodyId, nameres::demand::crate_body_owners, signatures::ConstSignature};
use hir_expand::name::Name;

use crate::{body_store::tests::db_of, consteval::deps};

/// Five constants of one crate, none reading another: hir-ty-mir's `items` fixture.
const NAMES: [&str; 5] = ["A", "B", "C", "D", "E"];

fn fixture(b: u32) -> String {
    format!(
        "//- minicore: sized, copy, add\n//- /main.rs crate:main deps:core new_source_root:local\n\
         pub const A: u32 = 1;\npub const B: u32 = {b};\npub const C: u32 = 3 + 4;\n\
         pub const D: u8 = 9;\npub const E: bool = true;\n"
    )
}

fn constant(db: &Db, name: &str) -> ConstId {
    let main = *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == "main"))
        .expect("the fixture's crate `main`");
    crate_body_owners(db, main)
        .into_iter()
        .find_map(|def| match def {
            DefWithBodyId::ConstId(it) if ConstSignature::of(db, it).name == Some(Name::new_root(name)) => Some(it),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no constant `{name}`"))
}

/// The control (test-the-cause): under the key this replaced, a constant's item key with its
/// crate's check key, which is made of every file of the crate, the same edit moves the key
/// of every one of the N constants: none would be found.
#[test]
fn keyed_by_its_crate_every_constant_misses() {
    let crate_key = |source: &str| ContentId::of("blaze v2 item store constant control 1", source.as_bytes());
    let (before, after) = (crate_key(&fixture(2)), crate_key(&fixture(20)));
    let db = db_of(&fixture(2));
    let moved = blaze_query::db::attach(&db, || {
        NAMES
            .iter()
            .filter(|name| {
                let item = hir_def::deps::const_key(&db, constant(&db, name)).expect("a module's constant has a key");
                deps::key(before, item) != deps::key(after, item)
            })
            .count()
    });
    assert_eq!(moved, NAMES.len(), "the crate's key in each constant's key: all N move");
}
