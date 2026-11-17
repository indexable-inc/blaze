//! LIR's unwinding in LLVM IR text (port27/tempo-run/UNWIND.md 3.8, piece P8a): a child of
//! [`crate::lower`] (declared there with `#[path]`), so it writes through the same `Emit`.
//!
//! lower_eh: invoke and cleanup reach LLVM's object.
//!
//! - An `invoke` is `invoke .. to label %b<normal> unwind label %b<landing>` (the call's text is
//!   [`crate::lower`]'s, this module writes the two edges and checks them).
//! - A landing block's head is `%v<e>.lp = landingpad { ptr, i32 } <clause>` and its one
//!   parameter, the exception, is `%v<e> = extractvalue { ptr, i32 } %v<e>.lp, 0`. The clause is
//!   the block's kind: `cleanup` ([`BlockKind::Cleanup`], drops then `resume`), `catch ptr null`
//!   ([`BlockKind::Catch`], the `try` intrinsic's catch-all: the personality's search phase
//!   finds a handler), `filter [0 x ptr] zeroinitializer` ([`BlockKind::Terminate`]: an empty
//!   filter, so the search phase stops the unwind at this frame). These are the three actions
//!   blaze-code unwind.rs's `Action` has, and the forms crate::read_lsda reads back.
//! - `resume` rebuilds the pair with selector 0 and is `resume { ptr, i32 }` (the selector is
//!   not read by `_Unwind_Resume`, which takes the exception alone).
//! - A body with a landing block names `personality ptr @rust_eh_personality`, a DECLARATION:
//!   the symbol is bound at placement (UNWIND.md 3.3), and crate::read checks that an object's
//!   frames name exactly it ([`crate::read::PERSONALITY`], the one home of the name). Its type is
//!   the pinned library's (LIB std/src/sys/personality/gcc.rs:337: `rust_eh_personality(version:
//!   c_int, actions: _Unwind_Action, exception_class: u64, exception_object, context) ->
//!   _Unwind_Reason_Code`).
//! - `FnAttrs::NOUNWIND` prints `nounwind` and every function is `uwtable "frame-pointer"="all"`:
//!   both are [`crate::lower`]'s header already (`FnAttrs::NAMED`, D100 point 4).
//!
//! A landing block reached by anything but an unwind edge (a jump, a branch, a switch, an
//! invoke's return, the function's own entry) is refused by name, [`LowerError::Landing`]: the
//! LIR verifier refuses it first, and LLVM's own verifier would refuse the text with a message
//! that names no LIR block.

use core::fmt::Write as _;

use blaze_lir::Ty;
use blaze_lir::func::{Block, BlockKind, Value};
use blaze_own::{AString, String};

use super::{Emit, LowerError};
use crate::read::PERSONALITY;

/// What a landing pad yields and `resume` takes: the exception and its selector.
const PAIR: &str = "{ ptr, i32 }";

impl<'f, 'n, 'c, 'o, 'a> Emit<'f, 'n, 'c, 'o, 'a> {
    /// The refusal of `block`, a landing block, reached by `by`.
    pub(super) fn landing(&self, block: Block, by: &'static str) -> LowerError {
        LowerError::Landing { body: self.body, block: block.0, by }
    }

    /// Whether a block of the body is a landing block (then the body names the personality).
    fn lands(&self) -> bool {
        let view = self.view;
        (0..view.blocks()).any(|b| view.block_kind(Block(b as u32)).is_landing())
    }

    /// ` personality ptr @rust_eh_personality` after the header's attributes, and the
    /// personality's declaration, when the body has a landing block; nothing otherwise.
    pub(super) fn personality(&mut self) -> Result<(), LowerError> {
        if !self.lands() {
            return Ok(());
        }
        write!(self.out, " personality ptr @{PERSONALITY}")?;
        let mut line: AString<'a> = String::new_in(self.cx.alloc);
        write!(line, "declare i32 @{PERSONALITY}(i32, i32, i64, ptr, ptr)")?;
        self.decls.push(line);
        Ok(())
    }

    /// The head of landing block `block` of kind `kind`: the landing pad, then its one
    /// parameter (the exception) taken from it. Written right after the block's label: LLVM
    /// wants the landing pad first, and a landing block has no phi (its parameter comes from
    /// the pad, not from an edge's arguments).
    pub(super) fn landing_head(&mut self, block: Block, kind: BlockKind) -> Result<(), LowerError> {
        let view = self.view;
        if block.0 == 0 {
            return Err(self.landing(block, "the function's entry"));
        }
        let clause = match kind {
            BlockKind::Cleanup => "cleanup",
            BlockKind::Catch => "catch ptr null",
            BlockKind::Terminate => "filter [0 x ptr] zeroinitializer",
            BlockKind::Normal | BlockKind::Cold => return Err(self.refuse(None, "a landing head on a block that is not a landing block")),
        };
        let params = view.params(block);
        if params.len() != 1 || view.ty(Value(params.start as u32)) != Ty::PTR {
            return Err(self.refuse(None, "a landing block whose parameters are not the one exception pointer"));
        }
        let e = params.start;
        writeln!(self.out, "  %v{e}.lp = landingpad {PAIR} {clause}")?;
        writeln!(self.out, "  %v{e} = extractvalue {PAIR} %v{e}.lp, 0")?;
        Ok(())
    }

    /// An invoke's two edges, after its arguments' `)`: ` to label %b<normal> unwind label
    /// %b<unwind>`. The return edge reaches a normal block without parameters (the invoke's
    /// value is defined at its head: blaze-lir verify.rs), the unwind edge a landing block.
    pub(super) fn invoke_edges(&mut self, inst: usize, normal: Block, unwind: Block) -> Result<(), LowerError> {
        let view = self.view;
        if view.block_kind(normal).is_landing() {
            return Err(self.landing(normal, "an invoke's return edge"));
        }
        if normal.0 == 0 || unwind.0 == 0 {
            return Err(self.refuse(Some(inst), "a branch to the entry block"));
        }
        if !view.params(normal).is_empty() {
            return Err(self.refuse(Some(inst), "an invoke returning to a block with parameters"));
        }
        if !view.block_kind(unwind).is_landing() {
            return Err(self.refuse(Some(inst), "an invoke unwinding to a block that is not a landing block"));
        }
        write!(self.out, " to label %b{} unwind label %b{}", normal.0, unwind.0)?;
        Ok(())
    }

    /// `resume e`: the pair rebuilt with selector 0, then `resume { ptr, i32 }`.
    pub(super) fn resume(&mut self, inst: usize) -> Result<(), LowerError> {
        let view = self.view;
        let exception = view.operands(inst).value();
        if view.ty(exception) != Ty::PTR {
            return Err(self.refuse(Some(inst), "a resume of a value that is not the exception pointer"));
        }
        write!(self.out, "  %v{inst}.r0 = insertvalue {PAIR} poison, ptr ")?;
        self.val(exception)?;
        self.out.push_str(", 0\n");
        writeln!(self.out, "  %v{inst}.r1 = insertvalue {PAIR} %v{inst}.r0, i32 0, 1")?;
        writeln!(self.out, "  resume {PAIR} %v{inst}.r1")?;
        Ok(())
    }
}
