//! Mnemonics: how a name and its size suffix are split, and which family
//! of instruction each name belongs to.
//!
//! Sizes are spelled `move.w` in Motorola source and `movew` in GNU source,
//! and GNU as takes both spellings in both of its modes, so this does too.
//! A bare trailing `b`/`w`/`l`/`s` is taken as a size only when what is left
//! is a mnemonic that accepts that size. That keeps `bls`, `scs` and `tas`
//! intact, reads `divsl` as `divs.l` the way GNU as does, and lets `extb`
//! stand for itself, since `ext` has no byte form.

use super::{M68000UP, M68010UP, M68020UP};

/// Every family, with the opcode bits that distinguish its members.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    Move,
    MoveA,
    MoveQ,
    MoveM,
    MoveC,
    Lea,
    Pea,
    Exg,
    Swap,
    Ext,
    ExtB,
    /// `clr`, `neg`, `negx`, `not`: base opcode.
    Unary(u16),
    Tst,
    /// `tas`, `nbcd`: byte-only, data-alterable.
    UnaryB(u16),
    /// `add`/`sub`: `D000`/`9000`.
    AddSub(u16),
    /// `adda`/`suba`: `D0C0`/`90C0`.
    AddSubA(u16),
    /// `addi`, `subi`, `cmpi`, `andi`, `ori`, `eori`: base opcode.
    Immed(u16),
    /// `addq`/`subq`: `5000`/`5100`.
    Quick(u16),
    /// `addx`/`subx` (`D100`/`9100`), `abcd`/`sbcd` (`C100`/`8100`).
    X(u16, bool),
    Cmp,
    CmpA,
    CmpM,
    /// `and`/`or`: `C000`/`8000`.
    Logic(u16),
    Eor,
    /// `mulu`, `muls`, `divu`, `divs`: 16-bit base opcode, then whether it is
    /// a divide and whether it is signed.
    MulDiv(u16, bool, bool),
    /// `divul`/`divsl`: 32-bit quotient and remainder. Signed?
    DivL(bool),
    Chk,
    /// `chk2`/`cmp2`: whether it traps.
    Chk2(bool),
    /// Shift type (`as`, `ls`, `rox`, `ro`) and direction (left?).
    Shift(u8, bool),
    /// `btst`, `bchg`, `bclr`, `bset`.
    Bit(u8),
    Scc(u8),
    /// Condition, and whether it is GNU's always-relaxing `jbCC` spelling.
    Bcc(u8, bool),
    DBcc(u8),
    Jmp(u16),
    Trap,
    Link,
    Unlk,
    /// `stop`, `rtd`: opcode and a 16-bit immediate.
    Word(u16),
    Bkpt,
    /// No operands.
    Fixed(u16),
    /// Bit-field instructions: opcode and operand shape.
    Bf(u16, BfShape),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BfShape {
    /// `bftst ea{o:w}`
    Ea,
    /// `bfextu ea{o:w},dn`
    EaReg,
    /// `bfins dn,ea{o:w}`
    RegEa,
}

/// Size suffixes, as a set.
pub const B: u8 = 1;
pub const W: u8 = 2;
pub const L: u8 = 4;
pub const S: u8 = 8;
const BWL: u8 = B | W | L;

#[derive(Copy, Clone, Debug)]
pub struct Def {
    pub kind: Kind,
    /// Suffixes accepted. A family with none still takes no suffix at all.
    pub sizes: u8,
    /// The CPUs that have it, as `table::feature` bits, for a
    /// spelling GNU's table does not name; see `table::HAND_ARCH`.
    pub(crate) arch: u32,
}

const fn d(kind: Kind, sizes: u8) -> Def {
    Def {
        kind,
        sizes,
        arch: M68000UP,
    }
}

const fn d10(kind: Kind, sizes: u8) -> Def {
    Def {
        kind,
        sizes,
        arch: M68010UP,
    }
}

const fn d20(kind: Kind, sizes: u8) -> Def {
    Def {
        kind,
        sizes,
        arch: M68020UP,
    }
}

/// Condition codes by name. `hs`/`lo` are the unsigned spellings of `cc`/`cs`.
pub fn condition(name: &str) -> Option<u8> {
    Some(match name {
        "t" => 0,
        "f" => 1,
        "hi" => 2,
        "ls" => 3,
        "cc" | "hs" => 4,
        "cs" | "lo" => 5,
        "ne" => 6,
        "eq" => 7,
        "vc" => 8,
        "vs" => 9,
        "pl" => 10,
        "mi" => 11,
        "ge" => 12,
        "lt" => 13,
        "gt" => 14,
        "le" => 15,
        _ => return None,
    })
}

