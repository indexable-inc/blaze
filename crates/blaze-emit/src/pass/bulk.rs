//! `memcpy`, `memmove` and `memset` (lib.rs, "Bulk memory"): inline for a constant length up
//! to the bound, else a call of the system routine; and the copies of `byval` arguments.

use alloc::vec::Vec;
use alloc::{alloc::Allocator, alloc::Global};

use blaze_lir::{Op, Value};

use super::{EmitError, Pass, mask, unsupported};
use crate::{
    MOVE_INLINE_MOST, Routine,
    enc::{self, Cond, Reg, Size, X0, X1, X2, X9, X10, X11, X13},
};

/// The registers a `memmove` holds its pieces in: none that `mem` takes for an address.
const MOVE_REGS: [Reg; 13] = [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 17];

/// The pieces of `len` bytes: 8-byte ones, then a 4, 2 and 1-byte tail as needed, each
/// `(offset, size)`. Every offset is a multiple of its size.
fn pieces(len: u64) -> impl Iterator<Item = (u64, Size)> {
    let whole = len / 8;
    let tail = len % 8;
    let at = whole * 8;
    (0..whole).map(|k| (8 * k, Size::X)).chain(
        [(4u64, Size::W), (2, Size::H), (1, Size::B)].into_iter().filter(move |&(bytes, _)| tail & bytes != 0).map(
            move |(bytes, size)| {
                // The tail pieces in decreasing size: each starts where the larger ones end.
                let before = tail & !(2 * bytes - 1);
                (at + before, size)
            },
        ),
    )
}

impl<A: Allocator> Pass<'_, '_, A> {
    pub(super) fn bulk(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        let (dst, src, len) = (o.value(), o.value(), o.value());
        self.width_of(inst, op, len)?;
        let known = self.constant_of(len);
        let routine = match op {
            Op::Memcpy => Routine::Memcpy,
            Op::Memmove => Routine::Memmove,
            _ => Routine::Memset,
        };
        let most = match op {
            Op::Memmove => self.inline_most.min(MOVE_INLINE_MOST),
            _ => self.inline_most,
        };
        match known {
            Some(n) if n <= most => {
                self.ld(X9, dst);
                self.ld(X10, src);
                match op {
                    Op::Memcpy => self.copy_inline(X9, X10, n),
                    Op::Memmove => {
                        if n > 8 * MOVE_REGS.len() as u64 {
                            return Err(unsupported(inst, op, "an inline memmove past its registers"));
                        }
                        let mut regs = MOVE_REGS.iter();
                        let mut held = [(0u64, Size::B, 0 as Reg); MOVE_REGS.len()];
                        let mut count = 0;
                        for (off, size) in pieces(n) {
                            let Some(&reg) = regs.next() else {
                                return Err(unsupported(inst, op, "an inline memmove past its registers"));
                            };
                            self.mem(true, size, reg, X10, off as i32);
                            held[count] = (off, size, reg);
                            count += 1;
                        }
                        for &(off, size, reg) in &held[..count] {
                            self.mem(false, size, reg, X9, off as i32);
                        }
                    }
                    _ => {
                        // The byte in every byte of a register.
                        self.mov_imm(X11, 0x0101_0101_0101_0101);
                        self.put(enc::mul(X13, X10, X11));
                        for (off, size) in pieces(n) {
                            self.mem(false, size, X13, X9, off as i32);
                        }
                    }
                }
            }
            _ => {
                self.ld(X0, dst);
                self.ld(X1, src);
                self.ld(X2, len);
                self.call_routine(routine);
            }
        }
        Ok(())
    }

    /// Copies `len` bytes from `[src]` to `[dst]` (not overlapping): inline up to the bound,
    /// else by `memcpy`, which may take every caller-saved register.
    pub(super) fn copy(&mut self, dst: Reg, src: Reg, len: u64) {
        match len <= self.inline_most {
            true => self.copy_inline(dst, src, len),
            false => {
                self.put(enc::mov(X0, dst));
                self.put(enc::mov(X1, src));
                self.mov_imm(X2, len);
                self.call_routine(Routine::Memcpy);
            }
        }
    }

