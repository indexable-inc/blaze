//! Default field values ACROSS CRATES (RFC 3681, `default_field_values`; D126, rule 83
//! `options-struct`): the options struct is declared in the library crate `opts`, and the
//! user crate writes base-less literals of it (`Opts { name: 1, .. }`). Its head, made from the
//! library's text, files its field defaults (front2/heads.rs `Store::adt`), so the user's body
//! reads them through the world's `field_defaults` as a one-crate body does.
//!
//! The ADT's head record (front2/head_record.rs, form 8) carries those defaults: the record of
//! `opts::Opts` decodes to a head equal to the store's (`same`), its field defaults the store's
//! keys, each a fn record of its own (the anonymous constant `const _: TY = EXPR;`). A record
//! of a previous record version is refused as another version, never read.
//!
//! Expected lines (test `an_options_struct_from_another_crate`):
//! `cross: made: clean`, `cross: sum: clean`, `cross: missing: diags [..MissingFields..]` (E0063), then
//! `record: opts::Opts round-trips with 2 field defaults` and the test `ok`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_field_defaults.rs.

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::head_record::{self, HeadAt, HeadRecordError, decode, encode, from_store, same};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{Namespace, PathKind};

/// The library: an options struct with two defaulted fields and one without.
const OPTS: &str = "pub struct Opts { pub width: u32 = 80, pub wrap: bool = false, pub name: u8 }
";

/// The user crate: two base-less literals and one that leaves `name` (no default) unwritten.
const USER: &str = "use opts::Opts;
pub fn made() -> Opts { Opts { name: 1, .. } }
pub fn sum() -> u32 { let o = Opts { name: 2, .. }; o.width + o.name as u32 }
pub fn missing() -> Opts { Opts { width: 1, .. } }
pub fn main() {}
";

/// The texts of the two-crate layout: the library's file for crate `dep`, the rest as
/// [`Inputs`] reads them.
struct WithDep<'t> {
    base: &'t Texts<'t>,
    dep: usize,
    files: &'t [(String, String)],
}

impl<'t> SourceTexts<'t> for WithDep<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == self.dep {
            return self.files.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        self.base.read(krate, path)
    }
}

/// Expected: see the header.
#[test]
fn an_options_struct_from_another_crate() {
    let alloc = Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    // Both crates on the blaze edition: the feature is on unasked.
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user_files = [("/user/src/main.rs".to_string(), USER.to_string())];
    let dep_files = [("/opts/src/lib.rs".to_string(), OPTS.to_string())];
    let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates: Vec<CrateInput<'_>> = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    assert!(sysroot.core.is_some() && sysroot.bstd.is_some(), "the toolchain at {shelf} has `core` and `bstd` units");
    let dep = crates.len();
    let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("opts", dep)].into_boxed_slice());
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = user_deps;
    crates.push(CrateInput {
        name: "opts",
        key: CrateItemKey(ContentId::of("blaze-ladder field defaults cross test crate", b"opts")),
        edition: crate_edition,
        implicit,
        root: "/opts/src/lib.rs",
        shown_from: "/opts/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (two crates): {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let sources = [SourceFile { path: "src/main.rs", text: USER }];
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let mut buckets = None;

    let mut got: Vec<(String, String)> = Vec::new();
    for n in ["made", "sum", "missing"] {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let outcome = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => format!("diags {:?}", done.diags),
                    Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                    Err(e) => format!("not checked: {e}"),
                }
            }
        };
        println!("cross: {n}: {outcome}");
        got.push((n.to_string(), outcome));
    }
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{n}: {o}")).collect();
    let lines = lines.join("\n");
    assert_eq!(got[0].1, "clean", "`made` must check clean\n{lines}");
    assert_eq!(got[1].1, "clean", "`sum` must check clean\n{lines}");
    assert!(got[2].1.starts_with("diags ") && got[2].1.matches("(MissingFields,").count() == 1, "`missing` leaves `name` (no default) unwritten: one E0063\n{lines}");
    // Named as the library's head spells it (front2/check.rs `name_about`).
    assert!(got[2].1.contains("about `name`"), "`missing`'s E0063 names the field `name`\n{lines}");

    // The library ADT's head record carries its field defaults.
    let found = names::path(&names, names.root(dep), Namespace::Types, Written { kind: PathKind::Plain, segments: &["Opts"] }, alloc).unwrap_or_else(|e| panic!("`opts::Opts`: {e}"));
    let lib = found.whole(1).expect("`opts::Opts` named").key;
    store.ensure(&names, &mut front.keys, Want::Adt(lib)).unwrap_or_else(|e| panic!("`opts::Opts`'s head: {e:?}"));
    let filed: Vec<_> = store.field_defaults.get(&checker_of(lib)).expect("`opts::Opts` files its field defaults").iter().copied().collect();
    assert_eq!(filed.iter().filter(|d| d.is_some()).count(), 2, "`width` and `wrap` have defaults, `name` none: {filed:?}");
    let bytes = encode(from_store(&store, HeadAt::Adt(lib)).expect("made"), store.tys.view(), alloc).unwrap_or_else(|e| panic!("`opts::Opts` not encoded: {e}"));
    let decoded = decode(&bytes, None, &store.tys, alloc).unwrap_or_else(|e| panic!("`opts::Opts` not decoded: {e}"));
    let lowered = from_store(&store, HeadAt::Adt(lib)).expect("made");
    if let Some(part) = same(lowered, decoded.as_ref(), store.tys.view()) {
        panic!("`opts::Opts`: the decoded head differs from the lowered one in {part}");
    }
    // Each default is a fn record of its own: the anonymous constant, with its text.
    for k in filed.iter().flatten() {
        let head = from_store(&store, HeadAt::Fn(*k)).unwrap_or_else(|e| panic!("the default {k:?}'s head: {e}"));
        let text = head.text().expect("a default's head keeps its text");
        assert!(text.starts_with("const _: "), "the default {k:?}'s text is its constant: {text:?}");
        let fn_bytes = encode(head, store.tys.view(), alloc).unwrap_or_else(|e| panic!("the default {k:?} not encoded: {e}"));
        decode(&fn_bytes, Some(text), &store.tys, alloc).unwrap_or_else(|e| panic!("the default {k:?} not decoded: {e}"));
    }
    // A record of the previous version is another version: refused, never read.
    let mut old = bytes.to_vec();
    old[4..8].copy_from_slice(&(head_record::VERSION - 1).to_le_bytes());
    match decode(&old, None, &store.tys, alloc) {
        Err(HeadRecordError::Version { found }) if found == head_record::VERSION - 1 => {}
        Err(e) => panic!("a previous-version record: refused, but not as another version: {e}"),
        Ok(_) => panic!("a previous-version record was read"),
    }
    println!("record: opts::Opts round-trips with 2 field defaults");
}
