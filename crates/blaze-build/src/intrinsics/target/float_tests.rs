//! The generic float intrinsics' names (target/float.rs) as the one set `target::named` asks.
//! The names are the pinned library's: stdarch `aarch64/neon/generated.rs`
//! `link_name = "llvm.roundeven.v4f32"` (`vrndnq_f32`), `"llvm.roundeven.f32"` (`vrndns_f32`),
//! `"llvm.fma.v2f64"` (`vfmaq_f64`), `"llvm.sqrt.v2f64"` (`vsqrtq_f64`).
//!
//! `every_float_name_round_trips` prints "90 float names, each its own: llvm.roundeven.v4f32
//! is Nearest at V4F32"; `a_barrier_stays_a_barrier` prints "isb stays a barrier;
//! llvm.roundeven.f16 refused: `llvm.roundeven.f16` is not a target intrinsic blaze knows".

use std::alloc::Global;

use blaze_lir::func::BlockKind;
use blaze_lir::{Builder, Full, Op, Ty};

use super::float::{FNS, FloatCall, FloatFn, SHAPES, Shape};
use super::{Target, build, named};
use crate::BuildError;

/// Every function at every shape is found by `target::named` by its own link name and no
/// other, as a `Target::Float`. Without `named` asking target/float.rs first, the first name
/// is refused ("`llvm.roundeven.f32` is not a target intrinsic blaze knows") and this fails.
#[test]
fn every_float_name_round_trips() {
    let mut seen = 0;
    for func in FNS {
        for shape in SHAPES {
            let call = FloatCall { func, shape };
            let name = call.link_name();
            assert!(name.starts_with("llvm.") && name.len() > "llvm.".len(), "{name}");
            assert_eq!(named(name), Ok(Target::Float(call)), "{name}");
            assert_eq!(Target::Float(call).link_name(), name);
            seen += 1;
        }
    }
    assert_eq!(seen, 90);
    let v4 = named("llvm.roundeven.v4f32").expect("roundeven at v4f32 is in the set");
    let Target::Float(call) = v4 else { panic!("{v4:?} is not a float function") };
    assert_eq!((call.func, call.shape), (FloatFn::Roundeven, Shape::V4F32));
    assert_eq!(call.func.op(), Op::Nearest);
    assert_eq!((call.shape.lanes(), call.shape.stride(), call.shape.elem()), (Some(4), 4, Ty::F32));
    // `nearbyint` and `rint` are ties-to-even in Rust's default float environment.
    assert_eq!(FloatFn::Nearbyint.op(), Op::Nearest);
    assert_eq!(FloatFn::Rint.op(), Op::Nearest);
    assert_eq!((FloatFn::Fma.op(), FloatFn::Fma.arity()), (Op::Fma, 3));
    assert_eq!((FloatFn::Copysign.op(), FloatFn::Copysign.arity()), (Op::Fcopysign, 2));
    println!("{seen} float names, each its own: {} is {:?} at {:?}", call.link_name(), call.func.op(), call.shape);
}

/// The barriers keep their names and their build; an `f16` shape and an unknown function stay
/// refused by name; a float function handed to the barrier build is refused by name, never
/// declared as `fn(i32)`.
#[test]
fn a_barrier_stays_a_barrier() {
    assert_eq!(named("llvm.aarch64.isb"), Ok(Target::Aarch64Isb));
    let f16 = named("llvm.roundeven.f16").expect_err("no f16 lane is built");
    assert_eq!(f16.to_string(), "`llvm.roundeven.f16` is not a target intrinsic blaze knows");
    assert!(named("llvm.roundeven.v4f33").is_err());
    assert!(named("llvm.cos.f64").is_err(), "`cos` is no LIR op: refused, never guessed");

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(blaze_lir::func::Conv::C, blaze_lir::builder::Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let imm = b.iconst(Ty::I32, 15);
    let call = FloatCall { func: FloatFn::Sqrt, shape: Shape::F64 };
    let error = build(&mut b, Target::Float(call), imm).expect_err("a float function is no barrier");
    assert!(
        matches!(error, BuildError::Target { target: Target::Float(_), expr: None, why: "a generic float function built as a barrier (a build bug)" }),
        "{error}"
    );
    build(&mut b, Target::Aarch64Isb, imm).expect("isb #15 builds");
    println!("isb stays a barrier; llvm.roundeven.f16 refused: {f16}");
}