/// The family a base mnemonic (without size) belongs to.
pub fn base(name: &str) -> Option<Def> {
    use Kind::*;
    let def = match name {
        "move" | "mov" => d(Move, BWL),
        "movea" => d(MoveA, W | L),
        "moveq" => d(MoveQ, L),
        "movem" => d(MoveM, W | L),
        "movec" => d10(MoveC, L),
        "lea" => d(Lea, L),
        "pea" => d(Pea, L),
        "exg" => d(Exg, L),
        "swap" => d(Swap, W),
        "ext" => d(Ext, W | L),
        "extb" => d20(ExtB, L),
        "clr" => d(Unary(0x4200), BWL),
        "neg" => d(Unary(0x4400), BWL),
        "negx" => d(Unary(0x4000), BWL),
        "not" => d(Unary(0x4600), BWL),
        "tst" => d(Tst, BWL),
        "tas" => d(UnaryB(0x4ac0), B),
        "nbcd" => d(UnaryB(0x4800), B),
        "add" => d(AddSub(0xd000), BWL),
        "sub" => d(AddSub(0x9000), BWL),
        "adda" => d(AddSubA(0xd0c0), W | L),
        "suba" => d(AddSubA(0x90c0), W | L),
        "addi" => d(Immed(0x0600), BWL),
        "subi" => d(Immed(0x0400), BWL),
        "cmpi" => d(Immed(0x0c00), BWL),
        "andi" => d(Immed(0x0200), BWL),
        "ori" => d(Immed(0x0000), BWL),
        "eori" => d(Immed(0x0a00), BWL),
        "addq" => d(Quick(0x5000), BWL),
        "subq" => d(Quick(0x5100), BWL),
        "addx" => d(X(0xd100, true), BWL),
        "subx" => d(X(0x9100, true), BWL),
        "abcd" => d(X(0xc100, false), B),
        "sbcd" => d(X(0x8100, false), B),
        "cmp" => d(Cmp, BWL),
        "cmpa" => d(CmpA, W | L),
        "cmpm" => d(CmpM, BWL),
        "and" => d(Logic(0xc000), BWL),
        "or" => d(Logic(0x8000), BWL),
        "eor" => d(Eor, BWL),
        "mulu" => d(MulDiv(0xc0c0, false, false), W | L),
        "muls" => d(MulDiv(0xc1c0, false, true), W | L),
        "divu" => d(MulDiv(0x80c0, true, false), W | L),
        "divs" => d(MulDiv(0x81c0, true, true), W | L),
        "divul" => d20(DivL(false), L),
        "divsl" => d20(DivL(true), L),
        "chk" => d(Chk, W | L),
        "chk2" => d20(Chk2(true), BWL),
        "cmp2" => d20(Chk2(false), BWL),
        "asr" => d(Shift(0, false), BWL),
        "asl" => d(Shift(0, true), BWL),
        "lsr" => d(Shift(1, false), BWL),
        "lsl" => d(Shift(1, true), BWL),
        "roxr" => d(Shift(2, false), BWL),
        "roxl" => d(Shift(2, true), BWL),
        "ror" => d(Shift(3, false), BWL),
        "rol" => d(Shift(3, true), BWL),
        "btst" => d(Bit(0), B | L),
        "bchg" => d(Bit(1), B | L),
        "bclr" => d(Bit(2), B | L),
        "bset" => d(Bit(3), B | L),
        "bra" | "jra" | "jbra" => d(Bcc(0, name.starts_with('j')), B | W | L | S),
        "bsr" | "jbsr" => d(Bcc(1, name.starts_with('j')), B | W | L | S),
        "dbra" => d(DBcc(1), W),
        "jmp" => d(Jmp(0x4ec0), 0),
        "jsr" => d(Jmp(0x4e80), 0),
        "trap" => d(Trap, 0),
        "link" => d(Link, W | L),
        "unlk" => d(Unlk, 0),
        "stop" => d(Word(0x4e72), 0),
        "rtd" => d10(Word(0x4e74), 0),
        "bkpt" => d10(Bkpt, 0),
        "rts" => d(Fixed(0x4e75), 0),
        "rte" => d(Fixed(0x4e73), 0),
        "rtr" => d(Fixed(0x4e77), 0),
        "trapv" => d(Fixed(0x4e76), 0),
        "nop" => d(Fixed(0x4e71), 0),
        "reset" => d(Fixed(0x4e70), 0),
        "illegal" => d(Fixed(0x4afc), 0),
        "bftst" => d20(Bf(0xe8c0, BfShape::Ea), 0),
        "bfextu" => d20(Bf(0xe9c0, BfShape::EaReg), 0),
        "bfchg" => d20(Bf(0xeac0, BfShape::Ea), 0),
        "bfexts" => d20(Bf(0xebc0, BfShape::EaReg), 0),
        "bfclr" => d20(Bf(0xecc0, BfShape::Ea), 0),
        "bfffo" => d20(Bf(0xedc0, BfShape::EaReg), 0),
        "bfset" => d20(Bf(0xeec0, BfShape::Ea), 0),
        "bfins" => d20(Bf(0xefc0, BfShape::RegEa), 0),
        _ => return conditional(name),
    };
    Some(def)
}

