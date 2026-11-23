//! A CAST WHOSE TARGET IS WRITTEN WITH `_` (`x as _`, `p as *const _`), walked over NAMES
//! RECORDS from `main`: each row is a `no_core` program whose walk must check every body clean
//! with the records' log empty, or (the error rows) be refused with the named diagnostic.
//!
//! The stop that started it (2026-11-17, lane RF9): the entry-programs test (tests/front2_main.rs
//! `the_entry_programs_as_far_as_each_gets`) refused core's `<impl ErrorKind>::from_prim` with 44
//! E0277 "can't compare `u32` with `ErrorKind`" (`u32: PartialEq<ErrorKind>`), one per variant.
//! Its body (library/core/src/io/error.rs) is a `macro_rules!` declared in the fn whose arms are
//! `v if v == ($Enum::$Variant as _) => Some($Enum::$Variant)`. The cast's `_` is a fresh type
//! variable (blaze-typeck machine.rs, `PNode::Infer` under `Lower::Body`), the cast expression
//! has it as its type (rules/cast.rs `expr_cast`) and the cast is judged after fallback
//! (rules/fallback.rs `end_sequence`, `check_casts`); there the trivial-cast coercion of
//! `ErrorKind` to a variable still open UNIFIED them, giving `_` the operand's type. rustc's
//! `CastCheck::check` structurally resolves both sides first: a side still a variable is E0282,
//! never the other side's type (rules/cast.rs `check_casts` now does the same).
//!
//! The Reference, "Type cast expressions" ([expr.as]) and "Inferred type": `_` "asks the
//! compiler to infer the type if possible based on the surrounding information available";
//! the comparison `u32 == ?T` is decided by its operator's trait goal `u32: PartialEq<?T>`,
//! whose one impl gives `?T = u32` (rules/op.rs `binop`, state 2: the goal is registered and
//! the right side resolved by selection before the builtin rule is asked).
//!
//! Rule 23, row by row (the change: rules/cast.rs `check_casts`, the two
//! `structurally_resolve` lines):
//! - `never_constrained_is_e0282` fails without them: `let w = 7u8 as _;` is then ACCEPTED, its
//!   `_` unified with `u8` by the trivial-cast coercion.
//! - `match_guard_shape`, `macro_in_fn_shape`, `macro_in_fn_with_the_const_line`,
//!   `plain_comparison` and `shape_over_core_macro_impls` give `_` its type through the
//!   operator's goal; WITHOUT the change they still pass when selection settles that goal before
//!   the casts are judged (it does in these programs, which have one impl), and with core's
//!   world they were the failure. They are the guard that the change does not break the shape
//!   and the probe of the selection: if one fails here, the goal `u32: PartialEq<?T>` was not
//!   settled by the solver (with the change the finding is then E0282 at the cast, not E0277).
//! - `target_from_the_let`, `target_from_the_parameter`, `partial_hole_in_a_pointer` and
//!   `resolved_by_a_later_statement` are the deferral's rows: each fails if the cast were judged
//!   when it is walked (its target is still a variable then).
//! - `invalid_cast_found_after_inference` is E0606 on today's tree too: the target is `bool`
//!   once the later `let` is checked, and an enum is no cast to `bool` (rustc names it E0054,
//!   "cannot cast as bool"; this tree files it under E0606, "casting is invalid").
//!
//! The test prints one line per row:
//! ```text
//! cast infer match_guard_shape: clean (<counts>); main returns 2 when run
//! cast infer never_constrained_is_e0282: refused as wanted (E0282)
//! ```
//!
//! The fixture, reader and walk are tests/front2_block_use_types.rs's (one test file cannot use
//! another's helpers), with the allocator a frame made at the top of the test, and the macro
//! expansions of the solve kept as the loader serves them (tests/const_fn_pointers.rs `Kept`),
//! in that frame. The walk makes every body's code records; running `main` for its status is
//! not done here (see the rows' `want`): the run (`run::run_with`) takes a `SharedAlloc`, which
//! no frame gives.

