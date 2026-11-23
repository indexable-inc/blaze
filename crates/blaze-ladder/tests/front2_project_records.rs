//! THE PROJECT WORLD OVER RECORDS (drive.md, "End state: the whole pipeline on the command",
//! piece 5): `ProjectWorld::from_records` gives, for every body of a project, the verdict
//! `ProjectWorld::new` gives over the solved names, and a body is found by the identity the
//! command names it by, (its file, its item chain).
//!
//! What is compared per body ([`verdict_text`]): its diagnostics (code, item-relative range,
//! text), how many gaps it has (a gap's text holds a printed place, which the two sources spell
//! differently: tests/front2_records.rs `gap_identity`), its refusal's class, the digest of its
//! read list and the read list itself (front2 reads.rs: every fact and answer digest).
//!
//! The chain is `blaze_file_items::item_chain` (the chain's one home). PLANTED BUG: the index
//! made with a chain that leaves out the entries an item is written in (its impl's part): the
//! two members `A::get` and `B::get` then have one chain, and the index must refuse them
//! (`ChainError::Collision`), never answer one of the two.
//!
//! Expected lines of `a_world_over_records_checks_every_body_as_the_solved_names_do`:
//! `PROJECT RECORDS: 4 bodies, every verdict equal over both sources`
//! `BODY IDENTITY: 3 bodies found by (file, chain) over both sources, 1 unplaced (main written by an expansion)`
//!
//! D104: the root's `main` is `std::main`'s input, and bstd's `main!` replaces it with the
//! launcher `fn main() -> Exit`, text a macro wrote: it has no entry in the file's outline and is
//! the one body counted unplaced (front2/project_records.rs: such a body is named by its call's
//! expansion, which the (file, chain) identity does not cover yet). Unplaced is not unchecked:
//! `ProjectWorld::each_body` checks the launcher and, inside it, the written `main` (a local fn
//! of the launcher's body): tests/front2_check_written_bodies.rs.
//! `PLANT caught: a chain without its impl's part: ... have one item chain ...`
//!
//! `every_rung_body_checks_the_same_over_records` does the same for every program under
//! `crates/blaze-cli/tests/ladder/front2/` (the piece's "every rung body"). It is `#[ignore]`:
//! each of the 124 rungs solves its whole crate graph (bstd and core) first, minutes in all.
//! Run it with `-- --ignored`; it prints one `RUNG <name>: <n> bodies equal` line per rung and
//! `RUNGS: <n> rungs, <m> bodies, every verdict equal over both sources`.

#![feature(allocator_api)]

// The rungs' helper also holds what only the execution tests use.
#[allow(dead_code)]
mod rungs;

use std::collections::HashMap;

use blaze_file_items::{CHAIN_DOMAIN, Outline, head_of};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::front2::project_records::ChainError;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::resolve::{At, aligned};
use blaze_scope::{items_record, names_record};
use blaze_surface::Item;

const EDITION: &str = "blaze-2026.11";

/// Four bodies: `main` (D104: std::main's launcher, which a macro wrote), `bad` (one E0308), and
/// two members of one name in two impls.
const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}

struct A;
struct B;

impl A {
    fn get(&self) -> u8 {
        1
    }
}

impl B {
    fn get(&self) -> u8 {
        2
    }
}

fn bad() {
    let x: u8 = \"a\";
}
";

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve: the test's effect layer (as
/// tests/front2_records.rs keeps them).
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, (&'static str, &'static [u8])>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let (text, bytes) = self.expansions.get(krate)?.get(&call).copied()?;
        let origins = if bytes.is_empty() {
            (&[][..], &[][..])
        } else {
            blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("origins of {call:?}: {e}"))
        };
        Some(LoadedExpansion { text, origins: Some(origins) })
    }
}

/// A verdict as text, for comparing the two sources (module docs).
fn verdict_text(v: &BodyVerdict<'_>) -> String {
    // The per-crate bucket rows (`FactKind::CrateBucket`) exist over records only (the solved
    // names read no bucket), and the list's digest covers them: both are left out, and every
    // other row (the checker's questions and answers) must be equal.
    let facts = v.facts.as_ref().map(|list| {
        let rows: Vec<_> = list.rows.iter().filter(|row| row.fact.kind != blaze_ladder::front2::reads::FactKind::CrateBucket).copied().collect();
        (list.reach, rows)
    });
    format!("diagnostics {:?}; gaps {}; refused {:?}; finished {}; facts {:?}", v.diagnostics, v.gaps.len(), v.refused.as_ref().map(|r| r.text()), v.reads.is_some(), facts)
}

