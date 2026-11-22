//! The crate's promises, each with its oracle:
//!
//! - [`ReadList::under`] accepts a list exactly when a model (written apart from the code: the
//!   globs as plain text tests) admits every entry and no path was seen two ways, refuses the
//!   first inadmissible entry in the order pushed by its kind, and on success holds each sight
//!   once in canonical order;
//! - the checked list's digest does not depend on the order things were seen in;
//! - a planted bug of each kind is caught by its invariant (controls);
//! - [`Mounts::resolve`] maps spellings to a root and a canonical path, and refuses an escape.

use core::fmt;
use std::{string::String, vec::Vec as StdVec};

use blaze_own::{Alloc, Vec};
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::{
    Digest, Glob, Limits, Mount, Mounts, Outside, PathError, ReadList, ReadListOpts, RelPath, Root, RootId, RootKind, Roots,
    RootsPlant, Seen, Unmounted, normalize_into,
};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const PATHS: [&str; 10] =
    ["", "src", "src/lib.rs", "src/a/b.rs", "build.rs", "abi", "abi/x.json", "abi/x.txt", "abi/y/z.json", "README.md"];

/// The model of the package root's globs `src/**`, `build.rs`, `abi/*.json`, by plain text.
fn model_file(path: &str) -> bool {
    path.starts_with("src/")
        || path == "build.rs"
        || (path.starts_with("abi/") && !path["abi/".len()..].contains('/') && path.ends_with(".json"))
}

fn model_dir(path: &str) -> bool {
    path.is_empty() || path == "src" || path.starts_with("src/") || path == "abi"
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Tree,
    Package,
    Output,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum RootsBroken {
    /// An entry the model refuses was accepted.
    Admitted { path: String },
    /// An entry the model admits was refused, or refused for another reason or entry.
    Refused { expected: Option<(String, &'static str)>, got: Option<(String, &'static str)> },
    /// A path seen two ways was accepted.
    MovedMissed { path: String },
    /// The checked entries are not the model's sorted set.
    Entries,
    /// Two orders of one run's sights gave two digests.
    Digest,
}

impl fmt::Display for RootsBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootsBroken::Admitted { path } => write!(f, "`{path}` was accepted, and the model refuses it"),
            RootsBroken::Refused { expected, got } => write!(f, "the model expects {expected:?}, the check gave {got:?}"),
            RootsBroken::MovedMissed { path } => write!(f, "`{path}` was seen two ways and accepted"),
            RootsBroken::Entries => f.write_str("the checked entries are not the model's sorted set"),
            RootsBroken::Digest => f.write_str("two orders of one run gave two digests"),
        }
    }
}

blaze_sim::plants! {
    impl RootsPlant breaks RootsBroken {
        None,
        GlobsIgnored => RootsBroken::Admitted { .. },
        OutputReadable => RootsBroken::Admitted { .. },
        MovedKept => RootsBroken::MovedMissed { .. },
    }
}

fn why_name(why: Outside) -> &'static str {
    match why {
        Outside::UnknownRoot { .. } => "unknown root",
        Outside::NotDeclared { .. } => "not declared",
        Outside::OutputRead { .. } => "output read",
        Outside::Moved { .. } => "moved",
    }
}

fn seen_of(tape: &Tape) -> Seen {
    match tape.int_in(0u8..=4, "seen") {
        0 => Seen::Absent,
        1 => Seen::Listing { digest: Digest([1; 32]) },
        4 => Seen::Dir,
        n => Seen::File { digest: Digest([n; 32]), len: u64::from(n) },
    }
}