#![feature(allocator_api)]

use core::fmt::Write as _;

use blaze_emit::{Isa, Os};
use blaze_id::CrateItemKey;
use blaze_id::ItemKey as LibKey;
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_mem::Frame;
use blaze_own::{AString, AVec, Alloc, Erased};
use blaze_scope::expansion_origins::OriginsError;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The lang items the walk needs (as tests/front2_block_use_types.rs writes them), `Copy` for
/// the scalars the rows read through a reference or a pointer, `PartialEq` as core declares it
/// (cmp.rs: the `eq` lang item, `Rhs` defaulted to `Self`), and the fieldless enum the rows cast.
const HEAD: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for u16 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i16 {}
impl Copy for i32 {}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

#[lang = \"eq\"]
trait PartialEq<Rhs = Self> {
    fn eq(&self, other: &Rhs) -> bool;
}

/// core's `ErrorKind` in small: fieldless, so `K::B as u32` is 1.
enum K {
    A,
    B,
    C,
}

";

/// `u32`'s `PartialEq`, written out: the one impl the goal `u32: PartialEq<?T>` can take.
const IMPLS_WRITTEN: &str = "
impl PartialEq for u32 {
    fn eq(&self, other: &u32) -> bool {
        *self == *other
    }
}

";

/// The scalars' `PartialEq` as core makes them (cmp.rs `partial_eq_impl!`): one impl per type,
/// each self type a `ty` fragment of a macro call.
const IMPLS_BY_MACRO: &str = "
macro_rules! partial_eq_impl {
    ($($t:ty)*) => ($(
        impl PartialEq for $t {
            #[inline]
            fn eq(&self, other: &$t) -> bool {
                (*self) == (*other)
            }
        }
    )*)
}

partial_eq_impl! { bool u8 u16 u32 u64 i16 i32 }

";

const MAIN: &str = "

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    f()
}
";

/// What a row's walk must give.
#[derive(Clone, Copy)]
enum Want {
    /// Every body checked clean; `main` returns this status when the program is run.
    Clean(u8),
    /// Refused, the refusal's text naming this diagnostic code.
    Refused(&'static str),
}

/// core's own declarations, for the rows that ask whether a form of core's breaks the goal
/// (lane RF11, 2026-11-17): `PartialEq` as cmp.rs writes it (`const trait`, `Rhs: PointeeSized =
/// Self`, the supertrait `PointeeSized`), and the fieldless enum.
const HEAD_CORE_FORM: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs, const_trait_impl)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for u16 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i16 {}
impl Copy for i32 {}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

#[lang = \"eq\"]
const trait PartialEq<Rhs: PointeeSized = Self>: PointeeSized {
    fn eq(&self, other: &Rhs) -> bool;
}

enum K {
    A,
    B,
    C,
}

";

/// `u32`'s impl written out, and core's blanket impl over fn pointers (ptr/mod.rs:2622) with its
/// lang trait (ops/function.rs:330): the goal `u32: PartialEq<?T>` has two impl candidates, and
/// only `u32: FnPtr` having no solution (builtin.rs `assemble_fn_ptr_trait`) leaves one.
const IMPLS_WITH_FN_PTR: &str = "
#[lang = \"fn_ptr_trait\"]
trait FnPtr: Copy {}

impl<F: FnPtr> PartialEq for F {
    fn eq(&self, other: &Self) -> bool {
        true
    }
}

impl PartialEq for u32 {
    fn eq(&self, other: &u32) -> bool {
        *self == *other
    }
}

";

/// The impls as core writes them (cmp.rs:2155 `impl_partial_eq_for_primitive!`: `const impl`,
/// each self type a `ty` fragment, `other: &Self`), beside the blanket fn-pointer impl.
const IMPLS_CORE_FORM: &str = "
#[lang = \"fn_ptr_trait\"]
trait FnPtr: Copy {}

