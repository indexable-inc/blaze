//! The sealed layer's core under a simulated machine: two installs of one cover interleaved
//! under every schedule Hegel draws, the cover's inputs, prebuilt records turned off, and
//! which crates a home resolves from source once a layer is installed.

use std::path::{Path, PathBuf};

use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use super::*;

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

fn base(n: usize) -> Base {
    Base::Dir(PathBuf::from(format!("/bases/{n}")))
}

fn a_cover() -> Cover {
    cover(&CoverOf { tree: "d1", on: &[], makers: ContentId([7; 32]), host: "aarch64-apple-darwin" })
}

/// The machine both installs run on: whether the layer's head is there, who holds the cover's
/// lock, which bases hold the pack, and every import stepped.
struct Machine {
    head: bool,
    lock: Option<usize>,
    has: Vec<bool>,
    imports: Vec<(usize, Base)>,
}

/// Two installs of one cover, `schedule` saying which moves at each turn (a blocked one gives
/// the turn to the other): how each ended, and the machine. `plant` sets the planted bug.
fn interleaved(has: &[bool], schedule: &[bool], plant: impl Fn(&mut Install)) -> ([Installed; 2], Machine) {
    let bases: Vec<Base> = (0..has.len()).map(base).collect();
    let mut machine = Machine { head: false, lock: None, has: has.to_vec(), imports: Vec::new() };
    let mut installs: Vec<Install> =
        (0..2).map(|_| Install::new(a_cover(), PathBuf::from("/m/std/x"), bases.clone(), true)).collect();
    for it in &mut installs {
        plant(it);
    }
    let mut pending: Vec<Option<Step>> = installs.iter_mut().map(|it| Some(it.step(None))).collect();
    let mut ended: [Option<Installed>; 2] = [None, None];
    let mut turn = schedule.iter().copied().chain(std::iter::repeat(false));
    for _ in 0..200 {
        if ended.iter().all(Option::is_some) {
            break;
        }
        let wanted = usize::from(turn.next().unwrap_or(false));
        let blocked = |who: usize, pending: &[Option<Step>], machine: &Machine| match &pending[who] {
            None => true,
            Some(Step::Lock { .. }) => machine.lock.is_some_and(|holder| holder != who),
            Some(_) => false,
        };
        let who = if blocked(wanted, &pending, &machine) { 1 - wanted } else { wanted };
        assert!(!blocked(who, &pending, &machine), "both installs are blocked: {pending:?}");
        let step = pending[who].take().expect("a pending step");
        let outcome = match step {
            Step::Stat { .. } => Outcome::Head(machine.head),
            Step::Lock { .. } => {
                machine.lock = Some(who);
                Outcome::Locked
            }
            Step::Import { base, .. } => {
                let at = bases.iter().position(|it| *it == base).expect("a base it was given");
                machine.imports.push((who, base));
                match machine.has[at] {
                    true => {
                        machine.head = true;
                        Outcome::Imported(Ok((3, 300)))
                    }
                    false => Outcome::Absent,
                }
            }
            Step::Done(installed) => {
                if machine.lock == Some(who) {
                    machine.lock = None;
                }
                ended[who] = Some(installed);
                continue;
            }
        };
        pending[who] = Some(installs[who].step(Some(outcome)));
    }
    let [Some(a), Some(b)] = ended else { panic!("an install did not end in 200 turns") };
    ([a, b], machine)
}

/// The oracle: what is wrong with a run, if anything.
fn wrong(has: &[bool], ended: &[Installed; 2], machine: &Machine) -> Option<String> {
    let packs = has.iter().any(|it| *it);
    let pack_imports = machine.imports.iter().filter(|(_, base)| has[(0..has.len()).position(|n| self::base(n) == *base).unwrap()]).count();
    if packs && pack_imports != 1 {
        return Some(format!("{pack_imports} imports of a pack, not one: {:?}", machine.imports));
    }
    for it in ended {
        match it {
            Installed::Already | Installed::Now { .. } if packs => {}
            Installed::NoPack { looked } if !packs && looked.len() == has.len() => {}
            other => return Some(format!("an install ended {other:?} with bases {has:?}")),
        }
    }
    None
}

