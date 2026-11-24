//! The owner's `main`, as written, through front2 for the first time: lowered and converted
//! (front2/body.rs), then checked with the check loop (front2/check.rs) over the toolchain's
//! names. It ends one of two ways, both useful: his `main` type-checks, or the test prints the
//! work list (every lowering finding, every `Gap`, or the one typed failure, each with the item
//! it names, in the order met) and the README's port notes carry it. It needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain`, as tests/names.rs does.

#![feature(allocator_api)]

use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::program::walk_report::{Ask, BuildFrom, Receivers, RootRead, Stage, WalkHooks, World, walk};
use blaze_ladder::front2::types::Why;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::world_items::Gap;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{Namespace, PathKind};

/// His `Cargo.toml`'s edition line.
const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs`, as written (D104: `main` names its grants; no whole `Io`).
const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";
/// `crate::name` of a library key, for the work list.
fn shown(names: &Names<'_>, crates: &[&str], key: blaze_id::ItemKey) -> String {
    let krate = names.crate_of(key).and_then(|c| crates.get(c).copied()).unwrap_or("?");
    // An impl has no name: its header as written (`impl<A> AsRef<OsStr> for OsString<A>`).
    let name = match names.item_text(key) {
        Some(t) => match t.name {
            Some(n) => n.to_string(),
            None => t.text.split('{').next().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "),
        },
        None => "(no text)".to_string(),
    };
    format!("{krate}::{name} ({key:x})")
}

/// A check's outcome as text, for comparing two checks of one body: the diagnostics (code and
/// range) and each callee (expression and item) of a finished check, else the failure as printed.
fn outcome(checked: &Result<blaze_typeck::Checked<'static>, CheckFail>) -> String {
    match checked {
        Ok(done) => {
            let diags: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
            let callees: Vec<String> = (0..done.callees.len()).map(|i| format!("{:?} -> {:?}", done.callees.column::<0>()[i], done.callees.column::<1>()[i])).collect();
            format!("checked: diags {diags:?}; callees {callees:?}")
        }
        Err(e) => format!("not checked: {e}"),
    }
}

/// A body's own types never reach the records later bodies read: two bodies (one with a
/// closure, whose type only a body has; one asking a trait's definition, `Clone`) checked over
/// ONE set of records give, in either order, what each gives from empty records. Reduced from
/// the census's first poisoning (`bstd::spec::RunSpec::put_grant`'s `binary_search_by(|it| ..)`
/// left its closure type in the shared table; a later `TraitDef` answer converted that table:
/// `Malformed/BodyTypeInRecord/TraitDef`). The Reference, "Closure types": "a closure
/// expression produces a closure value with a unique, anonymous type": a type of that body.
#[test]
fn shared_records_hold_no_body_type() {
    const BODY: &str = "fn sorts(v: &[u32]) -> usize {\n    let f = |a: &u32| *a;\n    v.len() + f(&v[0]) as usize\n}\nfn copies(x: u32) -> u32 {\n    x.clone()\n}\nfn main() {}\n";
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BODY.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: BODY }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let key = |n: &str| {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key
    };
    let (sorts, copies) = (key("sorts"), key("copies"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let sized = front.index.lang("sized");
    // Each fn of `order` lowered and checked in turn over one store; the outcomes by fn.
    let mut run = |order: &[blaze_id::ItemKey]| -> Vec<(blaze_id::ItemKey, String)> {
        let (mut store, mut buckets, mut consts) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
        let mut got = Vec::new();
        for &k in order {
            let body = lower_fn(&names, prepared, &mut front.keys, &mut store, k).unwrap_or_else(|e| panic!("lowering {k:x}: {e}"));
            let view = body.lowered.view();
            let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling {k:x}: {e:?}"));
            let scope = body.scope(&spell);
            let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
            got.push((k, outcome(&check_body(&mut records, &body, &scope))));
        }
        got.sort();
        got
    };
    let alone = [run(&[sorts]), run(&[copies])].concat();
    let mut alone_sorted = alone.clone();
    alone_sorted.sort();
    let forward = run(&[sorts, copies]);
    let backward = run(&[copies, sorts]);
    println!("alone: {alone_sorted:?}\nforward: {forward:?}\nbackward: {backward:?}");
    assert!(!format!("{forward:?}{backward:?}").contains("BodyTypeInRecord"), "a record held a body's type: {forward:?} / {backward:?}");
    assert_eq!(forward, alone_sorted, "checked after `sorts` over shared records");
    assert_eq!(backward, alone_sorted, "checked after `copies` over shared records");
}

/// An inherent const a macro call writes inside an impl body (core's `impl u32 { uint_impl! {
/// .. } }` makes `MAX`) is a member front2 reads from `Names::members`: a body naming it
/// type-checks clean. The body is a plain fn `most`, not the root's `main`: under a blaze
/// edition the root's `main` is `std::main`'s input (D104) and a lookup of `main` finds the
/// launcher it writes, not the written body (see [`a_plain_main_is_std_mains_input`]).
#[test]
fn a_macro_written_member_checks() {
    const BODY: &str = "fn most() {\n    let _most: u32 = u32::MAX;\n}\n";
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BODY.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: BODY }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    assert!(names.notes(0).is_empty(), "crate 0's notes: {:?}", names.notes(0));
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["most"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`most`: {e}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`most` not named")).key;
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, main).unwrap_or_else(|e| panic!("lowering `most`: {e}"));
    let view = body.lowered.view();
    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `most`: {e:?}"));
    let scope = body.scope(&spell);
    let mut consts = Consts::default();
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
    let checked = check_body(&mut records, &body, &scope);
    let line = format!("u32::MAX through front2: {}", outcome(&checked));
    println!("{line}");
    // The checker scripts keep no test output: `FRONT2_MEMBER_LINE` names a file for the line.
    if let Some(path) = std::env::var_os("FRONT2_MEMBER_LINE") {
        std::fs::write(&path, &line).expect("writing FRONT2_MEMBER_LINE");
    }
    let clean = checked.as_ref().is_ok_and(|done| done.diags.is_empty());
    assert!(clean, "{line}");
}

/// D104: a blaze-edition binary's root `fn main()` that takes nothing is `std::main`'s input
/// (blaze-scope `IMPLICIT_MAIN`), and bstd's `main!` has an arm for it (crates/blaze-std/src/
/// entry.rs, "A `main` that takes nothing"), whose output is the launcher `fn main() -> Exit`
/// filed in the root under its written name (the Reference, "Attribute macros": the item is
/// replaced by the macro's output, whose items resolve at the call site). So after names
/// settle, crate 0 has no note and `main` is found in the root module. Prints
/// `plain main: the root's `main` is std::main's launcher; notes 0`.
/// Fails, naming the `Expand` note, while the pinned toolchain's bstd lacks that arm (PINNED
/// `ad2d1aaa..`, bstd pack `b926e476..`: one arm, `fn main($cx:tt : ..)`): the control.
#[test]
fn a_plain_main_is_std_mains_input() {
    const BODY: &str = "fn main() {}\n";
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BODY.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    assert!(names.notes(0).is_empty(), "crate 0's notes: {:?}", names.notes(0));
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["main"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`main` after std::main: {e}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`main` not named")).key;
    let text = names.item_text(main).unwrap_or_else(|| panic!("`main` ({main:x}) has no text"));
    // The launcher, not the written `fn main() {}`: it returns the program's exit.
    assert!(text.text.contains("->"), "the root's `main` is the written one, not std::main's launcher: {:?}", text.text);
    println!("plain main: the root's `main` is std::main's launcher; notes {}", names.notes(0).len());
}

/// His main walked by the library's walk (front2/program/walk_report.rs `walk`, which holds
/// this test's walk since 2026-11-16: the same format strings, moved), with this test's own
/// lines between the walk's ([`HisMain`]). Prints, with `FRONT2_WORK_LIST` set, the lines the
/// measurer compares run to run (port27/front2/list-AI.txt): `ORDER: ..`, `his main's
/// diagnostics: 0 []`, `REACHABLE: N of M reachable functions type-check (his main and ..)`,
/// its classes, `BUILDS: N of M clean reachable functions' instances (..)`, its classes, then
/// the ctor and `T::X` rules, the impls and `his main type-checks` or its work list.
#[test]
fn his_main_through_the_checker() {
    let start = Instant::now();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let resolved = start.elapsed();
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let survey_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["main"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`main` from his crate root: {e}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`main` not named")).key;

    let mut out = format!("front2 on his main: names resolved in {resolved:?}\n");
    // The variants his body reaches (`Ok`; `?` reaches lang variants): the key a path gives,
    // against the keys of the enum's row in the library's `enums` table.
    let root = names.root(0);
    let at = |ns, path: &'static str| {
        names::path(&names, root, ns, Written { kind: PathKind::Plain, segments: &[path] }, Alloc::global()).ok().and_then(|f| f.whole(1)).map(|d| d.key)
    };
    let ok = at(Namespace::Values, "Ok");
    let result = at(Namespace::Types, "Result");
    let row = result.and_then(|r| names.crate_of(r).and_then(|c| names.krate(c).tables.enums.get(&r)));
    let row_keys: Vec<String> = row.map(|vs| vs.iter().map(|v| format!("{:x}", v.key)).collect()).unwrap_or_default();
    let ok_shown = ok.map(|k| format!("{k:x}"));
    let in_any = ok.map(|k| names.crates().iter().any(|c| c.tables.enums.values().any(|vs| vs.iter().any(|v| v.key == k))));
    out.push_str(&format!("probe: `Ok` is {ok_shown:?}, in some enums row: {in_any:?}; `Result`'s row: {row_keys:?}\n"));
    // The walk: his main lowered and checked, `REACHABLE:` and `BUILDS:`; a lowering that fails
    // is its `WORK LIST (lowering ..)` line and nothing more (the walk returns before any hook).
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let world = World { names: &names, frame: &frame, survey_frame: &survey_frame, prepared, sources: &sources, crate_names: &crate_names, body_sources: None, receivers: Receivers::NotLookedUp };
    let ask = Ask { roots: &[main], build_from: BuildFrom::Main, read: RootRead::Callees, label: "his main" };
    let mut his = HisMain { names: &names, crate_names: &crate_names, frame: &frame, sources: &sources, main, ok };
    walk(&world, &ask, &mut his, &mut out, Alloc::global()).unwrap_or_else(|e| panic!("{e}"));
    report(out, start);
}

/// The hello world's own lines around the walk, each as this test has always printed it: after
/// his main is checked, the determinism check (`ORDER:`), his diagnostics and the `FRONT2_DUMP`
/// dump; after `BUILDS:`, the ctor and `T::X` rules, the impls and his main's outcome.
struct HisMain<'n> {
    names: &'n Names<'n>,
    crate_names: &'n [&'n str],
    frame: &'n blaze_mem::Frame,
    sources: &'n [SourceFile<'n>],
    main: blaze_id::ItemKey,
    /// `Ok`'s key, from his root.
    ok: Option<blaze_id::ItemKey>,
}

