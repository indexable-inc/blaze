//! `MirBody::held_across_calls_in`: what a call that may suspend keeps alive, by the analyses
//! `held_locals_in` runs across a yield.

use hir_ty::{
    db::HirDatabase,
    mir::TerminatorKind,
    next_solver::TyKind,
};
use hir_expand::fixture;
use rustc_type_ir::inherent::IntoKind;

use crate::function;

const WORK: &str = r#"
//- minicore: sized, copy
pub fn park() {}
pub fn work(a: u8) -> (u8, u16) {
    let kept: u16 = 1;
    let dead: u32 = 2;
    let r: u64 = 3;
    let p = &r;
    park();
    (a, kept)
}
"#;

/// Held across `park()`: the parameter and `kept`, used after it, and `r`, borrowed before it
/// while its storage lasts. Not held: `dead` and `p`, never used after, and the call's own
/// destination.
#[test]
fn a_call_holds_what_is_used_after_it_or_borrowed_before_it() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let body = db.mir_body(function(&db, "work").into()).expect("the body");
        let calls: Vec<_> = (body.basic_blocks.iter())
            .filter(|(_, block)| {
                matches!(&block.terminator, Some(it) if matches!(it.kind, TerminatorKind::Call { .. }))
            })
            .map(|(id, _)| id)
            .collect();
        let [call] = calls[..] else { panic!("{} calls", calls.len()) };
        let held: Vec<String> = blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let held = body.held_across_calls_in(frame, &[call, call]);
            assert_eq!(held.len(), 2);
            assert_eq!(held[0], held[1]);
            (held[0].iter())
                .map(|&local| match body.locals[local].ty.as_ref().kind() {
                    TyKind::Uint(it) => format!("{it:?}"),
                    other => format!("{other:?}"),
                })
                .collect()
        });
        assert_eq!(held, ["u8", "u16", "u64"]);
    });
}