impl<F: FnPtr> PartialEq for F {
    fn eq(&self, other: &Self) -> bool {
        true
    }
}

mod impls {
    use crate::PartialEq;

    macro_rules! impl_partial_eq_for_primitive {
        ($($t:ty)*) => ($(
            const impl PartialEq for $t {
                #[inline]
                fn eq(&self, other: &Self) -> bool { *self == *other }
            }
        )*)
    }

    impl_partial_eq_for_primitive! { bool u8 u16 u32 u64 i16 i32 }
}

";

/// core's `from_prim` in its home: a `const fn` of an inherent impl over 44 variants (as many as
/// `ErrorKind`'s list), so 44 goals `u32: PartialEq<?T>` are open in one body.
const ITEMS_44: &str = "enum E {
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15, V16, V17, V18, V19, V20, V21,
    V22, V23, V24, V25, V26, V27, V28, V29, V30, V31, V32, V33, V34, V35, V36, V37, V38, V39, V40, V41,
    V42, V43,
}

impl E {
    const fn from_prim(ek: u32) -> u8 {
        macro_rules! from_prim {
            ($prim:expr => $Enum:ident { $($Variant:ident),* $(,)? }) => {{
                // Force a compile error if the list gets out of date.
                const _: fn(e: $Enum) = |e: $Enum| match e {
                    $($Enum::$Variant => (),)*
                };
                match $prim {
                    $(v if v == ($Enum::$Variant as _) => ($Enum::$Variant as u8) + 1,)*
                    _ => 0,
                }
            }}
        }
        from_prim!(ek => E {
            V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15, V16, V17, V18, V19, V20,
            V21, V22, V23, V24, V25, V26, V27, V28, V29, V30, V31, V32, V33, V34, V35, V36, V37, V38, V39,
            V40, V41, V42, V43,
        })
    }
}

fn f() -> i32 {
    E::from_prim(9) as i32
}";

/// One program: its head ([`HEAD`] or [`HEAD_CORE_FORM`]), its `PartialEq` impls, its items (a
/// fn `f`), and `main` returning `f()`.
struct Row {
    name: &'static str,
    head: &'static str,
    impls: &'static str,
    items: &'static str,
    want: Want,
}

