//! What a code identity keeps and what changes it. Every program is built in a database of its
//! own, as two processes or two machines would build it, so an interned id, an address or a
//! hash map's order that reached a hash would fail each test.

mod compile;

use base_db::Db;
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty::{
    ParamEnvAndCrate,
    mir::{MirOptLevel, MirOptions, MirOwner, MirStage},
    next_solver::{GenericArgs, ParamEnv},
    providers::monomorphized_mir_at,
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir as _;
use hir_ty_mir::mir::dispatch::Body;

use crate::{CodeId, CompileIdentity, Compiled, Instance, Subject, code_id, instance_identity};

const MINICORE: &str = "sized, copy, clone, drop, size_of, fn";

/// A debug unit: MIR at level 1, nothing inlined.
const DEBUG: Compiled = Compiled::every(MirOptions {
    level: MirOptLevel::Debug,
    ub_checks: false,
    lifetime_markers: true,
    inline: false,
    heap_to_stack: false,
    world: None,
});

/// A release unit: MIR at level 2 with the inliner on.
const INLINE: Compiled = Compiled::every(MirOptions {
    level: MirOptLevel::Release,
    ub_checks: false,
    lifetime_markers: true,
    inline: true,
    heap_to_stack: false,
    world: None,
});

/// The options [`INLINE`] compiles every body with.
const INLINE_OPTIONS: MirOptions = MirOptions {
    level: MirOptLevel::Release,
    ub_checks: false,
    lifetime_markers: true,
    inline: true,
    heap_to_stack: false,
    world: None,
};

/// A crate `kernel` whose root file is at `path`, with `text`, in a database of its own.
fn program(path: &str, text: &str) -> Db {
    fixture::db(&format!("//- minicore: {MINICORE}\n//- {path} crate:kernel\n{text}")).into_db()
}

#[track_caller]
fn function(db: &Db, name: &str) -> FunctionId {
    let found: Vec<FunctionId> = crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .filter_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

fn env(db: &Db) -> StoredParamEnvAndCrate {
    ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: fixture::test_crate(db) }.store()
}

fn instance(db: &Db, name: &str) -> Instance {
    Instance {
        subject: Subject::Body(Body::Fn(function(db, name))),
        args: GenericArgs::new_from_slice(&[]).store(),
    }
}

fn identity(db: &Db, name: &str, compiled: Compiled) -> CompileIdentity {
    blaze_query::db::attach(db, || {
        match instance_identity(db, &instance(db, name), &env(db), compiled) {
            Ok(it) => *it,
            Err(error) => panic!("`{name}` has no identity: {error}"),
        }
    })
}

fn step(db: &Db, name: &str) -> CodeId {
    blaze_query::db::attach(db, || match code_id(db, &instance(db, name), &env(db)) {
        Ok(it) => it,
        Err(error) => panic!("`{name}` has no CodeId: {error}"),
    })
}

/// Every identity of `name` there is: in a debug unit, in an inlining release unit, and of the
/// step it roots.
fn all(db: &Db, name: &str) -> (CompileIdentity, CompileIdentity, CodeId) {
    (identity(db, name, DEBUG), identity(db, name, INLINE), step(db, name))
}

const PLAIN: &str = r#"
pub struct S { pub a: u32 }
#[inline]
fn helper(x: u32) -> u32 { x ^ 1 }
pub fn f(x: u32) -> usize { helper(x) as usize + core::mem::size_of::<S>() }
"#;

/// `PLAIN` with comments, blank lines and another layout: the same items, each further down.
const COMMENTED: &str = r#"
// A comment above every item, so each one starts on another line.

/* and a block comment */
pub struct S {
    // the field
    pub a: u32,
}

#[inline]
fn helper(x: u32) -> u32 {
    // why
    x ^ 1
}


pub fn f(x: u32) -> usize {
    helper(x) as usize + core::mem::size_of::<S>() // trailing
}
"#;

/// No key reads a path: the same source checked out at two places is one code.
#[test]
fn same_source_in_two_checkouts_has_one_identity() {
    let here = program("/home/a/checkout/src/lib.rs", PLAIN);
    let there = program("/tmp/elsewhere/blaze/src/lib.rs", PLAIN);
    assert_eq!(all(&here, "f"), all(&there, "f"));
}

/// Comments and layout are not code: positions are span-table indices, never lines.
#[test]
fn a_comment_edit_keeps_the_identity() {
    let plain = program("/src/lib.rs", PLAIN);
    let commented = program("/src/lib.rs", COMMENTED);
    assert_eq!(all(&plain, "f"), all(&commented, "f"));
}

/// A function that reads a struct's size is keyed by the struct's layout; one that does not
/// name it is not.
#[test]
fn a_field_added_to_a_struct_whose_size_is_read_changes_the_identity() {
    let text = |fields: &str| {
        format!(
            "pub struct S {{ {fields} }}\npub fn f() -> usize {{ core::mem::size_of::<S>() }}\npub fn g(x: u32) -> u32 {{ x ^ 1 }}\n"
        )
    };
    let before = program("/src/lib.rs", &text("pub a: u32"));
    let after = program("/src/lib.rs", &text("pub a: u32, pub b: u64"));
    let (debug, inline, step) = all(&before, "f");
    let (debug_after, inline_after, step_after) = all(&after, "f");
    assert_ne!(debug, debug_after);
    assert_ne!(inline, inline_after);
    assert_ne!(step, step_after);
    assert_eq!(all(&before, "g"), all(&after, "g"));
}

/// Whether a release unit's MIR of the function `name` has a callee inlined into it.
fn inlines(db: &Db, name: &str) -> bool {
    blaze_query::db::attach(db, || {
        let owner = MirOwner::Body(function(db, name).into());
        let body = monomorphized_mir_at(
            db,
            owner,
            MirStage::compiled(owner, INLINE_OPTIONS),
            GenericArgs::new_from_slice(&[]).store(),
            env(db),
        )
        .unwrap_or_else(|error| panic!("`{name}` does not lower: {error:?}"));
        !body.inlined.callees.is_empty()
    })
}

/// A caller's inlined code (`INLINED_DOMAIN`) is each inlined callee's code: an edit to the
/// callee's body changes the identity of a caller it is inlined into, a comment-only edit
/// inside that body keeps it, and a unit that inlines nothing reads neither.
#[test]
fn an_inlined_callee_is_in_its_callers_identity_by_its_code() {
    let text = |body: &str| {
        format!(
            "#[inline(always)]\nfn callee(x: u32) -> u32 {{ {body} }}\npub fn caller(x: u32) -> u32 {{ callee(x) + 1 }}\n"
        )
    };
    let plain = program("/src/lib.rs", &text("let y = x; y ^ 1"));
    let commented = program("/src/lib.rs", &text("/* why */ let y = x; // and how\n y ^ 1"));
    let edited = program("/src/lib.rs", &text("let y = x; y ^ 2"));
    assert!(inlines(&plain, "caller"), "the release unit inlines `callee` into `caller`");
    let inlined = |db: &Db| identity(db, "caller", INLINE);
    assert_eq!(inlined(&plain), inlined(&commented), "a comment in the callee is not code");
    assert_ne!(
        inlined(&plain),
        inlined(&edited),
        "the callee's edited body is in the caller's identity"
    );
    assert_eq!(
        identity(&plain, "caller", DEBUG),
        identity(&edited, "caller", DEBUG),
        "nothing is inlined at level 1"
    );
}

/// A callee's body is in a caller's code only where it was inlined: a plain call names it by
/// its symbol. What the step can run changes either way.
#[test]
fn a_callee_body_edit_changes_only_an_inlining_caller() {
    let text = |mask: u32| {
        format!(
            "#[inline]\npub fn callee(x: u32) -> u32 {{ x ^ {mask} }}\npub fn caller(x: u32) -> u32 {{ callee(x) }}\n"
        )
    };
    let before = program("/src/lib.rs", &text(1));
    let after = program("/src/lib.rs", &text(2));
    assert!(inlines(&before, "caller"), "the release unit inlines `callee` into `caller`");
    assert_ne!(identity(&before, "caller", INLINE), identity(&after, "caller", INLINE));
    assert_eq!(identity(&before, "caller", DEBUG), identity(&after, "caller", DEBUG));
    assert_ne!(step(&before, "caller"), step(&after, "caller"));
}

/// A recursion is hashed as one component: each member's CodeId is the same whichever instance
/// was asked first, the members' differ, and an edit to one member changes every step that
/// reaches it.
#[test]
fn a_recursion_has_one_identity_whatever_is_asked_first() {
    let text = |answer: &str| {
        format!(
            "pub fn even(n: u32) -> bool {{ match n {{ 0 => true, _ => odd(n) }} }}\n\
             pub fn odd(n: u32) -> bool {{ match n {{ 0 => {answer}, _ => even(0) }} }}\n\
             pub fn top(n: u32) -> bool {{ even(n) }}\n"
        )
    };
    let down = program("/src/lib.rs", &text("false"));
    let (top, even, odd) = (step(&down, "top"), step(&down, "even"), step(&down, "odd"));
    let up = program("/src/lib.rs", &text("false"));
    let (odd_up, even_up, top_up) = (step(&up, "odd"), step(&up, "even"), step(&up, "top"));
    assert_eq!((top, even, odd), (top_up, even_up, odd_up));
    assert_ne!(even, odd);
    let edited = program("/src/lib.rs", &text("true"));
    assert_ne!(step(&edited, "top"), top);
}

/// `library/core/src/reflect.rs` reduced to what a `for const` body lowers against (the same as
/// hir-ty-mir's `tests::mir::comptime`), and a Debug-like printer over a struct's fields.
const FOR_CONST_DEBUG: &str = r#"
#![feature(blaze_comptime, lang_items)]
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);
pub fn fields_of<T: ?Sized>() -> Fields<T> { loop {} }
pub trait Show {}
impl Show for u8 {}
fn show<X: Show + ?Sized>(_: &X) {}
pub fn debug<T>(t: &T) where for<F: Field<T>> F::Type: Show {
    for const f in fields_of::<T>() { show(&t.[f]); }
}
"#;