/// The invariant: over every interleaving of two installs of one cover on one machine, under
/// many seeds, exactly one import of a pack is stepped and both end `Already` or `Now`;
/// `NoPack` only when every base answered `Absent`, naming each.
#[test]
fn the_layer_is_installed_once_and_a_second_home_imports_nothing() {
    run_cases(512, |tc| {
        let has = tc.draw(gs::vecs(gs::booleans()).min_size(1).max_size(3));
        let schedule = tc.draw(gs::vecs(gs::booleans()).max_size(24));
        let (ended, machine) = interleaved(&has, &schedule, |_| {});
        if let Some(why) = wrong(&has, &ended, &machine) {
            panic!("bases {has:?}, schedule {schedule:?}: {why}");
        }
    });
}

/// Control: an install that skips the stat under the lock imports a second time when the two
/// stat before either imports, and the oracle says so.
#[test]
fn the_oracle_catches_an_install_that_skips_the_stat() {
    let has = [true];
    // Both stat first (turns 0 and 1), then each locks and imports in turn.
    let (ended, machine) = interleaved(&has, &[false, true, false, false, false, true, true, true], |it| it.plant_skip_stat = true);
    let said = wrong(&has, &ended, &machine).expect("the oracle catches two imports");
    assert!(said.contains("2 imports"), "{said}");
}

/// The invariant: a cover changes with the tree's digest, with each dependency's cover, with
/// what makes the records and with the host, and with nothing else (the dependencies' order,
/// a dependency named twice).
#[test]
fn a_cover_moves_with_each_input_and_with_nothing_else() {
    run_cases(256, |tc| {
        let tree = tc.draw(gs::text().alphabet("abc-").max_size(12));
        let host = tc.draw(gs::text().alphabet("abc-").max_size(12));
        let on: Vec<Cover> =
            tc.draw(gs::vecs(gs::integers::<u8>()).max_size(4)).into_iter().map(|n| Cover(ContentId([n; 32]))).collect();
        let makers = ContentId([tc.draw(gs::integers::<u8>()); 32]);
        let of = CoverOf { tree: &tree, on: &on, makers, host: &host };
        let at = cover(&of);
        let mut turned = on.clone();
        turned.reverse();
        turned.extend(on.first().copied());
        assert_eq!(at, cover(&CoverOf { on: &turned, ..of }), "the dependencies' order or a repeat moved it");
        let other_tree = format!("{tree}x");
        assert_ne!(at, cover(&CoverOf { tree: &other_tree, ..of }), "the tree");
        let other_host = format!("{host}x");
        assert_ne!(at, cover(&CoverOf { host: &other_host, ..of }), "the host");
        assert_ne!(at, cover(&CoverOf { makers: ContentId([!makers.0[0]; 32]), ..of }), "the makers");
        let mut more = on.clone();
        more.push(Cover(ContentId([0xee; 32])));
        if !on.contains(&Cover(ContentId([0xee; 32]))) {
            assert_ne!(at, cover(&CoverOf { on: &more, ..of }), "a dependency");
        }
        // A tree and a host whose bytes run together hash apart (each length first).
        if !host.is_empty() {
            let joined = format!("{tree}{}", &host[..host.chars().next().map_or(0, char::len_utf8)]);
            let rest = &host[host.chars().next().map_or(0, char::len_utf8)..];
            assert_ne!(at, cover(&CoverOf { tree: &joined, host: rest, ..of }), "a field boundary");
        }
    });
}

/// The steps one install takes alone, with `has` saying which bases hold the pack.
fn alone(install: &mut Install, has: &[bool]) -> (Installed, Vec<Base>) {
    let mut asked = Vec::new();
    let mut step = install.step(None);
    for _ in 0..32 {
        let outcome = match step {
            Step::Stat { .. } => Outcome::Head(false),
            Step::Lock { .. } => Outcome::Locked,
            Step::Import { base, .. } => {
                let at = (0..has.len()).position(|n| self::base(n) == base).unwrap();
                asked.push(base);
                if has[at] { Outcome::Imported(Ok((1, 1))) } else { Outcome::Absent }
            }
            Step::Done(installed) => return (installed, asked),
        };
        step = install.step(Some(outcome));
    }
    panic!("the install did not end");
}

/// The invariant: with prebuilt records off, a source that has the cover's pack is never
/// asked and the install ends `NoPack` with nothing looked at.
#[test]
fn prebuilt_off_asks_no_source() {
    let mut install = Install::new(a_cover(), PathBuf::from("/m/std/x"), vec![base(0)], false);
    let (ended, asked) = alone(&mut install, &[true]);
    assert_eq!((ended, asked), (Installed::NoPack { looked: Vec::new() }, Vec::new()));
}

