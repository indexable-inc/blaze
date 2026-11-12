//! A dependency's bodies part (`hir_ty::check_cache`) whose bodies' inference evaluated constants
//! in types reads back in another database, whose ids differ, as that database's own inference.
//! Here, not with hir-ty's other `check_cache` tests: the solver evaluates such a constant by MIR,
//! which only this crate builds.

use std::{alloc::Global, sync::Arc};

use base_db::{Crate, Db, all_crates};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_query::db::AsId;
use hir_def::{
    DefWithBodyId, FunctionId, LoweringMode, nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::{fixture, name::Name};

use crate::{check_cache, infer_query_with_inspect};

/// A dependency whose array types have lengths the solver evaluates: an expression, and one
/// naming a constant item.
const DEP: &str = r#"
//- minicore: sized, copy, add
//- /dep.rs crate:dep deps:core new_source_root:library
pub const N: usize = 3;
pub fn sum() -> u8 { let a = [1u8; 4]; let b: [u8; 2 + 2] = a; let [x, ..] = b; x }
pub fn named() -> u8 { let a = [2u8; 4]; let b: [u8; N + 1] = a; let [.., y] = b; y }
//- /main.rs crate:main deps:core,dep new_source_root:local
fn main() { let _ = dep::sum() + dep::named(); }
"#;

/// A crate of nothing `DEP`'s crates name, its functions interned before theirs.
const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub fn p2() {}
pub const P: usize = 1 + 1;
";

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
    crate_body_owners(db, krate)
        .into_iter()
        .find_map(|def| match def {
            DefWithBodyId::FunctionId(it)
                if FunctionSignature::of(db, it).name == Name::new_root(name) =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

#[test]
fn a_part_of_evaluated_constants_reads_back_in_another_database() {
    let first = fixture::db(DEP).into_db();
    let first_dep = crate_named(&first, "dep");
    let bytes = check_cache::encode(&first, first_dep, &|_| true).unwrap();

    let at = DEP.find("//- /").expect("a fixture with a file");
    let mut db = fixture::db(&format!("{}{PAD}{}", &DEP[..at], &DEP[at..])).into_db();
    let dep = crate_named(&db, "dep");
    let bits = |db: &Db, krate| function(db, krate, "sum").as_id().as_u32();
    assert_ne!(bits(&first, first_dep), bits(&db, dep), "the two databases' ids differ");

    check_cache::open(&bytes).unwrap();
    // alloc: test setup
    let mut parts = HashMap::new_in(Global);
    parts.insert(dep, Arc::new(Aligned::copy_from(&bytes)) as hir_def::deps::ImageBuf);
    check_cache::set_served(&mut db, parts);

    for name in ["sum", "named"] {
        let def = DefWithBodyId::FunctionId(function(&db, dep, name));
        let served =
            check_cache::served_inference(&db, def).unwrap_or_else(|| panic!("`{name}` is served"));
        let own = infer_query_with_inspect(&db, def, None, LoweringMode::Analysis);
        assert!(!own.has_errors, "`{name}` infers with no error: {own:?}");
        assert!(served == own, "`{name}`: served {served:?}\nown {own:?}");
        assert!(check_cache::served_passed(&db, def), "`{name}`'s verdict");
    }
}

// A dependency's runtime MIR as entries of its crate's value (`hir_ty::check_cache::runtime`).

mod runtime {
    use std::sync::Arc;

    use base_db::{Crate, Db};
    use blaze_format::Aligned;
    use blaze_mem::Tag;
    use blaze_query::db::Database as _;
    use hir_def::{DefWithBodyId, nameres::frozen::pages::{PageSource, Whole}};
    use hir_expand::fixture;
    use hir_ty::{
        InferBodyId,
        check_cache::runtime::{self, testing},
        mir::{MirOwner, MirStage},
        providers::mir_at,
    };

    use super::{PAD, crate_named, function};

    /// A dependency with a generic function, a closure, a call across its own functions and
    /// a branch.
    const DEP: &str = r#"
//- minicore: sized, copy, add, fn
//- /dep.rs crate:dep deps:core new_source_root:library
pub struct Pair<T> { pub a: T, pub b: T }
pub fn first<T: Copy>(pair: &Pair<T>) -> T { pair.a }
pub fn total(pair: &Pair<u32>) -> u32 { pair.a + pair.b }
pub fn twice(x: u32) -> u32 { let double = |y: u32| y + y; double(x) }
pub fn pick(flag: bool, x: u32) -> u32 { if flag { x } else { total(&Pair { a: x, b: 1 }) } }
//- /main.rs crate:main deps:core,dep new_source_root:local
fn main() { let _ = dep::pick(true, dep::first(&dep::Pair { a: 1u32, b: 2 })); }
"#;

    const NAMES: [&str; 4] = ["first", "total", "twice", "pick"];

    fn plain() -> (Db, Crate) {
        let db = fixture::db(DEP).into_db();
        let dep = crate_named(&db, "dep");
        (db, dep)
    }

    /// [`DEP`] with a crate before its own, so `dep`'s ids are not the plain database's.
    fn padded() -> (Db, Crate) {
        let at = DEP.find("//- /").expect("a fixture with a file");
        let db = fixture::db(&format!("{}{PAD}{}", &DEP[..at], &DEP[at..])).into_db();
        let dep = crate_named(&db, "dep");
        (db, dep)
    }

    /// The MIR owners of `dep`'s functions: each function, then the closures its runtime
    /// body names.
    fn owners<'db>(db: &'db Db, dep: Crate) -> Vec<MirOwner<'db>> {
        let mut owners = Vec::new();
        for name in NAMES {
            let def = DefWithBodyId::FunctionId(function(db, dep, name));
            let owner = MirOwner::Body(InferBodyId::DefWithBodyId(def));
            owners.push(owner);
            let body = mir_at(db, owner, MirStage::Runtime)
                .unwrap_or_else(|error| panic!("`{name}` lowers: {error:?}"));
            owners.extend(body.closures.iter().map(|&closure| MirOwner::Closure(closure)));
        }
        owners
    }

    /// `dep`'s value as a database that records writes it, after lowering every owner.
    fn value_of(db: &Db, dep: Crate) -> (Vec<u8>, runtime::Encoded) {
        let _recording = runtime::record(db).unwrap();
        let asked = owners(db, dep).len();
        let (value, encoded) = runtime::encode(db, dep).unwrap();
        assert_eq!(encoded.entries, asked, "every owner lowered has an entry: {encoded:?}");
        (value, encoded)
    }

    fn source(value: &[u8]) -> Arc<dyn PageSource> {
        Arc::new(Whole(Arc::new(Aligned::copy_from(value))))
    }

    /// The round trip, against the heap value: an entry written in one database reads in
    /// another, whose ids differ, as the runtime body that database builds itself.
    #[test]
    fn a_runtime_entry_reads_in_another_database_as_the_body_built_there() {
        let (first, first_dep) = plain();
        let written: Vec<Vec<u8>> = owners(&first, first_dep)
            .into_iter()
            .map(|owner| testing::entry_bytes(&first, owner).unwrap())
            .collect();
        assert!(written.len() > NAMES.len(), "the closure of `twice` is an owner");

        let (second, dep) = padded();
        for (owner, bytes) in owners(&second, dep).into_iter().zip(&written) {
            let read = testing::entry_body(&second, dep, bytes)
                .unwrap_or_else(|why| panic!("{owner:?}: {why}"));
            let own = mir_at(&second, owner, MirStage::Runtime).unwrap();
            assert!(read == *own, "{owner:?}: read {read:?}\nown {own:?}");
        }
    }

    /// The control of the round trip: another owner's entry is another body, so the
    /// comparison above tells a wrong entry from the right one.
    #[test]
    fn the_oracle_catches_an_entry_of_another_owner() {
        let (db, dep) = plain();
        let owners = owners(&db, dep);
        let total = testing::entry_bytes(&db, owners[1]).unwrap();
        let read = testing::entry_body(&db, dep, &total).unwrap();
        assert!(read != *mir_at(&db, owners[0], MirStage::Runtime).unwrap());
        assert!(read == *mir_at(&db, owners[1], MirStage::Runtime).unwrap());
    }

    /// Deterministic bytes: two databases of the same sources, whose ids differ, write the
    /// same value.
    #[test]
    fn the_same_runtime_value_from_two_databases() {
        let (first, first_dep) = plain();
        let (second, dep) = padded();
        let (a, _) = value_of(&first, first_dep);
        let (b, _) = value_of(&second, dep);
        assert!(a == b, "two databases of one source wrote other bytes");
    }

    /// The control of the bytes' test: a body edited is another value.
    #[test]
    fn the_runtime_value_of_another_source_is_other_bytes() {
        let (first, first_dep) = plain();
        let edited = DEP.replace("pair.a + pair.b", "pair.b + pair.a");
        let other = fixture::db(&edited).into_db();
        let other_dep = crate_named(&other, "dep");
        assert!(value_of(&first, first_dep).0 != value_of(&other, other_dep).0);
    }

    /// The count: a database that reads `dep` from its value makes one MIR value an owner it
    /// is asked for (the runtime body, from its entry) and no built body, so nothing of the
    /// owner is lowered or inferred there (`mir_body_query` runs 0 times; `infer` and `body`
    /// are reached only through it).
    #[test]
    fn a_crate_read_from_its_value_builds_no_body_it_holds() {
        let (first, first_dep) = plain();
        let (value, _) = value_of(&first, first_dep);

        let (mut db, dep) = padded();
        assert!(runtime::serve(&mut db, dep, Some(source(&value))).unwrap());
        assert!(runtime::is_served(&db, dep));
        let before = db.storage().values_of(Tag::Mir);
        let asked = owners(&db, dep).len();
        assert_eq!(db.storage().values_of(Tag::Mir) - before, asked);
    }

    /// The control of the count: the same database given no value builds each owner's body
    /// before its runtime body, two MIR values an owner at least.
    #[test]
    fn the_control_a_crate_with_no_value_builds_every_body() {
        let (db, dep) = padded();
        assert!(!runtime::is_served(&db, dep));
        let before = db.storage().values_of(Tag::Mir);
        let asked = owners(&db, dep).len();
        assert!(db.storage().values_of(Tag::Mir) - before >= 2 * asked);
    }

    /// The refinement: what a database answers from the value is what it answers after the
    /// value is taken away and the bodies are built from source.
    #[test]
    fn a_served_runtime_body_is_the_one_built_from_source() {
        let (first, first_dep) = plain();
        let (value, _) = value_of(&first, first_dep);
        let (mut db, dep) = padded();
        runtime::serve(&mut db, dep, Some(source(&value))).unwrap();
        let served: Vec<String> = owners(&db, dep)
            .into_iter()
            .map(|owner| format!("{:?}", mir_at(&db, owner, MirStage::Runtime).unwrap()))
            .collect();
        assert!(runtime::serve(&mut db, dep, None).unwrap(), "the value is taken away");
        let built: Vec<String> = owners(&db, dep)
            .into_iter()
            .map(|owner| format!("{:?}", mir_at(&db, owner, MirStage::Runtime).unwrap()))
            .collect();
        assert!(served == built, "served {served:#?}\nbuilt {built:#?}");
    }

    /// No fallback: a value whose entry does not read stops the query naming the crate.
    #[test]
    fn an_entry_that_does_not_read_is_a_panic_naming_the_crate() {
        let (first, first_dep) = plain();
        let (mut value, _) = value_of(&first, first_dep);
        // The first entry: after the header (32 bytes) and the buckets (64 each), at a
        // multiple of 16; its key is 32 bytes, then its buffer's own header, damaged here.
        let buckets = u32::from_le_bytes(value[12..16].try_into().unwrap()) as usize;
        let entry = (32 + buckets * 64).next_multiple_of(16);
        for byte in &mut value[entry + 32..entry + 48] {
            *byte ^= 0xff;
        }
        let (mut db, dep) = padded();
        runtime::serve(&mut db, dep, Some(source(&value))).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for owner in owners(&db, dep) {
                drop(mir_at(&db, owner, MirStage::Runtime));
            }
        }))
        .unwrap_err();
        // The message, when the panic carries the query's own: the engine may wrap it.
        if let Some(message) = panic.downcast_ref::<String>() {
            assert!(message.contains("runtime MIR value of crate"), "{message}");
            assert!(message.contains("not built instead"), "{message}");
        }
    }

    #[test]
    fn a_runtime_value_of_another_layout_is_refused_by_name() {
        let (first, first_dep) = plain();
        let (mut value, _) = value_of(&first, first_dep);
        value[8] ^= 0x7f;
        let (mut db, dep) = padded();
        let why = runtime::serve(&mut db, dep, Some(source(&value))).unwrap_err();
        assert!(why.contains("layout") && why.contains("dep"), "{why}");
    }
}