fn under_roots(tape: &Tape, plant: impl Planted<RootsPlant>) -> Result<(), Broken<RootsBroken>> {
    let globs = [Glob::new("src/**").unwrap(), Glob::new("build.rs").unwrap(), Glob::new("abi/*.json").unwrap()];
    let kinds = tape.vec(1..=3, "roots", |t| match t.int_in(0u8..=2, "kind") {
        0 => Kind::Tree,
        1 => Kind::Package,
        _ => Kind::Output,
    });
    // At most one output: a second is a package.
    let mut seen_output = false;
    let kinds: StdVec<Kind> = kinds
        .into_iter()
        .map(|kind| match kind {
            Kind::Output if seen_output => Kind::Package,
            Kind::Output => {
                seen_output = true;
                Kind::Output
            }
            other => other,
        })
        .collect();
    let root_list: StdVec<Root<'_>> = kinds
        .iter()
        .map(|kind| Root {
            name: "drawn",
            kind: match kind {
                Kind::Tree => RootKind::Tree { tree: Digest([7; 32]) },
                Kind::Package => RootKind::Package { globs: &globs },
                Kind::Output => RootKind::Output,
            },
        })
        .collect();
    let roots = Roots::new(&root_list).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
    let reads = tape.vec(0..=8, "reads", |t| {
        let root = t.int_in(0u8..=3, "root");
        let path = PATHS[t.index(PATHS.len(), "path")];
        (root, path, seen_of(t))
    });
    // A path seen twice is mostly seen the same way: a move is rare.
    let mut reads = reads;
    for at in 1..reads.len() {
        if tape.coin("again") {
            let (root, path, seen) = reads[at - 1];
            reads[at] = (root, path, if tape.one_in(4, "moved") { seen_of(tape) } else { seen });
        }
    }
    // The model: the first inadmissible entry in the order pushed, else a move, else the set.
    let admits = |root: u8, path: &str, seen: Seen| -> Result<(), &'static str> {
        match kinds.get(usize::from(root)) {
            None => Err("unknown root"),
            Some(Kind::Output) => Err("output read"),
            Some(Kind::Tree) => Ok(()),
            Some(Kind::Package) => {
                let ok = match seen {
                    Seen::File { .. } => model_file(path),
                    Seen::Absent => model_file(path) || model_dir(path),
                    Seen::Listing { .. } | Seen::Dir => model_dir(path),
                };
                if ok { Ok(()) } else { Err("not declared") }
            }
        }
    };
    let expected_refusal = reads.iter().find_map(|&(root, path, seen)| admits(root, path, seen).err().map(|why| (String::from(path), why)));
    let mut set: StdVec<(u8, &str, Seen)> = reads.clone();
    set.sort_by(|a, b| (a.0, a.1.as_bytes()).cmp(&(b.0, b.1.as_bytes())));
    // Two sights of one path agree when equal, or when one lists the directory the other saw.
    let agree = |a: Seen, b: Seen| a == b || matches!((a, b), (Seen::Listing { .. }, Seen::Dir) | (Seen::Dir, Seen::Listing { .. }));
    let moved = set
        .iter()
        .enumerate()
        .find(|&(at, a)| set[..at].iter().any(|b| b.0 == a.0 && b.1 == a.1 && !agree(a.2, b.2)))
        .map(|(_, a)| String::from(a.1));
    let mut list = ReadList::new(ReadListOpts { limits: Limits::SHIPPED, alloc: heap() });
    for &(root, path, seen) in &reads {
        list.push(RootId(root), RelPath::new(path, 4096).unwrap(), seen).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
    }
    let got = list.under_planted(&roots, plant);
    match (expected_refusal, &moved, got) {
        (Some(expected), _, Err(refused)) => {
            let got = (String::from(refused.entry().path.as_str()), why_name(refused.why()));
            if got != expected {
                return Err(Broken::Oracle(RootsBroken::Refused { expected: Some(expected), got: Some(got) }));
            }
        }
        (Some((path, _)), _, Ok(_)) => return Err(Broken::Oracle(RootsBroken::Admitted { path })),
        (None, Some(path), Ok(_)) => return Err(Broken::Oracle(RootsBroken::MovedMissed { path: path.clone() })),
        (None, Some(_), Err(refused)) if matches!(refused.why(), Outside::Moved { .. }) => {}
        (None, _, Err(refused)) => {
            let got = Some((String::from(refused.entry().path.as_str()), why_name(refused.why())));
            return Err(Broken::Oracle(RootsBroken::Refused { expected: None, got }));
        }
        (None, None, Ok(checked)) => {
            // A listing covers a sight of the same directory; then each path once.
            let listed: StdVec<(u8, &str)> = set.iter().filter(|e| matches!(e.2, Seen::Listing { .. })).map(|e| (e.0, e.1)).collect();
            set.retain(|e| e.2 != Seen::Dir || !listed.contains(&(e.0, e.1)));
            set.dedup();
            let entries: StdVec<(u8, String, Seen)> = checked.entries().map(|e| (e.root.0, String::from(e.path.as_str()), e.seen)).collect();
            let model: StdVec<(u8, String, Seen)> = set.iter().map(|&(r, p, s)| (r, String::from(p), s)).collect();
            if entries != model {
                return Err(Broken::Oracle(RootsBroken::Entries));
            }
            // The same sights pushed in reverse give the same digest.
            let mut again = ReadList::new(ReadListOpts { limits: Limits::SHIPPED, alloc: heap() });
            for &(root, path, seen) in reads.iter().rev() {
                again.push(RootId(root), RelPath::new(path, 4096).unwrap(), seen).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
            }
            let again = again.under(&roots).map_err(|e| Broken::Harness(std::format!("{e:?}")))?;
            if again.digest() != checked.digest() {
                return Err(Broken::Oracle(RootsBroken::Digest));
            }
        }
    }
    Ok(())
}