/// Control: an install that ignores the setting asks the source.
#[test]
fn the_oracle_catches_an_install_that_ignores_prebuilt_off() {
    let mut install = Install::new(a_cover(), PathBuf::from("/m/std/x"), vec![base(0)], false);
    install.plant_ignore_prebuilt = true;
    let (_, asked) = alone(&mut install, &[true]);
    assert_eq!(asked, vec![base(0)], "the planted bug asks the source");
}

/// A refusing base is the install's end, naming it: never the next base, never a build.
#[test]
fn a_base_that_refuses_ends_the_install_naming_it() {
    let mut install = Install::new(a_cover(), PathBuf::from("/m/std/x"), vec![base(0), base(1)], true);
    assert!(matches!(install.step(None), Step::Stat { .. }));
    let mut step = install.step(Some(Outcome::Head(false)));
    assert!(matches!(step, Step::Lock { .. }));
    step = install.step(Some(Outcome::Locked));
    assert!(matches!(step, Step::Stat { .. }));
    step = install.step(Some(Outcome::Head(false)));
    assert!(matches!(step, Step::Import { .. }));
    step = install.step(Some(Outcome::Imported(Err("a record does not hash to its manifest's entry".into()))));
    assert_eq!(
        step,
        Step::Done(Installed::Refused { base: "/bases/0".into(), why: "a record does not hash to its manifest's entry".into() })
    );
}

