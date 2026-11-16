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
use blaze_ladder::front2::reach::{Reached, reach};
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

/// His `src/main.rs`, as written.
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
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
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: BODY }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let key = |n: &str| {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key
    };
    let (sorts, copies) = (key("sorts"), key("copies"));
    let prepared = Prepares::new_in(Alloc::global());
    let sized = front.index.lang("sized");
    // Each fn of `order` lowered and checked in turn over one store; the outcomes by fn.
    let mut run = |order: &[blaze_id::ItemKey]| -> Vec<(blaze_id::ItemKey, String)> {
        let (mut store, mut buckets, mut consts) = (Store::new(sized, front.frame), None, Consts::default());
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
/// type-checks clean.
#[test]
fn a_macro_written_member_checks() {
    const BODY: &str = "fn main() {\n    let _most: u32 = u32::MAX;\n}\n";
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BODY.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: BODY }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["main"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`main`: {e}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`main` not named")).key;
    let mut store = Store::new(front.index.lang("sized"), front.frame);
    let mut buckets = None;
    let prepared = Prepares::new_in(Alloc::global());
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, main).unwrap_or_else(|e| panic!("lowering `main`: {e}"));
    let view = body.lowered.view();
    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `main`: {e:?}"));
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

#[test]
fn his_main_through_the_checker() {
    let start = Instant::now();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let resolved = start.elapsed();
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["main"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`main` from his crate root: {e}"));
    let main = found.whole(1).unwrap_or_else(|| panic!("`main` not named")).key;
    let mut store = Store::new(front.index.lang("sized"), front.frame);
    let mut buckets = None;

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
    let prepared = Prepares::new_in(Alloc::global());
    let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, main) {
        Ok(b) => b,
        Err(e) => {
            out.push_str(&format!("WORK LIST (lowering {}): {e}\n", shown(&names, &crate_names, main)));
            report(out, start);
            return;
        }
    };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling his main's names: {e:?}"));
    let scope = body.scope(&spell);
    let mut consts = Consts::default();
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
    let mut checked = check_body(&mut records, &body, &scope);
    // The check of one body is a function of the program alone: checked again from empty
    // records in this process, every noted item made in the REVERSED order, it must end the
    // same: its diagnostics, its gaps (or refusal) and its recorded callees.
    let first = outcome(&checked);
    {
        // The command's one frame (the names' lifetime is the frame's).
        let mut front_r = Front2::new(&names, front.frame, &sources).unwrap_or_else(|e| panic!("front2 (reversed): {e}"));
        let mut store_r = Store::new(front_r.index.lang("sized"), front_r.frame);
        let mut buckets_r = None;
        let body_r = lower_fn(&names, prepared, &mut front_r.keys, &mut store_r, main).unwrap_or_else(|e| panic!("lowering main again: {e}"));
        let view_r = body_r.lowered.view();
        let spell_r = Spelling::of(&view_r).unwrap_or_else(|e| panic!("spelling his main's names again: {e:?}"));
        let scope_r = body_r.scope(&spell_r);
        let mut consts_r = Consts::default();
        let mut records_r = Records { names: &names, keys: &mut front_r.keys, index: &front_r.index, store: &mut store_r, buckets: &mut buckets_r, order: BuildOrder::Reversed, consts: Some((&mut consts_r, prepared)) };
        let again = outcome(&check_body(&mut records_r, &body_r, &scope_r));
        assert_eq!(first, again, "his main's check depends on the order its records were made in");
    }
    out.push_str(&format!("ORDER: the same outcome with the build order reversed: {first}\n"));
    // A finished check is clean only with no diagnostic: `?`'s variant lang items (`Break`,
    // `Continue`) missing from the index left 11 (E0425, E0277) that read as "type-checks".
    if let Ok(done) = &checked {
        let codes: Vec<&str> = done.diags.iter().map(|d| d.code.code()).collect();
        out.push_str(&format!("his main's diagnostics: {} {codes:?}\n", codes.len()));
    }
    // The trait questions selection was asked while checking, each with its candidates (as
    // filed) and its answer: the traits of the E0277s are among them, and an empty answer
    // over candidates says which header did not fit. A large dump (ASKED / HEAD / PREDS /
    // NODE lines): printed only with `FRONT2_DUMP` set.
    if let Some(b) = records.buckets.as_ref().filter(|_| std::env::var_os("FRONT2_DUMP").is_some()) {
        for (t, s, got) in b.answered() {
            let tn = records.keys.library(t).map_or_else(|| format!("{t:?}"), |k| shown(&names, &crate_names, k));
            let cands = b.trait_candidates(t, s, b.home_of(t, s));
            out.push_str(&format!("ASKED {tn} at {s:?}: {} candidates, {} answer\n", cands.len(), got.len()));
            for k in cands.iter().take(12) {
                let h = records.store.impls.get(&blaze_ladder::front2::keys::checker_of(*k));
                let how = match h {
                    None => "no header".to_string(),
                    Some(h) if h.trait_ == Some(t) => format!("trait fits, picked {}", got.contains(&blaze_ladder::front2::keys::checker_of(*k))),
                    Some(h) => format!("header trait {:?}", h.trait_.and_then(|x| records.keys.library(x)).map(|x| shown(&names, &crate_names, x))),
                };
                out.push_str(&format!("    {} [{how}] unread={} failed={}\n", shown(&names, &crate_names, *k), b.is_unread(*k), b.has_failed(*k)));
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
                    blaze_typeck::PClause::Const { .. } => {}
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
        for (k, why) in b.failed.iter().take(20) {
            out.push_str(&format!("FAILED HEADER {}: {why}\n", shown(&names, &crate_names, *k)));
        }
    }
    // C: every fn reachable from his main, each in its own env (a member's: its container's
    // generics first, then its own; `Self` the impl's self type or the trait's parameter 0).
    // A `main` that does not check is no dead end: the walk goes on from its calls by name.
    let main_ok = usize::from(checked.is_ok());
    let first = match &mut checked {
        Ok(done) => blaze_ladder::front2::reach::First::Checked(done),
        Err(_) => blaze_ladder::front2::reach::First::Unchecked(&body),
    };
    let census = Some(reach(&mut records, prepared, body.owner, first, &scope));
    if let Some(c) = &census {
        let ok = c.fns.iter().filter(|(_, r)| matches!(r, Reached::Checked)).count();
        let no_body = c.fns.iter().filter(|(_, r)| matches!(r, Reached::NoBody)).count();
        let ctors = c.fns.iter().filter(|(_, r)| matches!(r, Reached::Ctor)).count();
        let elsewhere = c.fns.iter().filter(|(_, r)| matches!(r, Reached::AtInstance | Reached::AtObject)).count();
        out.push_str(&format!(
            "REACHABLE: {} of {} reachable functions type-check (his main and {} reached; {ctors} ctors; {elsewhere} trait calls walked where the instance or object is made; {no_body} required trait methods not walked; {} calls not followed: their caller did not check and only its types name them; capped: {})\n",
            ok + main_ok,
            c.fns.len() + 1 - no_body - ctors - elsewhere,
            c.fns.len(),
            c.not_followed,
            c.capped
        ));
        let mut causes: Vec<(&str, usize, String)> = Vec::new();
        for (k, r) in c.fns.iter() {
            if let Reached::Failed { class, detail } = r {
                match causes.iter_mut().find(|x| x.0 == &**class) {
                    Some(x) => x.1 += 1,
                    None => {
                        let name = records.keys.library(*k).map_or_else(|| format!("{k:?}"), |l| shown(&names, &crate_names, l));
                        causes.push((class, 1, format!("{name}: {detail}")));
                    }
                }
            }
        }
        causes.sort_by(|a, b| b.1.cmp(&a.1));
        for (class, n, first) in &causes {
            out.push_str(&format!("  {n} x {class}; the first: {first}\n"));
        }
    }
    // D: what the CODE BUILDER makes of the clean ones: the program with every clean fn tabled
    // (the rest left out), each instance from his `main` built as the run builds it, through the
    // seam; a refusal is filed by its typed name and the walk goes on.
    {
        let mut left_out = Vec::new_in(std::alloc::Global);
        let survey_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
        match blaze_ladder::front2::program::Front2Program::survey(&names, &survey_frame, &sources, &mut left_out) {
            Err(e) => out.push_str(&format!("BUILDS: no survey: {e}\n")),
            Ok(mut program) => {
                let mut refused = Vec::new_in(std::alloc::Global);
                match blaze_ladder::run::build_survey(&mut program, &mut refused) {
                    Err(e) => out.push_str(&format!("BUILDS: the survey stopped: {e}\n")),
                    Ok(counts) => {
                        out.push_str(&format!(
                            "BUILDS: {} of {} clean reachable functions' instances ({} refused; {} generic with no instance; {} left out unchecked)\n",
                            counts.built,
                            counts.tried,
                            counts.tried - counts.built,
                            counts.no_instance.len(),
                            left_out.len()
                        ));
                        let mut classes: Vec<(&str, usize, &str)> = Vec::new();
                        for (f, class) in refused.iter() {
                            match classes.iter_mut().find(|x| x.0 == &**class) {
                                Some(x) => x.1 += 1,
                                None => classes.push((class, 1, f)),
                            }
                        }
                        classes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                        for (class, n, first) in &classes {
                            out.push_str(&format!("  {n} x {class}; the first: `{first}`\n"));
                        }
                    }
                }
            }
        }
    }
    // The ctor rule (the Reference, "Enumerated types"): `Ok` in the value namespace is a fn
    // generic over `Result`'s two parameters, from its one field to `Result<T, E>`; it is made
    // with `Result`'s head, which his `?` and `Ok(())` make the check fill.
    if let Some(c) = ok.and_then(|k| store.ctors.get(&blaze_ladder::front2::keys::checker_of(k))) {
        assert_eq!((c.generics, c.inputs.len()), (2, 1), "`Ok`'s ctor fn: generic over `T, E`, one input");
        out.push_str("ctor rule: `Ok` is a fn over 2 generics with 1 input\n");
    }
    // The `T::X` rule (the Reference, "Paths in types"): `T::X` with `T` a type parameter is
    // `<T as Tr>::X` for the one trait among `T`'s bounds, supertraits included, declaring `X`.
    // core's `ChangeOutputType<T: Try<..>, V>` is written `<T::Residual as Residual<V>>::TryType`
    // (an alias's bounds are read for this): its head must be made, or refused for another cause.
    if let Some(core) = crate_names.iter().position(|n| *n == "core") {
        let segs = ["ops", "try_trait", "ChangeOutputType"];
        let found = names::path(&names, names.root(core), Namespace::Types, Written { kind: PathKind::Plain, segments: &segs }, Alloc::global());
        let key = found.ok().and_then(|f| f.whole(segs.len()).map(|d| d.key));
        match key.map(|k| (store.maps.aliases.get(&k), store.maps.alias_gaps.get(&k))) {
            Some((Some(Some(_)), _)) => out.push_str("T::X rule: core's ChangeOutputType is made, `T::Residual` read through `T: Try`\n"),
            Some((_, Some(why))) => {
                assert!(!matches!(why, Why::AssocNotFound(..) | Why::AssocAmbiguous(..)), "`T::Residual` through `T: Try` must be found: {why:?}");
                out.push_str(&format!("T::X rule: core's ChangeOutputType refused for another cause: {why:?}\n"));
            }
            other => out.push_str(&format!("T::X rule: core's ChangeOutputType not asked by this check ({:?})\n", other.map(|o| o.0.is_some()))),
        }
    }
    let lowered = buckets.as_ref().map_or(0, |b| b.lowered);
    let filed = buckets.as_ref().map(|b| b.filed());
    out.push_str(&format!("impls on a builtin the checker has no type for (filed nowhere): {:?}\n", buckets.as_ref().map(|b| b.no_checker_type)));
    out.push_str(&format!("impl headers lowered by the check: {lowered} of the program's {}; filed (trait+shape, trait any, inherent, inherent any, unread): {filed:?}\n", front.index.impls.len()));
    // The unread impls by class: a count and samples from different crates.
    if let Some(b) = buckets.as_ref() {
        let mut classes: Vec<(String, usize, Vec<String>)> = Vec::new();
        for (k, why) in b.unread_why.iter() {
            let class = why.class();
            let krate = names.crate_of(*k).and_then(|c| crate_names.get(c).copied()).unwrap_or("?");
            let sample = format!("[{krate}] {} : {why:?}", shown(&names, &crate_names, *k));
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
        for (k, why) in b.unread_why.iter() {
            let krate = names.crate_of(*k).and_then(|c| crate_names.get(c).copied()).unwrap_or("?");
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
                        None => unread.push((row, 1, format!("{} {cause}", shown(&names, &crate_names, *k)))),
                    }
                }
            }
            for (row, n, first) in &unread {
                out.push_str(&format!("  unread impls in {row}: {n}; the first: {first}\n"));
            }
            for (i, gap) in gaps.iter().filter(|g| !matches!(g, Gap::UnreadImpl(..))).enumerate() {
                let item = match gap {
                    Gap::VariantOfNoEnum(k) => format!("(lang `{}`)", front.index.lang_name_of(*k).unwrap_or("?")),
                    Gap::TypeAlias(k) => shown(&names, &crate_names, *k),
                    Gap::WhereConst(k) | Gap::CtorFn(k) | Gap::ImplAssocType(k, _) => match front.keys.library(*k) {
                        Some(lib) => shown(&names, &crate_names, lib),
                        None => format!("{k:?}"),
                    },
                    _ => String::new(),
                };
                out.push_str(&format!("  {}. {gap} {item}\n", i + 1));
            }
        }
        Err(CheckFail::Head { error: HeadError::Item(k, why), .. }) => {
            let text = names.item_text(k).map(|t| t.text).unwrap_or("");
            let at = match &why {
                Why::Unsupported(_, r)
                | Why::Program(_, r)
                | Why::Resolve(_, r)
                | Why::NoCheckerType(_, r)
                | Why::AssocNotFound(_, r)
                | Why::AssocAmbiguous(_, _, r)
                | Why::TwoPrincipals(_, _, r)
                | Why::TypeMacro(_, r)
                | Why::PatternType(_, r)
                | Why::ConstPath(_, r) => text.get(r.start as usize..r.end as usize),
                | Why::NeedConst(_, r) => text.get(r.start as usize..r.end as usize),
                Why::Need(_) => None,
            };
            out.push_str(&format!("WORK LIST (one failure): the head of {}: {why:?}, at `{}`\n", shown(&names, &crate_names, k), at.unwrap_or("?")));
            // The probe for a name that names nothing: the same text from the item's module,
            // and its prefixes from his root, through the names library.
            if let (Some(module), Some(word)) = (names.module_of(k), at) {
                let from_item = names::path(&names, module, Namespace::Types, Written { kind: PathKind::Plain, segments: &[word] }, Alloc::global());
                out.push_str(&format!("probe: `{word}` from the item's module: {:?}\n", from_item.as_ref().map(|f| f.whole(1).map(|d| d.key)).map_err(|e| format!("{e}"))));
                for segs in [&["std"][..], &["std", "ffi"][..], &["std", "ffi", word][..], &["std", "path", "Path"][..], &["std", "alloc", "Allocator"][..], &["Allocator"][..], &["Path"][..]] {
                    let got = names::path(&names, module, Namespace::Types, Written { kind: PathKind::Plain, segments: segs }, Alloc::global());
                    out.push_str(&format!("probe: `{}` from the item's module: {:?}\n", segs.join("::"), got.as_ref().map(|f| f.whole(segs.len()).map(|d| d.key)).map_err(|e| format!("{e}"))));
                }
            }
        }
        Err(e) => out.push_str(&format!("WORK LIST (one failure): {e}\n")),
    }
    report(out, start);
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

/// D59's interim choice, from the root file's outline alone: each `main` shape's roots, the
/// refusal of a third parameter by the rule's name, and nothing appended beside an exported
/// entry or on a Rust edition.
#[test]
fn the_entry_is_chosen_by_mains_shape() {
    use blaze_ladder::front2::entry::{EntryError, Roots, append};
    let cfg: [(&str, Option<&str>); 0] = [];
    let e = blaze_surface::Edition::E2024;
    let choose = |text: &str, blaze: bool| {
        let mut t = text.to_string();
        let got = append(&mut t, e, blaze, &cfg).map(|a| a.map(|a| a.roots));
        if let Ok(Some(r)) = got {
            assert_eq!(t, format!("{text}{}", r.text()), "the module is appended after the user's last byte");
        } else {
            assert_eq!(t, text, "nothing appended");
        }
        got
    };
    assert_eq!(choose("fn main() {}\n", true), Ok(Some(Roots::None)));
    assert_eq!(choose("fn main(cx: &Cx) {}\n", true), Ok(Some(Roots::Cx)));
    assert_eq!(choose("fn main(io: Io) {}\n", true), Ok(Some(Roots::Io)));
    assert_eq!(choose(MAIN, true), Ok(Some(Roots::CxIo)));
    assert_eq!(choose(MAIN, false), Ok(None));
    assert_eq!(choose("fn main() {}\n#[unsafe(no_mangle)]\npub extern \"C\" fn bstd_hosted_start() -> i32 { 0 }\n", true), Ok(None));
    assert_eq!(choose("fn main() {}\nmod m {\n    #[export_name = \"main\"]\n    fn start() {}\n}\n", true), Ok(None));
    let three = "fn main(cx: &Cx, io: Io, more: u8) {}\n";
    match choose(three, true) {
        Err(EntryError::TooManyRoots { params: 3, range }) => {
            assert_eq!(&three[range.start as usize..range.end as usize], "&Cx, io: Io, more: u8", "reported at main's parameters");
        }
        other => panic!("a three-parameter `main` is refused by `entry-takes-roots`: {other:?}"),
    }
    assert!(matches!(choose("async fn main() {}\n", true), Err(EntryError::NotPlain { .. })));
}

/// The entry programs of D59's must-keep-passing list through the run path's front half
/// (`run::build_new` over the installed toolchain), each as far as it gets today: built, or
/// its named refusal. With `FRONT2_ENTRY_LIST` set, the lines are written there.
#[test]
fn the_entry_programs_as_far_as_each_gets() {
    use blaze_ladder::run::{NewFront, RunError, build_new};
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let programs: [(&str, &str); 5] = [
        ("main()", "fn main() {}\n"),
        ("main(cx: &Cx)", "fn main(cx: &Cx) {\n    let _ = cx;\n}\n"),
        ("main(io: Io)", "fn main(io: Io) -> Result<(), Error> {\n    io.need_out()?;\n    Ok(())\n}\n"),
        ("main(cx: &Cx, io: Io)", MAIN),
        ("main(cx: &Cx, io: Io, more: u8)", "fn main(cx: &Cx, io: Io, more: u8) {}\n"),
    ];
    let mut out = String::from("ENTRY PROGRAMS (D59 interim, edition blaze-2026.11):\n");
    for (shape, text) in programs {
        let files = [SourceFile { path: "src/main.rs", text }];
        let got = build_new(&files, "src/main.rs", NewFront { package: "entry", edition: EDITION, shelf: Some(&shelf) });
        let line = match &got {
            Ok(_) => "BUILT (front, checker and builder: no refusal)".to_string(),
            Err(e) => format!("REFUSED: {e}"),
        };
        out.push_str(&format!("{shape}: {}\n", line.chars().take(600).collect::<String>()));
        if shape.ends_with("more: u8)") {
            match got {
                Err(RunError::Entry { range, why, .. }) => {
                    assert!(why.contains("entry-takes-roots"), "{why}");
                    assert_eq!(&text[range.start as usize..range.end as usize], "&Cx, io: Io, more: u8");
                }
                Err(other) => panic!("a three-parameter `main` is refused by `entry-takes-roots`, not: {other}"),
                Ok(_) => panic!("a three-parameter `main` is refused by `entry-takes-roots`, not built"),
            }
        }
    }
    print!("{out}");
    if let Ok(path) = std::env::var("FRONT2_ENTRY_LIST") {
        std::fs::write(&path, &out).unwrap_or_else(|e| panic!("writing the entry list to {path}: {e}"));
    }
}
