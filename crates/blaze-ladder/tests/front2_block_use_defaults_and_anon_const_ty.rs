//! Block-local `use` / local-item generic default resolution and evaluated primitive type
//! preservation for inferred anonymous constants in front2.
//!
//! Covers:
//! 1. Block-local `use` of generic ADTs with default type parameters (`HashMap::<K, V>::new()`),
//!    chained block-local `use`, struct literals via block-local `use`, local variable non-shadowing,
//!    and block-local `struct`/`type` items with defaults (`AliasPaths::expand`).
//! 2. Block-local `const` items inside a `const` using block-local `use` + `{ ConstBool::VALUE }`
//!    const-generic arguments selecting between `impl<T> Gen<T, true>` and `impl<T> Gen<T, false>`
//!    (`BodyTypes::arg_param` + `body_anon` preserving the evaluated primitive type).
//! 3. Negative controls:
//!    - Too few required generic arguments through a block-local `use` -> E0107 (`GenericArgCount`).
//!    - Mismatched const-generic argument types (`{ 1usize }` for `const B: bool`, `{ true }` for
//!      `const N: usize`, and mismatched inferred anonymous constants on method calls) -> E0308
//!      (`Mismatch`).

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::locals::LocalKind;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const PRELUDE: &str = r#"#![feature(no_core, lang_items, decl_macro)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}
impl Copy for u8 {}
impl Copy for usize {}
impl Copy for bool {}

mod m {
    pub struct Map<K, V, S = (), A = ()> {
        pub k: K,
        pub v: V,
        pub s: S,
        pub a: A,
    }

    impl<K, V> Map<K, V, (), ()> {
        pub fn new(k: K, v: V) -> Self {
            Map { k, v, s: (), a: () }
        }
    }

    pub mod sub {
        pub use super::Map;
    }

    pub struct OnlyType {
        pub x: i32,
    }

    impl OnlyType {
        pub const ASSOC: i32 = 41;
    }

    pub struct Driver;

    pub struct HasNewTextSignature<T>(T);

    impl HasNewTextSignature<Driver> {
        pub const VALUE: bool = true;
    }

    pub struct U8Flag;
    impl U8Flag {
        pub const VALUE: u8 = 1u8;
    }

    pub struct PyClassDocGenerator<T, const B: bool>(T);

    impl<T> PyClassDocGenerator<T, true> {
        pub const DOC_PIECES: i32 = 11;
    }

    impl<T> PyClassDocGenerator<T, false> {
        pub const DOC_PIECES: i32 = 22;
    }

    pub struct SizedGen<T, const N: usize>(T);

    impl<T> SizedGen<T, 1> {
        pub const VALUE: i32 = 33;
    }

    pub struct Caller;

    impl Caller {
        pub fn take_bool<const B: bool>(&self) -> bool {
            B
        }
        pub fn take_usize<const N: usize>(&self) -> usize {
            N
        }
    }
}
"#;

const MOST_LENS: u32 = 16;

fn check_items(text: &str, items: &[&str]) -> Vec<(String, String)> {
    let program = format!("{PRELUDE}{text}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind)
        .unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in items {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let root_k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut queue = vec![root_k];
        let mut qi = 0;
        let mut outcome = "clean".to_string();
        while qi < queue.len() {
            let k = queue[qi];
            qi += 1;
            let mut lens = 0;
            let res = loop {
                match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                    Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. })
                    | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. })
                        if lens < MOST_LENS =>
                    {
                        lens += 1;
                        let mut records = Records {
                            names: &names,
                            keys: &mut front.keys,
                            index: &front.index,
                            store: &mut store,
                            buckets: &mut buckets,
                            order: BuildOrder::AsAsked,
                            consts: Some((&mut consts, prepared)),
                        };
                        if let Err(e) = eval_len(&mut records, c) {
                            break format!("not lowered: const {c:x}: {e}");
                        }
                    }
                    Err(e) => break format!("not lowered: {e}"),
                    Ok(body) => {
                        if let Some(b) = store.locals.body(checker_of(k)) {
                            for row in b.rows {
                                if matches!(row.kind, LocalKind::Fn | LocalKind::Const | LocalKind::Static)
                                    && !queue.contains(&row.key)
                                {
                                    queue.push(row.key);
                                }
                            }
                        }
                        let view = body.lowered.view();
                        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                        let scope = body.scope(&spell);
                        let mut records = Records {
                            names: &names,
                            keys: &mut front.keys,
                            index: &front.index,
                            store: &mut store,
                            buckets: &mut buckets,
                            order: BuildOrder::AsAsked,
                            consts: Some((&mut consts, prepared)),
                        };
                        break match check_body(&mut records, &body, &scope) {
                            Ok(done) if done.diags.is_empty() => "clean".to_string(),
                            Ok(done) => {
                                let d: Vec<String> = done.diags.iter().map(|d| format!("{:?}({})", d.code, d.code.code())).collect();
                                format!("diags {d:?}")
                            }
                            Err(CheckFail::Diags { diags, .. }) => {
                                let d: Vec<String> = diags.iter().map(|d| format!("{:?}({})", d.0, d.0.code())).collect();
                                format!("diags {d:?}")
                            }
                            Err(e) => format!("not checked: {e} ({e:?})"),
                        };
                    }
                }
            };
            if res != "clean" {
                outcome = res;
                break;
            }
        }
        got.push((n.to_string(), outcome));
    }
    got
}