    fn copy_inline(&mut self, dst: Reg, src: Reg, len: u64) {
        for (off, size) in pieces(len) {
            self.mem(true, size, X13, src, off as i32);
            self.mem(false, size, X13, dst, off as i32);
        }
    }

    /// `memcmp a, b, len` (an `i32` below, equal to or above 0: C's `memcmp`, the first
    /// differing byte pair compared as unsigned, 0 when `len` is 0). As the copies: inline for a
    /// constant length up to the bound, else a call of the system's `memcmp`.
    ///
    /// Inline, piece by piece ([`pieces`]: 8-byte words, then a 4, 2 and 1-byte tail), each
    /// pair loaded into `x0`, `x1` (zero-extended) and compared; the first unequal pair branches
    /// to the end, where both are byte-reversed (`rev`: little-endian memory, so the lowest
    /// address becomes the most significant byte, and an unsigned compare of the reversed words
    /// is the byte order) and compared once more: `hi` gives 1, `lo` -1. With every pair equal
    /// the loop falls into the same end with the last pair equal, which gives 0. No load is
    /// made of a byte past `len`, and none needs an alignment (`ldr`/`ldur` of any address).
    pub(super) fn memcmp(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        let (a, b, len) = (o.value(), o.value(), o.value());
        self.width_of(inst, op, len)?;
        match self.constant_of(len) {
            Some(0) => self.st(enc::ZR, inst),
            Some(n) if n <= self.inline_most => {
                self.ld(X9, a);
                self.ld(X10, b);
                let mut to_end: Vec<u32, Global> = Vec::new_in(Global);
                for (off, size) in pieces(n) {
                    // (`mem` takes `FAR`, `x16`, for an offset past its immediates, never `x0`, `x1`.)
                    self.mem(true, size, X0, X9, off as i32);
                    self.mem(true, size, X1, X10, off as i32);
                    self.put(enc::cmp(X0, X1));
                    to_end.push(self.here());
                    self.put(enc::b_cond(Cond::Ne, 0));
                }
                for at in to_end {
                    self.land(at)?;
                }
                self.put(enc::rev(X0, X0));
                self.put(enc::rev(X1, X1));
                self.put(enc::cmp(X0, X1));
                self.put(enc::cset(X0, Cond::Hi));
                self.put(enc::cset(X1, Cond::Lo));
                self.put(enc::sub(X0, X0, X1));
                self.narrow(X0, 32);
                self.st(X0, inst);
            }
            _ => {
                self.ld(X0, a);
                self.ld(X1, b);
                self.ld(X2, len);
                self.call_routine(Routine::Memcmp);
                // A C `int` in `w0`: the bits above it are unspecified.
                self.narrow(X0, 32);
                self.st(X0, inst);
            }
        }
        Ok(())
    }

    /// The value of `value` when an `iconst` defines it: the cell invariant's bits.
    pub(super) fn constant_of(&self, value: Value) -> Option<u64> {
        let at = value.index();
        if self.view.op(at) != Op::Iconst {
            return None;
        }
        let width = self.width(self.view.ty(value))?;
        let word = self.view.operands(at).imm();
        Some(match width {
            ..=32 => u64::from(word) & mask(width),
            _ => i64::from(word as i32) as u64,
        })
    }
}

#[cfg(test)]
#[test]
fn pieces_cover_every_byte_once() {
    for len in 0..=80u64 {
        let mut seen = vec![0u8; len as usize];
        for (off, size) in pieces(len) {
            let bytes = 1u64 << size as u32;
            assert_eq!(off % bytes, 0, "{len}: a piece of {bytes} at {off}");
            for b in off..off + bytes {
                seen[b as usize] += 1;
            }
        }
        assert!(seen.iter().all(|&it| it == 1), "{len}: {seen:?}");
    }
}
