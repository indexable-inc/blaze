//! The HIR evaluator of constants of the closed shape against the MIR evaluator: every
//! constant and discriminant of the shape in a fixture, and in generated programs, must have
//! the identical allocation or the identical error by both
//! (`docs/v2/plan/lanes/frontend/fast-const.md`).

use std::fmt::Write as _;

use hir_def::{AdtId, ConstId, ModuleDefId, signatures::ConstSignature};
use hir_expand::EditionedFileId;
use hir_expand::fixture;

use crate::{
    InferBodyId,
    consteval::hir_eval,
    db::HirDatabase,
    next_solver::GenericArgs,
    test_db::{TestDB, TestDBExt as _},
};

/// Compares the two paths on every constant and explicit discriminant of the shape in the
/// crate of `file_id`, and gives how many there were and how many were outside the shape.
#[track_caller]
pub(super) fn check_all(db: &TestDB, file_id: EditionedFileId) -> (usize, usize) {
    let def_map = db.module_for_file(file_id.file_id(db)).def_map(db);
    let mut owners = Vec::new();
    for (_, module) in def_map.modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                ModuleDefId::ConstId(konst) => owners.push(InferBodyId::from(konst)),
                ModuleDefId::AdtId(AdtId::EnumId(enum_)) => owners.extend(
                    enum_.enum_variants(db).variants.values().map(|(it, _)| InferBodyId::from(*it)),
                ),
                _ => {}
            }
        }
    }
    let (mut of_shape, mut outside) = (0, 0);
    for owner in owners {
        let Some([by_hir, by_mir]) = hir_eval::both(db, owner, GenericArgs::empty(), None) else {
            outside += 1;
            continue;
        };
        of_shape += 1;
        assert_eq!(by_hir, by_mir, "the two evaluations of {owner:?} differ");
    }
    (of_shape, outside)
}

fn const_named(db: &TestDB, file_id: EditionedFileId, name: &str) -> ConstId {
    let module = db.module_for_file(file_id.file_id(db));
    let named = |konst| {
        ConstSignature::of(db, konst)
            .name
            .as_ref()
            .is_some_and(|it| it.display(db, file_id.edition(db)).to_string() == name)
    };
    module
        .def_map(db)
        .module(db, module)
        .scope(db)
        .declarations()
        .find_map(|def| match def {
            ModuleDefId::ConstId(konst) if named(konst) => Some(konst),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no constant {name}"))
}

#[track_caller]
fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, of_shape: usize) {
    let fx = fixture::db(ra_fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || assert_eq!(check_all(&db, file_id).0, of_shape, "{ra_fixture}"));
}

const LAYOUT_FNS: &str = r#"
#[rustc_intrinsic]
pub const fn size_of<T>() -> usize;
#[rustc_intrinsic]
pub const fn align_of<T>() -> usize;
struct S { a: u8, b: u32 }
"#;

#[test]
fn the_shape() {
    check(
        r#"
const A: u8 = 7;
const B: i8 = -128;
const C: i32 = -(A as i32) * 3 + (1 << 4);
const D: bool = A > 3 && C != 0 || false;
const E: char = 65u8 as char;
const F: u32 = 'é' as u32;
const G: u8 = !0;
const H: i64 = { C as i64 };
enum En { X = 1 << 3, Y = A as isize, Z }
"#,
        // `Z`'s discriminant is the one before it plus one: no body of its own.
        10,
    );
}

#[test]
fn the_checks() {
    check(
        r#"
const ADD: u8 = 255 + 1;
const SUB: u32 = 0 - 1;
const MUL: i16 = 300 * 300;
const DIV: i32 = 1 / 0;
const REM: u64 = 1 % 0;
const DIV_MIN: i8 = -128 / -1;
const REM_MIN: i64 = i64_min() % -1;
const SHL: u8 = 1 << 8;
const SHR: i128 = 1 >> 128;
const SHL_NEG: i32 = 1 << -1;
const NEG: i8 = -B;
const B: i8 = -128;
const READS_FAILING: u8 = ADD + 1;
const FAILING_FIRST: u8 = ADD + (1 / 0);
const SHORT: bool = false && (1 / 0 == 0);
enum E { X = 1 << 64 }
const fn i64_min() -> i64 { -9223372036854775808 }
"#,
        // `REM_MIN` calls a function: outside the shape.
        15,
    );
}

#[test]
fn layout_facts() {
    check(
        &format!(
            "{LAYOUT_FNS}
const A: usize = size_of::<S>();
const B: usize = align_of::<S>();
const C: usize = size_of::<[u16; 3]>() * 2;
const D: usize = size_of::<(u8, u64)>() + align_of::<u128>();
const E: usize = size_of::<()>();
"
        ),
        5,
    );
}

#[test]
fn outside_the_shape() {
    check(
        r#"
const fn f() -> u8 { 1 }
const A: u8 = f();
const B: f64 = 1.5;
const C: u8 = { let x = 1; x };
const D: u8 = if true { 1 } else { 2 };
struct P(u8);
const E: P = P(1);
const F: &str = "s";
const G: u8 = match 1 { _ => 2 };
"#,
        0,
    );
}

/// A seeded xorshift: the generated programs are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Int {
    name: &'static str,
    bits: u32,
    signed: bool,
}

