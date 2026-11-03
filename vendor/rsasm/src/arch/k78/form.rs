//! Reads the rows of [`table`](super::table) into instruction forms.
//!
//! The table is kept as the manual prints it, strings and all, so that a
//! reviewer compares like with like. This module is the only place that
//! interprets those strings, and it is strict about it: a row whose operand
//! column and code columns disagree — an `r` operand with no `R2 R1 R0` bits,
//! a `saddr` operand with no `Saddr-offset` byte, a letter repeated the wrong
//! number of times — is rejected rather than half-understood. The tests
//! require every row to be accepted, so a transcription slip that breaks that
//! agreement fails the build instead of producing a wrong byte.

use super::table::{Note, ROWS, Row};
use std::sync::OnceLock;

/// One operand position of a form, as named in the table's operand column.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Slot {
    /// `r`: an 8-bit register, placed in the `R2 R1 R0` bits.
    R,
    /// `rp`: a register pair, placed in the `P1 P0` bits.
    Rp,
    /// A register the form names outright (`A`, `X`, `B`, `C`), by its `R`
    /// code from section 4.2.1.
    Reg(u8),
    /// `AX`, named outright.
    Ax,
    Sp,
    Psw,
    Cy,
    /// The `1` of `ROR A,1`.
    One,
    /// `#byte`
    Byte,
    /// `#word`
    Word,
    Saddr,
    Saddrp,
    Sfr,
    Sfrp,
    /// `!addr16`
    Addr16,
    /// `!addr11`
    Addr11,
    /// `[addr5]`
    Addr5,
    /// `$addr16`
    Rel,
    /// `[DE]`
    De,
    /// `[HL]`
    Hl,
    /// `[HL+byte]`
    HlByte,
    /// `[HL+B]`
    HlB,
    /// `[HL+C]`
    HlC,
    SaddrBit,
    SfrBit,
    ABit,
    PswBit,
    HlBit,
    /// `RBn`
    Bank,
}

impl Slot {
    fn parse(s: &str) -> Option<Slot> {
        Some(match s {
            "r" => Slot::R,
            "rp" => Slot::Rp,
            "X" => Slot::Reg(0),
            "A" => Slot::Reg(1),
            "C" => Slot::Reg(2),
            "B" => Slot::Reg(3),
            "AX" => Slot::Ax,
            "SP" => Slot::Sp,
            "PSW" => Slot::Psw,
            "CY" => Slot::Cy,
            "1" => Slot::One,
            "#byte" => Slot::Byte,
            "#word" => Slot::Word,
            "saddr" => Slot::Saddr,
            "saddrp" => Slot::Saddrp,
            "sfr" => Slot::Sfr,
            "sfrp" => Slot::Sfrp,
            "!addr16" => Slot::Addr16,
            "!addr11" => Slot::Addr11,
            "[addr5]" => Slot::Addr5,
            "$addr16" => Slot::Rel,
            "[DE]" => Slot::De,
            "[HL]" => Slot::Hl,
            "[HL+byte]" => Slot::HlByte,
            "[HL+B]" => Slot::HlB,
            "[HL+C]" => Slot::HlC,
            "saddr.bit" => Slot::SaddrBit,
            "sfr.bit" => Slot::SfrBit,
            "A.bit" => Slot::ABit,
            "PSW.bit" => Slot::PswBit,
            "[HL].bit" => Slot::HlBit,
            "RBn" => Slot::Bank,
            _ => return None,
        })
    }

    fn is_bit(self) -> bool {
        matches!(
            self,
            Slot::SaddrBit | Slot::SfrBit | Slot::ABit | Slot::PswBit | Slot::HlBit
        )
    }
}

/// A named byte column.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Field {
    Data,
    LowByte,
    HighByte,
    SaddrOffset,
    SfrOffset,
    LowAddr,
    HighAddr,
    Jdisp,
    /// `fa7–0`, the low byte of an `addr11`.
    Fa7_0,
}

/// A value spread over opcode bits: one of the table's letters.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Var {
    /// `R2 R1 R0`
    R,
    /// `P1 P0`
    P,
    /// `B2 B1 B0`
    B,
    /// `RB1`, `RB0`
    N,
    /// `fa10–8`
    F,
    /// `ta4–0`
    T,
}

impl Var {
    const ALL: [Var; 6] = [Var::R, Var::P, Var::B, Var::N, Var::F, Var::T];

    fn letter(self) -> char {
        match self {
            Var::R => 'r',
            Var::P => 'p',
            Var::B => 'b',
            Var::N => 'n',
            Var::F => 'f',
            Var::T => 't',
        }
    }

    /// How many bits the manual gives the value.
    pub fn width(self) -> u32 {
        match self {
            Var::R | Var::B | Var::F => 3,
            Var::P | Var::N => 2,
            Var::T => 5,
        }
    }
}

/// One byte column of a form.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Code {
    /// Opcode bits: the fixed bits, plus which variable (if any) owns each
    /// bit position, index 0 being bit 7.
    Bits {
        fixed: u8,
        vars: [Option<Var>; 8],
    },
    Field(Field),
}