/// `Bcc`, `DBcc`, `Scc`, and GNU's `jCC`/`jbCC`.
fn conditional(name: &str) -> Option<Def> {
    if let Some(c) = name.strip_prefix("db").and_then(condition) {
        return Some(d(Kind::DBcc(c), W));
    }
    if let Some(c) = name.strip_prefix("jb").and_then(condition)
        && c >= 2
    {
        return Some(d(Kind::Bcc(c, true), B | W | L | S));
    }
    // `jmp` and `jsr` were matched before this, so a `j` here is a condition.
    if let Some(c) = name.strip_prefix('j').and_then(condition)
        && c >= 2
    {
        return Some(d(Kind::Bcc(c, true), B | W | L | S));
    }
    if let Some(c) = name.strip_prefix('b').and_then(condition)
        && c >= 2
    {
        return Some(d(Kind::Bcc(c, false), B | W | L | S));
    }
    if let Some(c) = name.strip_prefix('s').and_then(condition) {
        return Some(d(Kind::Scc(c), B));
    }
    None
}

pub fn size_bit(c: char) -> Option<u8> {
    Some(match c {
        'b' => B,
        'w' => W,
        'l' => L,
        's' => S,
        _ => return None,
    })
}

/// Splits a lower-case mnemonic into its family and size letter.
///
/// Returns `Err` with a message when the name is recognisable but the suffix
/// is not one it takes.
pub fn resolve(name: &str) -> Result<(Def, Option<char>, &str), String> {
    if let Some((stem, suffix)) = name.rsplit_once('.') {
        let Some(def) = base(stem) else {
            return Err(format!("unknown instruction `{name}`"));
        };
        let mut chars = suffix.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Err(format!("`.{suffix}` is not a size suffix"));
        };
        return match size_bit(c) {
            Some(bit) if def.sizes & bit != 0 => Ok((def, Some(c), stem)),
            Some(_) => Err(no_size(stem, c)),
            None => Err(format!("`.{suffix}` is not a size suffix")),
        };
    }
    if let Some(c) = name.chars().last()
        && let Some(bit) = size_bit(c)
    {
        let stem = &name[..name.len() - 1];
        if let Some(def) = base(stem)
            && def.sizes & bit != 0
        {
            return Ok((def, Some(c), stem));
        }
    }
    match base(name) {
        Some(def) => Ok((def, None, name)),
        None => Err(format!("unknown instruction `{name}`")),
    }
}

fn no_size(stem: &str, c: char) -> String {
    format!("`{stem}` does not take a `.{c}` size")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(name: &str) -> (Kind, Option<char>) {
        let (d, c, _) = resolve(name).unwrap();
        (d.kind, c)
    }

    #[test]
    fn suffixes_do_not_eat_condition_names() {
        assert_eq!(split("bls"), (Kind::Bcc(3, false), None));
        assert_eq!(split("blss"), (Kind::Bcc(3, false), Some('s')));
        assert_eq!(split("scs"), (Kind::Scc(5), None));
        assert_eq!(split("tas"), (Kind::UnaryB(0x4ac0), None));
        assert_eq!(split("movew"), (Kind::Move, Some('w')));
        assert_eq!(split("move.w"), (Kind::Move, Some('w')));
        assert_eq!(split("extb"), (Kind::ExtB, None));
        assert_eq!(split("extbl"), (Kind::ExtB, Some('l')));
        assert_eq!(split("divsl").0, Kind::MulDiv(0x81c0, true, true));
        assert_eq!(split("divsl.l").0, Kind::DivL(true));
        assert_eq!(split("jmp").0, Kind::Jmp(0x4ec0));
        assert_eq!(split("jmi").0, Kind::Bcc(11, true));
        assert_eq!(split("st").0, Kind::Scc(0));
        assert!(resolve("lea.b").is_err());
        assert!(resolve("frob").is_err());
    }
}