/// The planted bug: an entry's chain from its own part alone, without the entries it is
/// written in.
fn chain_without_parents(_outline: &Outline<'_>, item: &Item, text: &str) -> ContentId {
    let mut h = Hasher::new(CHAIN_DOMAIN);
    let name = item.name.map_or("", |range| item.text(text, range));
    h.update(&[item.kind().byte()]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&item.disambiguator.to_le_bytes());
    if item.name.is_none() {
        h.update(&head_of(item).0);
    }
    h.finish()
}

/// What one project gave.
struct Compared {
    bodies: usize,
    unplaced: u32,
    /// Each unplaced body over the solved names: `<name> written by an expansion` when its place
    /// is an expansion, else `<body> at <place>` (which no listed body should be).
    unplaced_bodies: Vec<String>,
    /// The planted index's refusal, as text; `None` when the planted chain collided nothing.
    planted: Option<String>,
}

/// Checks every body of the project `files` (path relative to its directory, text; its root
/// `src/main.rs`) over the solved names and over the records written from them, and finds
/// each by (file, chain) over both. Panics, naming the body, where the two differ.
fn compare(label: &str, files: &[(String, String)]) -> Compared {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let placed: Vec<(String, String)> = files.iter().map(|(path, text)| (format!("/project/{path}"), text.clone())).collect();
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "project", dir: "/project/", root: "/project/src/main.rs", files: &placed, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("{label}: the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    let core = position("core").expect("a crate named core");
    let lang_crates: Vec<usize> = [Some(0), Some(core), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();
    let g = Alloc::global();

    // Over the solved names: every verdict, the bodies by chain, the planted index, and the
    // records written from the solve. Nothing else of the solved names leaves this block.
    let (kept, solved_bodies, solved_verdicts, solved_rows, unplaced, unplaced_bodies, planted) = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{label}: resolve_crates: {e}"));
        let sources: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let mut world = ProjectWorld::new(&names, &frame, &sources, g).unwrap_or_else(|e| panic!("{label}: front2: {e}"));
        let bodies: Vec<String> = world.bodies(g).iter().map(|(b, at)| format!("{b:?} at {at:?}")).collect();
        let listed: Vec<BodyRef<'_>> = world.bodies(g).iter().map(|(b, _)| *b).collect();
        let verdicts: Vec<String> = listed.iter().map(|b| verdict_text(&world.check(*b, g))).collect();
        let (rows, unplaced) = {
            let index = world.chains().unwrap_or_else(|e| panic!("{label}: the solved bodies by chain: {e}"));
            (index.rows().to_vec(), index.unplaced())
        };
        for (file, chain, body) in rows.iter() {
            let found = world.body_by_chain(*file, *chain).unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(found, Some(*body), "{label}: the body at file {file} chain {chain:?} over the solved names");
        }
        assert_eq!(rows.len() + unplaced as usize, listed.len(), "{label}: every listed body is indexed or counted unplaced");
        // The unplaced bodies by name and place: a body with no outline entry must be one a macro
        // wrote (its place an expansion), never one of the file's that lost its entry.
        let unplaced_bodies: Vec<String> = world
            .listed(g)
            .iter()
            .filter(|l| l.entry.is_none())
            .map(|l| match l.body {
                BodyRef::Free(key) => {
                    let name = (&names).name_of(key).unwrap_or_else(|| panic!("{label}: the unplaced body {key:x} has no name"));
                    match (&names).place_of(key).map(|p| p.at) {
                        Some(At::Expansion(_)) => format!("{name} written by an expansion"),
                        other => format!("{name} at {other:?}"),
                    }
                }
                member => format!("{member:?}"),
            })
            .collect();
        let planted = match world.index_with(&chain_without_parents) {
            Err(e @ ChainError::Collision { .. }) => Some(e.to_string()),
            Err(e) => panic!("{label}: the planted index failed another way: {e}"),
            Ok(_) => None,
        };
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            // By each file's path in its package: what the items record asks for (D96).
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(
                c.expansions
                    .iter()
                    .filter_map(|(call, e)| {
                        let m = e.made?;
                        let bytes: &'static [u8] = match &m.origins {
                            Ok(b) => aligned(b, blaze_own::testing::bump()),
                            Err(blaze_scope::expansion_origins::OriginsError::Rewritten { .. } | blaze_scope::expansion_origins::OriginsError::Unplaced { .. }) => &[],
                            Err(err) => panic!("origins of {call:x}: {err:?}"),
                        };
                        Some((*call, (leak(m.text), bytes)))
                    })
                    .collect(),
            );
        }
        let shown: Vec<String> = rows.iter().map(|(file, chain, body)| format!("{file} {chain:?} {body:?}")).collect();
        (kept, bodies, verdicts, shown, unplaced, unplaced_bodies, planted)
    };

    // Over the records alone.
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, g)));
    let mut world = ProjectWorld::from_records(records, frame, &lang_crates, g).unwrap_or_else(|e| panic!("{label}: front2 over records: {e}"));
    let bodies: Vec<String> = world.bodies(g).iter().map(|(b, at)| format!("{b:?} at {at:?}")).collect();
    assert_eq!(bodies, solved_bodies, "{label}: the bodies listed over records");
    let listed: Vec<BodyRef<'static>> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    for (i, body) in listed.iter().enumerate() {
        let got = verdict_text(&world.check(*body, g));
        let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{label}: body {i} ({body:?}): the records filed {errors:?}");
        // Compared by a bool so a failure prints where the texts part, not two whole verdicts.
        let parts = got.bytes().zip(solved_verdicts[i].bytes()).position(|(a, b)| a != b).unwrap_or(got.len().min(solved_verdicts[i].len()));
        let from = (0..=parts.saturating_sub(60)).rev().find(|at| got.is_char_boundary(*at) && solved_verdicts[i].is_char_boundary(*at)).unwrap_or(0);
        let shown = |text: &str| text[from..].chars().take(200).collect::<String>();
        assert!(
            got == solved_verdicts[i],
            "{label}: body {i} ({body:?}): over records its verdict differs from the solved names' at byte {parts}: records `{}` solved `{}`",
            shown(&got),
            shown(&solved_verdicts[i])
        );
    }
    let rows = world.chains().unwrap_or_else(|e| panic!("{label}: the bodies by chain over records: {e}")).rows().to_vec();
    let shown: Vec<String> = rows.iter().map(|(file, chain, body)| format!("{file} {chain:?} {body:?}")).collect();
    assert_eq!(shown, solved_rows, "{label}: the bodies by (file, chain) over records");
    for (file, chain, body) in rows.iter() {
        let found = world.body_by_chain(*file, *chain).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(found, Some(*body), "{label}: the body at file {file} chain {chain:?} over records");
    }
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{label}: the records filed {errors:?}");
    Compared { bodies: listed.len(), unplaced, unplaced_bodies, planted }
}

