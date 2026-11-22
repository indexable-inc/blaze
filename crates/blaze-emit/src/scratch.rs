//! The composer's scratch registers as values (docs/v2/plan/design/end-state.md D193 point 6): a
//! scratch register is TAKEN from a set and GIVEN BACK to it, so two uses of one register cannot
//! overlap. The class this makes impossible was found in the Reference tier on 2026-11-21: an
//! overflow operation kept its flag in x12 while the far-frame store used x12 as its address
//! scratch (61 false overflow panics on a real project; pass.rs `FAR`).
//!
//! # The two kinds of use, and the one place both are checked
//!
//! - A HELD use: the composer keeps a value in a scratch register over several templates (a
//!   constant made in a general register before it is stored to a cell; an indirect callee). It
//!   asks [`Pool::take`] for a [`Scratch`], a token that is neither `Copy` nor `Clone`, and hands
//!   it back with [`Pool::give`]. While the token lives, no other `take` and no template
//!   temporary gets that register.
//! - A LENT use: a template's own temporaries (piece 0's `TEMP0`/`TEMP1` fields: a far cell's
//!   address, the stack probe's count) are lent from the FREE registers for that one template
//!   only ([`Pool::lend`]), so a temporary is never a register the composer holds.
//!
//! [`Pool::lend`] runs once per template, in the composer's `place`, the one function that copies
//! template bytes into the code: it lends the temporaries and checks the template's registers
//! against the pool. A scratch register given as an input must be held; one given as the result
//! must be held (a write into a register nobody holds would be a value nobody owns); a held
//! register in the template's clobbers (data in the table: piece 0's `Template::clobbers`) is
//! refused, unless the template is a call and the register an input it reads (`blr x16`: read
//! before the callee clobbers it), which is then SPENT: read again before it is given back and
//! taken again, it is refused by name. A token that is never given back leaves its
//! register held, so the composer's end-of-instruction check ([`Pool::is_idle`]) refuses it
//! (rule 3): a lost token is a refusal, never a silent overlap.
//!
//! Why a token and not a guard that borrows the pool: the pool lives in the composer, and every
//! template is placed through `&mut` the composer, so a guard borrowing the pool would forbid
//! placing any template while a scratch register is held, which is the one thing a held register
//! is for. The token is owned instead, and every way to use its register goes through the pool.

use blaze_tier::{Reg, RegClass, RegDesc, RegSet};

/// One scratch register held: taken by [`Pool::take`], given back by [`Pool::give`].
#[must_use = "a scratch register is given back to its pool (`Pool::give`) when its use ends"]
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Scratch {
    reg: Reg,
}

impl Scratch {
    /// The register, to name in a template's operands while the token is held.
    pub(crate) const fn reg(&self) -> Reg {
        self.reg
    }

    /// The register as enc.rs's encoders take it (the Reference tier's pass names registers
    /// so).
    #[allow(dead_code, reason = "T3 step 2: the Reference tier's helpers encode their held registers through this")]
    pub(crate) const fn enc(&self) -> crate::enc::Reg {
        self.reg.0 as crate::enc::Reg
    }
}

/// The Reference tier's general scratch registers: x9 to x17 (pass.rs: x9 to x15 its operands'
/// and results', x16 and x17 its call sites', veneers' and stack probe's).
#[allow(dead_code, reason = "T3 step 2: the Reference tier's pass makes its pool over this set")]
pub(crate) const REFERENCE_SCRATCH: RegSet = RegSet(((1u64 << 18) - 1) & !((1u64 << 9) - 1));

/// Why the pool refused. Each is a bug of the composer or of the description, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScratchError {
    /// The register description cannot give the composer a pool.
    Description(&'static str),
    /// No scratch register is free: the composer holds them all, or a template wants more
    /// temporaries than are free.
    Exhausted,
    /// A pool register named as a template's input or result while nobody holds it.
    NotHeld(Reg),
    /// A held register read after a template clobbered it.
    Spent(Reg),
    /// A template clobbers a held register it does not read.
    Clobbered(Reg),
    /// A token given back to a pool that does not hold its register.
    Foreign(Reg),
}

impl ScratchError {
    /// The static reason a refusal of the tier carries.
    pub(crate) const fn what(self) -> &'static str {
        match self {
            ScratchError::Description(what) => what,
            ScratchError::Exhausted => {
                "no scratch register is free for a template's temporary or a held use: the composer holds them all"
            }
            ScratchError::NotHeld(_) => "a scratch register named in a template that no taken token holds",
            ScratchError::Spent(_) => "a scratch register read after a template clobbered it",
            ScratchError::Clobbered(_) => "a template that clobbers a scratch register the composer holds",
            ScratchError::Foreign(_) => "a scratch token given back to a pool that does not hold its register",
        }
    }
}

