//! `purity::operands` against the code generator's `operands_of` it replaced: the same
//! operands, in order, of every rvalue but `BinaryOp`, which the old copy skipped and which is
//! the only binary operation from runtime MIR on.

use hir_ty::{
    InferBodyId,
    db::HirDatabase,
    mir::{MirOwner, MirStage},
};
use hir_ty_mir::mir::{Operand, Rvalue, StatementKind, purity::operands};
use hir_expand::fixture;

use crate::function;

const BODIES: &str = r#"
//- minicore: sized, copy, add
pub struct P(pub u32, pub u32);
pub fn reads(a: u32, b: u32, c: bool) -> u32 {
    let sum = a + b;
    let again = a + sum;
    let pair = P(sum, again);
    let arr = [a; 3];
    let wide = b as u64;
    let _ = (c, arr, wide);
    pair.0 + pair.1
}
"#;

/// The code generator's copy.
fn operands_of(rvalue: &Rvalue) -> impl Iterator<Item = &Operand> {
    let none: &[Operand] = &[];
    let (fixed, rest) = match rvalue {
        Rvalue::UnaryOp(_, it)
        | Rvalue::Cast(_, it, _)
        | Rvalue::Repeat(it, _)
        | Rvalue::Use(it) => ([Some(it), None], none),
        Rvalue::CheckedBinaryOp(_, left, right) => ([Some(left), Some(right)], none),
        Rvalue::Aggregate(_, operands) => ([None, None], &operands[..]),
        _ => ([None, None], none),
    };
    fixed.into_iter().flatten().chain(rest)
}

fn same<'a>(a: impl Iterator<Item = &'a Operand>, b: impl Iterator<Item = &'a Operand>) -> bool {
    a.map(|it| it as *const Operand).eq(b.map(|it| it as *const Operand))
}

#[test]
fn operands_is_the_code_generators_copy_plus_binary_op() {
    let fx = fixture::db(BODIES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let reads: InferBodyId<'_> = function(&db, "reads").into();
        let mut binary = 0;
        for stage in [MirStage::Built, MirStage::Runtime] {
            let body = db.mir(MirOwner::Body(reads), stage).expect("the MIR of `reads`");
            for (_, block) in body.basic_blocks.iter() {
                for statement in &block.statements {
                    let StatementKind::Assign(_, rvalue) = &statement.kind else { continue };
                    match rvalue {
                        Rvalue::BinaryOp(_, a, b) => {
                            binary += 1;
                            assert!(same(operands(rvalue), [a, b].into_iter()), "{rvalue:?}");
                        }
                        _ => assert!(same(operands(rvalue), operands_of(rvalue)), "{rvalue:?}"),
                    }
                }
            }
        }
        assert!(binary > 0, "runtime MIR of `reads` has a `BinaryOp`");
    });
}
