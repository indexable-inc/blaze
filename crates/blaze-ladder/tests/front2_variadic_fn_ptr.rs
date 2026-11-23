//! A C-variadic fn pointer TYPE in an impl header: `unsafe impl<R, A> E for unsafe extern "C"
//! fn(A, ...) -> R` (objc2 0.6 src/encode.rs, its `Encode` impls for fn pointers, made by a macro
//! for several arities). Before the fix the `...` was read as a parameter with no type and the
//! head was refused (`Unsupported("a fn pointer parameter with no type", ..)`), and every question
//! "the impls of the trait Encode" had a gap.
//!
//! The rule. The Reference, "Function pointer types": a fn pointer type may end its parameters
//! with `...` ("Variadic parameters can only be specified with extern function types with the
//! "C" or "cdecl" calling convention"); it is part of the type, so `unsafe extern "C" fn(A, ...)
//! -> R` and `unsafe extern "C" fn(A) -> R` are two types and the two impls below do not overlap.
//!
//! What a wrong fix fails. A head still refused: `no head`. A `...` dropped from the type (the
//! two self types equal): the self types compare equal, and `ask` and `ask_v` cannot both pick
//! their own impl's `K`. A `...` read as one more input: the self type's list has three entries.
//!
//! The run prints, and must print, exactly: `variadic fn pointers: 2 heads, 2 clean, 1 refused, 0 failed`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::Resolver;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::PNode;

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
unsafe trait E { type K; }
unsafe impl<R, A> E for unsafe extern \"C\" fn(A, ...) -> R { type K = u16; }
unsafe impl<R, A> E for unsafe extern \"C\" fn(A) -> R { type K = u8; }
fn pick<T: E>(_: T) -> Option<T::K> { None }
fn ask(f: unsafe extern \"C\" fn(u8) -> u8) -> Option<u8> { pick(f) }
fn ask_v(f: unsafe extern \"C\" fn(u8, ...) -> u8) -> Option<u16> { pick(f) }
fn wrong(f: unsafe extern \"C\" fn(u8) -> u8) -> Option<u16> { pick(f) }
";

/// The bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 2] = [
    ("ask", "`unsafe extern \"C\" fn(u8) -> u8: E` selects only `fn(A) -> R`'s impl (`K = u8`)"),
    ("ask_v", "`unsafe extern \"C\" fn(u8, ...) -> u8: E` selects only `fn(A, ...) -> R`'s impl (`K = u16`)"),
];

/// The bodies that must be refused, each with the diagnostic it must give.
const REFUSED: [(&str, &str, &str); 1] = [("wrong", "Mismatch", "the non-variadic pointer's `K` is `u8`, never the variadic impl's `u16`: E0308")];

#[test]
fn c_variadic_fn_pointer_types_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/vfp/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "vfp", dir: "/vfp/", root: "/vfp/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let mut failed: Vec<String> = Vec::new();

    // Both impl heads are made; their self types are two fn pointers of two inputs-and-output
    // entries each (`A`, `R`), C-variadic exactly where `...` is written.
    let resolver = Resolver { names: &names };
    let mut selves: Vec<(bool, blaze_typeck::PTy)> = Vec::new();
    for (k, _, _) in names.items(0) {
        if !matches!(resolver.detail(k), Some(blaze_surface::ItemDetail::Impl)) {
            continue;
        }
        let header = names.item_text(k).and_then(|t| t.impl_header).unwrap_or("?");
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Impl(k)) {
            failed.push(format!("impl `{header}`: no head: {e}"));
            continue;
        }
        let Some(head) = store.impls.get(&checker_of(k)) else {
            failed.push(format!("impl `{header}`: made, but no `ImplHead` filed"));
            continue;
        };
        let written = header.contains("...");
        let tys = store.tys.view();
        match tys.get(head.self_ty) {
            Some(PNode::FnPtr(l, h)) => {
                let n = tys.list(*l).map_or(usize::MAX, |x| x.len());
                if h.c_variadic != written || n != 2 {
                    failed.push(format!("impl `{header}`: self type's header {h:?} with {n} entries (want c_variadic {written}, 2 entries)"));
                }
            }
            other => failed.push(format!("impl `{header}`: self type is no fn pointer: {other:?}")),
        }
        selves.push((written, head.self_ty));
    }
    match selves.as_slice() {
        [(a, ta), (b, tb)] if a != b => {
            if ta == tb {
                failed.push(format!("the two impls' self types are one type ({ta:?}): `...` is not part of the type"));
            }
        }
        other => failed.push(format!("want one variadic and one plain impl head, got {other:?}")),
    }
    let heads = selves.len();

    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut clean = 0usize;
    let mut refused = 0usize;
    for (name, proves) in CLEAN {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            failed.push(format!("{name}: no head: {e:?} ({proves})"));
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{name}: does not lower: {e} ({proves})"));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Ok(_) => clean += 1,
            Err(CheckFail::Diags { diags, .. }) => failed.push(format!("{name}: diagnostics {diags:?} ({proves})")),
            Err(other) => failed.push(format!("{name}: {other:?} ({proves})")),
        }
    }
    for (name, code, proves) in REFUSED {
        let body = match store.ensure(&names, &mut front.keys, Want::Fn(lib(name))).map_err(|e| format!("{e:?}")).and_then(|_| {
            lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)).map_err(|e| format!("{e}"))
        }) {
            Ok(b) => b,
            Err(e) => {
                failed.push(format!("{name}: no body to refuse: {e} ({proves})"));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Err(CheckFail::Diags { diags, .. }) if format!("{diags:?}").contains(code) => refused += 1,
            Ok(_) => failed.push(format!("{name}: checked clean, not refused with {code} ({proves})")),
            Err(other) => failed.push(format!("{name}: not refused with {code} ({proves}): {other:?}")),
        }
    }
    for f in &failed {
        println!("{f}");
    }
    println!("variadic fn pointers: {heads} heads, {clean} clean, {refused} refused, {} failed", failed.len());
    assert!(failed.is_empty(), "{} of the variadic fn pointer cases failed (lines above)", failed.len());
    assert_eq!((heads, clean, refused), (2, CLEAN.len(), REFUSED.len()));
}