/// The crates of a hello-world on a toolchain with two sealed trees, the second depending on
/// the first (as a shipped bstd is on the library), and its own crate.
fn hello(machine: &Path) -> (Vec<Placed>, Vec<(&'static str, PathBuf)>) {
    let makers = ContentId([1; 32]);
    let host = "x86_64-unknown-linux-gnu";
    let lib = cover(&CoverOf { tree: "lib-digest", on: &[], makers, host });
    let second = cover(&CoverOf { tree: "second-digest", on: &[lib], makers, host });
    let placed = vec![
        Placed { dir: machine.join("library/lib-digest"), mounted: None, digest: "lib-digest".to_owned(), cover: lib },
        Placed { dir: machine.join("bstd/second-digest"), mounted: None, digest: "second-digest".to_owned(), cover: second },
    ];
    let crates = vec![
        ("core", machine.join("library/lib-digest/library/core")),
        ("alloc", machine.join("library/lib-digest/library/alloc")),
        ("std", machine.join("library/lib-digest/library/std")),
        ("bstd", machine.join("bstd/second-digest/crates/blaze-std")),
        ("blaze_core", machine.join("bstd/second-digest/crates/blaze-core")),
        ("hello", PathBuf::from("/work/hello")),
        // The same package at a path a person edits: never sealed.
        ("bstd_edited", PathBuf::from("/src/blaze/crates/blaze-std")),
    ];
    (placed, crates)
}

/// The invariant: once both trees' layers are installed, a hello-world on them resolves none
/// of their crates from source: only its own crate and a path a person edits.
#[test]
fn a_hello_world_on_installed_layers_resolves_none_of_their_crates_from_source() {
    let machine = Path::new("/m");
    let (placed, crates) = hello(machine);
    let crates: Vec<(&str, &Path)> = crates.iter().map(|(name, dir)| (*name, dir.as_path())).collect();
    let all: Vec<Cover> = placed.iter().map(|it| it.cover).collect();
    assert_eq!(from_source(&crates, &placed, |it| all.contains(&it)), vec!["hello", "bstd_edited"]);
}

/// Control: with only the first tree's layer installed, the second tree's crates are resolved
/// from source, and the check names them.
#[test]
fn the_oracle_catches_a_layer_left_out() {
    let machine = Path::new("/m");
    let (placed, crates) = hello(machine);
    let crates: Vec<(&str, &Path)> = crates.iter().map(|(name, dir)| (*name, dir.as_path())).collect();
    let first = placed[0].cover;
    let named = from_source(&crates, &placed, |it| it == first);
    assert_eq!(named, vec!["bstd", "blaze_core", "hello", "bstd_edited"]);
}

fn locked(name: &str, checksum: &str, dependencies: &[&str]) -> Locked {
    Locked {
        name: name.to_owned(),
        version: "1.0.0".to_owned(),
        checksum: checksum.to_owned(),
        dependencies: dependencies.iter().map(|it| (*it).to_owned()).collect(),
    }
}

/// The registry packages of a lock file of `a` on `b` on `c`, and `d` alone, after the
/// library: their covers by name. `plant_flat`: the planted bug, a cover on no dependency.
fn registry_covers(checksums: [&str; 4], plant_flat: bool) -> Vec<(String, Cover)> {
    let lock = [
        locked("a", checksums[0], &["b", "serde_path_dep"]),
        locked("b", checksums[1], &["c 1.0.0"]),
        locked("c", checksums[2], &[]),
        locked("d", checksums[3], &[]),
    ];
    let library = Tree { dir: PathBuf::from("/m/library/L"), mounted: None, digest: "L".to_owned(), on: Vec::new() };
    let mut trees = vec![library];
    let mut packages = registry(&lock, &[0], 1, |it| Ok(PathBuf::from(format!("/src/{}-{}", it.name, it.version)))).unwrap();
    if plant_flat {
        packages.iter_mut().for_each(|it| it.on.retain(|on| *on == 0));
    }
    trees.extend(packages);
    let placed = placed(&trees, ContentId([7; 32]), "aarch64-apple-darwin").unwrap();
    let name = |dir: &Path| dir.file_name().unwrap().to_string_lossy().split('-').next().unwrap().to_owned();
    placed[1..].iter().map(|it| (name(&it.dir), it.cover)).collect()
}

/// The invariant: a registry package is a sealed tree by the same rule as a shipped one, its
/// cover on its own checksum and, through the covers of what it depends on, on every
/// checksum below it: changing `c`'s moves `c`, `b` and `a` and not `d`; changing `d`'s moves
/// `d` alone. A dependency with no checksum (a path package) adds nothing, a list comes out
/// dependencies first, and a cycle is refused naming a package of it. The control is a cover on
/// no dependency: the change to `c` no longer moves `a`.
#[test]
fn a_registry_package_is_sealed_by_its_checksum_and_its_cover_follows_its_dependencies() {
    let before = registry_covers(["ca", "cb", "cc", "cd"], false);
    let order: Vec<&str> = before.iter().map(|(name, _)| name.as_str()).collect();
    let at = |name: &str| order.iter().position(|it| *it == name).unwrap();
    assert!(at("c") < at("b") && at("b") < at("a"), "not dependencies first: {order:?}");
    let cover_of = |covers: &[(String, Cover)], name: &str| covers.iter().find(|it| it.0 == name).unwrap().1;
    let moved = |after: &[(String, Cover)]| -> Vec<&str> {
        ["a", "b", "c", "d"].into_iter().filter(|name| cover_of(&before, name) != cover_of(after, name)).collect()
    };
    assert_eq!(moved(&registry_covers(["ca", "cb", "cc2", "cd"], false)), ["a", "b", "c"]);
    assert_eq!(moved(&registry_covers(["ca", "cb", "cc", "cd2"], false)), ["d"]);
    assert_eq!(moved(&registry_covers(["ca", "cb", "cc", "cd"], false)), Vec::<&str>::new());

    let cycle = [locked("x", "cx", &["y"]), locked("y", "cy", &["x"])];
    let refused = registry(&cycle, &[], 0, |_| Ok(PathBuf::from("/src"))).unwrap_err();
    assert!(refused.contains("cycle") && (refused.contains("x 1.0.0") || refused.contains("y 1.0.0")), "{refused}");
    let ahead = [Tree { dir: PathBuf::from("/t"), mounted: None, digest: "t".to_owned(), on: vec![0] }];
    assert!(placed(&ahead, ContentId([7; 32]), "h").unwrap_err().contains("not listed before it"));

    // The control: a cover on no dependency misses the change below it.
    let flat = registry_covers(["ca", "cb", "cc", "cd"], true);
    let flat_moved = registry_covers(["ca", "cb", "cc2", "cd"], true);
    assert_eq!(cover_of(&flat, "a"), cover_of(&flat_moved, "a"), "the planted bug moved `a`: the oracle tests nothing");
}

/// A package as the plan names one: `source` and `checksum` as `blaze_units` writes them.
fn package(manifest: &str, source: Option<&str>, checksum: Option<[u8; 32]>, is_member: bool) -> blaze_units::query::PackagePlan {
    blaze_units::query::PackagePlan {
        id: manifest.to_owned(),
        name: "p".to_owned(),
        version: "1.0.0".to_owned(),
        manifest_path: manifest.to_owned(),
        source: source.map(str::to_owned),
        is_member,
        default_run: None,
        checksum,
        tree: None,
        tests: blaze_units::query::TestSandbox::default(),
        const_macros: None,
    }
}

/// A git package's tree pinned in `blaze.lock`, as the plan names it (`PackagePlan::tree`).
fn with_tree(package: blaze_units::query::PackagePlan, tree: &str) -> blaze_units::query::PackagePlan {
    blaze_units::query::PackagePlan { tree: Some(tree.to_owned()), ..package }
}

const TREE: &str = "blake3:9f2c000000000000000000000000000000000000000000000000000000000001";
const OTHER_TREE: &str = "blake3:9f2c000000000000000000000000000000000000000000000000000000000002";

const SOURCES: &str = "/home/a/.blaze/sources";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_COMMIT: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn shipped_trees() -> Vec<Placed> {
    let makers = ContentId([7; 32]);
    let trees = [Tree { dir: PathBuf::from("/m/bstd/d1"), mounted: None, digest: "d1".to_owned(), on: Vec::new() }];
    placed(&trees, makers, "aarch64-apple-darwin").unwrap()
}

fn pinned<'a>(package: &'a blaze_units::query::PackagePlan, shipped: &'a [Placed]) -> Option<Pin<'a>> {
    pin(package, Path::new(SOURCES), || Ok::<_, ()>(shipped)).unwrap()
}

/// The invariant: a package is sealed exactly when a digest pins its tree: a shipped tree's,
/// a registry checksum, a git dependency's tree digest in `blaze.lock` at its whole locked
/// commit; a workspace member and a path dependency never are, whatever they hold, a git
/// checkout named by a prefix pins nothing, and nor does a whole commit alone (SHA-1). The
/// control: the path dependency would be sealed by its origin (it is no member, which is all
/// the origin rule asked of a library), and this rule says no.
#[test]
fn a_package_is_sealed_exactly_when_a_digest_pins_its_tree() {
    let shipped = shipped_trees();
    let registry = package("/home/a/.blaze/sources/registry/src/r/p-1.0.0/Cargo.toml", Some("(registry `crates-io`)"), Some([3; 32]), false);
    let git_dir = format!("{SOURCES}/git/checkouts/p-0123/{COMMIT}/crates/p/Cargo.toml");
    let commit_only = package(&git_dir, Some("(https://github.com/a/p#01234567)"), None, false);
    let git = with_tree(commit_only.clone(), TREE);
    assert_eq!(pinned(&commit_only, &shipped), None, "a whole commit without its tree's digest");
    let shipped_package = package("/m/bstd/d1/crates/blaze-std/Cargo.toml", Some("(/m/bstd/d1/crates/blaze-std)"), None, false);
    let path = package("/home/a/w/vendor/p/Cargo.toml", Some("(/home/a/w/vendor/p)"), None, false);
    let member = package("/m/bstd/d1/crates/blaze-std/Cargo.toml", None, Some([3; 32]), true);
    let prefix_dir = format!("{SOURCES}/git/checkouts/p-0123/01234567/Cargo.toml");
    let prefix = package(&prefix_dir, Some("(https://github.com/a/p#01234567)"), None, false);
    assert_eq!(pinned(&registry, &shipped), Some(Pin::Registry(&[3; 32])));
    assert_eq!(pinned(&git, &shipped), Some(Pin::Git { commit: COMMIT, tree: TREE }));
    assert_eq!(pinned(&shipped_package, &shipped), Some(Pin::Shipped(&shipped[0])));
    assert_eq!(pinned(&path, &shipped), None, "a path dependency");
    assert_eq!(pinned(&member, &shipped), None, "a workspace member, even under a shipped tree");
    assert_eq!(pinned(&prefix, &shipped), None, "a checkout named by a prefix of its commit");
    // The control: by origin (no member, so a library) the path dependency is sealed.
    let by_origin = |it: &blaze_units::query::PackagePlan| !it.is_member;
    assert!(by_origin(&path), "the origin rule seals the path dependency");
    // Each kind's digest is its own: no two pins above are one.
    let digests = [&registry, &git, &shipped_package].map(|it| pinned(it, &shipped).unwrap().digest());
    assert!(digests[0] != digests[1] && digests[1] != digests[2] && digests[0] != digests[2]);
}

/// The invariant: a git dependency's pin, and the cover its tree is installed under, move
/// with its locked commit and with its tree's digest, and with nothing of the checkout's
/// directory else. The control: two checkouts of one commit and one tree under two idents
/// have one pin.
#[test]
fn a_git_dependencys_cover_moves_with_its_commit_and_tree() {
    let shipped = shipped_trees();
    let at = |ident: &str, commit: &str, tree: &str| {
        with_tree(
            package(
                &format!("{SOURCES}/git/checkouts/{ident}/{commit}/Cargo.toml"),
                Some("(https://github.com/a/p#0123)"),
                None,
                false,
            ),
            tree,
        )
    };
    let (one, two) = (at("p-1", COMMIT, TREE), at("p-2", COMMIT, TREE));
    let (moved, retree) = (at("p-1", OTHER_COMMIT, TREE), at("p-1", COMMIT, OTHER_TREE));
    let cover_of = |it: &blaze_units::query::PackagePlan| {
        let pin = pinned(it, &shipped).unwrap();
        let tree = pin.tree();
        (pin.digest(), cover(&CoverOf { tree: &tree, on: &[], makers: ContentId([7; 32]), host: "aarch64-apple-darwin" }))
    };
    assert_ne!(cover_of(&one), cover_of(&moved), "another commit is another cover");
    assert_ne!(cover_of(&one), cover_of(&retree), "another tree is another cover");
    assert_eq!(cover_of(&one), cover_of(&two), "the commit and the tree pin it");
}

/// The invariant: every crate of the library blaze placed is sealed as a build names it, under
/// the mount the daemon reads it through (`/rustc/<commit>/library`, `serve::unit`'s
/// `library`), never by the placed directory alone: `core` (no member) and `std` (a member of
/// `library/Cargo.toml`, as the plan of the library's own workspace says) both pin the
/// library's tree, so their records and outputs go into the machine's layer and a second home
/// finds them. A member under the placed directory (a person building in it), a member of the
/// build's own workspace and a path dependency still are not. The control is the list as it was
/// until 2026-11-13, with no mount: no crate of std is sealed, which is what put every std
/// record into each home (`a_second_home_on_one_machine_compiles_no_std_crate`).
#[test]
fn every_crate_of_the_placed_library_is_sealed_where_builds_name_it() {
    let library = blaze_units::stdlib::library_at(COMMIT);
    let tree = |mounted: Option<PathBuf>| {
        let trees = [Tree { dir: PathBuf::from("/m/library/L"), mounted, digest: "L".to_owned(), on: Vec::new() }];
        placed(&trees, ContentId([7; 32]), "aarch64-apple-darwin").unwrap()
    };
    let shipped = tree(Some(PathBuf::from(&library)));
    let core = package(&format!("{library}/core/Cargo.toml"), Some(format!("({library}/core)").as_str()), None, false);
    let std = package(&format!("{library}/std/Cargo.toml"), None, None, true);
    let placed_member = package("/m/library/L/library/std/Cargo.toml", None, None, true);
    let own_member = package("/home/a/w/Cargo.toml", None, None, true);
    let path = package("/home/a/w/vendor/p/Cargo.toml", Some("(/home/a/w/vendor/p)"), None, false);
    assert_eq!(pinned(&core, &shipped), Some(Pin::Shipped(&shipped[0])), "core, under the mount");
    assert_eq!(pinned(&std, &shipped), Some(Pin::Shipped(&shipped[0])), "std, a member of the library's own workspace");
    assert_eq!(pinned(&placed_member, &shipped), None, "a member under the placed directory");
    assert_eq!(pinned(&own_member, &shipped), None, "a member of the build's workspace");
    assert_eq!(pinned(&path, &shipped), None, "a path dependency");
    // Another commit's mount is another library: none of it is this tree's.
    let other = blaze_units::stdlib::library_at(OTHER_COMMIT);
    let elsewhere = package(&format!("{other}/core/Cargo.toml"), Some(format!("({other}/core)").as_str()), None, false);
    assert_eq!(pinned(&elsewhere, &shipped), None, "a library mounted for another commit");

    // The control: the list with the placed directory alone seals no crate of std.
    let unmounted = tree(None);
    assert_eq!(pinned(&core, &unmounted), None, "the planted bug sealed core: the oracle tests nothing");
    assert_eq!(pinned(&std, &unmounted), None, "the planted bug sealed std: the oracle tests nothing");
}