#[test]
fn a_read_list_is_under_its_roots_exactly_when_the_model_says() {
    check(Config::cases(2000), |tape| under_roots(tape, NoPlant));
}

#[test]
fn every_roots_plant_is_caught_by_its_invariant() {
    controls(Config::cases(2000), |tape, plant: RootsPlant| under_roots(tape, plant));
}

#[test]
fn paths_have_one_spelling() {
    assert_eq!(RelPath::new("a/b", 64).map(RelPath::as_str), Ok("a/b"));
    assert_eq!(RelPath::new("/a", 64), Err(PathError::Absolute));
    assert_eq!(RelPath::new("a//b", 64), Err(PathError::EmptySegment));
    assert_eq!(RelPath::new("a/", 64), Err(PathError::EmptySegment));
    assert_eq!(RelPath::new("a/../b", 64), Err(PathError::DotSegment));
    assert_eq!(RelPath::new("abcd", 3), Err(PathError::TooLong { most: 3 }));
    assert_eq!(RelPath::from_bytes(b"a\xff", 64), Err(PathError::NotUtf8));
    let mut out = Vec::new_in(heap());
    assert_eq!(normalize_into(b"a/./b//c/../d", 64, &mut out), Ok(()));
    assert_eq!(out.as_slice(), b"a/b/d");
    assert_eq!(normalize_into(b"a/../..", 64, &mut out), Err(PathError::Escapes));
}

#[test]
fn mounts_resolve_by_the_longest_spelling_and_refuse_an_escape() {
    let globs = [Glob::new("**").unwrap()];
    let list = [
        Root { name: "workspace", kind: RootKind::Package { globs: &globs } },
        Root { name: "member", kind: RootKind::Tree { tree: Digest([2; 32]) } },
        Root { name: "out", kind: RootKind::Output },
    ];
    let roots = Roots::new(&list).unwrap();
    let mounts = [
        Mount { spelling: b"/workspace", root: RootId(0) },
        Mount { spelling: b"/workspace/member", root: RootId(1) },
        Mount { spelling: b"/out", root: RootId(2) },
    ];
    let mounts = Mounts::new(&mounts, Some(b"/workspace/member"), &roots).unwrap();
    let mut out = Vec::new_in(heap());
    assert_eq!(mounts.resolve(b"/workspace/member/src/lib.rs", 64, &mut out), Ok(RootId(1)));
    assert_eq!(out.as_slice(), b"src/lib.rs");
    assert_eq!(mounts.resolve(b"abi/../Cargo.toml", 64, &mut out), Ok(RootId(1)));
    assert_eq!(out.as_slice(), b"Cargo.toml");
    assert_eq!(mounts.resolve(b"../Cargo.lock", 64, &mut out), Ok(RootId(0)));
    assert_eq!(out.as_slice(), b"Cargo.lock");
    assert_eq!(mounts.resolve(b"/workspace", 64, &mut out), Ok(RootId(0)));
    assert_eq!(out.as_slice(), b"");
    assert_eq!(mounts.resolve(b"/workspacex/a", 64, &mut out), Err(Unmounted::Outside));
    assert_eq!(mounts.resolve(b"/etc/passwd", 64, &mut out), Err(Unmounted::Outside));
    assert_eq!(mounts.resolve(b"../../../etc/passwd", 64, &mut out), Err(Unmounted::Path(PathError::Escapes)));
}

#[test]
fn globs_match_by_segments() {
    let path = |p| RelPath::new(p, 64).unwrap();
    let g = Glob::new("src/**/*.rs").unwrap();
    assert!(g.matches(path("src/lib.rs")));
    assert!(g.matches(path("src/a/b/c.rs")));
    assert!(!g.matches(path("src/a/b/c.txt")));
    assert!(!g.matches(path("other/lib.rs")));
    assert!(g.reaches(path("src/a")));
    assert!(!g.reaches(path("other")));
    assert_eq!(Glob::new("a**"), Err(crate::GlobError::MixedDoubleStar));
    assert_eq!(Glob::new("../x"), Err(crate::GlobError::NotRelative));
}
