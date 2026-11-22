//! A served tree's reads stay inside it (E1, 2026-11-20): the root is resolved once
//! ([`super::RealRoot`]) and each node by one `realpath` of its absolute path and a prefix check
//! against that root. A link out of the tree, by a file, a folder or a link deeper down, is
//! refused; a link that stays inside is served. The planted control removes the prefix check, and
//! the oracle must see a file outside the tree served.

use std::fmt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use blaze_sim::{Broken, Config, Tape};

use super::{RealRoot, StartPlant, TreeFault, TreePlant, disk_node};

/// What the oracle saw broken.
#[derive(Debug)]
enum EscapeBroken {
    /// A path whose target is outside the tree was served.
    Served { rel: &'static str },
}

impl fmt::Display for EscapeBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EscapeBroken::Served { rel } => write!(f, "`{rel}` leads out of the served tree and was served"),
        }
    }
}

blaze_sim::plants! {
    impl TreePlant breaks EscapeBroken {
        None,
        PrefixUnchecked => EscapeBroken::Served { .. },
    }
}

/// Where a path of the fixture leads.
#[derive(Clone, Copy, Debug)]
enum Leads {
    Inside,
    Outside,
    Absent,
}

/// The fixture's paths, each with where it leads.
const ASKED: [(&str, Leads); 7] = [
    ("in/a.txt", Leads::Inside),
    ("ok", Leads::Inside),
    ("in", Leads::Inside),
    ("leak", Leads::Outside),
    ("up/secret", Leads::Outside),
    ("d/deep", Leads::Outside),
    ("missing", Leads::Absent),
];

/// `<tmp>/<name>/outside/secret` and `<tmp>/<name>/tree` with its links; the tree's folder.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-escape-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tree = dir.join("tree");
    std::fs::create_dir_all(dir.join("outside")).expect("the outside folder");
    std::fs::write(dir.join("outside/secret"), b"secret").expect("the outside file");
    std::fs::create_dir_all(tree.join("in")).expect("a folder inside");
    std::fs::create_dir_all(tree.join("d")).expect("a folder inside");
    std::fs::write(tree.join("in/a.txt"), b"inside").expect("a file inside");
    symlink("in/a.txt", tree.join("ok")).expect("a link inside");
    symlink("../outside/secret", tree.join("leak")).expect("a file link out");
    symlink("../outside", tree.join("up")).expect("a folder link out");
    symlink("../../outside/secret", tree.join("d/deep")).expect("a deeper link out");
    tree
}

fn asks(tape: &Tape, root: &RealRoot, plant: TreePlant) -> Result<(), Broken<EscapeBroken>> {
    let count = tape.below(6, "asks") + 1;
    for _ in 0..count {
        let Some(&(rel, leads)) = tape.pick(&ASKED, "a path") else { return Err(Broken::Harness("no path to pick".to_owned())) };
        match (leads, disk_node(root, rel, plant)) {
            (Leads::Inside, Ok(Some(_))) | (Leads::Absent, Ok(None)) | (Leads::Outside, Err(TreeFault::Outside(_))) => {}
            (Leads::Outside, Ok(Some(_))) => return Err(Broken::Oracle(EscapeBroken::Served { rel })),
            (leads, other) => return Err(Broken::Harness(format!("`{rel}` ({leads:?}): {other:?}"))),
        }
    }
    Ok(())
}

/// A link out of the tree is refused and one inside it is served; with the prefix check
/// removed, a file outside is served and the oracle names it.
#[test]
fn a_link_out_of_the_served_tree_is_refused() {
    let tree = fixture("links");
    let root = RealRoot::resolve(&tree).expect("the fixture's root resolves");
    blaze_sim::controls(Config::cases(64), |tape, plant: TreePlant| asks(tape, &root, plant));
    std::fs::remove_dir_all(tree.parent().expect("the fixture's folder")).expect("the fixture removed");
}

/// The root is taken as given only when absolute: a relative one is refused by name, never
/// resolved against the process's folder (rule 75).
#[test]
fn a_relative_root_is_refused() {
    assert!(matches!(RealRoot::resolve(std::path::Path::new(".")), Err(super::RootUnresolved::Relative(_))));
}

/// What the oracle saw broken at the command's edge.
#[derive(Debug)]
enum StartBroken {
    /// The checkout's root was left relative.
    Relative { checkout: &'static str },
}

impl fmt::Display for StartBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartBroken::Relative { checkout } => write!(f, "the checkout `{checkout}` was left relative: its root is not absolute and its files are not served"),
        }
    }
}

blaze_sim::plants! {
    impl StartPlant breaks StartBroken {
        None,
        LeftRelative => StartBroken::Relative { .. },
    }
}

/// Checkouts relative to the folder the test runs in (cargo runs a crate's tests in its
/// folder), each with a file beneath it.
const CHECKOUTS: [(&str, &str); 3] = [(".", "src/script_sandbox.rs"), ("src", "script_sandbox.rs"), ("src/script_sandbox", "escape_tests.rs")];

/// The edge's one resolution ([`RealRoot::of_start`]) takes the checkout as the command was
/// given it, relative to where it started, and reads no environment variable: it has none to
/// read (no `PWD`: `env -i` leaves it unset). The root it makes is absolute and the checkout's
/// files are served at it. The control leaves the root relative, and the oracle sees it.
#[test]
fn the_checkout_root_is_resolved_once_with_no_environment() {
    blaze_sim::controls(Config::cases(16), |tape: &Tape, plant: StartPlant| {
        let Some(&(checkout, file)) = tape.pick(&CHECKOUTS, "a checkout") else { return Err(Broken::Harness("no checkout to pick".to_owned())) };
        let root = RealRoot::of_start(std::path::Path::new(checkout), plant).map_err(|error| Broken::Harness(format!("`{checkout}`: {error}")))?;
        if !root.path().is_absolute() {
            return Err(Broken::Oracle(StartBroken::Relative { checkout }));
        }
        match disk_node(&root, file, blaze_own::planted::NoPlant) {
            Ok(Some(_)) => Ok(()),
            other => Err(Broken::Harness(format!("`{checkout}`: `{file}` not served: {other:?}"))),
        }
    });
}