impl Code {
    /// The byte with `value` placed into `var`'s positions, most significant
    /// bit first. Positions belonging to other variables are left clear.
    pub fn place(&self, var: Var, value: u32, byte: u8) -> u8 {
        let Code::Bits { vars, .. } = self else {
            return byte;
        };
        let positions: Vec<usize> = (0..8).filter(|i| vars[*i] == Some(var)).collect();
        let mut out = byte;
        for (k, pos) in positions.iter().enumerate() {
            // `k` counts from the most significant of the value's bits that
            // land in this byte. Only `f` and `t` are wide enough to matter,
            // and each sits entirely in one byte.
            let bit = (value >> (positions.len() - 1 - k)) & 1;
            if bit != 0 {
                out |= 0x80 >> pos;
            }
        }
        out
    }
}

/// A table row, understood.
#[derive(Clone, Debug)]
pub struct Form {
    pub row: &'static Row,
    pub slots: Vec<Slot>,
    pub codes: Vec<Code>,
}

impl Form {
    /// The instruction's length in bytes.
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Whether the form uses `var` anywhere in its opcode bits.
    pub fn uses(&self, var: Var) -> bool {
        self.codes
            .iter()
            .any(|c| matches!(c, Code::Bits { vars, .. } if vars.contains(&Some(var))))
    }

    /// The values `var` may take in this form, honouring the row's footnote.
    ///
    /// `f` and `t` range over every value, because they come from an address
    /// rather than from a register choice.
    pub fn values(&self, var: Var) -> Vec<u32> {
        let all = 0..(1u32 << var.width());
        match (var, self.row.note) {
            // `A` is `R1`.
            (Var::R, Note::ExceptA) => all.filter(|v| *v != 1).collect(),
            // `AX` is `RP0`.
            (Var::P, Note::OnlyBcDeHl) => all.filter(|v| *v != 0).collect(),
            _ => all.collect(),
        }
    }

    /// The bytes of the form with every variable bound, as `Some(byte)`, and
    /// every operand field left as `None`.
    ///
    /// `bind` gives each variable's value; variables the form does not use are
    /// ignored.
    pub fn pattern(&self, bind: impl Fn(Var) -> u32) -> Vec<Option<u8>> {
        self.codes
            .iter()
            .map(|c| match c {
                Code::Bits { fixed, .. } => Some(
                    Var::ALL
                        .iter()
                        .fold(*fixed, |b, v| c.place(*v, bind(*v), b)),
                ),
                Code::Field(_) => None,
            })
            .collect()
    }
}

fn parse_code(text: &str) -> Result<Code, String> {
    let field = match text {
        "Data" => Some(Field::Data),
        "Low byte" => Some(Field::LowByte),
        "High byte" => Some(Field::HighByte),
        "Saddr-offset" => Some(Field::SaddrOffset),
        "Sfr-offset" => Some(Field::SfrOffset),
        "Low addr" => Some(Field::LowAddr),
        "High addr" => Some(Field::HighAddr),
        "jdisp" => Some(Field::Jdisp),
        "fa7-0" => Some(Field::Fa7_0),
        _ => None,
    };
    if let Some(f) = field {
        return Ok(Code::Field(f));
    }
    let bits: Vec<char> = text.chars().filter(|c| *c != ' ').collect();
    if bits.len() != 8 {
        return Err(format!("`{text}` is neither a field name nor eight bits"));
    }
    let mut fixed = 0u8;
    let mut vars = [None; 8];
    for (i, ch) in bits.iter().enumerate() {
        match ch {
            '0' => {}
            '1' => fixed |= 0x80 >> i,
            _ => match Var::ALL.iter().find(|v| v.letter() == *ch) {
                Some(v) => vars[i] = Some(*v),
                None => return Err(format!("unknown bit `{ch}` in `{text}`")),
            },
        }
    }
    Ok(Code::Bits { fixed, vars })
}

/// Interprets one row, checking that its operand and code columns agree.
pub fn parse_row(row: &'static Row) -> Result<Form, String> {
    let slots = if row.operands.is_empty() {
        Vec::new()
    } else {
        row.operands
            .split(',')
            .map(|s| Slot::parse(s).ok_or_else(|| format!("unknown operand `{s}`")))
            .collect::<Result<Vec<_>, _>>()?
    };
    let codes = row
        .codes
        .iter()
        .map(|c| parse_code(c))
        .collect::<Result<Vec<_>, _>>()?;
    if codes.is_empty() || codes.len() > 4 {
        return Err(format!("{} code columns", codes.len()));
    }
    if !matches!(codes[0], Code::Bits { .. }) {
        return Err("the first byte must be an opcode".into());
    }
    let form = Form { row, slots, codes };
    check_agreement(&form)?;
    Ok(form)
}

