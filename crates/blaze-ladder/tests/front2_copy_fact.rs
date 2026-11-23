//! Lane CP: hello world's borrows are checked. `blaze check` of the owner's hello world (03:29,
//! 2026-11-17) printed `note[borrows-not-checked]`: "a trait fact of an ADT is not known:
//! FactUnknown { adt: .., fact: \"Copy\" }": the borrow check's facts (front2 borrows.rs
//! `gather_facts`) ask the selection `ImplsOf(Copy, Adt(..))` per ADT of the body, and it gave
//! no answer. Over the solved names the same body's facts are all known; the command reads
//! records (`ProjectWorld::from_records`, the impl index filed lazily from each crate's impl
//! rows), so this test checks over records, as the command does.
//!
//! The rule kept: "does T implement Tr" is answered NO only when every candidate impl was
//! read; a refused candidate makes the answer unknown (the refusal names it,
//! `ImplsUnknown::RefusedCandidate`).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::{BorrowRefused, ImplsUnknown};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::expansion_origins::OriginsError;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};

const EDITION: &str = "blaze-2026.11";

/// The owner's hello world as he wrote it (the blaze edition expands its `fn main` by
/// `std::main` unasked).
const HELLO: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (tests/front2_project_records.rs's loader).
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    origins: Vec<HashMap<ItemKey, &'static [u8]>>,
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
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none). None kept for the call: none served.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

/// The origin column an expansion's text keeps (its tokens' marks: a derive's `$crate` is read
/// at the derive's crate): empty for a text that keeps none by design.
fn made_origins(call: ItemKey, origins: &Result<&[u8], OriginsError>) -> &'static [u8] {
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

/// Every body of the program `text` checked for borrows over its records: per body its name
/// and `clean`, its findings' codes, or its refusal (a `FactUnknown` with the ADT's and the
/// refused candidate impl's texts).
fn borrows_over_records(text: &str, edition_name: &str) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let placed = [("/project/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(edition_name).expect("a known edition");
    let user = UserCrate { name: "project", dir: "/project/", root: "/project/src/main.rs", files: &placed, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    let core = position("core").expect("a crate named core");
    let lang_crates: Vec<usize> = [Some(0), Some(core), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();
    let g = Alloc::global();
    let kept = {
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), origins: Vec::new() };
        for c in names.crates().iter() {
            kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode_with(c, &names, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
            kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
            kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
            kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
        }
        kept
    };
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, g)));
    let mut world = ProjectWorld::from_records(records, frame, &lang_crates, g).unwrap_or_else(|e| panic!("front2 over records: {e}"));
    let shown = |k: ItemKey| -> String {
        let krate = records.crate_of(k).and_then(|c| crate_names.get(c).copied()).unwrap_or("?");
        let text = records.text_of(k).map(|t| t.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect::<String>()).unwrap_or_else(|| String::from("(no text)"));
        format!("{krate}: `{text}`")
    };
    let listed: Vec<BodyRef<'static>> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    let mut out = Vec::new();
    for b in listed {
        let name = match b {
            BodyRef::Free(k) => records.name_of(k).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        };
        let (_, borrows) = world.check_borrows(b, g);
        let said = match borrows.refused {
            Some(BorrowRefused::FactUnknown { adt, item, fact, why }) => {
                let adt_text = item.map(&shown).unwrap_or_else(|| format!("{adt:?}"));
                let why_text = match why {
                    ImplsUnknown::RefusedCandidate { candidate, why, .. } => format!("refused candidate {candidate:x} of {}: {}", shown(candidate), why.as_str()),
                    other => format!("{other:?}"),
                };
                format!("FactUnknown {fact} of {adt_text}: {why_text}")
            }
            Some(other) => format!("refused: {other:?}"),
            None if borrows.findings.is_empty() => String::from("clean"),
            None => format!("findings {:?}", borrows.findings.iter().map(|f| f.code).collect::<Vec<_>>()),
        };
        eprintln!("borrows {name}: {said}");
        out.push((name, said));
    }
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
    out
}

/// Hello world's every body has its `Copy` and `Drop` facts known over records (the borrow
/// check may still refuse a later form; this lane's piece is the facts).
#[test]
fn hello_copy_facts_known_over_records() {
    let got = borrows_over_records(HELLO, EDITION);
    let unknown: Vec<String> = got.iter().filter(|(_, s)| s.starts_with("FactUnknown")).map(|(n, s)| format!("{n}: {s}")).collect();
    assert!(unknown.is_empty(), "every ADT fact of hello world is known: {}", unknown.join(" | ").replace('\n', " "));
}

/// The four forms of a `Copy` fact, each body moving its value twice:
/// - `derived`: `#[derive(Clone, Copy)]`: clean;
/// - `by_macro`: `Clone` and `Copy` impls a `macro_rules!` call writes: clean;
/// - `beside_unread`: derived `Copy`, and beside it an impl at the same type whose trait path
///   names nothing (`nowhere::Marker`): that refused candidate could add an impl of `Copy`,
///   never take the derived one away, so the answer is YES and the body is clean (bstd's
///   `Exit`, whose `impl std::process::Termination for Exit` the records did not resolve, was
///   hello world's refusal); and a `Copy` type has no `Drop` impl (E0184), so the same refused
///   candidate does not make its `Drop` fact unknown either;
/// - `moved_twice`: no `Copy` impl: the second move is E0382, with an impl of a trait that
///   names nothing written for an UNRELATED type (`Unrelated`), which is no candidate here:
///   its self type's outermost constructor is read and is another type's.
const FACTS: &str = "#[derive(Clone, Copy)]
struct Derived(u32);

macro_rules! copy_impl {
    ($t:ident) => {
        impl Clone for $t {
            fn clone(&self) -> $t {
                *self
            }
        }
        impl Copy for $t {}
    };
}

struct ByMacro(u32);
copy_impl!(ByMacro);

#[derive(Clone, Copy)]
struct Beside(u32);
impl nowhere::Marker for Beside {}

struct NotCopy(u32);

struct Unrelated(u32);
impl nowhere::Copy for Unrelated {}

fn derived() {
    let a = Derived(1);
    let _b = a;
    let _c = a;
}

fn by_macro() {
    let a = ByMacro(1);
    let _b = a;
    let _c = a;
}

fn beside_unread() {
    let a = Beside(1);
    let _b = a;
    let _c = a;
}

fn moved_twice() {
    let a = NotCopy(1);
    let _b = a;
    let _c = a;
}

fn main() {}
";

#[test]
fn copy_facts_over_records() {
    let got = borrows_over_records(FACTS, "2024");
    let of = |name: &str| got.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone()).unwrap_or_else(|| format!("no body named `{name}` in {got:?}"));
    let said: Vec<String> = ["derived", "by_macro", "beside_unread", "moved_twice"].iter().map(|n| format!("{n}: {}", of(n))).collect();
    let line = said.join(" | ").replace('\n', " ");
    assert_eq!(of("derived"), "clean", "{line}");
    assert_eq!(of("by_macro"), "clean", "{line}");
    assert_eq!(of("beside_unread"), "clean", "{line}");
    assert!(of("moved_twice").starts_with("findings") && of("moved_twice").contains("E0382"), "{line}");
}
