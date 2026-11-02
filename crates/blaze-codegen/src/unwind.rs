//! The call frame rules a function's code gives its unwinder, as DWARF has them.

use serde::{Deserialize, Serialize};

/// A DWARF call frame instruction. Registers are DWARF register numbers, offsets are bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameRule {
    Cfa(u16, i32),
    CfaRegister(u16),
    CfaOffset(i32),
    Offset(u16, i32),
    Restore(u16),
    SameValue(u16),
    Undefined(u16),
    RememberState,
    RestoreState,
    ArgsSize(u32),
    /// AArch64's return-address signing state from here (`true`: signed), DWARF's
    /// `RA_SIGN_STATE` pseudo-register 34 as `ValExpression(34, [DW_OP_lit0 or DW_OP_lit1])`,
    /// as Cranelift's `Aarch64SetPointerAuth` row writes it.
    RaSignState(bool),
}