fn count_slots(form: &Form, pred: impl Fn(Slot) -> bool) -> usize {
    form.slots.iter().filter(|s| pred(**s)).count()
}

fn field_at(form: &Form, i: usize) -> Option<Field> {
    match form.codes.get(i) {
        Some(Code::Field(f)) => Some(*f),
        _ => None,
    }
}

/// The checks that make a mistyped row fail loudly.
fn check_agreement(form: &Form) -> Result<(), String> {
    // Each letter appears exactly as often as its bit name has bits, and only
    // when the operand that supplies it is present.
    for var in Var::ALL {
        let n: u32 = form
            .codes
            .iter()
            .map(|c| match c {
                Code::Bits { vars, .. } => vars.iter().filter(|v| **v == Some(var)).count() as u32,
                Code::Field(_) => 0,
            })
            .sum();
        let wanted = match var {
            Var::R => count_slots(form, |s| s == Slot::R),
            Var::P => count_slots(form, |s| s == Slot::Rp),
            Var::B => count_slots(form, Slot::is_bit),
            Var::N => count_slots(form, |s| s == Slot::Bank),
            Var::F => count_slots(form, |s| s == Slot::Addr11),
            Var::T => count_slots(form, |s| s == Slot::Addr5),
        };
        if wanted > 1 {
            return Err(format!("more than one operand supplies `{}`", var.letter()));
        }
        let expect = if wanted == 1 { var.width() } else { 0 };
        if n != expect {
            return Err(format!(
                "`{}` appears {n} times; expected {expect}",
                var.letter()
            ));
        }
        // The encoder fills `f` and `t` from one fixup each, so they must not
        // be split across bytes.
        if n > 0 && matches!(var, Var::F | Var::T) {
            let bytes = form
                .codes
                .iter()
                .filter(|c| matches!(c, Code::Bits { vars, .. } if vars.contains(&Some(var))))
                .count();
            if bytes != 1 {
                return Err(format!("`{}` spans {bytes} bytes", var.letter()));
            }
        }
    }

    let fields = |f: Field| form.codes.iter().filter(|c| **c == Code::Field(f)).count();
    let want = [
        (
            Field::Data,
            count_slots(form, |s| matches!(s, Slot::Byte | Slot::HlByte)),
        ),
        (Field::LowByte, count_slots(form, |s| s == Slot::Word)),
        (Field::HighByte, count_slots(form, |s| s == Slot::Word)),
        (
            Field::SaddrOffset,
            count_slots(form, |s| {
                matches!(s, Slot::Saddr | Slot::Saddrp | Slot::SaddrBit)
            }),
        ),
        (
            Field::SfrOffset,
            count_slots(form, |s| matches!(s, Slot::Sfr | Slot::Sfrp | Slot::SfrBit)),
        ),
        (Field::LowAddr, count_slots(form, |s| s == Slot::Addr16)),
        (Field::HighAddr, count_slots(form, |s| s == Slot::Addr16)),
        (Field::Jdisp, count_slots(form, |s| s == Slot::Rel)),
        (Field::Fa7_0, count_slots(form, |s| s == Slot::Addr11)),
    ];
    for (f, n) in want {
        if fields(f) != n {
            return Err(format!("{} `{f:?}` columns; expected {n}", fields(f)));
        }
    }

    // Two-byte values are little-endian and contiguous; `fa7–0` directly
    // follows the byte holding `fa10–8`; a displacement is the last byte.
    for (i, c) in form.codes.iter().enumerate() {
        match c {
            Code::Field(Field::LowByte) if field_at(form, i + 1) != Some(Field::HighByte) => {
                return Err("`Low byte` is not followed by `High byte`".into());
            }
            Code::Field(Field::LowAddr) if field_at(form, i + 1) != Some(Field::HighAddr) => {
                return Err("`Low addr` is not followed by `High addr`".into());
            }
            Code::Field(Field::Fa7_0) => {
                let prev = i.checked_sub(1).and_then(|p| form.codes.get(p));
                let ok = matches!(prev, Some(Code::Bits { vars, .. })
                    if vars[1..4].iter().all(|v| *v == Some(Var::F)));
                if !ok {
                    return Err("`fa7-0` does not follow bits 6..4 holding `fa10-8`".into());
                }
            }
            Code::Field(Field::Jdisp) if i + 1 != form.codes.len() => {
                return Err("`jdisp` is not the last byte".into());
            }
            Code::Bits { vars, .. }
                if vars.contains(&Some(Var::T))
                    && !vars[2..7].iter().all(|v| *v == Some(Var::T)) =>
            {
                return Err("`ta4-0` is not in bits 5..1".into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Every row that parses, in table order.
///
/// A row that does not parse is left out rather than panicking: the backend
/// must never panic, and `tests/k78.rs` asserts that no row is left out.
pub fn forms() -> &'static [Form] {
    static FORMS: OnceLock<Vec<Form>> = OnceLock::new();
    FORMS.get_or_init(|| ROWS.iter().filter_map(|r| parse_row(r).ok()).collect())
}
