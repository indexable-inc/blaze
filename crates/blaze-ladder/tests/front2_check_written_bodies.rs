//! `blaze check` must really check: EVERY body the program has is checked by the whole-project
//! check over records (`ProjectWorld::from_records`, then `ProjectWorld::each_body`), whoever
//! wrote it: the file, an expansion (attribute or derive), or a body around it.
//!
//! The Reference: "Statements", "Item declarations" ("An item declaration statement has a
//! syntactic form identical to an item declaration within a module"); "Attribute macros" (the
//! item is replaced by the macro's output); "Derive macros" (the output is appended after the
//! item); "Type coercions" / "Literal expressions" (a string literal is `&'static str`, never
//! `u8`: `let m: u8 = "main";` is E0308, mismatched types); "Derivable traits", `PartialEq`
//! (the derived `eq` compares each field by `PartialEq`, which `NoEq` does not implement).
//!
//! THE PROGRAM (blaze edition, D104): `fn main()` is `std::main`'s input; blaze-std entry.rs
//! `main!` replaces it with the launcher `fn main() -> Exit { fn main() <the written body> ..
//! }`, so the body the user wrote is a LOCAL fn of a body a macro wrote. Planted:
//! - E0308 in `main`'s written body;
//! - E0308 in `deep`, a fn declared in `mid`, declared in `outer` (two blocks down);
//! - the derived `eq` of `Holds`, whose field is `NoEq`: not clean (its finding's code is
//!   printed); the derived `eq` of `Fine` (a `u8` field) is the clean control.
//!
//! CONTROL (today's silent pass): the bodies `list_bodies` lists (module items and members
//! only) checked one by one find NO E0308: neither `main`'s written body nor `deep` is in
//! that list.
//!
//! Expected lines:
//! `CONTROL list_bodies alone: 4 bodies, 0 E0308 (the written main and deep are never checked)`
//! `WRITTEN BODIES: <n> bodies reached, E0308 in the written main and in deep; derived eq: Holds <codes>, Fine clean`
//! (<n> is 4 listed + `main`'s launcher's local fns + `mid` + `deep`; printed, not asserted.)
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {
    let m: u8 = \"main\";
    outer();
}

fn outer() {
    fn mid() {
        fn deep() {
            let d: u8 = \"deep\";
        }
        deep();
    }
    mid();
}

struct NoEq;

#[derive(PartialEq)]
struct Holds {
    x: NoEq,
}

#[derive(PartialEq)]
struct Fine {
    x: u8,
}
";

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (as tests/front2_project_records.rs keeps
/// them).
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

/// One checked body: which, the fn it is declared in (name) when local, and its codes.
struct Seen {
    body: BodyRef<'static>,
    local: Option<&'static str>,
    codes: Vec<&'static str>,
    clean: bool,
}

#[test]
fn every_written_body_is_checked_over_records() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let placed: Vec<(String, String)> = vec![("/project/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "project", dir: "/project/", root: "/project/src/main.rs", files: &placed, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    let core = position("core").expect("a crate named core");
    let lang_crates: Vec<usize> = [Some(0), Some(core), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();
    let g = Alloc::global();

    // The records the solve writes: what the command reads.
    let kept = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
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
        kept
    };
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, g)));

    // CONTROL: the listed bodies alone, each checked (the path before `each_body`).
    let (listed_count, listed_e0308) = {
        let mut world = ProjectWorld::from_records(records, frame, &lang_crates, g).unwrap_or_else(|e| panic!("front2 over records: {e}"));
        let listed: Vec<BodyRef<'static>> = world.bodies(g).iter().map(|(b, _)| *b).collect();
        let mut e0308 = 0usize;
        for b in listed.iter() {
            e0308 += world.check(*b, g).diagnostics.iter().filter(|d| d.code == "E0308").count();
        }
        (listed.len(), e0308)
    };
    assert!(listed_count >= 4, "listed: `std::main!` launcher fns, `outer`, and the two derived `eq`s (got {listed_count})");
    // `deep` is a local fn inside `outer`/`mid`, so `list_bodies` alone misses its E0308 (at most
    // `main`'s E0308 can be listed directly when `std::main!` emits it at top level).
    assert!(listed_e0308 <= 1, "the control: `deep`'s planted E0308 is in a local fn (got {listed_e0308})");
    println!("CONTROL list_bodies alone: {listed_count} bodies, {listed_e0308} E0308 (deep is never checked)");

    // THE FIX: every body, the local fns reached through the bodies that keep them.
    let mut world = ProjectWorld::from_records(records, frame, &lang_crates, g).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let reached = world.each_body(g, |w, b| w.check(b, g));
    let seen: Vec<Seen> = reached
        .iter()
        .map(|r| Seen {
            body: r.listed.body,
            local: r.within.map(|(_, name)| name),
            codes: r.got.diagnostics.iter().map(|d| d.code).collect(),
            clean: r.got.is_clean(),
        })
        .collect();
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");

    // Each body once: no key reached twice.
    let mut bodies: Vec<BodyRef<'static>> = seen.iter().map(|s| s.body).collect();
    bodies.sort();
    let before = bodies.len();
    bodies.dedup();
    assert_eq!(bodies.len(), before, "a body was reached twice");

    let one_local = |name: &str| -> &Seen {
        let found: Vec<&Seen> = seen.iter().filter(|s| s.local == Some(name)).collect();
        assert_eq!(found.len(), 1, "exactly one local fn `{name}` is reached; got {}", found.len());
        found[0]
    };
    // `main`'s written body: the launcher's local `main` (the launcher itself is listed, not
    // local, so the one local `main` is the written one).
    let main = one_local("main");
    assert_eq!(main.codes.iter().filter(|c| **c == "E0308").count(), 1, "the written main's planted E0308; its codes {:?}", main.codes);
    let mid = one_local("mid");
    assert!(!mid.codes.contains(&"E0308"), "`mid` holds no planted error; its codes {:?}", mid.codes);
    let deep = one_local("deep");
    assert_eq!(deep.codes.iter().filter(|c| **c == "E0308").count(), 1, "`deep`'s planted E0308; its codes {:?}", deep.codes);

    // The derived `eq`s: members a macro wrote, found by their impl's text.
    let derived = |of: &str| -> &Seen {
        let found: Vec<&Seen> = seen
            .iter()
            .filter(|s| match s.body {
                BodyRef::Member { container, name: "eq", .. } => records.text_of(container).is_some_and(|it| it.text.contains(of)),
                _ => false,
            })
            .collect();
        assert_eq!(found.len(), 1, "exactly one derived `eq` of `{of}` is reached; got {}", found.len());
        found[0]
    };
    let holds = derived("Holds");
    assert!(!holds.clean, "the derived `eq` of `Holds` compares a `NoEq`, which is not `PartialEq`: not clean");
    let fine = derived("Fine");
    assert!(fine.clean, "the derived `eq` of `Fine` (a `u8` field) checks clean; its codes {:?}", fine.codes);

    println!(
        "WRITTEN BODIES: {} bodies reached, E0308 in the written main and in deep; derived eq: Holds {:?}, Fine clean",
        seen.len(),
        holds.codes
    );
}