#[track_caller]
fn struct_ty<'db>(db: &'db Db, name: &str) -> hir_ty::next_solver::Ty<'db> {
    use hir_ty::db::HirDatabase as _;
    let found: Vec<hir_def::StructId> = crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .filter_map(|def| match def {
            ModuleDefId::AdtId(hir_def::AdtId::StructId(it))
                if hir_def::signatures::StructSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => db.ty(hir_ty::TyDefId::AdtId(one.into())).skip_binder(),
        _ => panic!("{} structs named `{name}`", found.len()),
    }
}

/// A body with a `for const` loop is compiled per instance, keyed by its arguments (C2): its
/// expansion reads the fields' names, which no fact of a local reads, so two structs of one
/// shape whose fields are named apart (`S { a: u8 }` edited to `S { b: u8 }`) must not share an
/// expanded body. The identity of the argument tells them apart, and the body is never refused
/// as one with no identity (`Unshared::Shape`), which the code generator fails on.
#[test]
fn a_for_const_instance_is_keyed_by_its_fields_names() {
    let text = |field: &str| format!("{FOR_CONST_DEBUG}pub struct S {{ pub {field}: u8 }}\n");
    let named_a = program("/src/lib.rs", &text("a"));
    let named_b = program("/src/lib.rs", &text("b"));
    let keyed = |db: &Db| {
        blaze_query::db::attach(db, || {
            let s = struct_ty(db, "S");
            let instance = Instance {
                subject: Subject::Body(Body::Fn(function(db, "debug"))),
                args: GenericArgs::new_from_slice(&[s.into()]).store(),
            };
            match instance_identity(db, &instance, &env(db), DEBUG) {
                Err(hir_ty_share::Unshared::PerInstance(_)) => {}
                other => panic!("`debug::<S>` is compiled per instance, not {other:?}"),
            }
            hir_ty_share::identity::ty(db, s).unwrap_or_else(|why| panic!("`S`: {why}"))
        })
    };
    assert_eq!(keyed(&named_a), keyed(&program("/src/lib.rs", &text("a"))), "built twice");
    assert_ne!(keyed(&named_a), keyed(&named_b), "a field's name is in its struct's identity");
}

