//! The call frame rules a function's code gives its unwinder, as DWARF has them, and the
//! compact encoding macOS's unwinder takes instead for a frame of the usual shape
//! ([`compact_aarch64`]; docs/v2/plan/design/backend-next.md, (c)).

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
    /// as Cranelift's `Aarch64SetPointerAuth` row writes it. Only where the state changes:
    /// a frame starts unsigned, so a function that signs nothing has no such row.
    RaSignState(bool),
}

/// Apple's compact unwind encodings for arm64 (`<mach-o/compact_unwind_encoding.h>`).
pub mod compact {
    /// A frame-pointer frame: `x29` is the CFA less 16, `x29`/`x30` saved there, and the
    /// pairs below them as [`PAIRS`] lays them out.
    pub const FRAME: u32 = 0x0400_0000;
    /// No frame record: the return address stays in `x30`, the CFA is `sp` plus the stack
    /// size, in 16-byte units at bits 12..24 ([`STACK_SIZE`]).
    pub const FRAMELESS: u32 = 0x0200_0000;
    /// The function's DWARF FDE describes it: what every function the encoder refuses keeps.
    pub const DWARF: u32 = 0x0300_0000;
    pub const STACK_SIZE: u32 = 0x00FF_F000;
    /// The callee-saved pairs, in the order a frame saves them below its frame record: each
    /// pair's DWARF registers (`x19`..`x28` are 19..28, `d8`..`d15` are 72..79) and its bit.
    pub const PAIRS: [(u16, u16, u32); 9] = [
        (19, 20, 0x001),
        (21, 22, 0x002),
        (23, 24, 0x004),
        (25, 26, 0x008),
        (27, 28, 0x010),
        (72, 73, 0x100),
        (74, 75, 0x200),
        (76, 77, 0x400),
        (78, 79, 0x800),
    ];
}

/// The DWARF registers a state tracks: `x0`..`x30`, `sp`, then up to `v31` (95).
const REGS: usize = 96;
/// How deep `RememberState` nests before the encoder refuses.
const REMEMBERED: usize = 4;

/// The unwind state at a point of a function: the CFA rule and where each register is saved,
/// as an offset from the CFA.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct State {
    cfa: (u16, i32),
    saved: [Option<i32>; REGS],
}

impl State {
    /// AArch64 at entry: the CFA is `sp`, nothing saved.
    const ENTRY: State = State { cfa: (31, 0), saved: [None; REGS] };

    fn saves(&self) -> impl Iterator<Item = (usize, i32)> + '_ {
        self.saved.iter().enumerate().filter_map(|(reg, at)| at.map(|at| (reg, at)))
    }

    /// Every save of `self` is one of `body`'s, at the same offset.
    fn within(&self, body: &State) -> bool {
        self.saves().all(|(reg, at)| body.saved[reg] == Some(at))
    }
}

/// Replays `rows` from entry, handing `each` the state after every code offset's rows.
/// `None` for a row the compact encodings cannot hold (a signed return address, `ArgsSize`,
/// `Undefined`, a register past [`REGS`], a `RestoreState` with nothing remembered, nesting
/// past [`REMEMBERED`]) or when `each` says no.
fn replay(rows: &[(u32, FrameRule)], mut each: impl FnMut(&State) -> bool) -> Option<()> {
    let mut state = State::ENTRY;
    let mut remembered = [State::ENTRY; REMEMBERED];
    let mut depth = 0;
    let mut at = 0;
    while at < rows.len() {
        let offset = rows[at].0;
        while at < rows.len() && rows[at].0 == offset {
            match rows[at].1 {
                FrameRule::Cfa(reg, off) => state.cfa = (reg, off),
                FrameRule::CfaRegister(reg) => state.cfa.0 = reg,
                FrameRule::CfaOffset(off) => state.cfa.1 = off,
                FrameRule::Offset(reg, off) => *state.saved.get_mut(usize::from(reg))? = Some(off),
                FrameRule::Restore(reg) | FrameRule::SameValue(reg) => {
                    *state.saved.get_mut(usize::from(reg))? = None;
                }
                FrameRule::RememberState => {
                    *remembered.get_mut(depth)? = state;
                    depth += 1;
                }
                FrameRule::RestoreState => {
                    depth = depth.checked_sub(1)?;
                    state = remembered[depth];
                }
                FrameRule::RaSignState(false) => {}
                FrameRule::RaSignState(true) | FrameRule::ArgsSize(_) | FrameRule::Undefined(_) => {
                    return None;
                }
            }
            at += 1;
        }
        if !each(&state) {
            return None;
        }
    }
    Some(())
}

/// The compact encoding of an AArch64 function whose frame rules are `rows` (each at its code
/// offset, in order, from the CIE's entry state), or `None` when no compact encoding describes
/// it exactly, so it keeps its DWARF FDE ([`compact::DWARF`]).
///
/// Accepted: a frame-pointer frame whose body (the state on `x29 + 16` with the most saves)
/// has `x30` at -8, `x29` at -16 and whole pairs of [`compact::PAIRS`] below them in that order
/// from -24 down, as clang lays them out (checked against `clang -O2` on macOS arm64: four x
/// pairs give `0x0400000f`, two d pairs `0x04000300`), every other state on `x29` holding a
/// subset of the body's saves and every state off it on `sp`; or, with no state on `x29`, a
/// function that saves nothing and whose CFA is `sp` plus 0 or one size. Refused besides what
/// [`replay`] refuses: a CFA on another register or another offset of `x29`, a save outside the
/// pairs or at another offset (Cranelift's own layout, `x19` above `x20`, is one), a lone half
/// of a pair.
///
/// Like Apple's compilers' own encodings it is the body's: a walk from inside the prologue or
/// epilogue sees the body's frame. Unwinding from a call, which panics do, is exact.
/// Allocates nothing.
pub fn compact_aarch64(rows: &[(u32, FrameRule)]) -> Option<u32> {
    let mut body: Option<State> = None;
    let mut sp_saves = false;
    // The one nonzero stack size states on `sp` have, and whether there are two.
    let mut size: Option<i32> = None;
    let mut sizes = false;
    replay(rows, |state| match state.cfa {
        (29, 16) => {
            if body.is_none_or(|it| state.saves().count() > it.saves().count()) {
                body = Some(*state);
            }
            true
        }
        (31, off) => {
            sp_saves |= state.saves().next().is_some();
            match (off, size) {
                (0, _) => {}
                (off, None) => size = Some(off),
                (off, Some(it)) => sizes |= off != it,
            }
            true
        }
        _ => false,
    })?;
    let Some(body) = body else {
        let size = size.unwrap_or(0);
        if sp_saves || sizes || size < 0 || size % 16 != 0 || size / 16 > 0xFFF {
            return None;
        }
        return Some(compact::FRAMELESS | ((size / 16) as u32) << 12);
    };
    replay(rows, |state| state.cfa.0 != 29 || state.within(&body))?;
    if body.saved[30] != Some(-8) || body.saved[29] != Some(-16) {
        return None;
    }
    let (mut bits, mut next, mut covered) = (0, -24, 2);
    for (first, second, bit) in compact::PAIRS {
        match (body.saved[usize::from(first)], body.saved[usize::from(second)]) {
            (None, None) => {}
            (Some(a), Some(b)) if a == next && b == next - 8 => {
                bits |= bit;
                next -= 16;
                covered += 2;
            }
            _ => return None,
        }
    }
    // A save outside the frame record and the pairs.
    if body.saves().count() != covered {
        return None;
    }
    Some(compact::FRAME | bits)
}

#[cfg(test)]
mod tests;
