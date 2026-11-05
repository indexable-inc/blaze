//! RISC-V register names.
//!
//! Every register has two spellings: the numbered one (`x5`, `f10`) and the
//! ABI one (`t0`, `fa0`). Both are accepted everywhere.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegClass {
    /// Integer registers `x0`-`x31`.
    X,
    /// Floating-point registers `f0`-`f31`.
    F,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    pub num: u8,
}

impl Reg {
    pub const fn x(num: u8) -> Reg {
        Reg {
            class: RegClass::X,
            num,
        }
    }

    pub const fn f(num: u8) -> Reg {
        Reg {
            class: RegClass::F,
            num,
        }
    }

    pub fn is_x(self) -> bool {
        self.class == RegClass::X
    }

    /// True for the eight registers the compressed encodings can name.
    ///
    /// RVC spends three bits on most register fields, which reaches only
    /// `x8`-`x15` (`s0`-`a5`) — the registers the ABI makes most use of.
    pub fn is_popular(self) -> bool {
        (8..=15).contains(&self.num)
    }

    /// This register's number in a three-bit compressed field.
    pub fn popular_bits(self) -> u32 {
        (self.num as u32) & 7
    }

    pub fn bits(self) -> u32 {
        self.num as u32
    }
}

pub const ZERO: Reg = Reg::x(0);
pub const RA: Reg = Reg::x(1);
/// The alternate link register, which `tail` clobbers.
pub const T1: Reg = Reg::x(6);

#[rustfmt::skip]
const X_ABI: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2",
    "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5",
    "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7",
    "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

#[rustfmt::skip]
const F_ABI: [&str; 32] = [
    "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7",
    "fs0", "fs1", "fa0", "fa1", "fa2", "fa3", "fa4", "fa5",
    "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7",
    "fs8", "fs9", "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
];

/// Parses `x12`, `f31` and similar: a class letter followed by a number.
fn numbered(name: &str, prefix: char, class: RegClass) -> Option<Reg> {
    let rest = name.strip_prefix(prefix)?;
    if rest.is_empty() || rest.len() > 2 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num: u8 = rest.parse().ok()?;
    (num < 32).then_some(Reg { class, num })
}

pub fn lookup(name: &str) -> Option<Reg> {
    // `fp` is the traditional name for the frame pointer, which the ABI calls
    // `s0`; both name `x8`.
    if name == "fp" {
        return Some(Reg::x(8));
    }
    if let Some(i) = X_ABI.iter().position(|n| *n == name) {
        return Some(Reg::x(i as u8));
    }
    if let Some(i) = F_ABI.iter().position(|n| *n == name) {
        return Some(Reg::f(i as u8));
    }
    numbered(name, 'x', RegClass::X).or_else(|| numbered(name, 'f', RegClass::F))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_and_numbered_names_agree() {
        assert_eq!(lookup("zero"), Some(Reg::x(0)));
        assert_eq!(lookup("x0"), Some(Reg::x(0)));
        assert_eq!(lookup("fp"), lookup("s0"));
        assert_eq!(lookup("s0"), Some(Reg::x(8)));
        assert_eq!(lookup("a0"), lookup("x10"));
        assert_eq!(lookup("t6"), Some(Reg::x(31)));
        assert_eq!(lookup("fa0"), lookup("f10"));
        assert_eq!(lookup("ft11"), Some(Reg::f(31)));
    }

    #[test]
    fn out_of_range_and_junk_are_not_registers() {
        assert_eq!(lookup("x32"), None);
        assert_eq!(lookup("f32"), None);
        assert_eq!(lookup("x"), None);
        assert_eq!(lookup("x1x"), None);
        assert_eq!(lookup("main"), None);
        assert_eq!(lookup("x999"), None);
    }
}