const INTS: [Int; 12] = [
    Int { name: "i8", bits: 8, signed: true },
    Int { name: "i16", bits: 16, signed: true },
    Int { name: "i32", bits: 32, signed: true },
    Int { name: "i64", bits: 64, signed: true },
    Int { name: "i128", bits: 128, signed: true },
    Int { name: "isize", bits: 64, signed: true },
    Int { name: "u8", bits: 8, signed: false },
    Int { name: "u16", bits: 16, signed: false },
    Int { name: "u32", bits: 32, signed: false },
    Int { name: "u64", bits: 64, signed: false },
    Int { name: "u128", bits: 128, signed: false },
    Int { name: "usize", bits: 64, signed: false },
];

/// A type of a generated expression: an integer type, `bool` or `char`.
#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Int(Int),
    Bool,
    Char,
}

impl Ty {
    fn name(self) -> &'static str {
        match self {
            Ty::Int(int) => int.name,
            Ty::Bool => "bool",
            Ty::Char => "char",
        }
    }
}

struct Gen {
    rng: Rng,
    /// The constants so far and their types.
    consts: Vec<(String, Ty)>,
}

impl Gen {
    fn ty(&mut self) -> Ty {
        match self.rng.below(8) {
            0 => Ty::Bool,
            1 => Ty::Char,
            _ => Ty::Int(INTS[self.rng.below(INTS.len())]),
        }
    }

    /// A literal of `int`, an edge of its range half the time.
    fn int_literal(&mut self, int: Int, out: &mut String) {
        let max = if int.signed { (1u128 << (int.bits - 1)) - 1 } else { u128::MAX >> (128 - int.bits) };
        let v = match self.rng.below(8) {
            0 => 0,
            1 => 1,
            2 => max,
            3 => u128::from(int.bits),
            _ => (u128::from(self.rng.next()) << 64 | u128::from(self.rng.next())) & max,
        };
        let negative = int.signed && self.rng.below(3) == 0;
        if negative && v == max && int.bits == 128 {
            // `-170141183460469231731687303724105728i128` is no literal rustc lexes as one.
            write!(out, "(-{max}{} - 1)", int.name).unwrap();
        } else if negative {
            // The literal one above `MAX` is `MIN` negated: `-128i8`.
            write!(out, "-{}{}", v + u128::from(v == max), int.name).unwrap();
        } else {
            write!(out, "{v}{}", int.name).unwrap();
        }
    }