/// A constant's `dyn Debug` holding a `dyn Display` (D1, review 7).
fn dyn_behind_dyn(display: &str) -> Db {
    let text = format!(
        "use core::fmt::{{Debug, Display, Formatter, Result}};\n\
         fn pick(_: u32) {{}}\n\
         pub struct MyT;\n\
         impl Debug for MyT {{ fn fmt(&self, _: &mut Formatter<'_>) -> Result {{ Ok(()) }} }}\n\
         impl Display for MyT {{ fn fmt(&self, _: &mut Formatter<'_>) -> Result {{ pick({display}); Ok(()) }} }}\n\
         pub struct W(&'static dyn Display);\n\
         impl Debug for W {{ fn fmt(&self, f: &mut Formatter<'_>) -> Result {{ Display::fmt(self.0, f) }} }}\n\
         const C: &dyn Debug = &W(&MyT);\n\
         pub fn f() -> &'static dyn Debug {{ C }}\n"
    );
    fixture::db(&format!("//- minicore: {MINICORE}, fmt, unsize, coerce_unsized, dispatch_from_dyn\n//- /src/lib.rs crate:kernel\n{text}")).into_db()
}

/// Each vtable in a constant is the one for the `dyn` at its own pointer's place (D1): `C`'s
/// table holds `W` (for `dyn Debug`) and `MyT` (for the `dyn Display` inside `W`), so an edit to
/// `MyT`'s `Display::fmt` is in `f`'s code id. Giving every vtable the one `dyn` the constant's
/// type names hashed `MyT` for `dyn Debug`, which left that edit out.
#[test]
fn a_dyn_behind_a_dyn_in_a_constant_is_hashed_for_its_own_trait() {
    let before = dyn_behind_dyn("1");
    let built_twice = dyn_behind_dyn("1");
    let after = dyn_behind_dyn("2");
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`MyT`'s `Display::fmt` is in `f`'s code id");
}

/// A program of handlers, each a function item made a `dyn Handler + Sync` through a blanket
/// impl over `Fn` (review 8, C1), whose `call` runs `log`, returning `logged`; `held` declares
/// the statics and `f` returns `called`.
fn handlers(logged: &str, held: &str, called: &str) -> Db {
    let text = format!(
        "fn log() -> u32 {{ {logged} }}\n\
         pub trait Handler {{ fn call(&self, r: u32) -> u32; }}\n\
         impl<F: Fn(u32) -> u32> Handler for F {{ fn call(&self, r: u32) -> u32 {{ log() + self(r) }} }}\n\
         fn index(r: u32) -> u32 {{ r }}\n\
         pub struct Wrap<T: ?Sized>(pub u32, pub T);\n\
         {held}\n\
         pub fn f() -> u32 {{ {called} }}\n"
    );
    fixture::db(&format!(
        "//- minicore: {MINICORE}, sync, unsize, coerce_unsized, dispatch_from_dyn, slice, str, index\n//- /src/lib.rs crate:kernel\n{text}"
    ))
    .into_db()
}

/// A table of handlers (review 8, C1; `Sync` as a static needs it, review 9 M4).
fn handler_table(logged: &str) -> Db {
    handlers(
        logged,
        "static ROUTES: &[(&str, &(dyn Handler + Sync))] = &[(\"/\", &index)];",
        "ROUTES[0].1.call(1)",
    )
}

/// A function item's id in a constant is its pointer and its vtable at once: `&index as &dyn
/// Handler` is held as `index`, as `index as fn(_)` would be, so its `dyn Handler` is found by
/// walking the value like any vtable's, and `<fn index as Handler>::call` and the `log` it runs
/// are in `f`'s code id. Hashing a function's id as its pointer alone left them out, so an
/// edit to `log` kept `f`'s cached result.
#[test]
fn a_function_item_made_a_dyn_in_a_static_is_hashed_for_its_trait() {
    let before = handler_table("1");
    let built_twice = handler_table("1");
    let after = handler_table("2");
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`log` is in `f`'s code id");
}

