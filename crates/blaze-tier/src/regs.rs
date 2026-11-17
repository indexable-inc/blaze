//! A target's register description: what the composer may allocate, what a call keeps, what is
//! scratch, which register holds the run context (D98), and how the prologue saves pairs.
//!
//! Numbering, one space for both classes: general registers are 0..32 by their hardware number;
//! vector and float registers are 32..64 (`v0` / `xmm0` is 32). On arm64 31 is `sp`/`xzr`, never
//! in a set. A [`RegSet`] is one bit per number.

pub mod x86_64;

/// A target's instruction set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Arch {
    Arm64,
    X86_64,
}

/// The operating system a record is made for (its calling convention's variants and its traps).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Os {
    MacOs,
    Linux,
}

/// The machine a tier compiles for. `features` is a bit set ([`Machine::LSE`], ..); it is part of
/// every record key (point 6).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Machine {
    pub arch: Arch,
    pub os: Os,
    pub features: u64,
}

impl Machine {
    /// arm64: the large-system atomics (`cas`, `ldadd`, ..).
    pub const LSE: u64 = 1;
}

/// One register by its number (see the module's documentation).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Reg(pub u8);

impl Reg {
    /// The register's class by its number.
    pub const fn class(self) -> RegClass {
        if self.0 < 32 { RegClass::Gpr } else { RegClass::Fpr }
    }

    /// The number the encoding holds: the hardware number within its class.
    pub const fn hw(self) -> u8 {
        self.0 & 31
    }
}

/// A register's class: what a value of a type needs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RegClass {
    /// Integers of at most 64 bits and pointers.
    Gpr,
    /// `f32`, `f64` and 128-bit vectors.
    Fpr,
}

/// A set of registers, one bit per number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct RegSet(pub u64);

impl RegSet {
    pub const EMPTY: RegSet = RegSet(0);

    pub const fn contains(self, reg: Reg) -> bool {
        reg.0 < 64 && self.0 & (1u64 << reg.0) != 0
    }

    pub const fn with(self, reg: Reg) -> RegSet {
        RegSet(self.0 | (1u64 << (reg.0 & 63)))
    }

    pub const fn without(self, reg: Reg) -> RegSet {
        RegSet(self.0 & !(1u64 << (reg.0 & 63)))
    }

    pub const fn union(self, other: RegSet) -> RegSet {
        RegSet(self.0 | other.0)
    }

    pub const fn minus(self, other: RegSet) -> RegSet {
        RegSet(self.0 & !other.0)
    }

    /// The registers of `class` in the set.
    pub const fn of(self, class: RegClass) -> RegSet {
        match class {
            RegClass::Gpr => RegSet(self.0 & 0xffff_ffff),
            RegClass::Fpr => RegSet(self.0 & 0xffff_ffff_0000_0000),
        }
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The lowest-numbered register in the set.
    pub const fn first(self) -> Option<Reg> {
        if self.0 == 0 { None } else { Some(Reg(self.0.trailing_zeros() as u8)) }
    }
}

/// A target's registers as the composer and the templates see them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegDesc {
    pub arch: Arch,
    /// What the allocator may give a value. Never holds a scratch register, the run context,
    /// the stack, frame or link register, or an argument register.
    pub allocatable: RegSet,
    /// What a call keeps (for an Fpr on arm64: its low 64 bits only).
    pub callee_saved: RegSet,
    /// Registers a template or the composer's moves may use freely; never allocated and never
    /// live across a template.
    pub scratch: RegSet,
    /// A register reserved for a run context; `None` on every target today (D100 point 3: the
    /// poll is a load site). Kept so a target that needs one says so in data.
    pub run_context: Option<Reg>,
    /// Argument registers by class, in order.
    pub args_gpr: &'static [Reg],
    pub args_fpr: &'static [Reg],
    /// Return registers by class, in order.
    pub rets_gpr: &'static [Reg],
    pub rets_fpr: &'static [Reg],
    pub stack: Reg,
    pub frame: Reg,
    /// The link register; `None` where the return address is on the stack (x86-64).
    pub link: Option<Reg>,
    /// The callee-saved registers as the prologue saves them, in pairs, in the order compact
    /// unwind can encode (clang's: x19/x20 first, the vector pairs after the general ones).
    pub save_pairs: &'static [(Reg, Reg)],
}

const fn regs<const N: usize>(from: u8) -> [Reg; N] {
    let mut out = [Reg(0); N];
    let mut i = 0;
    while i < N {
        out[i] = Reg(from + i as u8);
        i += 1;
    }
    out
}

const ARM64_ARGS_GPR: [Reg; 8] = regs::<8>(0);
const ARM64_ARGS_FPR: [Reg; 8] = regs::<8>(32);
const ARM64_RETS_GPR: [Reg; 2] = regs::<2>(0);
const ARM64_RETS_FPR: [Reg; 4] = regs::<4>(32);
const ARM64_SAVE_PAIRS: [(Reg, Reg); 9] = [
    (Reg(19), Reg(20)),
    (Reg(21), Reg(22)),
    (Reg(23), Reg(24)),
    (Reg(25), Reg(26)),
    (Reg(27), Reg(28)),
    (Reg(40), Reg(41)),
    (Reg(42), Reg(43)),
    (Reg(44), Reg(45)),
    (Reg(46), Reg(47)),
];

/// arm64 under AAPCS64, Apple's and Linux's alike for what the composer uses. x18 is in no set
/// (Apple reserves it; leaving it out on Linux too keeps one description). x16 and x17 are the
/// scratch pair (import sites and the poll load through x16, today's emitter's own convention).
/// No register is reserved for a run context (D100 point 3). Allocatable: x9..x15, x19..x28,
/// v8..v31; the argument registers x0..x8 and v0..v7 are only moved into at calls and returns.
pub const ARM64: RegDesc = RegDesc {
    arch: Arch::Arm64,
    allocatable: RegSet(0xffff_ff00_1ff8_fe00),
    callee_saved: RegSet(0x0000_ff00_7ff8_0000),
    scratch: RegSet(0x0003_0000),
    run_context: None,
    args_gpr: &ARM64_ARGS_GPR,
    args_fpr: &ARM64_ARGS_FPR,
    rets_gpr: &ARM64_RETS_GPR,
    rets_fpr: &ARM64_RETS_FPR,
    stack: Reg(31),
    frame: Reg(29),
    link: Some(Reg(30)),
    save_pairs: &ARM64_SAVE_PAIRS,
};
