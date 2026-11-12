//! A front end lowered awaiting ([`crate::lower_awaiting`]) resolves no crate in the write that
//! adds it (restore.md, F2): the crates are awaited (`hir_def::deps::awaited`) until the
//! driver's serve asks about them. Counted here by what the maintainers report
//! (`base_db::Maintained`: one record a crate a maintainer went over in a pass) and by the
//! rows of the item-tree tables, on a chain of library crates under one workspace binary, at
//! two sizes. The serve with records is counted where a store is (blaze-cargo's
//! `serve::unit::tests`).

use base_db::{Env, Maintained};
use hir_def::deps::{ImageBuf, ImageStore, Mode as ImageMode};

use super::*;

/// A workspace binary `b` over `libraries` library crates in a chain (`l0` on `l1` on ...),
/// each a package of the standard library's root with one file: the units, and the library's
/// files. The binary is the last unit.
fn chain(libraries: usize) -> (Units, Vec<(String, Vec<u8>)>) {
    let b = "path+file:///w/b#0.1.0";
    let mut units_of = Vec::new();
    let mut plans = Vec::new();
    let mut packages = Vec::new();
    let mut files = Vec::new();
    for at in 0..libraries {
        let id = format!("path+file://{LIBRARY}/l{at}#0.0.0");
        let name = format!("l{at}");
        let dependencies = match at + 1 < libraries {
            true => vec![dep(at + 1, &format!("l{}", at + 1), false)],
            false => vec![],
        };
        let src = format!("{LIBRARY}/l{at}/src/lib.rs");
        units_of.push(unit(&id, target("lib", &name, &src, "2024"), Mode::Build, true, dependencies));
        plans.push(plan(&[], &[], None));
        packages.push(package(
            &id,
            &name,
            &format!("{LIBRARY}/l{at}/Cargo.toml"),
            Some(&format!("({LIBRARY}/l{at})")),
            false,
        ));
        files.push((
            format!("l{at}/Cargo.toml"),
            format!("[package]\nname = \"l{at}\"\n").into_bytes(),
        ));
        files.push((
            format!("l{at}/src/lib.rs"),
            format!("pub fn f{at}() {{}}\npub struct S{at};\nimpl S{at} {{ pub fn m(&self) {{}} }}\n")
                .into_bytes(),
        ));
    }
    units_of.push(unit(
        b,
        target("bin", "b", "/w/b/src/main.rs", "2024"),
        Mode::Build,
        false,
        vec![dep(0, "l0", false)],
    ));
    plans.push(plan(&[], &[("CARGO_PKG_NAME", "b")], None));
    packages.push(package(b, "b", "/w/b/Cargo.toml", Some("(/w/b)"), true));
    packages.sort_by(|x, y| x.id.cmp(&y.id));
    let mut units = units();
    units.graph.units = units_of;
    units.graph.roots = vec![libraries];
    units.plans = plans;
    units.packages = packages;
    (units, files)
}

/// [`chain`] lowered into `db`, with the maintainers' records kept from before the lowering.
fn lowered_chain(mut db: Db, libraries: usize) -> Lowered {
    let (units, files) = chain(libraries);
    let tree = MemTree::default();
    let root = view(&tree);
    let files: Vec<(&str, &[u8])> =
        files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
    let config = config(root, tree.root(&files));
    db.keep_maintained(true);
    crate::lower_into(db, Lower { units: &units, config: &config, files: &tree, root, host_cpu: None })
        .unwrap()
}

/// A database that awaits the crates added to it, as [`crate::lower_awaiting`] makes one.
fn awaiting() -> Db {
    let mut db = Db::new();
    hir_def::deps::await_added(&mut db);
    db
}

/// The crates name resolution and the impls maintainer went over since the records were last
/// taken, a crate once a pass it was gone over in.
fn maintained(lowered: &mut Lowered) -> (Vec<Crate>, Vec<Crate>) {
    let mut all: Vec<Maintained> = Vec::new();
    lowered.served_db().take_maintained(&mut all);
    let of = |maintainer: &str| -> Vec<Crate> {
        all.iter().filter(|it| it.maintainer == maintainer).map(|it| it.krate).collect()
    };
    (of("nameres"), of("impls"))
}

/// The rows of the front end's item-tree tables, and every table's name for a failure to say.
fn item_tree_rows(lowered: &mut Lowered) -> (u64, Vec<&'static str>) {
    let tables = lowered.tables();
    let rows = tables.iter().filter(|it| it.0.contains("item_tree")).map(|it| it.1).sum();
    (rows, tables.iter().map(|it| it.0).collect())
}

fn all_crates(lowered: &Lowered) -> Vec<Crate> {
    base_db::all_crates(lowered.db()).iter().copied().collect()
}

/// A store with nothing in it: every crate asked of it is a miss.
struct Nothing;

