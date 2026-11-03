//! A64 register names, condition codes and vector arrangements.
//!
//! A64 register names are regular enough to recognise by shape rather than by
//! table lookup: a class letter followed by a number, plus a handful of names
//! for register 31 and the ABI aliases GNU as accepts.

/// What a register name denotes, and how wide the view of it is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegClass {
    /// 32-bit general purpose: `w0`..`w30`, `wzr`, `wsp`.
    W,
    /// 64-bit general purpose: `x0`..`x30`, `xzr`, `sp`.
    X,
    /// Scalar views of a SIMD register, named for their width.
    B,
    H,
    S,
    D,
    Q,
}

impl RegClass {
    pub fn is_gpr(self) -> bool {
        matches!(self, RegClass::W | RegClass::X)
    }

    /// Width in bytes. Load and store opcodes are selected by this.
    pub fn bytes(self) -> u8 {
        match self {
            RegClass::B => 1,
            RegClass::H => 2,
            RegClass::S | RegClass::W => 4,
            RegClass::D | RegClass::X => 8,
            RegClass::Q => 16,
        }
    }

    pub fn letter(self) -> char {
        match self {
            RegClass::W => 'w',
            RegClass::X => 'x',
            RegClass::B => 'b',
            RegClass::H => 'h',
            RegClass::S => 's',
            RegClass::D => 'd',
            RegClass::Q => 'q',
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    /// Encoding number, 0-31.
    pub num: u8,
    /// True when the name was `sp`/`wsp` rather than `xzr`/`wzr`.
    ///
    /// Both encode as register 31; which of the two a field means depends on
    /// the instruction, so the spelling has to survive parsing and be checked
    /// where the encoding is built.
    pub sp: bool,
}

impl Reg {
    pub fn is_gpr(self) -> bool {
        self.class.is_gpr()
    }

    /// The `sf` bit: 1 for a 64-bit operation.
    pub fn sf(self) -> u32 {
        u32::from(self.class == RegClass::X)
    }

    /// True for `xzr`/`wzr`, the register-31 reading that is not the stack
    /// pointer.
    pub fn is_zr(self) -> bool {
        self.is_gpr() && self.num == 31 && !self.sp
    }

    pub fn is_sp(self) -> bool {
        self.num == 31 && self.sp
    }

    /// A register of the same number in another class, for aliases such as
    /// `neg`, which is `sub` against the zero register.
    pub fn zero(class: RegClass) -> Reg {
        Reg {
            class,
            num: 31,
            sp: false,
        }
    }

    pub fn name(self) -> String {
        match (self.class, self.num, self.sp) {
            (RegClass::X, 31, true) => "sp".into(),
            (RegClass::W, 31, true) => "wsp".into(),
            (RegClass::X, 31, false) => "xzr".into(),
            (RegClass::W, 31, false) => "wzr".into(),
            (c, n, _) => format!("{}{n}", c.letter()),
        }
    }
}

/// Parses a register name. `name` must already be lowercase.
pub fn lookup(name: &str) -> Option<Reg> {
    let gpr = |class, num, sp| Some(Reg { class, num, sp });
    match name {
        // Register 31 has two spellings per width and they are not
        // interchangeable: `mov sp, x0` and `mov xzr, x0` are different
        // instructions.
        "sp" => return gpr(RegClass::X, 31, true),
        "wsp" => return gpr(RegClass::W, 31, true),
        "xzr" => return gpr(RegClass::X, 31, false),
        "wzr" => return gpr(RegClass::W, 31, false),
        // Procedure-call standard aliases, which GNU as accepts.
        "fp" => return gpr(RegClass::X, 29, false),
        "lr" => return gpr(RegClass::X, 30, false),
        "ip0" => return gpr(RegClass::X, 16, false),
        "ip1" => return gpr(RegClass::X, 17, false),
        _ => {}
    }
    let (class, rest) = split_class(name)?;
    let num = small_number(rest)?;
    // The general-purpose files stop at 30: 31 is only reachable through the
    // `sp`/`zr` names handled above.
    let limit = if class.is_gpr() { 30 } else { 31 };
    if num > limit {
        return None;
    }
    gpr(class, num, false)
}

fn split_class(name: &str) -> Option<(RegClass, &str)> {
    let (first, rest) = name.split_at_checked(1)?;
    let class = match first {
        "w" => RegClass::W,
        "x" => RegClass::X,
        "b" => RegClass::B,
        "h" => RegClass::H,
        "s" => RegClass::S,
        "d" => RegClass::D,
        "q" => RegClass::Q,
        _ => return None,
    };
    Some((class, rest))
}

/// Parses a bare decimal register number, rejecting anything with a leading
/// zero (`x00` is not a register) or more than two digits.
fn small_number(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() == 2 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

/// How the lanes of a vector register are divided up (`v0.4s`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Arrangement {
    /// Lane width in bits: 8, 16, 32 or 64.
    pub elem_bits: u8,
    /// Number of lanes, or 0 when the name selects a single element (`v0.s`,
    /// used with an index).
    pub lanes: u8,
}

impl Arrangement {
    /// The `Q` bit: 1 for the 128-bit arrangements.
    #[allow(dead_code)]
    pub fn q(self) -> u32 {
        u32::from(self.elem_bits as u32 * self.lanes as u32 == 128)
    }

    /// The two-bit `size` field shared by most SIMD data-processing encodings.
    #[allow(dead_code)]
    pub fn size(self) -> u32 {
        match self.elem_bits {
            8 => 0,
            16 => 1,
            32 => 2,
            _ => 3,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct VecReg {
    pub num: u8,
    pub arr: Arrangement,
}

/// Parses `v<n>.<arrangement>`, for example `v0.4s`, `v31.16b` or `v2.d`.
pub fn vector(name: &str) -> Option<VecReg> {
    let rest = name.strip_prefix('v')?;
    let (num, arr) = rest.split_once('.')?;
    let num = small_number(num)?;
    if num > 31 {
        return None;
    }
    let arr = arrangement(arr)?;
    Some(VecReg { num, arr })
}

fn arrangement(s: &str) -> Option<Arrangement> {
    let a = |elem_bits, lanes| Some(Arrangement { elem_bits, lanes });
    match s {
        "8b" => a(8, 8),
        "16b" => a(8, 16),
        "4h" => a(16, 4),
        "8h" => a(16, 8),
        "2s" => a(32, 2),
        "4s" => a(32, 4),
        "1d" => a(64, 1),
        "2d" => a(64, 2),
        // Single-element forms, used with a lane index: `v0.s[1]`.
        "b" => a(8, 0),
        "h" => a(16, 0),
        "s" => a(32, 0),
        "d" => a(64, 0),
        _ => None,
    }
}

/// Condition codes, in encoding order.
const CONDS: [&str; 16] = [
    "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al", "nv",
];

/// Parses a condition name, including the `hs`/`lo` spellings of `cs`/`cc`.
pub fn cond(name: &str) -> Option<u8> {
    match name {
        "hs" => return Some(2),
        "lo" => return Some(3),
        _ => {}
    }
    CONDS.iter().position(|c| *c == name).map(|i| i as u8)
}

pub fn cond_name(code: u8) -> &'static str {
    CONDS.get(code as usize & 15).copied().unwrap_or("??")
}

/// True if `name` is any register spelling this backend knows.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    lookup(name).is_some() || vector(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_general_purpose_files() {
        assert_eq!(lookup("x0").unwrap().num, 0);
        assert_eq!(lookup("w30").unwrap().num, 30);
        assert_eq!(lookup("x30").unwrap().class, RegClass::X);
        // 31 is only spelled by name, never as a number.
        assert!(lookup("x31").is_none());
        assert!(lookup("w31").is_none());
        assert!(lookup("x00").is_none());
        assert!(lookup("x100").is_none());
        assert!(lookup("x").is_none());
    }

    #[test]
    fn stack_pointer_and_zero_register_stay_distinct() {
        let sp = lookup("sp").unwrap();
        let zr = lookup("xzr").unwrap();
        assert_eq!((sp.num, zr.num), (31, 31));
        assert!(sp.is_sp() && !sp.is_zr());
        assert!(zr.is_zr() && !zr.is_sp());
    }

    #[test]
    fn scalar_simd_registers_carry_their_width() {
        assert_eq!(lookup("q31").unwrap().class.bytes(), 16);
        assert_eq!(lookup("b0").unwrap().class.bytes(), 1);
        assert!(lookup("q32").is_none());
    }

    #[test]
    fn vector_names_split_into_number_and_arrangement() {
        let v = vector("v3.4s").unwrap();
        assert_eq!(v.num, 3);
        assert_eq!(v.arr.elem_bits, 32);
        assert_eq!(v.arr.lanes, 4);
        assert_eq!(v.arr.q(), 1);
        assert_eq!(vector("v3.2s").unwrap().arr.q(), 0);
        assert!(vector("v3.3s").is_none());
        assert!(vector("v32.4s").is_none());
    }

    #[test]
    fn condition_aliases_share_an_encoding() {
        assert_eq!(cond("hs"), cond("cs"));
        assert_eq!(cond("lo"), cond("cc"));
        assert_eq!(cond("al"), Some(14));
        assert_eq!(cond("nope"), None);
    }
}
