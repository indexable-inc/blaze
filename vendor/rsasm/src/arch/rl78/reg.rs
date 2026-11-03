//! Register names and their encoding numbers.
//!
//! The numbers are the ones the opcode fields use, which are not alphabetical:
//! the 8-bit registers are numbered `X A C B E D L H`, because `AX`, `BC`,
//! `DE` and `HL` are the pairs (high byte second) numbered 0 to 3. The
//! reference's own table (`gas/config/rl78-parse.y`, `token_table`) also
//! accepts the functional names `r0`–`r7` and `rp0`–`rp3`, so these do too.

/// `A`, the accumulator, whose operands get the short opcodes.
pub const A: u8 = 1;
/// `X`, the low half of `AX`.
pub const X: u8 = 0;
/// `C`.
pub const C: u8 = 2;
/// `B`.
pub const B: u8 = 3;

/// `AX`, the 16-bit accumulator.
pub const AX: u8 = 0;
/// `BC`.
pub const BC: u8 = 1;
/// `DE`.
pub const DE: u8 = 2;
/// `HL`.
pub const HL: u8 = 3;

/// The SFR byte of `ES`, which `mov` treats specially.
pub const SFR_ES: u8 = 0xfd;
/// The SFR byte of `PSW`, the only SFR `push` and `pop` take.
pub const SFR_PSW: u8 = 0xfa;

/// An 8-bit register: `x a c b e d l h`, or `r0`–`r7`.
pub fn reg8(name: &str) -> Option<u8> {
    Some(match name {
        "x" | "r0" => 0,
        "a" | "r1" => 1,
        "c" | "r2" => 2,
        "b" | "r3" => 3,
        "e" | "r4" => 4,
        "d" | "r5" => 5,
        "l" | "r6" => 6,
        "h" | "r7" => 7,
        _ => return None,
    })
}

/// A 16-bit register pair: `ax bc de hl`, or `rp0`–`rp3`.
pub fn reg16(name: &str) -> Option<u8> {
    Some(match name {
        "ax" | "rp0" => 0,
        "bc" | "rp1" => 1,
        "de" | "rp2" => 2,
        "hl" | "rp3" => 3,
        _ => return None,
    })
}

/// A special function register with a name of its own, as the low byte of its
/// address in `0xFFF00`–`0xFFFFF`. `SP` is not here: as a 16-bit register it
/// has operand forms of its own, while `SPL` and `SPH` are its two halves.
pub fn sfr(name: &str) -> Option<u8> {
    Some(match name {
        "spl" => 0xf8,
        "sph" => 0xf9,
        "psw" => 0xfa,
        "cs" => 0xfc,
        "es" => 0xfd,
        "pmc" => 0xfe,
        "mem" => 0xff,
        _ => return None,
    })
}

/// A register bank for `sel`: `rb0`–`rb3`.
pub fn bank(name: &str) -> Option<u8> {
    Some(match name {
        "rb0" => 0,
        "rb1" => 1,
        "rb2" => 2,
        "rb3" => 3,
        _ => return None,
    })
}

/// Every word the reference reads as a register rather than a symbol.
///
/// A label named `x` or `psw` cannot be referenced from an operand there, and
/// accepting it here would make the same source mean two different things.
pub fn is_reserved(name: &str) -> bool {
    reg8(name).is_some()
        || reg16(name).is_some()
        || sfr(name).is_some()
        || bank(name).is_some()
        || matches!(name, "sp" | "cy")
}