const ROWS: &[Row] = &[
    // (a) The shape: a match guard comparing the scrutinee with a variant cast to `_`.
    Row {
        name: "match_guard_shape",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn from_prim(ek: u32) -> u8 {
    match ek {
        v if v == (K::A as _) => 1,
        v if v == (K::B as _) => 2,
        _ => 0,
    }
}

fn f() -> i32 {
    from_prim(1) as i32
}",
        want: Want::Clean(2),
    },
    // (b) The real form: a `macro_rules!` declared in the fn writes the arms with
    // `$Enum::$Variant as _`.
    Row {
        name: "macro_in_fn_shape",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn from_prim(ek: u32) -> u8 {
    macro_rules! from_prim {
        ($prim:expr => $Enum:ident { $($Variant:ident),* $(,)? }) => {{
            match $prim {
                $(v if v == ($Enum::$Variant as _) => ($Enum::$Variant as u8) + 1,)*
                _ => 0,
            }
        }}
    }
    from_prim!(ek => K { A, B, C })
}

fn f() -> i32 {
    from_prim(1) as i32
}",
        want: Want::Clean(2),
    },
    // (b) with core's guard line too: an anonymous constant holding a closure coerced to a fn
    // pointer, whose `match` must name every variant.
    Row {
        name: "macro_in_fn_with_the_const_line",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn from_prim(ek: u32) -> u8 {
    macro_rules! from_prim {
        ($prim:expr => $Enum:ident { $($Variant:ident),* $(,)? }) => {{
            // Force a compile error if the list gets out of date.
            const _: fn(e: $Enum) = |e: $Enum| match e { $($Enum::$Variant => ()),* };
            match $prim {
                $(v if v == ($Enum::$Variant as _) => ($Enum::$Variant as u8) + 1,)*
                _ => 0,
            }
        }}
    }
    from_prim!(ek => K { A, B, C, })
}

fn f() -> i32 {
    from_prim(1) as i32
}",
        want: Want::Clean(2),
    },
    // (a) over impls made as core makes them (a `ty` fragment as each self type).
    Row {
        name: "shape_over_core_macro_impls",
        head: HEAD,
        impls: IMPLS_BY_MACRO,
        items: "fn from_prim(ek: u32) -> u8 {
    match ek {
        v if v == (K::A as _) => 1,
        v if v == (K::B as _) => 2,
        _ => 0,
    }
}

fn f() -> i32 {
    from_prim(1) as i32
}",
        want: Want::Clean(2),
    },
    // (c) The target from the `let`'s written type.
    Row {
        name: "target_from_the_let",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn f() -> i32 {
    let x: u64 = 3u8 as _;
    x as i32
}",
        want: Want::Clean(3),
    },
    // (d) The target from the parameter the cast is passed to.
    Row {
        name: "target_from_the_parameter",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn take(x: i16) -> i16 {
    x
}

fn f() -> i32 {
    take(200u32 as _) as i32
}",
        want: Want::Clean(200),
    },
    // (e) A hole inside a known head: `*const _`, filled by the cast's coercion.
    Row {
        name: "partial_hole_in_a_pointer",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn f() -> i32 {
    let p = &5u8 as *const _;
    let v: u8 = unsafe { *p };
    v as i32
}",
        want: Want::Clean(5),
    },
    // (f) Resolved by a LATER statement: why the cast is judged at the end of the body.
    Row {
        name: "resolved_by_a_later_statement",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn f() -> i32 {
    let y = 7u8 as _;
    let z: u16 = y;
    z as i32
}",
        want: Want::Clean(7),
    },
    // (g) Never constrained: type annotations needed, not `u8`.
    Row {
        name: "never_constrained_is_e0282",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn f() -> i32 {
    let w = 7u8 as _;
    0
}",
        want: Want::Refused("E0282"),
    },
    // (h) Invalid only once inference has run: an enum is no cast to `bool`.
    Row {
        name: "invalid_cast_found_after_inference",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn f() -> i32 {
    let s = K::A as _;
    let t: bool = s;
    0
}",
        want: Want::Refused("E0606"),
    },
    // (i) The comparison alone, in a plain fn.
    Row {
        name: "plain_comparison",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: "fn f() -> i32 {
    if 1u32 == (2u8 as _) { 1 } else { 3 }
}",
        want: Want::Clean(3),
    },
    // THE FORMS OF CORE'S WORLD (lane RF11): each adds one thing core's `from_prim` has and
    // the rows above do not, so the row that fails names the form that leaves the goal
    // `u32: PartialEq<?T>` unsettled (E0282 at the cast).
    // (j) core's blanket impl over fn pointers beside `u32`'s: two impl candidates, one held.
    Row {
        name: "fn_ptr_blanket_beside_u32_impl",
        head: HEAD,
        impls: IMPLS_WITH_FN_PTR,
        items: "fn from_prim(ek: u32) -> u8 {
    match ek {
        v if v == (K::A as _) => 1,
        v if v == (K::B as _) => 2,
        _ => 0,
    }
}