#[test]
fn block_local_use_and_item_generic_defaults() {
    let text = r#"
fn pos_block_use_defaults() -> m::Map<i32, i32> {
    use m::Map;
    Map::<i32, i32>::new(1, 2)
}

fn pos_block_use_struct_lit_defaults() -> m::Map<i32, i32> {
    use m::Map;
    Map::<i32, i32> { k: 1, v: 2, s: (), a: () }
}

fn pos_chained_block_use_defaults() -> m::Map<i32, i32> {
    use m::sub as s;
    use s::Map;
    Map::<i32, i32>::new(1, 2)
}

fn pos_deep_20_hop_chained_block_use_defaults() -> m::Map<i32, i32> {
    use m::sub as s0;
    use s0 as s1;
    use s1 as s2;
    use s2 as s3;
    use s3 as s4;
    use s4 as s5;
    use s5 as s6;
    use s6 as s7;
    use s7 as s8;
    use s8 as s9;
    use s9 as s10;
    use s10 as s11;
    use s11 as s12;
    use s12 as s13;
    use s13 as s14;
    use s14 as s15;
    use s15 as s16;
    use s16 as s17;
    use s17 as s18;
    use s18 as s19;
    use s19 as s20;
    use s20::Map;
    Map::<i32, i32>::new(1, 2)
}

fn pos_chained_type_only_use_does_not_shadow_local_fn() -> i32 {
    use crate::m::OnlyType as A;
    use A as X;
    fn X() -> i32 {
        1
    }
    let a = X();
    let b = X::ASSOC;
    let _ = a;
    b
}

fn pos_local_var_does_not_shadow_type_path() -> m::Map<i32, i32> {
    use m::Map;
    let Map = 0i32;
    let _ = Map;
    Map::<i32, i32>::new(1, 2)
}

fn pos_local_struct_defaults() -> u8 {
    struct S<T, U = i32>(T, U);
    impl<T, U> S<T, U> {
        fn new(t: T, u: U) -> Self {
            S(t, u)
        }
    }
    let s = S::<u8>::new(7, 9);
    s.0
}

fn pos_local_type_alias_defaults() -> m::Map<u8, i32> {
    type LocalMap<K, V = i32> = m::Map<K, V>;
    LocalMap::<u8>::new(1, 2)
}

mod def_mod {
    pub mod m {
        pub struct S<T, U = i32>(pub T, pub U);
        impl<T, U> S<T, U> {
            pub fn new(t: T, u: U) -> Self {
                S(t, u)
            }
        }
    }
    pub macro emit_use($x:ident) {
        use m::S as $x;
    }
}

mod wrong {
    pub struct S;
}

fn pos_opaque_macro_use_ignores_caller_local_m() -> def_mod::m::S<u8, i32> {
    use crate::wrong as m;
    def_mod::emit_use!(X);
    X::<u8>::new(1, 2)
}

fn pos_opaque_macro_use_chained_ignores_caller_local_m() -> def_mod::m::S<u8, i32> {
    use crate::wrong as m;
    def_mod::emit_use!(A);
    use A as X;
    X::<u8>::new(1, 2)
}

fn neg_block_use_too_few_required_args() -> m::Map<i32, i32> {
    use m::Map;
    Map::<i32>::new(1, 2)
}

fn neg_cyclic_local_use_terminates() {
    use a as b;
    use b as a;
    a();
}
"#;
    let items = [
        "pos_block_use_defaults",
        "pos_block_use_struct_lit_defaults",
        "pos_chained_block_use_defaults",
        "pos_deep_20_hop_chained_block_use_defaults",
        "pos_chained_type_only_use_does_not_shadow_local_fn",
        "pos_local_var_does_not_shadow_type_path",
        "pos_local_struct_defaults",
        "pos_local_type_alias_defaults",
        "pos_opaque_macro_use_ignores_caller_local_m",
        "pos_opaque_macro_use_chained_ignores_caller_local_m",
        "neg_block_use_too_few_required_args",
        "neg_cyclic_local_use_terminates",
    ];
    let got = check_items(text, &items);
    for (name, outcome) in &got {
        if name.starts_with("pos_") {
            assert_eq!(outcome, "clean", "`{name}` must check clean, got: {outcome}");
        } else if name == "neg_cyclic_local_use_terminates" {
            assert!(
                outcome.contains("UnresolvedPath") && outcome.contains("E0425"),
                "`{name}` must terminate with E0425 UnresolvedPath, got: {outcome}"
            );
        } else {
            assert!(
                outcome.contains("GenericArgCount") && outcome.contains("E0107"),
                "`{name}` must fail with E0107 GenericArgCount, got: {outcome}"
            );
        }
    }
}