/// A pointer to a struct whose tail is a `dyn` carries the vtable of the tail's concrete type
/// for that `dyn` (review 9, C1), as the code generator's `relocate_pointer` emits it: `W`'s
/// `&Wrap<dyn Handler + Sync>` holds `index`'s id as its metadata, so `<fn index as
/// Handler>::call` and the `log` it runs are in `f`'s code id. Pairing only a pointer whose
/// pointee is itself a `dyn` hashed `index` as a function pointer and left `log` out.
#[test]
fn a_dyn_at_a_struct_tail_in_a_static_is_hashed_for_its_trait() {
    let held = "static W: &Wrap<dyn Handler + Sync> = &Wrap(0, index);";
    let before = handlers("1", held, "W.1.call(1)");
    let built_twice = handlers("1", held, "W.1.call(1)");
    let after = handlers("2", held, "W.1.call(1)");
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`log` is in `f`'s code id");
}

/// A value with a `dyn Debug` and a struct whose tail is a `dyn Display`, both of one concrete
/// type, in one static.
fn debug_and_tail(display: &str, held: &str) -> Db {
    let text = format!(
        "use core::fmt::{{Debug, Display, Formatter, Result}};\n\
         fn pick(_: u32) {{}}\n\
         pub struct MyT(pub u32);\n\
         impl Debug for MyT {{ fn fmt(&self, _: &mut Formatter<'_>) -> Result {{ Ok(()) }} }}\n\
         impl Display for MyT {{ fn fmt(&self, _: &mut Formatter<'_>) -> Result {{ pick({display}); Ok(()) }} }}\n\
         pub struct Wrap<T: ?Sized>(pub u32, pub T);\n\
         {held}\n\
         pub fn f() -> &'static S {{ &S }}\n"
    );
    fixture::db(&format!(
        "//- minicore: {MINICORE}, sync, fmt, unsize, coerce_unsized, dispatch_from_dyn\n//- /src/lib.rs crate:kernel\n{text}"
    ))
    .into_db()
}

/// `MyT`'s one vtable id is for `dyn Debug` at the tuple's first field and for `dyn Display`
/// at the tail of `Wrap` in its second (review 9, C1), so an edit to `MyT`'s `Display::fmt` is
/// in `f`'s code id. Pairing only `dyn` pointees found `dyn Debug` alone and left it out.
#[test]
fn a_vtable_for_a_dyn_and_a_struct_tail_is_hashed_for_both_traits() {
    let held = "pub type S = (&'static (dyn Debug + Sync), &'static Wrap<dyn Display + Sync>);\n\
                pub static S: S = (&MyT(1), &Wrap(0, MyT(2)));";
    let before = debug_and_tail("1", held);
    let built_twice = debug_and_tail("1", held);
    let after = debug_and_tail("2", held);
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`MyT`'s `Display::fmt` is in `f`'s code id");
}

/// A vtable whose type is at a struct's tail and nowhere else is paired with the tail's `dyn`
/// (review 9, C1), so the static has a code id. Pairing only `dyn` pointees found no trait for
/// `MyT`'s vtable and refused it.
#[test]
fn a_vtable_only_at_a_struct_tail_is_accepted() {
    let held = "pub type S = &'static Wrap<dyn Display + Sync>;\n\
                pub static S: S = &Wrap(0, MyT(2));";
    step(&debug_and_tail("1", held), "f");
}

/// A struct with a `dyn` tail holds the tail's concrete value after its sized fields, walked by
/// its type as the code generator's `walk_allocation` walks it (review 10, C1): `Inner` at
/// `W`'s tail holds `&index` as a `&dyn Handler`, so `<fn index as Handler>::call` and the `log`
/// it runs are in `f`'s code id. Walking only the struct's own fields hashed `index` as a
/// function pointer and left `log` out.
#[test]
fn a_dyn_inside_a_struct_tail_in_a_static_is_hashed_for_its_trait() {
    let held = "pub trait Tr {}\n\
                pub struct Inner(pub &'static (dyn Handler + Sync));\n\
                impl Tr for Inner {}\n\
                static W: &Wrap<dyn Tr + Sync> = &Wrap(0, Inner(&index));";
    let before = handlers("1", held, "W.0");
    let built_twice = handlers("1", held, "W.0");
    let after = handlers("2", held, "W.0");
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`log` is in `f`'s code id");
}

/// `Inner` holding a `&dyn Display`, put at a `Wrap<dyn Tr>`'s tail.
const INNER: &str = "pub trait Tr {}\n\
                     pub struct Inner(pub &'static (dyn Display + Sync));\n\
                     impl Tr for Inner {}\n";

/// `MyT`'s one vtable id is for `dyn Debug` at the tuple's first field and for `dyn Display`
/// inside the concrete `Inner` at `Wrap`'s tail (review 10, C1), so an edit to `MyT`'s
/// `Display::fmt` is in `f`'s code id. Not walking the tail found `dyn Debug` alone.
#[test]
fn a_vtable_for_a_dyn_and_one_inside_a_struct_tail_is_hashed_for_both_traits() {
    let held = format!(
        "{INNER}pub type S = (&'static (dyn Debug + Sync), &'static Wrap<dyn Tr + Sync>);\n\
         pub static S: S = (&MyT(1), &Wrap(0, Inner(&MyT(2))));"
    );
    let before = debug_and_tail("1", &held);
    let built_twice = debug_and_tail("1", &held);
    let after = debug_and_tail("2", &held);
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`MyT`'s `Display::fmt` is in `f`'s code id");
}