impl<'n> WalkHooks<'n> for HisMain<'n> {
    fn checked(&mut self, at: &mut Stage<'_, '_, 'n>, sink: &mut dyn std::fmt::Write) -> std::fmt::Result {
        let (names, crate_names, prepared, main) = (self.names, self.crate_names, at.prepared, self.main);
        let body = &at.bodies[at.walked[0]];
        let checked = &at.checked[0];
        let records = &mut *at.records;
        let mut out = String::new();
        // The check of one body is a function of the program alone: checked again from empty
        // records in this process, every noted item made in the REVERSED order, it must end the
        // same: its diagnostics, its gaps (or refusal) and its recorded callees.
        let first = outcome(checked);
        {
            // The command's one frame (the names' lifetime is the frame's).
            let mut front_r = Front2::new(names, self.frame, self.sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2 (reversed): {e}"));
            let mut store_r = Store::new(front_r.index.lang("sized"), front_r.frame, blaze_own::Alloc::global());
            let mut buckets_r = None;
            let body_r = lower_fn(names, prepared, &mut front_r.keys, &mut store_r, main).unwrap_or_else(|e| panic!("lowering main again: {e}"));
            let view_r = body_r.lowered.view();
            let spell_r = Spelling::of(&view_r).unwrap_or_else(|e| panic!("spelling his main's names again: {e:?}"));
            let scope_r = body_r.scope(&spell_r);
            let mut consts_r = Consts::default();
            let mut records_r = Records { names, keys: &mut front_r.keys, index: &front_r.index, store: &mut store_r, buckets: &mut buckets_r, order: BuildOrder::Reversed, consts: Some((&mut consts_r, prepared)) };
            let again = outcome(&check_body(&mut records_r, &body_r, &scope_r));
            assert_eq!(first, again, "his main's check depends on the order its records were made in");
        }
        out.push_str(&format!("ORDER: the same outcome with the build order reversed: {first}\n"));
        // A finished check is clean only with no diagnostic: `?`'s variant lang items (`Break`,
        // `Continue`) missing from the index left 11 (E0425, E0277) that read as "type-checks".
        if let Ok(done) = checked {
            let codes: Vec<&str> = done.diags.iter().map(|d| d.code.code()).collect();
            out.push_str(&format!("his main's diagnostics: {} {codes:?}\n", codes.len()));
        }
        // The trait questions selection was asked while checking, each with its candidates (as
        // filed) and its answer: the traits of the E0277s are among them, and an empty answer
        // over candidates says which header did not fit. A large dump (ASKED / HEAD / PREDS /
        // NODE lines): printed only with `FRONT2_DUMP` set.
        if let Some(b) = records.buckets.as_ref().filter(|_| std::env::var_os("FRONT2_DUMP").is_some()) {
            for (t, s, got) in b.answered() {
                let tn = records.keys.library_in(names, t).map_or_else(|| format!("{t:?}"), |k| shown(names, crate_names, k));
                let cands = b.trait_candidates(t, s, b.home_of(t, s));
                out.push_str(&format!("ASKED {tn} at {s:?}: {} candidates, {} answer\n", cands.len(), got.len()));
                for k in cands.iter().take(12) {
                    let h = records.store.impls.get(&blaze_ladder::front2::keys::checker_of(*k));
                    let how = match h {
                        None => "no header".to_string(),
                        Some(h) if h.trait_ == Some(t) => format!("trait fits, picked {}", got.contains(&blaze_ladder::front2::keys::checker_of(*k))),
                        Some(h) => format!("header trait {:?}", h.trait_.and_then(|x| records.keys.library_in(names, x)).map(|x| shown(names, crate_names, x))),
                    };
                    out.push_str(&format!("    {} [{how}] unread={} failed={}\n", shown(names, crate_names, *k), b.is_unread(*k), b.has_failed(*k)));
                }
            }
            // The heads the obligations are made from, as the store holds them: his main's
            // signature and clauses, each picked impl's header and clauses, and the asked traits'
            // fns (`branch`, `from_residual`, `from_output`).
            let tys = records.store.tys.view();
            let mut picked: Vec<blaze_typeck::ItemKey> = Vec::new();
            let mut traits: Vec<blaze_typeck::ItemKey> = Vec::new();
            for (t, _, got) in b.answered() {
                picked.extend_from_slice(got);
                traits.push(t);
            }
            let clauses = |item: blaze_typeck::ItemKey, roots: &mut Vec<blaze_typeck::PTy>| -> String {
                let Some(p) = records.store.preds.get(&item) else { return "no preds".to_string() };
                let mut s = format!("{} conds;", p.conds.len());
                for c in p.clauses.iter() {
                    s.push_str(&format!(" {c:?}"));
                    match *c {
                        blaze_typeck::PClause::Trait { args, .. } => roots.extend_from_slice(tys.list(args).unwrap_or(&[])),
                        blaze_typeck::PClause::Projection { args, term, .. } => {
                            roots.extend_from_slice(tys.list(args).unwrap_or(&[]));
                            roots.push(term);
                        }
                        blaze_typeck::PClause::Const { .. } | blaze_typeck::PClause::RegionOutlives { .. } => {}
                        blaze_typeck::PClause::TypeOutlives { ty, .. } => roots.push(ty),
                        blaze_typeck::PClause::ForFields { container, args, .. } => {
                            roots.push(container);
                            roots.extend_from_slice(tys.list(args).unwrap_or(&[]));
                        }
                    }
                }
                s
            };
            let mut roots: Vec<blaze_typeck::PTy> = Vec::new();
            if let Some(f) = records.store.fns.get(&body.owner) {
                out.push_str(&format!("HEAD main: parent {} own {} inputs {:?} output {:?}\n", f.parent, f.own, f.inputs, f.output));
                roots.extend_from_slice(&f.inputs);
                roots.push(f.output);
            }
            out.push_str(&format!("PREDS main: {}\n", clauses(body.owner, &mut roots)));
            for k in picked.iter() {
                if let Some(h) = records.store.impls.get(k) {
                    out.push_str(&format!("HEAD impl {k:?}: generics {} self {:?} trait args {:?} assoc {:?}\n", h.generics, h.self_ty, tys.list(h.trait_args.list()), h.assoc));
                    roots.push(h.self_ty);
                    roots.extend_from_slice(tys.list(h.trait_args.list()).unwrap_or(&[]));
                    roots.extend(h.assoc.iter().map(|a| a.1));
                }
                out.push_str(&format!("PREDS impl {k:?}: {}\n", clauses(*k, &mut roots)));
            }
            for (fk, f) in records.store.fns.iter() {
                if *fk != body.owner {
                    let sig = f.text.find("fn ").map_or(f.text, |at| &f.text[at..]);
                    let sig: String = sig.chars().take(160).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
                    out.push_str(&format!("HEAD fn `{sig}` {fk:?} in {:?}: parent {} own {} self_param {} inputs {:?} output {:?}\n", f.container, f.parent, f.own, f.self_param, f.inputs, f.output));
                    roots.extend_from_slice(&f.inputs);
                    roots.push(f.output);
                    out.push_str(&format!("PREDS fn {fk:?}: {}\n", clauses(*fk, &mut roots)));
                }
            }
            for (ck, c) in records.store.ctors.iter() {
                out.push_str(&format!("CTOR {ck:?} of {:?}: generics {} inputs {:?} output {:?}\n", c.adt, c.generics, c.inputs, c.output));
                roots.extend_from_slice(&c.inputs);
                roots.push(c.output);
            }
            for (ak, a) in records.store.adts.iter() {
                out.push_str(&format!("ADT {ak:?}: generics {} variants {}\n", a.generics, a.variants.len()));
            }
            // Every node under those roots, each once, by a work list.
            let mut seen = std::collections::BTreeSet::new();
            while let Some(t) = roots.pop() {
                if !seen.insert(t.0) {
                    continue;
                }
                let mut kids: Vec<blaze_typeck::PTy> = Vec::new();
                blaze_ladder::front2::types::children_of(tys, t, &mut kids);
                roots.extend(kids.iter().copied());
            }
            for id in seen {
                let mut kids: Vec<blaze_typeck::PTy> = Vec::new();
                blaze_ladder::front2::types::children_of(tys, blaze_typeck::PTy(id), &mut kids);
                let kids: Vec<u32> = kids.iter().map(|k| k.0).collect();
                out.push_str(&format!("  NODE {id} = {:?} over {kids:?}\n", tys.node(blaze_typeck::PTy(id))));
            }
            for (k, why) in b.index().failed.iter().take(20) {
                out.push_str(&format!("FAILED HEADER {}: {why}\n", shown(names, crate_names, *k)));
            }
        }
        sink.write_str(&out)
    }

    fn walked(&mut self, at: &mut Stage<'_, '_, 'n>, sink: &mut dyn std::fmt::Write) -> std::fmt::Result {
        let (names, crate_names, ok) = (self.names, self.crate_names, self.ok);
        let checked = &at.checked[0];
        let records = &mut *at.records;
        let mut out = String::new();
        // The ctor rule (the Reference, "Enumerated types"): `Ok` in the value namespace is a fn
        // generic over `Result`'s two parameters, from its one field to `Result<T, E>`; it is made
        // with `Result`'s head, which his `?` and `Ok(())` make the check fill.
        if let Some(c) = ok.and_then(|k| records.store.ctors.get(&blaze_ladder::front2::keys::checker_of(k))) {
            assert_eq!((c.generics, c.inputs.len()), (2, 1), "`Ok`'s ctor fn: generic over `T, E`, one input");
            out.push_str("ctor rule: `Ok` is a fn over 2 generics with 1 input\n");
        }
        // The `T::X` rule (the Reference, "Paths in types"): `T::X` with `T` a type parameter is
        // `<T as Tr>::X` for the one trait among `T`'s bounds, supertraits included, declaring `X`.
        // core's `ChangeOutputType<T: Try<..>, V>` is written `<T::Residual as Residual<V>>::TryType`
        // (an alias's bounds are read for this): its head must be made, or refused for another cause.
        if let Some(core) = crate_names.iter().position(|n| *n == "core") {
            let segs = ["ops", "try_trait", "ChangeOutputType"];
            let found = names::path(names, names.root(core), Namespace::Types, Written { kind: PathKind::Plain, segments: &segs }, Alloc::global());
            let key = found.ok().and_then(|f| f.whole(segs.len()).map(|d| d.key));
            match key.map(|k| (records.store.maps.aliases.get(&k), records.store.maps.alias_gaps.get(&k))) {
                Some((Some(Some(_)), _)) => out.push_str("T::X rule: core's ChangeOutputType is made, `T::Residual` read through `T: Try`\n"),
                Some((_, Some(gap))) => {
                    let why = gap.why.as_ref().expect("a refusal lowered here keeps its Why");
                    assert!(!matches!(why, Why::AssocNotFound(..) | Why::AssocAmbiguous(..)), "`T::Residual` through `T: Try` must be found: {why:?}");
                    out.push_str(&format!("T::X rule: core's ChangeOutputType refused for another cause: {why:?}\n"));
                }
                other => out.push_str(&format!("T::X rule: core's ChangeOutputType not asked by this check ({:?})\n", other.map(|o| o.0.is_some()))),
            }
        }
        let lowered = records.buckets.as_ref().map_or(0, |b| b.index().lowered);
        let filed = records.buckets.as_ref().map(|b| b.filed());
        out.push_str(&format!("impls on a builtin the checker has no type for (filed nowhere): {:?}\n", records.buckets.as_ref().map(|b| b.index().no_checker_type)));
        out.push_str(&format!("impl headers lowered by the check: {lowered} of the program's {}; filed (trait+shape, trait any, inherent, inherent any, unread): {filed:?}\n", records.index.impls.len()));
        // The unread impls by class: a count and samples from different crates.
        if let Some(b) = records.buckets.as_ref() {
            let mut classes: Vec<(String, usize, Vec<String>)> = Vec::new();
            for (k, why) in b.index().unread_why.iter() {
                let class = why.class();
                let krate = names.crate_of(*k).and_then(|c| crate_names.get(c).copied()).unwrap_or("?");
                let sample = format!("[{krate}] {} : {why:?}", shown(names, crate_names, *k));
                match classes.iter_mut().find(|c| c.0 == class) {
                    Some(c) => {
                        c.1 += 1;
                        if c.2.len() < 8 && !c.2.iter().any(|s| s.starts_with(&format!("[{krate}]"))) {
                            c.2.push(sample);
                        }
                    }
                    None => classes.push((class, 1, vec![sample])),
                }
            }
            classes.sort_by(|a, b| b.1.cmp(&a.1));
            // By crate and path as written: the reductions to send to the names library's lane.
            let mut by_path: Vec<(String, usize)> = Vec::new();
            for (k, why) in b.index().unread_why.iter() {
                let krate =names.crate_of(*k).and_then(|c| crate_names.get(c).copied()).unwrap_or("?");
                let path = match why {
                    blaze_ladder::front2::buckets::Unread::Resolve { path, .. } | blaze_ladder::front2::buckets::Unread::NotATrait { path, .. } => &**path,
                    _ => "",
                };
                let row = format!("{krate}: `{path}`");
                match by_path.iter_mut().find(|x| x.0 == row) {
                    Some(x) => x.1 += 1,
                    None => by_path.push((row, 1)),
                }
            }
            by_path.sort_by(|a, b| b.1.cmp(&a.1));
            for (row, n) in by_path.iter().take(12) {
                out.push_str(&format!("unread by crate and path: {n} x {row}\n"));
            }
            for (class, n, samples) in &classes {
                out.push_str(&format!("unread class {class}: {n}\n"));
                for s in samples {
                    out.push_str(&format!("    {s}\n"));
                }
            }
        }
        match checked {
            Ok(_) => out.push_str("his main type-checks\n"),
            Err(CheckFail::Gap { gaps, .. }) => {
                out.push_str(&format!("WORK LIST ({} gaps):\n", gaps.len()));
                // The unread impls' failures, by crate and cause (the why's variant): a count and
                // the first, so each owner's share is read off one line.
                let mut unread: Vec<(String, usize, String)> = Vec::new();
                for gap in gaps.iter() {
                    if let Gap::UnreadImpl(k, why) = gap {
                        let krate = names.crate_of(*k).and_then(|c| crate_names.get(c).copied()).unwrap_or("?");
                        let cause = format!("{why}");
                        let row = format!("{krate} / {}", cause.split(['(', ':']).next().unwrap_or(""));
                        match unread.iter_mut().find(|u| u.0 == row) {
                            Some(u) => u.1 += 1,
                            None => unread.push((row, 1, format!("{} {cause}", shown(names, crate_names, *k)))),
                        }
                    }
                }
                for (row, n, first) in &unread {
                    out.push_str(&format!("  unread impls in {row}: {n}; the first: {first}\n"));
                }
                for (i, gap) in gaps.iter().filter(|g| !matches!(g, Gap::UnreadImpl(..))).enumerate() {
                    let item = match gap {
                        Gap::VariantOfNoEnum(k) => format!("(lang `{}`)", records.index.lang_name_of(*k).unwrap_or("?")),
                        Gap::TypeAlias(k) => shown(names, crate_names, *k),
                        Gap::WhereConst(k) | Gap::CtorFn(k) | Gap::ImplAssocType(k, _) => match records.keys.library_in(names, *k) {
                            Some(lib) => shown(names, crate_names, lib),
                            None => format!("{k:?}"),
                        },
                        _ => String::new(),
                    };
                    out.push_str(&format!("  {}. {gap} {item}\n", i + 1));
                }
            }
            Err(CheckFail::Head { error: HeadError::Item(k, why), .. }) => {
                let k = *k;
                let text = names.item_text(k).map(|t| t.text).unwrap_or("");
                let at = match why {
                    Why::Unsupported(_, r)
                    | Why::UnsupportedIn(_, _, r)
                    | Why::Program(_, r)
                    | Why::Resolve(_, r)
                    | Why::NoCheckerType(_, r)
                    | Why::AssocNotFound(_, r)
                    | Why::AssocAmbiguous(_, _, r)
                    | Why::TwoPrincipals(_, _, r)
                    | Why::TypeMacro(_, r)
                    | Why::PatternType(_, r)
                    | Why::ConstPath(_, r)
                    | Why::NeedConst(_, r) => text.get(r.start as usize..r.end as usize),
                    Why::Need(_) | Why::Regions(_) => None,
                };
                out.push_str(&format!("WORK LIST (one failure): the head of {}: {why:?}, at `{}`\n", shown(names, crate_names, k), at.unwrap_or("?")));
                // The probe for a name that names nothing: the same text from the item's module,
                // and its prefixes from his root, through the names library.
                if let (Some(module), Some(word)) = (names.module_of(k), at) {
                    let from_item = names::path(names, module, Namespace::Types, Written { kind: PathKind::Plain, segments: &[word] }, Alloc::global());
                    out.push_str(&format!("probe: `{word}` from the item's module: {:?}\n", from_item.as_ref().map(|f| f.whole(1).map(|d| d.key)).map_err(|e| format!("{e}"))));
                    for segs in [&["std"][..], &["std", "ffi"][..], &["std", "ffi", word][..], &["std", "path", "Path"][..], &["std", "alloc", "Allocator"][..], &["Allocator"][..], &["Path"][..]] {
                        let got = names::path(names, module, Namespace::Types, Written { kind: PathKind::Plain, segments: segs }, Alloc::global());
                        out.push_str(&format!("probe: `{}` from the item's module: {:?}\n", segs.join("::"), got.as_ref().map(|f| f.whole(segs.len()).map(|d| d.key)).map_err(|e| format!("{e}"))));
                    }
                }
            }
            Err(e) => out.push_str(&format!("WORK LIST (one failure): {e}\n")),
        }
        sink.write_str(&out)
    }
}