fn f() -> i32 {
    from_prim(1) as i32
}",
        want: Want::Clean(2),
    },
    // (k) The blanket impl is the one a fn pointer's comparison takes.
    Row {
        name: "fn_pointers_compare_through_the_blanket_impl",
        head: HEAD,
        impls: IMPLS_WITH_FN_PTR,
        items: "fn g() {}

fn f() -> i32 {
    let p: fn() = g;
    if p == p { 1 } else { 0 }
}",
        want: Want::Clean(1),
    },
    // (l) core's declarations: `const trait PartialEq<Rhs: PointeeSized = Self>: PointeeSized`
    // and `const impl PartialEq for $t` by macro in a module, with the blanket impl.
    Row {
        name: "core_form_trait_and_impls",
        head: HEAD_CORE_FORM,
        impls: IMPLS_CORE_FORM,
        items: "fn from_prim(ek: u32) -> u8 {
    match ek {
        v if v == (K::A as _) => 1,
        v if v == (K::B as _) => 2,
        _ => 0,
    }
}

fn f() -> i32 {
    from_prim(1) as i32
}",
        want: Want::Clean(2),
    },
    // (m) core's body in its home: a `const fn` of an inherent impl, 44 variants.
    Row {
        name: "forty_four_variants_in_an_inherent_const_fn",
        head: HEAD,
        impls: IMPLS_WRITTEN,
        items: ITEMS_44,
        want: Want::Clean(10),
    },
    // (n) All of (j) to (m) at once: core's shape as far as a fixture writes it.
    Row {
        name: "core_s_whole_shape",
        head: HEAD_CORE_FORM,
        impls: IMPLS_CORE_FORM,
        items: ITEMS_44,
        want: Want::Clean(10),
    },
];

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"cast infer").get::<CodeBuilder>()
}

/// One macro call's expansion as the solve made it: the call, its text, its origin column.
type Made = (LibKey, &'static str, &'static [u8]);

/// The one crate's records, its one file's text and its expansions, in the test's frame.
struct Kept {
    names: &'static [u8],
    items: &'static [u8],
    path: &'static str,
    text: &'static str,
    expansions: &'static [Made],
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.names)
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        (krate == 0 && path.as_str() == self.path).then_some(self.text)
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        if krate != 0 {
            return None;
        }
        let row = self.expansions.iter().find(|m| m.0 == call)?;
        let bytes = row.2;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none).
        let origins = if bytes.is_empty() {
            (&[][..], &[][..])
        } else {
            blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
        };
        Some(LoadedExpansion { text: row.1, origins: Some(origins) })
    }
}

/// One program's names records: the solve's output, the solve itself gone.
struct Fixture {
    kept: &'static Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

/// `text` solved once, its one crate's two records and its expansions written from the solve
/// into `frame`.
fn fixture(frame: &'static Frame, a: Alloc<'static>, text: &str) -> Fixture {
    // `UserCrate` takes its files as std strings.
    let files = [(String::from(ROOT), String::from(text))];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let [krate] = crates.as_slice() else { panic!("{} crates (a no_core program is one)", crates.len()) };
    let keys: &'static [CrateItemKey] = frame.alloc_slice_copy(&[krate.key]);
    let name: &'static str = frame.alloc_str(krate.name);
    let crate_names: &'static [&'static str] = frame.alloc_slice_copy(&[name]);
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let [c] = names.crates() else { panic!("{} solved crates (one wanted)", names.crates().len()) };
    let names_bytes = aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record: {e:?}")), blaze_own::testing::bump());
    let items_bytes = aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record: {e:?}")), blaze_own::testing::bump());
    let [file] = c.files.as_slice() else { panic!("{} files (one wanted)", c.files.len()) };
    // Each call's made text and origins (tests/const_fn_pointers.rs `made_origins`: a text the
    // solve rewrote or could not place keeps no origins).
    let mut made: AVec<'static, Made> = AVec::new_in(a);
    for (call, e) in c.expansions.iter() {
        let Some(m) = e.made else { continue };
        let origins: &'static [u8] = match &m.origins {
            Ok(bytes) => frame.alloc_slice_copy(bytes),
            Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
            Err(err) => panic!("the origins of expansion {call:x}: {err:?}"),
        };
        made.push((*call, frame.alloc_str(m.text), origins));
    }
    let expansions: &'static [Made] = frame.alloc_slice_copy(made.as_slice());
    let kept: &'static Kept = frame.alloc(Kept {
        names: names_bytes,
        items: items_bytes,
        path: frame.alloc_str(file.package.as_str()),
        text: frame.alloc_str(file.text),
        expansions,
    });
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it, in `frame`.
fn reader(frame: &'static Frame, a: Alloc<'static>, f: &Fixture) -> NamesRecords<'static> {
    let log: &'static Log<'static> = frame.alloc(Log::new(f.crate_names, a));
    let records: &'static Records<'static> = frame.alloc(Records::new(f.keys, f.kept, log, frame, a));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// A stand-in for the command's BodyCheck keys that answers for every item (as