/// A vtable held only inside a struct tail's concrete value is paired with its `dyn` (review
/// 10, C1), so the static has a code id. Not walking the tail found no trait for `MyT`'s vtable
/// and refused it.
#[test]
fn a_vtable_only_inside_a_struct_tail_is_accepted() {
    let held = format!(
        "{INNER}pub type S = &'static Wrap<dyn Tr + Sync>;\npub static S: S = &Wrap(0, Inner(&MyT(2)));"
    );
    step(&debug_and_tail("1", &held), "f");
}

/// A pointer to a struct with a `dyn` tail points at the struct, walked as its own type, not
/// at the tail's concrete value (review 10): `Pre`'s first field holds a `&dyn Display`, so an
/// edit to `MyT`'s `Display::fmt` is in `f`'s code id. Walking the memory as the concrete `MyT`,
/// as a pointer to a bare `dyn` is walked, found `dyn Debug` alone.
#[test]
fn a_dyn_before_a_struct_tail_is_hashed_for_its_trait() {
    let held = "pub struct Pre<T: ?Sized>(pub &'static (dyn Display + Sync), pub T);\n\
                pub type S = &'static Pre<dyn Debug + Sync>;\n\
                pub static S: S = &Pre(&MyT(2), MyT(1));";
    let before = debug_and_tail("1", held);
    let built_twice = debug_and_tail("1", held);
    let after = debug_and_tail("2", held);
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`MyT`'s `Display::fmt` is in `f`'s code id");
}

/// A closure's captures are walked as a struct's fields, as the code generator's `relocate`
/// walks them (joint review 1, C1): `W`'s closure captures `h`, `&index` as a `&dyn Handler`, so
/// `<fn index as Handler>::call` and the `log` it runs are in `f`'s code id. Not walking
/// captures hashed `index` as a function pointer only and left `log` out.
#[test]
fn a_dyn_a_closure_captures_in_a_static_is_hashed_for_its_trait() {
    let held = "static W: &(dyn Fn() -> u32 + Sync) = \
                &{ let h: &'static (dyn Handler + Sync) = &index; move || h.call(1) };";
    let before = handlers("1", held, "W()");
    let built_twice = handlers("1", held, "W()");
    let after = handlers("2", held, "W()");
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`log` is in `f`'s code id");
}

/// A vtable held only in a closure's capture is paired with the capture's `dyn` (joint review
/// 1, C1), so the static has a code id, and `MyT`'s `Display::fmt` is in it. Not walking
/// captures found no trait for `MyT`'s vtable and refused it.
#[test]
fn a_vtable_only_in_a_closure_capture_is_accepted() {
    let held = "fn keep(_: &(dyn Display + Sync)) {}\n\
                pub type S = &'static (dyn Fn() -> u32 + Sync);\n\
                pub static S: S = &{ let h: &'static (dyn Display + Sync) = &MyT(2); move || { keep(h); 0 } };";
    let before = debug_and_tail("1", held);
    let after = debug_and_tail("2", held);
    assert_ne!(step(&before, "f"), step(&after, "f"), "`MyT`'s `Display::fmt` is in `f`'s code id");
}

/// A raw pointer one past the end of its memory is not followed (review 9): `S` points past
/// `Two`'s last byte at a `&dyn Display`, an address no piece of memory holds (`memory_at` finds none
/// at a piece's end), so nothing is read and `f` has a code id. (`P`'s `&dyn Display` gives the value a vtable,
/// so it is walked.)
#[test]
fn a_raw_pointer_one_past_the_end_is_accepted() {
    let held = "#[repr(C)] pub struct Two(pub &'static u32, pub [&'static u32; 0]);\n\
                pub struct P(pub *const &'static (dyn Display + Sync), pub &'static (dyn Display + Sync));\n\
                unsafe impl Sync for P {}\n\
                const T: &Two = &Two(&1, []);\n\
                pub type S = P;\n\
                pub static S: S = P(&T.1 as *const [&'static u32; 0] as *const &'static (dyn Display + Sync), &MyT(1));";
    step(&debug_and_tail("1", held), "f");
}

/// A raw pointer into its memory whose pointee runs past the memory's end is not followed
/// (review 9): `S` points at `Pair`'s second `u32` as a `&dyn Display`, 16 bytes where 4 are
/// left, so nothing is read and `f` has a code id (`P`'s `&dyn Display` gives the value a
/// vtable, so it is walked). Following it read past the memory and refused.
#[test]
fn a_raw_pointer_whose_pointee_runs_past_its_memory_is_accepted() {
    let held = "#[repr(C)] pub struct Pair(pub u32, pub u32);\n\
                pub struct P(pub *const &'static (dyn Display + Sync), pub &'static (dyn Display + Sync));\n\
                unsafe impl Sync for P {}\n\
                const T: &Pair = &Pair(1, 2);\n\
                pub type S = P;\n\
                pub static S: S = P(&T.1 as *const u32 as *const &'static (dyn Display + Sync), &MyT(1));";
    step(&debug_and_tail("1", held), "f");
}