#[test]
fn a_world_over_records_checks_every_body_as_the_solved_names_do() {
    let files = [("src/main.rs".to_string(), MAIN.to_string())];
    let got = compare("hello", &files);
    assert_eq!(got.bodies - got.unplaced as usize, 3, "three file-written bodies: `bad`, `A::get`, and `B::get`");
    assert!(got.unplaced >= 1, "at least one unplaced body from std::main's expansion");
    assert!(
        got.unplaced_bodies.iter().all(|s| s.ends_with(" written by an expansion")),
        "every unplaced body is written by std::main's expansion: {:?}",
        got.unplaced_bodies
    );
    println!("PROJECT RECORDS: {} bodies, every verdict equal over both sources", got.bodies);
    println!(
        "BODY IDENTITY: {} bodies found by (file, chain) over both sources, {} unplaced ({})",
        got.bodies - got.unplaced as usize,
        got.unplaced,
        got.unplaced_bodies.join(", ")
    );
    let planted = got.planted.expect("the planted bug (a chain without its impl's part) was not caught: `A::get` and `B::get` did not collide");
    assert!(planted.contains("have one item chain"), "the refusal names the collision: {planted}");
    println!("PLANT caught: a chain without its impl's part: {planted}");
}

#[test]
#[ignore = "124 rungs, each solving its whole crate graph (bstd and core) before its bodies are checked twice: minutes; run with -- --ignored"]
fn every_rung_body_checks_the_same_over_records() {
    let ladder = rungs::ladder().join("front2");
    let mut names: Vec<String> = std::fs::read_dir(&ladder).expect("the rungs' directory").map(|e| e.expect("a directory entry").file_name().to_str().expect("UTF-8").to_owned()).collect();
    names.sort();
    let (mut count, mut bodies) = (0, 0);
    for name in names.iter() {
        let dir = ladder.join(name);
        if !dir.join("src").join("main.rs").is_file() {
            continue;
        }
        let got = compare(name, &rungs::sources(&dir));
        println!("RUNG {name}: {} bodies equal", got.bodies);
        count += 1;
        bodies += got.bodies;
    }
    println!("RUNGS: {count} rungs, {bodies} bodies, every verdict equal over both sources");
}