    fn expr(&mut self, ty: Ty, depth: u32, out: &mut String) {
        if depth == 0 || self.rng.below(4) == 0 {
            return self.leaf(ty, out);
        }
        out.push('(');
        match ty {
            Ty::Char => {
                self.expr(Ty::Int(INTS[6]), depth - 1, out);
                out.push_str(" as char");
            }
            Ty::Bool => match self.rng.below(4) {
                0 => {
                    out.push('!');
                    self.expr(ty, depth - 1, out);
                }
                1 => {
                    let of = self.ty();
                    let op = ["==", "!=", "<", "<=", ">", ">="][self.rng.below(6)];
                    self.expr(of, depth - 1, out);
                    write!(out, " {op} ").unwrap();
                    self.expr(of, depth - 1, out);
                }
                _ => {
                    let op = ["&&", "||", "&", "|", "^"][self.rng.below(5)];
                    self.expr(ty, depth - 1, out);
                    write!(out, " {op} ").unwrap();
                    self.expr(ty, depth - 1, out);
                }
            },
            Ty::Int(int) => match self.rng.below(5) {
                0 if int.signed => {
                    out.push('-');
                    self.expr(ty, depth - 1, out);
                }
                0 => {
                    out.push('!');
                    self.expr(ty, depth - 1, out);
                }
                1 => {
                    let from = self.ty();
                    let from = if from == Ty::Bool || from == Ty::Char || self.rng.below(2) == 0 {
                        from
                    } else {
                        Ty::Int(INTS[self.rng.below(INTS.len())])
                    };
                    self.expr(from, depth - 1, out);
                    write!(out, " as {}", int.name).unwrap();
                }
                _ => {
                    let op = ["+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^"][self.rng.below(10)];
                    let rhs = if op == "<<" || op == ">>" {
                        Ty::Int(INTS[self.rng.below(INTS.len())])
                    } else {
                        ty
                    };
                    self.expr(ty, depth - 1, out);
                    write!(out, " {op} ").unwrap();
                    self.expr(rhs, depth - 1, out);
                }
            },
        }
        out.push(')');
    }

    fn leaf(&mut self, ty: Ty, out: &mut String) {
        let earlier: Vec<_> = self.consts.iter().filter(|(_, it)| *it == ty).collect();
        if !earlier.is_empty() && self.rng.below(3) == 0 {
            out.push_str(&earlier[self.rng.below(earlier.len())].0);
            return;
        }
        match ty {
            Ty::Bool => out.push_str(["true", "false"][self.rng.below(2)]),
            Ty::Char => out.push_str(["'a'", "'\\0'", "'\\u{10ffff}'", "'é'"][self.rng.below(4)]),
            Ty::Int(int) if int.name == "usize" && self.rng.below(4) == 0 => {
                let of = ["S", "u8", "u128", "[u16; 3]", "(u8, u64)", "()"][self.rng.below(6)];
                let f = ["size_of", "align_of"][self.rng.below(2)];
                write!(out, "{f}::<{of}>()").unwrap();
            }
            Ty::Int(int) => self.int_literal(int, out),
        }
    }
}

#[test]
fn generated_programs_match() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut of_shape, mut failed) = (0, 0);
    for _ in 0..30 {
        let mut generator = Gen { rng: Rng(rng.next() | 1), consts: Vec::new() };
        let mut source = String::from(LAYOUT_FNS);
        for i in 0..40 {
            let ty = generator.ty();
            let name = format!("C{i}");
            write!(source, "const {name}: {} = ", ty.name()).unwrap();
            generator.expr(ty, 4, &mut source);
            source.push_str(";\n");
            generator.consts.push((name, ty));
        }
        let fx = fixture::db(&source);
        let file_id = fx.only_file();
        let db = fx.into_db();
        crate::attach_db(&db, || {
            let (in_shape, outside) = check_all(&db, file_id);
            assert_eq!(outside, 0, "every generated constant is of the shape:\n{source}");
            of_shape += in_shape;
            for (name, _) in &generator.consts {
                let konst = const_named(&db, file_id, name);
                failed += usize::from(db.const_eval(konst, GenericArgs::empty(), None).is_err());
            }
        });
    }
    assert_eq!(of_shape, 30 * 40);
    // Both outcomes are exercised: the programs are not all overflow, nor none.
    assert!(failed >= 100 && of_shape - failed >= 100, "{failed} of {of_shape} failed");
}

#[test]
fn a_memo_hit_allocates_nothing() {
    let fx = fixture::db("const A: u8 = 1; const B: u8 = A + 2;");
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let (_, control) = blaze_alloc_count::measure(|| std::hint::black_box(Box::new(1u64)));
        assert_eq!(control.allocations, 1, "the test binary counts allocations");
        let b = const_named(&db, file_id, "B");
        let first = db.const_eval(b, GenericArgs::empty(), None).unwrap();
        let hit = blaze_alloc_count::assert_at_most(0, "a memo hit", || {
            db.const_eval(b, GenericArgs::empty(), None).unwrap()
        });
        assert_eq!(first, hit);
    });
}