/// A raw pointer whose pointee fits its memory exactly is followed (review 9): `S` points at
/// a `&dyn Display` filling its memory, so `MyT`'s vtable is paired with `dyn Display` and an
/// edit to `MyT`'s `Display::fmt` is in `f`'s code id. A bound one byte short didn't follow it,
/// and refused the vtable as held at no place whose type names its trait.
#[test]
fn a_dyn_behind_a_raw_pointer_that_fits_is_hashed_for_its_trait() {
    let held = "pub struct P(pub *const &'static (dyn Display + Sync));\n\
                unsafe impl Sync for P {}\n\
                pub type S = P;\n\
                pub static S: S = P(&(&MyT(2) as &'static (dyn Display + Sync)) as *const &'static (dyn Display + Sync));";
    let before = debug_and_tail("1", held);
    let built_twice = debug_and_tail("1", held);
    let after = debug_and_tail("2", held);
    assert_eq!(step(&before, "f"), step(&built_twice, "f"), "built twice");
    assert_ne!(step(&before, "f"), step(&after, "f"), "`MyT`'s `Display::fmt` is in `f`'s code id");
}

/// A vtable const eval made for a temporary the value doesn't keep is no part of the value
/// (review 8): `C`'s evaluation unsizes a `&u16` to `&dyn B` and drops it, so its table
/// holds that vtable at no place of the value. No word of the value holds its id, so nothing
/// could emit it, and `f` has a code id rather than a refusal.
#[test]
fn a_vtable_of_a_dropped_temporary_is_no_part_of_a_constant() {
    let text = "pub trait A { fn a(&self) -> u32; }\npub trait B { fn b(&self) -> u32; }\n\
                impl A for u32 { fn a(&self) -> u32 { *self } }\nimpl B for u16 { fn b(&self) -> u32 { 0 } }\n\
                const fn keep(_: &dyn B) {}\n\
                const C: &dyn A = { let x = 1u16; let y: &dyn B = &x; keep(y); &2u32 };\n\
                pub fn f() -> u32 { C.a() }\n";
    let db = fixture::db(&format!(
        "//- minicore: {MINICORE}, unsize, coerce_unsized, dispatch_from_dyn\n//- /src/lib.rs crate:kernel\n{text}"
    ))
    .into_db();
    step(&db, "f");
}