/// tests/local_statics.rs `EveryItem`).
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("cast infer every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// The checker key of crate 0's one free fn named `name`.
fn fn_named(records: &'static Records<'static>, name: &str) -> ItemKey {
    let mut found = None;
    let mut count = 0usize;
    assert!(
        records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
            found = Some(key);
            count += 1;
        }),
        "the program's items record lists its items"
    );
    assert_eq!(count, 1, "fns named `{name}` (one wanted)");
    checker_of(found.expect("one fn of the name"))
}

/// Each row's program walked from `main` over a fresh reader and a fresh `Maker`: a clean row
/// checks every body clean with nothing filed in the records' log; an error row is refused,
/// the refusal naming its diagnostic code.
#[test]
fn casts_to_inferred_targets_check_over_records() {
    // The test's one allocator, made at its top: a frame, its memory from the OS. The record
    // walk takes `Alloc<'static>` (run/make_one.rs `walk_records`), so the frame is kept for
    // the process (as tests/front2_block_use_types.rs keeps it).
    let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let b = builder();
    let mut failed = 0usize;
    for row in ROWS {
        let mut text = AString::new_in(a);
        text.push_str(row.head);
        text.push_str(row.impls);
        text.push_str(row.items);
        text.push_str(MAIN);
        let f = fixture(frame, a, text.as_str());
        let main = fn_named(reader(frame, a, &f).records, "main");
        let names = reader(frame, a, &f);
        let mut store = MemoryStore::new_in(a);
        let mut maker = Maker::new(names, Some(main), &b, a);
        let walked = walk_records(main, &EveryItem, &mut store, &b, &producer(), &mut maker, a);
        let errors = names.records.errors();
        match (walked, row.want) {
            (Ok((_, counts)), Want::Clean(status)) if errors.is_empty() => {
                println!("cast infer {}: clean ({counts}); main returns {status} when run", row.name);
            }
            (Ok((_, counts)), Want::Clean(_)) => {
                failed += 1;
                println!("cast infer {}: FAILED: the walk ({counts}) left {} errors in the records' log", row.name, errors.len());
                for e in errors.iter() {
                    println!("  logged: {e}");
                }
            }
            (Ok((_, counts)), Want::Refused(code)) => {
                failed += 1;
                println!("cast infer {}: FAILED: the walk ({counts}) was not refused (wanted {code})", row.name);
            }
            (Err(e), Want::Refused(code)) => {
                let mut said = AString::new_in(a);
                write!(said, "{e}").expect("a refusal's text writes into the frame");
                if said.as_str().contains(code) {
                    println!("cast infer {}: refused as wanted ({code})", row.name);
                } else {
                    failed += 1;
                    println!("cast infer {}: FAILED: refused, but not with {code}: {}", row.name, said.as_str());
                }
            }
            (Err(e), Want::Clean(_)) => {
                failed += 1;
                println!("cast infer {}: FAILED: the record walk: {e}", row.name);
                for logged in errors.iter() {
                    println!("  logged: {logged}");
                }
            }
        }
    }
    assert_eq!(failed, 0, "rows whose walk was not as wanted (each printed above with its refusal)");
}
