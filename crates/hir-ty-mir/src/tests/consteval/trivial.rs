//! The trivial-const fast path (`consteval::trivial_value`, rustc's `trivial_const`) gives, for
//! every body it takes, the value building and running the body's MIR gives, byte for byte, and
//! takes no body it must not: one with an operation, a path, a cast, a block or a type error.

use hir_def::{
    ModuleDefId,
    signatures::{ConstSignature, StaticSignature},
};
use hir_expand::fixture;

use crate::{
    InferBodyId,
    consteval::{evaluate_by_mir, trivial_value},
    db::HirDatabase,
    next_solver::GenericArgs,
    test_db::TestDBExt as _,
};

/// Every item named `T_*` is trivial, every one named `N_*` is not.
const BODIES: &str = r#"
const T_U8: u8 = 7;
const T_U8_SUFFIX: u8 = 0x7f_u8;
const T_BYTE: u8 = b'a';
#[allow(overflowing_literals)]
const T_U8_TRUNCATED: u8 = 300;
const T_I8_MIN: i8 = -128;
const T_I32: i32 = 123_456;
const T_I32_NEG: i32 = -1;
const T_U64: u64 = 0xdead_beef_cafe;
const T_U128_MAX: u128 = 340282366920938463463374607431768211455;
const T_I128_MIN: i128 = -170141183460469231731687303715884105728;
const T_USIZE: usize = 4096;
const T_ISIZE: isize = -3;
const T_F32: f32 = 1.5;
const T_F32_INF: f32 = 1e40;
const T_F64: f64 = 2.5e-3;
const T_TRUE: bool = true;
const T_FALSE: bool = false;
const T_CHAR: char = 'a';
const T_CHAR_WIDE: char = '\u{1F600}';
static T_STATIC: u64 = 0xffff_0000;
static mut T_STATIC_MUT: i16 = -300;

const N_SUM: u8 = 1 + 1;
const N_NEG_FLOAT: f64 = -1.5;
const N_DOUBLE_NEG: i32 = --5;
const N_CAST: u16 = 1u8 as u16;
const N_BLOCK: u8 = { 1 };
const N_PATH: u8 = T_U8;
const N_BOOL_FROM_INT: bool = 1;
const N_MISMATCH: u32 = 5u8;
const N_FLOAT_FROM_INT: f32 = 1;
const N_STR: &str = "a";
"#;

#[test]
fn the_fast_path_gives_what_mir_gives() {
    let fx = fixture::db(BODIES);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let module = db.module_for_file(file_id.file_id(&db));
        let edition = file_id.edition(&db);
        let (mut trivial, mut not) = (0, 0);
        for def in module.def_map(&db).module(&db, module).scope(&db).declarations() {
            let (name, owner): (String, InferBodyId<'_>) = match def {
                ModuleDefId::ConstId(it) => {
                    let Some(name) = ConstSignature::of(&db, it).name.as_ref() else { continue };
                    (name.display(&db, edition).to_string(), it.into())
                }
                ModuleDefId::StaticId(it) => {
                    let name = StaticSignature::of(&db, it).name.display(&db, edition).to_string();
                    (name, it.into())
                }
                _ => continue,
            };
            let fast = trivial_value(&db, owner);
            if name.starts_with("N_") {
                assert_eq!(fast, None, "{name} is not trivial");
                not += 1;
                continue;
            }
            assert!(name.starts_with("T_"), "{name} says neither T_ nor N_");
            let Some(fast) = fast else { panic!("{name} is trivial") };
            let slow = evaluate_by_mir(&db, owner, GenericArgs::empty(), None)
                .unwrap_or_else(|e| panic!("{name}'s MIR evaluates: {e:?}"));
            assert_eq!(fast.ty, slow.ty, "{name}'s type");
            assert_eq!(*fast.memory, *slow.memory, "{name}'s bytes");
            assert_eq!(fast.memory_map, slow.memory_map, "{name}'s memory map");
            // Interned: one row, so every byte of the two is the same.
            assert_eq!(fast, slow, "{name}'s allocation");
            if let ModuleDefId::ConstId(it) = def {
                let queried = db.const_eval(it, GenericArgs::empty(), None).unwrap();
                assert_eq!(queried, fast, "{name}'s value through `const_eval`");
            }
            trivial += 1;
        }
        assert_eq!((trivial, not), (21, 10), "every item of the fixture was checked");
    });
}