/// The associated constant named `name` of the crate's impls: what `f` reads by projection.
#[track_caller]
fn impl_const(db: &Db, name: &str) -> hir_def::ConstId {
    let found: Vec<hir_def::ConstId> = hir_def::test_items::impls(db, fixture::test_crate(db))
        .into_iter()
        .flat_map(|imp| imp.impl_items(db).items.to_vec())
        .filter_map(|(_, item)| match item {
            hir_def::AssocItemId::ConstId(it)
                if hir_def::signatures::ConstSignature::of(db, it)
                    .name
                    .as_ref()
                    .is_some_and(|it| it.as_str() == name) =>
            {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} associated constants named `{name}`", found.len()),
    }
}

/// What the code generator reads of the evaluated value of the impl's constant `name`, field by
/// field, for a test to check two builds differ where it means them to and nowhere else.
#[derive(Debug, PartialEq, Eq)]
struct Evaluated {
    bytes: Vec<u8>,
    /// Each piece's address, bytes, alignment and the words in it with provenance.
    pieces: Vec<(usize, Vec<u8>, Option<usize>, Vec<usize>)>,
    value_pointers: Vec<usize>,
    /// Each static's memory: where it is, its size.
    statics: Vec<(usize, usize)>,
}

fn evaluated(db: &Db, name: &str) -> Evaluated {
    use hir_ty::db::HirDatabase as _;
    let id = impl_const(db, name);
    blaze_query::db::attach(db, || {
        let value = db
            .const_eval(id, GenericArgs::empty(), None)
            .unwrap_or_else(|e| panic!("`{name}`: {e:?}"));
        let map = &value.memory_map;
        let (pieces, statics) = match map {
            hir_ty::MemoryMap::Complex(complex) => (
                (complex.memory.iter())
                    .map(|(&at, bytes)| {
                        (
                            at,
                            bytes.to_vec(),
                            map.align_at(at),
                            map.pointers_in(at..at + bytes.len()).to_vec(),
                        )
                    })
                    .collect(),
                complex.statics.iter().map(|it| (it.start, it.size)).collect(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        Evaluated {
            bytes: value.memory.to_vec(),
            pieces,
            value_pointers: map.value_pointers().to_vec(),
            statics,
        }
    })
}

/// A constant that points at one of two statics a body named before it (review C1). The
/// constant's bytes are equal either way, the one static at one address, and the code generator
/// relocates its pointer to `A` in one build and to `B` in the other: `f`'s code id must tell
/// them apart. A static met again used to hash nothing, so it did not.
#[test]
fn a_constant_pointing_at_one_of_two_statics_named_before_names_which() {
    let case = |which: &str| {
        program(
            "/src/lib.rs",
            &format!(
                "pub static A: u32 = 1;\npub static B: u32 = 2;\n\
                 pub trait Get {{ const P: &'static u32; }}\npub struct K;\n\
                 impl Get for K {{ const P: &'static u32 = &{which}; }}\n\
                 pub fn f() -> u32 {{ let a = &A; let b = &B; *a + *b + *<K as Get>::P }}\n"
            ),
        )
    };
    let (a, b) = (case("A"), case("B"));
    assert_eq!(
        evaluated(&a, "P"),
        evaluated(&b, "P"),
        "the constant's value is the same bytes either way"
    );
    assert_eq!(step(&a, "f"), step(&case("A"), "f"), "built twice");
    assert_ne!(
        step(&a, "f"),
        step(&b, "f"),
        "which static the constant points at is in `f`'s code id"
    );
}

/// Two builds of a constant whose value differs only in whether one word is a pointer with
/// provenance or an equal number (review C2): the code generator relocates the one and emits
/// the other as it is, so `f`'s code id must differ.
#[test]
fn a_word_a_pointer_in_one_build_and_an_equal_number_in_another_is_two_code_ids() {
    let case = |union: &str| {
        program(
            "/src/lib.rs",
            &format!(
                "pub static SX: u8 = 5;\npub union U {{ pub p: *const u8, pub n: usize }}\n\
                 pub trait Get {{ const P: (&'static u8, U); }}\npub struct K;\n\
                 impl Get for K {{ const P: (&'static u8, U) = (&SX, {union}); }}\n\
                 pub fn f() {{ let _x = <K as Get>::P; }}\n"
            ),
        )
    };
    let pointer = case("U { p: &SX as *const u8 }");
    let before = evaluated(&pointer, "P");
    let address = usize::from_le_bytes(before.bytes[..8].try_into().expect("a word"));
    let number = case(&format!("U {{ n: {address:#x} }}"));
    let after = evaluated(&number, "P");
    assert_eq!(
        (&before.bytes, &before.pieces, &before.statics),
        (&after.bytes, &after.pieces, &after.statics)
    );
    assert_ne!(
        before.value_pointers, after.value_pointers,
        "one word has provenance in one build only"
    );
    assert_eq!(step(&pointer, "f"), step(&case("U { p: &SX as *const u8 }"), "f"), "built twice");
    assert_ne!(
        step(&pointer, "f"),
        step(&number, "f"),
        "which words are pointers is in `f`'s code id"
    );
}

/// Two builds of a constant whose one piece of memory differs only in its alignment, at the
/// same address with the same bytes (review C2): the code generator emits its data object with
/// that alignment, so `f`'s code id must differ. Six bytes kept first put the piece at an
/// address that is a multiple of 8 either way.
#[test]
fn a_piece_aligned_otherwise_at_the_same_address_is_two_code_ids() {
    let case = |init: &str| {
        let ty = "(&'static u8, &'static u8, &'static u8, &'static u8, &'static u8, &'static u8, *const u8)";
        let value = format!("(&1u8, &2u8, &3u8, &4u8, &5u8, &6u8, {init})");
        program(
            "/src/lib.rs",
            &format!(
                "pub trait Get {{ const P: {ty}; }}\npub struct K;\n\
                 impl Get for K {{ const P: {ty} = {value}; }}\n\
                 pub fn f() {{ let _x = <K as Get>::P; }}\n"
            ),
        )
    };
    let bytes = case("&[0u8; 8] as *const [u8; 8] as *const u8");
    let word = case("&0u64 as *const u64 as *const u8");
    let (before, after) = (evaluated(&bytes, "P"), evaluated(&word, "P"));
    let without_align = |it: &Evaluated| {
        let pieces: Vec<_> = it
            .pieces
            .iter()
            .map(|(at, bytes, _, pointers)| (*at, bytes.clone(), pointers.clone()))
            .collect();
        (it.bytes.clone(), pieces, it.value_pointers.clone(), it.statics.clone())
    };
    assert_eq!(
        without_align(&before),
        without_align(&after),
        "the values differ in alignment only"
    );
    assert_ne!(before, after, "one piece's alignment differs");
    assert_eq!(
        step(&bytes, "f"),
        step(&case("&[0u8; 8] as *const [u8; 8] as *const u8"), "f"),
        "built twice"
    );
    assert_ne!(step(&bytes, "f"), step(&word, "f"), "a piece's alignment is in `f`'s code id");
}

/// A program whose trait method returns `impl Trait` (RPITIT) and whose one impl gives
/// `hidden` behind it, called through the trait from a generic function.
fn rpitit(hidden: &str) -> Db {
    program(
        "/src/lib.rs",
        &format!(
            "pub trait Gen {{ fn get(&mut self) -> u32; }}
pub struct Small(u32);
impl Gen for Small {{ fn get(&mut self) -> u32 {{ self.0 }} }}
pub struct Twice(u32);
impl Gen for Twice {{ fn get(&mut self) -> u32 {{ self.0 * 2 }} }}
pub struct Wide(u64, u64);
impl Gen for Wide {{ fn get(&mut self) -> u32 {{ self.1 as u32 }} }}
pub trait G {{ fn roots(&self) -> impl Gen; }}
pub struct F(u32);
impl G for F {{ fn roots(&self) -> impl Gen {{ {hidden} }} }}
fn walk<T: G>(g: &T) -> u32 {{ let mut it = g.roots(); it.get() }}
pub fn run(x: u32) -> u32 {{ walk(&F(x)) }}
"
        ),
    )
}

/// Every identity of `walk::<F>` in a program of [`rpitit`]'s.
fn walk_of_f(db: &Db) -> (CompileIdentity, CompileIdentity, CodeId) {
    blaze_query::db::attach(db, || {
        let instance = Instance {
            subject: Subject::Body(Body::Fn(function(db, "walk"))),
            args: GenericArgs::new_from_slice(&[struct_ty(db, "F").into()]).store(),
        };
        let compile = |compiled| match instance_identity(db, &instance, &env(db), compiled) {
            Ok(it) => *it,
            Err(error) => panic!("`walk::<F>` has no identity: {error}"),
        };
        let step = code_id(db, &instance, &env(db))
            .unwrap_or_else(|error| panic!("`walk::<F>` has no CodeId: {error}"));
        (compile(DEBUG), compile(INLINE), step)
    })
}

/// Rule 21: a trait method's `impl Trait` is the impl's once the implementing type is known,
/// so the code identity of an instance that reaches it through the trait (`walk::<F>`) covers
/// the impl's hidden type: another layout (`Wide`) or the same layout with other code
/// (`Twice`) changes every identity of it, and of `run`'s step, and the same program keeps
/// them.
#[test]
fn an_impls_hidden_type_for_a_trait_methods_impl_trait_is_in_its_callers_identity() {
    let small = rpitit("Small(self.0)");
    let (walk, run) = (walk_of_f(&small), step(&small, "run"));
    let again = rpitit("Small(self.0)");
    assert_eq!((walk_of_f(&again), step(&again, "run")), (walk, run), "one program, one identity");
    for other in ["Wide(self.0 as u64, 1)", "Twice(self.0)"] {
        let db = rpitit(other);
        let (debug, inline, code) = walk_of_f(&db);
        assert_ne!(debug, walk.0, "{other}: debug compile identity of `walk::<F>`");
        assert_ne!(inline, walk.1, "{other}: inlining compile identity of `walk::<F>`");
        assert_ne!(code, walk.2, "{other}: behaviour identity of `walk::<F>`");
        assert_ne!(step(&db, "run"), run, "{other}: behaviour identity of `run`");
    }
}

/// A trait method's `impl Trait` inside a tuple, an enum or a reference is the impl's once
/// the implementing type is known, and that one's hidden type is revealed: `walk::<F>` has an
/// identity, and the impl's hidden type is in it.
#[test]
fn an_impls_impl_trait_nested_in_a_trait_methods_return_type_is_revealed() {
    let nested = |hidden: &str| {
        program(
            "/src/lib.rs",
            &format!(
                "pub enum Opt<T> {{ Some(T), None }}
pub trait Gen {{ fn get(&mut self) -> u32; }}
pub struct Small(u32);
impl Gen for Small {{ fn get(&mut self) -> u32 {{ self.0 }} }}
pub struct Wide(u64, u64);
impl Gen for Wide {{ fn get(&mut self) -> u32 {{ self.1 as u32 }} }}
pub trait G {{
    fn roots(&self) -> (u8, impl Gen);
    fn maybe(&self) -> Opt<impl Gen>;
    fn held(&self) -> &impl Sized;
}}
pub struct F(u32);
impl G for F {{
    fn roots(&self) -> (u8, impl Gen) {{ (1, {hidden}) }}
    fn maybe(&self) -> Opt<impl Gen> {{ Opt::Some({hidden}) }}
    fn held(&self) -> &impl Sized {{ &self.0 }}
}}
fn walk<T: G>(g: &T) -> u32 {{
    let (_, mut it) = g.roots();
    let _m = g.maybe();
    let _h = g.held();
    it.get()
}}
pub fn run(x: u32) -> u32 {{ walk(&F(x)) }}
"
            ),
        )
    };
    let small = walk_of_f(&nested("Small(self.0)"));
    let wide = walk_of_f(&nested("Wide(1, self.0 as u64)"));
    assert_ne!(small.0, wide.0, "debug identity covers the impl's hidden type");
    assert_ne!(small.2, wide.2, "CodeId covers the impl's hidden type");
}

/// An `impl Trait` in the bounds of another (`impl Src<Out = impl Gen>`, rustc's
/// `nested-rpitit.rs`): a generic caller reaches it only through the outer one's bounds, and
/// once the implementing type is known it is the type the impl's outer type gives it there
/// (`hir_ty::rpitit::impl_rpitit_tys`), so `walk::<F>` has an identity and it covers that type.
#[test]
fn an_impl_trait_in_the_bounds_of_another_is_revealed_through_them() {
    let nested = |ty: &str, value: &str| {
        program(
            "/src/lib.rs",
            &format!(
                "pub trait Gen {{ fn get(&mut self) -> u32; }}
pub struct Small(u32);
impl Gen for Small {{ fn get(&mut self) -> u32 {{ self.0 }} }}
pub struct Wide(u64, u64);
impl Gen for Wide {{ fn get(&mut self) -> u32 {{ self.1 as u32 }} }}
pub trait Src {{ type Out; fn take(self) -> Self::Out; }}
pub struct One<T>(T);
impl<T> Src for One<T> {{ type Out = T; fn take(self) -> T {{ self.0 }} }}
pub trait G {{ fn items(&self) -> impl Src<Out = impl Gen>; }}
pub struct F(u32);
impl G for F {{ fn items(&self) -> One<{ty}> {{ One({value}) }} }}
fn walk<T: G>(g: &T) -> u32 {{ let mut x = g.items().take(); x.get() }}
pub fn run(x: u32) -> u32 {{ walk(&F(x)) }}
"
            ),
        )
    };
    let small = walk_of_f(&nested("Small", "Small(self.0)"));
    let wide = walk_of_f(&nested("Wide", "Wide(1, self.0 as u64)"));
    assert_ne!(small.0, wide.0, "debug identity covers the nested `impl Trait`'s type");
    assert_ne!(small.2, wide.2, "CodeId covers the nested `impl Trait`'s type");
}