/// What one template does with registers, as the pool checks it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Use {
    /// Every register the template reads (its input operands).
    pub(crate) inputs: RegSet,
    /// The register it writes its result to, if any.
    pub(crate) result: Option<Reg>,
    /// How many temporaries it names: `TEMP0` only is 1, `TEMP1` too is 2.
    pub(crate) temps: u8,
    /// The registers it writes besides its result (the table's data).
    pub(crate) clobbers: RegSet,
    /// A call's template: its inputs are read (`blr x16`) before anything is clobbered (by the
    /// callee). Only then may a held input be in the clobbers, and it is spent; a template that
    /// is not a call writes its clobbers at no stated point, so a held register among them is
    /// refused even when it is an input.
    pub(crate) call: bool,
}

/// The general scratch registers of one function's emission: each free or held, and a held one
/// maybe spent. Neither `Copy` nor `Clone`: two copies could each hand out one register.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Pool {
    all: RegSet,
    free: RegSet,
    spent: RegSet,
}

impl Pool {
    /// The pool of `regs`'s general scratch registers. Refused when it has fewer than two (a far
    /// store's address and a held value need two at once) or when the allocator may also hand
    /// one of them to a value (then a scratch use could overlap a live value).
    pub(crate) fn of(regs: &RegDesc) -> Result<Pool, ScratchError> {
        let all = regs.scratch.of(RegClass::Gpr);
        if all.len() < 2 {
            return Err(ScratchError::Description("a register description with fewer than two general scratch registers"));
        }
        if regs.scratch.0 & regs.allocatable.0 != 0 {
            return Err(ScratchError::Description("a register description whose scratch registers are also allocatable"));
        }
        Ok(Pool { all, free: all, spent: RegSet::EMPTY })
    }

    /// The pool of exactly `all`, for an emitter that allocates no register to a value: the
    /// Reference tier keeps every value in its cell (pass.rs), so each of its scratch
    /// registers ([`REFERENCE_SCRATCH`]) is only ever held or lent, and there is no allocatable
    /// set to check against (T3 step 1; [`Pool::of`] is the Fast tier's, over a description).
    /// Refused below two registers, as [`Pool::of`] is.
    #[allow(dead_code, reason = "T3 step 2: the Reference tier's mid-instruction helpers (pass.rs FAR, the probe's x17, call sites' x16, tls_index, the overflow flag) take their registers from this pool")]
    pub(crate) fn over(all: RegSet) -> Result<Pool, ScratchError> {
        if all.len() < 2 {
            return Err(ScratchError::Description("a scratch set of fewer than two general registers"));
        }
        Ok(Pool { all, free: all, spent: RegSet::EMPTY })
    }

    /// The registers held now.
    pub(crate) const fn held(&self) -> RegSet {
        self.all.minus(self.free)
    }

    /// Whether every register is free: the state between two instructions.
    pub(crate) const fn is_idle(&self) -> bool {
        self.free.0 == self.all.0
    }

    /// The lowest free register, held until its token is given back.
    pub(crate) fn take(&mut self) -> Result<Scratch, ScratchError> {
        let reg = self.free.first().ok_or(ScratchError::Exhausted)?;
        self.free = self.free.without(reg);
        Ok(Scratch { reg })
    }

    /// `s`'s register free again (and no longer spent).
    pub(crate) fn give(&mut self, s: Scratch) -> Result<(), ScratchError> {
        let reg = s.reg;
        if !self.all.contains(reg) || self.free.contains(reg) {
            return Err(ScratchError::Foreign(reg));
        }
        self.free = self.free.with(reg);
        self.spent = self.spent.without(reg);
        Ok(())
    }

    /// One template's use checked, and its temporaries lent from the free registers, lowest
    /// first: `[TEMP0, TEMP1]`, `None` past `use_.temps`. Nothing changes when it refuses.
    pub(crate) fn lend(&mut self, use_: Use) -> Result<[Option<Reg>; 2], ScratchError> {
        let held = self.held();
        let mut bits = use_.inputs.0 & self.all.0;
        while bits != 0 {
            let r = Reg(bits.trailing_zeros() as u8);
            bits &= bits - 1;
            if !held.contains(r) {
                return Err(ScratchError::NotHeld(r));
            }
            if self.spent.contains(r) {
                return Err(ScratchError::Spent(r));
            }
        }
        if let Some(r) = use_.result.filter(|&r| self.all.contains(r)) {
            if !held.contains(r) {
                return Err(ScratchError::NotHeld(r));
            }
        }
        let mut spent = self.spent;
        let mut bits = use_.clobbers.0 & held.0;
        while bits != 0 {
            let r = Reg(bits.trailing_zeros() as u8);
            bits &= bits - 1;
            match use_.call && use_.inputs.contains(r) && use_.result != Some(r) {
                true => spent = spent.with(r),
                false => return Err(ScratchError::Clobbered(r)),
            }
        }
        let mut temps = [None; 2];
        let mut free = self.free;
        for slot in temps.iter_mut().take(usize::from(use_.temps)) {
            let r = free.first().ok_or(ScratchError::Exhausted)?;
            free = free.without(r);
            *slot = Some(r);
        }
        // A result written fresh is a value again: no longer spent.
        if let Some(r) = use_.result {
            spent = spent.without(r);
        }
        self.spent = spent;
        Ok(temps)
    }
}