impl ImageStore for Nothing {
    fn get(&self, _: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(None)
    }

    fn put(&self, _: ContentId, _: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("the empty store of a test has no expansion server".to_owned())
    }
}

const BUILD: &str = "blaze v2 lower test build";

/// The invariant (`hir_def::deps::awaited`): the write that adds crates to a front end lowered
/// awaiting runs no maintainer over any of them, whatever their number: no name resolution, no
/// impl tables, no item tree, at 4 library crates and at 40 (both counts are 0). Each is
/// awaited, and a reader of one is stopped naming the crate it waits for. The serve that asks
/// about the binary then releases it and everything below it, and with an empty store each
/// crate is resolved exactly once, in that write.
///
/// Its control is [`the_control_a_front_end_that_awaits_nothing_resolves_every_crate_as_it_is_added`].
#[test]
fn a_crate_added_to_an_awaiting_front_end_is_not_resolved_until_it_is_asked_about() {
    for libraries in [4usize, 40] {
        let mut lowered = lowered_chain(awaiting(), libraries);
        let crates = all_crates(&lowered);
        assert_eq!(crates.len(), libraries + 1, "the libraries and the binary");
        let (resolved, impls) = maintained(&mut lowered);
        assert_eq!(
            resolved,
            Vec::<Crate>::new(),
            "{libraries} libraries: name resolution ran no crate as they were added"
        );
        assert_eq!(
            impls,
            Vec::<Crate>::new(),
            "{libraries} libraries: the impls maintainer went over no crate as they were added"
        );
        let (rows, names) = item_tree_rows(&mut lowered);
        assert_eq!(rows, 0, "{libraries} libraries: no item tree was made; tables: {names:?}");
        let db = lowered.db();
        assert!(crates.iter().all(|&it| hir_def::deps::is_awaited(db, it)), "each is awaited");
        let bin = lowered.krate(libraries).unwrap();
        let read = hir_def::nameres::Unmaintained::catch(std::panic::AssertUnwindSafe(|| {
            _ = hir_def::nameres::crate_def_map(db, bin);
        }));
        let error = read.expect_err("a crate awaited is not read");
        assert!(error.awaited.is_some(), "the error names the crate awaited: {error}");

        // The driver asks about the binary: it and all below it are released, and with no
        // records each gets its first run from source, once.
        let build = ContentId::of(BUILD, b"awaited");
        let served = lowered.serve_deps(&[bin], &[], build, &Nothing, ImageMode::Serve).unwrap();
        assert!(served.hits.is_empty());
        let db = lowered.db();
        assert!(!hir_def::deps::any_awaited(db), "nothing is awaited once the root was asked about");
        let (mut ran, mut tabled) = maintained(&mut lowered);
        ran.sort_unstable();
        tabled.sort_unstable();
        let mut each = crates.clone(); // clone: a test's list
        each.sort_unstable();
        assert_eq!(ran, each, "{libraries} libraries: each crate resolved once, in the release");
        assert_eq!(tabled, each, "{libraries} libraries: each crate's impl tables made once");
        let db = lowered.db();
        _ = hir_def::nameres::crate_def_map(db, bin);
        // A second serve of the same crates releases nothing and writes nothing.
        let view = lowered.db().nonce_and_revision().1;
        lowered.serve_deps(&[bin], &[], build, &Nothing, ImageMode::Serve).unwrap();
        assert_eq!(lowered.db().nonce_and_revision().1, view, "nothing awaited: no write");
    }
}

/// The control of [`a_crate_added_to_an_awaiting_front_end_is_not_resolved_until_it_is_asked_about`]:
/// a front end that awaits nothing (the await left out: [`crate::lower`]'s database) resolves
/// every crate in the write that adds it, which that test's counts refuse; and the serve
/// refuses such a front end by name, so no driver with a store runs on it.
#[test]
fn the_control_a_front_end_that_awaits_nothing_resolves_every_crate_as_it_is_added() {
    let libraries = 4usize;
    let mut lowered = lowered_chain(Db::new(), libraries);
    let (mut ran, _) = maintained(&mut lowered);
    ran.sort_unstable();
    ran.dedup();
    assert_eq!(ran.len(), libraries + 1, "name resolution ran every crate as it was added");
    let (rows, names) = item_tree_rows(&mut lowered);
    assert!(rows > 0, "and made their item trees; tables: {names:?}");
    let bin = lowered.krate(libraries).unwrap();
    _ = hir_def::nameres::crate_def_map(lowered.db(), bin);
    let build = ContentId::of(BUILD, b"awaited");
    let refused = lowered.serve_deps(&[bin], &[], build, &Nothing, ImageMode::Serve).unwrap_err();
    assert!(refused.what.contains("lower_awaiting"), "{}", refused.what);
}
