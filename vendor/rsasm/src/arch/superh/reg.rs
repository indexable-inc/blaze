//! SuperH register names.
//!
//! GNU as spells SH registers without a sigil, so a register is simply an
//! identifier that happens to be one of these names, compared without regard
//! to case. That makes every name here unavailable as a symbol in operand
//! position: `mov.l sr, r0` is a malformed control-register load, never a
//! PC-relative load from a label called `sr`. GNU as behaves the same way.

/// The control and system registers, which carry no number of their own:
/// the opcode says which one is meant.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Ctl {
    Sr,
    Gbr,
    Vbr,
    /// SH-3: saved status register.
    Ssr,
    /// SH-3: saved program counter.
    Spc,
    /// SH-4: saved general register 15.
    Sgr,
    /// SH-4: debug base register.
    Dbr,
    Mach,
    Macl,
    Pr,
    /// Only meaningful inside `@(disp,pc)`.
    Pc,
    /// FPU communication register.
    Fpul,
    /// FPU status and control register.
    Fpscr,
}

impl Ctl {
    pub fn name(self) -> &'static str {
        match self {
            Ctl::Sr => "sr",
            Ctl::Gbr => "gbr",
            Ctl::Vbr => "vbr",
            Ctl::Ssr => "ssr",
            Ctl::Spc => "spc",
            Ctl::Sgr => "sgr",
            Ctl::Dbr => "dbr",
            Ctl::Mach => "mach",
            Ctl::Macl => "macl",
            Ctl::Pr => "pr",
            Ctl::Pc => "pc",
            Ctl::Fpul => "fpul",
            Ctl::Fpscr => "fpscr",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Reg {
    /// `r0`-`r15`. `sp` is `r15`; the SH-DSP names `ix` / `is` and `iy` are
    /// `r8` and `r9`, and GNU as accepts them on every SH target.
    Gpr(u8),
    /// `r0_bank`-`r7_bank`, the SH-3 banked registers.
    Bank(u8),
    Ctl(Ctl),
    /// `fr0`-`fr15`, single-precision.
    Fr(u8),
    /// `dr0`-`dr14`, even numbers only: each names a pair of `fr` registers,
    /// and is encoded with the number of the first.
    Dr(u8),
    /// `fv0`, `fv4`, `fv8`, `fv12`: four `fr` registers as a vector.
    Fv(u8),
    /// The 4x4 matrix `fr0`-`fr15` that `ftrv` multiplies by.
    Xmtrx,
    /// A name GNU as reserves for an SH-2A or SH-DSP register this backend
    /// does not assemble. Recognising it keeps it from silently becoming a
    /// symbol reference that GNU as would reject.
    Reserved(&'static str),
}

/// Names GNU as treats as registers on SH-2A (`tbr`) and SH-DSP only.
const RESERVED: &[&str] = &[
    "tbr", "mod", "re", "rs", "dsr", "a0", "a1", "a0g", "a1g", "x0", "x1", "y0", "y1", "m0", "m1",
    "xd0", "xd2", "xd4", "xd6", "xd8", "xd10", "xd12", "xd14",
];

/// A decimal number with no leading zero, within `0..=max`. GNU as matches
/// register numbers character by character, so `r01` is not `r1`.
fn number(s: &str, max: u8) -> Option<u8> {
    if s.is_empty() || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    s.parse::<u8>().ok().filter(|n| *n <= max)
}

/// Looks up an identifier as a register.
pub fn lookup(name: &str) -> Option<Reg> {
    let lower = name.to_ascii_lowercase();
    let s = lower.as_str();
    let ctl = match s {
        "sr" => Some(Ctl::Sr),
        "gbr" => Some(Ctl::Gbr),
        "vbr" => Some(Ctl::Vbr),
        "ssr" => Some(Ctl::Ssr),
        "spc" => Some(Ctl::Spc),
        "sgr" => Some(Ctl::Sgr),
        "dbr" => Some(Ctl::Dbr),
        "mach" => Some(Ctl::Mach),
        "macl" => Some(Ctl::Macl),
        "pr" => Some(Ctl::Pr),
        "pc" => Some(Ctl::Pc),
        "fpul" => Some(Ctl::Fpul),
        "fpscr" => Some(Ctl::Fpscr),
        _ => None,
    };
    if let Some(c) = ctl {
        return Some(Reg::Ctl(c));
    }
    match s {
        "sp" => return Some(Reg::Gpr(15)),
        "ix" | "is" => return Some(Reg::Gpr(8)),
        "iy" => return Some(Reg::Gpr(9)),
        "xmtrx" => return Some(Reg::Xmtrx),
        _ => {}
    }
    if let Some(bank) = s.strip_prefix('r').and_then(|t| t.strip_suffix("_bank")) {
        return number(bank, 7).map(Reg::Bank);
    }
    if let Some(n) = s.strip_prefix("fr").and_then(|t| number(t, 15)) {
        return Some(Reg::Fr(n));
    }
    if let Some(n) = s.strip_prefix("dr").and_then(|t| number(t, 14)) {
        return (n % 2 == 0).then_some(Reg::Dr(n));
    }
    if let Some(n) = s.strip_prefix("fv").and_then(|t| number(t, 12)) {
        return (n % 4 == 0).then_some(Reg::Fv(n));
    }
    if let Some(n) = s.strip_prefix('r').and_then(|t| number(t, 15)) {
        return Some(Reg::Gpr(n));
    }
    RESERVED.iter().find(|r| **r == s).map(|r| Reg::Reserved(r))
}

/// How a register is written back in diagnostics.
pub fn name_of(r: Reg) -> String {
    match r {
        Reg::Gpr(n) => format!("r{n}"),
        Reg::Bank(n) => format!("r{n}_bank"),
        Reg::Ctl(c) => c.name().to_string(),
        Reg::Fr(n) => format!("fr{n}"),
        Reg::Dr(n) => format!("dr{n}"),
        Reg::Fv(n) => format!("fv{n}"),
        Reg::Xmtrx => "xmtrx".to_string(),
        Reg::Reserved(s) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_registers_and_their_aliases() {
        assert_eq!(lookup("r0"), Some(Reg::Gpr(0)));
        assert_eq!(lookup("R15"), Some(Reg::Gpr(15)));
        assert_eq!(lookup("sp"), Some(Reg::Gpr(15)));
        assert_eq!(lookup("ix"), Some(Reg::Gpr(8)));
        assert_eq!(lookup("r16"), None);
        assert_eq!(lookup("r01"), None);
    }

    #[test]
    fn banked_and_fpu_registers() {
        assert_eq!(lookup("r7_bank"), Some(Reg::Bank(7)));
        assert_eq!(lookup("r8_bank"), None);
        assert_eq!(lookup("dr14"), Some(Reg::Dr(14)));
        // A `dr` register names a pair, so only even numbers exist.
        assert_eq!(lookup("dr3"), None);
        assert_eq!(lookup("fv12"), Some(Reg::Fv(12)));
        assert_eq!(lookup("fv2"), None);
    }

    #[test]
    fn symbols_that_merely_start_like_registers_are_not_registers() {
        assert_eq!(lookup("sram"), None);
        assert_eq!(lookup("r1x"), None);
        assert_eq!(lookup("prefix"), None);
    }
}
