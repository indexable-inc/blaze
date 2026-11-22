//! A finding names its node (rule 19; src/diag/node.rs, lane RF13).
//!
//! core's `from_prim!(ek => ErrorKind { .. })` (library/core/src/io/error.rs) was refused with
//! one E0282 whose range was the whole macro call: every node the expansion wrote has the
//! call's range, so the range named none of them. A finding now keeps the node it was made at
//! (`Diag::node`), and `node_form`/`sketch` name it: `a cast`, `{integer} as _`.
//!
//! ```text
//! fn h() { let x = 1u8 as _; }
//! ```
//!
//! The cast's target `_` is a variable nothing sets: rustc's `CastCheck::check` resolves both
//! sides structurally first (rules/cast.rs), so the target is E0282 at the cast.

use core::fmt;

use super::check_body;
use crate::body::{Expr, IntSuffix};
use crate::diag::{DiagCode, Node, node_form, range_shared, sketch};
use crate::fixture::{Bb, Fx};
use crate::ids::{LangItem, UintTy};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::portable::PNode;

/// A sketch written into a fixed buffer (no heap): at most 128 bytes, past which writing fails.
struct Buf {
    bytes: [u8; 128],
    len: usize,
}

impl fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        let dst = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("<not utf-8>")
    }
}

/// The E0282 of an unset cast target is at the cast, and its `node` names the cast: kind `a
/// cast`, sketch `{integer} as _`. Without `Diag::node` (export setting `None`) this fails with
/// "the E0282 keeps no node".
///
/// Expected line: `test suite::diag_node::an_e0282_names_its_node ... ok`.
#[test]
fn an_e0282_names_its_node() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Default::default(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unit = fx.unit();
    let h = fx.func("h", 0, &[], unit, Default::default());
    fx.finish();

    let mut b = Bb::new();
    let operand = b.int(Some(IntSuffix::Uint(UintTy::U8)));
    let hole = b.ty(PNode::Infer);
    let cast = b.e(Expr::Cast { operand, ty: hole });
    let x = b.bind("x");
    let s = b.let_(x, None, cast);
    let root = b.block(&[s], None);
    let checked = check_body(&fx, &b, h, root, Plant::None, InferPlant::None);

    let d = checked.diags.iter().find(|d| d.code == DiagCode::AnnotationsNeeded);
    let Some(d) = d else { panic!("want an E0282, got {:?}", checked.diags) };
    let Some(node) = d.node else { panic!("the E0282 keeps no node: {d:?}") };
    assert_eq!(node, Node::Expr(cast), "the E0282 is at the cast: {d:?}");
    let view = b.view(h, root);
    assert_eq!(node_form(&view, node), Some("a cast"));
    // The fixture gives every node its own range: the range alone names it.
    assert_eq!(range_shared(&view, node), 1);
    let mut out = Buf { bytes: [0; 128], len: 0 };
    sketch(&view, node, &|_| None, &mut out).expect("the sketch fits");
    assert_eq!(out.text(), "{integer} as _");
}