/// The walk from TWO roots given as a list (front2/program/walk_report.rs `walk`): both are
/// lowered, checked and walked in one walk; `shared`, which both reach, is counted once, and
/// `only_second`, which only the second reaches, is counted; a root given twice is one root.
/// The Reference, "Call expressions": a call names its callee; nothing else is reached.
///
/// Prints: `REACHABLE: 4 of 4 reachable functions type-check (two roots and 2 reached; 0
/// ctors; 0 trait calls walked where the instance or object is made; 0 required trait methods
/// not walked; 0 calls not followed: their caller did not check and only its types name them;
/// capped: false)`
#[test]
fn two_roots_are_walked_once() {
    const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

fn shared() -> i32 {
    1
}

fn only_second() -> i32 {
    2
}

fn first() {
    shared();
}

fn second() {
    shared();
    only_second();
}
";
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = blaze_ladder::program_texts::ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the walk's frame");
    let survey_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let key = |name: &'static str| {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{name}` from the crate root: {e}"));
        found.whole(1).unwrap_or_else(|| panic!("`{name}` not named")).key
    };
    let (first, second) = (key("first"), key("second"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let world = World { names: &names, frame: &frame, survey_frame: &survey_frame, prepared, sources: &files, crate_names: &crate_names, body_sources: None, receivers: Receivers::Found };
    let ask = Ask { roots: &[first, second, first], build_from: BuildFrom::Roots, read: RootRead::Callees, label: "two roots" };
    let mut out = String::new();
    let counts = walk(&world, &ask, &mut blaze_ladder::front2::program::walk_report::NoHooks, &mut out, Alloc::global()).unwrap_or_else(|e| panic!("{e}"));
    let line = out.lines().find(|l| l.starts_with("REACHABLE:")).unwrap_or_else(|| panic!("no REACHABLE line in:\n{out}"));
    println!("{line}");
    assert_eq!((counts.roots, counts.walked, counts.roots_checked), (2, 2, 2), "`first` given twice is one root; both check:\n{out}");
    assert_eq!(counts.reached, 2, "`shared` once and `only_second` once:\n{out}");
    assert_eq!((counts.type_check, counts.reachable), (4, 4), "two roots and two reached, all clean:\n{out}");
    assert!(out.lines().any(|l| l.starts_with("BUILDS:")), "the builds survey's line is written, whatever it says:\n{out}");
}

/// Prints the outcome; with `FRONT2_WORK_LIST` set (a file path), writes it there too, since a
/// passing test's output is not shown by the lane's check script.
fn report(mut out: String, start: Instant) {
    out.push_str(&format!("front2 on his main: {:?} in all\n", start.elapsed()));
    print!("{out}");
    if let Ok(path) = std::env::var("FRONT2_WORK_LIST") {
        std::fs::write(&path, &out).unwrap_or_else(|e| panic!("writing the work list to {path}: {e}"));
    }
}

/// D104: every blaze `main` is `std::main`'s input, through the run path's front half
/// (`run::build_new` over the installed toolchain), each as far as it gets. The owner's hello
/// builds; a `main` that asks a whole world is refused in the words `Grant`'s
/// `#[diagnostic::on_unimplemented]` gives, which name what to write instead. With
/// `FRONT2_ENTRY_LIST` set, the lines are written there.
#[test]
fn the_entry_programs_as_far_as_each_gets() {
    use blaze_ladder::run::{NewFront, build_new};
    use std::{env, fs, thread};
    let home = env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    // `Io` is in no prelude (D104): the old shape names it by path, so the refusal is `Grant`'s,
    // not an unresolved name.
    let whole = "use std::Io;\n\nfn main(cx: &Cx, io: Io) -> Result<(), Error> {\n    io.need_out()?.line(cx, \"Hello, world!\")?;\n    Ok(())\n}\n";
    let programs: [(&str, &str); 4] = [
        ("main()", "fn main() {}\n"),
        ("main(cx: &Cx)", "fn main(cx: &Cx) {\n    let _ = cx;\n}\n"),
        ("main(cx: &Cx, out: Out)", MAIN),
        ("main(cx: &Cx, io: Io)", whole),
    ];
    let mut out = String::from("ENTRY PROGRAMS (D104, edition blaze-2026.11):\n");
    // Every row runs and is printed; what each missed is said at the end, all together.
    let alloc = blaze_own::testing::heap().alloc();
    let rows = thread::scope(|s| {
        let mut handles = blaze_own::Vec::with_capacity_in(programs.len(), alloc);
        for (shape, text) in programs {
            let shelf = shelf.as_str();
            handles.push(s.spawn(move || {
                let files = [SourceFile { path: "src/main.rs", text }];
                let got = build_new(&files, "src/main.rs", NewFront { package: "entry", edition: EDITION, shelf: Some(shelf), overflow: blaze_build::OverflowChecks::On }, blaze_own::Alloc::global());
                let line = match &got {
                    Ok(_) => "BUILT (front, checker and builder: no refusal)".to_string(),
                    Err(e) => format!("REFUSED: {e}"),
                };
                let cut = line.floor_char_boundary(600.min(line.len()));
                let out_line = format!("{shape}: {}\n", &line[..cut]);
                let miss = match shape {
                    "main(cx: &Cx, out: Out)" if got.is_err() => Some(format!("the owner's hello builds: {line}")),
                    "main(cx: &Cx, io: Io)" => match got {
                        Ok(_) => Some("a `main` taking a whole `Io` is refused, not built".into()),
                        Err(refused) => {
                            let shown = format!("{refused}");
                            // Without d104-A-6 the words are the bare goal `Io: Grant`; with `impl Grant
                            // for Io` left in bstd it builds.
                            if !shown.contains("`main` cannot take a `Io`") || !shown.contains("cannot be asked for") {
                                Some(format!("the `Io` row is refused in `Grant`'s words, naming what to write: {shown}"))
                            } else {
                                None
                            }
                        }
                    },
                    _ => None,
                };
                (out_line, miss)
            }));
        }
        let mut collected = blaze_own::Vec::with_capacity_in(handles.len(), alloc);
        for h in handles {
            collected.push(h.join().unwrap());
        }
        collected
    });
    let mut missed: Vec<String> = Vec::new();
    for (out_line, miss) in rows {
        out.push_str(&out_line);
        if let Some(m) = miss {
            missed.push(m);
        }
    }
    print!("{out}");
    if let Ok(path) = env::var("FRONT2_ENTRY_LIST") {
        fs::write(&path, &out).unwrap_or_else(|e| panic!("writing the entry list to {path}: {e}"));
    }
    assert!(missed.is_empty(), "{} of the entry programs missed:\n{}", missed.len(), missed.join("\n"));
}