#[test]
fn block_local_const_and_inferred_anon_const_type_preservation() {
    let text = r#"
const POS_PYCLASS_DOC: i32 = {
    use crate::m as impl_;
    const DOC_PIECES: i32 = impl_::PyClassDocGenerator::<
        impl_::Driver,
        { impl_::HasNewTextSignature::<impl_::Driver>::VALUE },
    >::DOC_PIECES;
    DOC_PIECES
};

fn pos_inferred_method_const_bool() -> bool {
    use crate::m as impl_;
    let c = impl_::Caller;
    c.take_bool::<{ impl_::HasNewTextSignature::<impl_::Driver>::VALUE }>()
}

const NEG_LOCAL_CONST_USIZE_FOR_BOOL: i32 = {
    use crate::m as impl_;
    const BAD: i32 = impl_::PyClassDocGenerator::<impl_::Driver, { 1usize }>::DOC_PIECES;
    BAD
};

const NEG_LOCAL_CONST_BOOL_FOR_USIZE: i32 = {
    use crate::m as impl_;
    const BAD: i32 = impl_::SizedGen::<impl_::Driver, { true }>::VALUE;
    BAD
};

fn neg_inferred_method_const_u8_for_bool() -> bool {
    use crate::m as impl_;
    let c = impl_::Caller;
    c.take_bool::<{ impl_::U8Flag::VALUE }>()
}

fn neg_inferred_method_const_bool_for_usize() -> usize {
    use crate::m as impl_;
    let c = impl_::Caller;
    c.take_usize::<{ impl_::HasNewTextSignature::<impl_::Driver>::VALUE }>()
}

struct BoolS<const B: bool>;

fn pos_bare_local_const_matching_ty() {
    const FLAG: bool = true;
    let _: BoolS<FLAG>;
}

fn neg_bare_local_const_wrong_ty() {
    const N: u8 = 1;
    let _: BoolS<N>;
}

fn neg_bare_local_const_wrong_ty_expr_path() -> i32 {
    const N: u8 = 1;
    crate::m::PyClassDocGenerator::<crate::m::Driver, N>::DOC_PIECES
}
"#;
    let items = [
        "POS_PYCLASS_DOC",
        "pos_inferred_method_const_bool",
        "pos_bare_local_const_matching_ty",
        "NEG_LOCAL_CONST_USIZE_FOR_BOOL",
        "NEG_LOCAL_CONST_BOOL_FOR_USIZE",
        "neg_inferred_method_const_u8_for_bool",
        "neg_inferred_method_const_bool_for_usize",
        "neg_bare_local_const_wrong_ty",
        "neg_bare_local_const_wrong_ty_expr_path",
    ];
    let got = check_items(text, &items);
    for (name, outcome) in &got {
        if name.starts_with("POS_") || name.starts_with("pos_") {
            assert_eq!(outcome, "clean", "`{name}` must check clean, got: {outcome}");
        } else {
            assert!(
                outcome.contains("Mismatch") && outcome.contains("E0308"),
                "`{name}` must fail with E0308 Mismatch, got: {outcome}"
            );
        }
    }
}
