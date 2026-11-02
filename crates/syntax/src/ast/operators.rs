//! Defines a bunch of data-less enums for unary and binary operators.
//!
//! Types here don't know about AST, this allows re-using them for both AST and
//! HIR.
use std::fmt;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RangeOp {
    /// `..`
    Exclusive,
    /// `..=`
    Inclusive,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// `*`
    Deref,
    /// `!`
    Not,
    /// `-`
    Neg,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    LogicOp(LogicOp),
    ArithOp(ArithOp),
    CmpOp(CmpOp),
    Assignment { op: Option<ArithOp> },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LogicOp {
    And,
    Or,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq { negated: bool },
    Ord { ordering: Ordering, strict: bool },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ordering {
    Less,
    Greater,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArithOp {
    Add,
    Mul,
    Sub,
    Div,
    Rem,
    Shl,
    Shr,
    BitXor,
    BitOr,
    BitAnd,
}

impl ArithOp {
    /// Every operator, in declaration order: persisted codes index into it.
    pub const ALL: [ArithOp; 10] = [
        ArithOp::Add,
        ArithOp::Mul,
        ArithOp::Sub,
        ArithOp::Div,
        ArithOp::Rem,
        ArithOp::Shl,
        ArithOp::Shr,
        ArithOp::BitXor,
        ArithOp::BitOr,
        ArithOp::BitAnd,
    ];

    /// Its place in [`ArithOp::ALL`]: a persisted code, which never changes.
    pub const fn code(self) -> u32 {
        self as u32
    }
}

impl BinaryOp {
    /// How many [`BinaryOp::code`]s there are.
    pub const CODES: u32 = 29;

    /// `&&` and `||` at 0 and 1, the ten arithmetic operators from 2 in [`ArithOp::ALL`]'s
    /// order, `==` and `!=` at 12, `<=`, `<`, `>=`, `>` from 14, `=` at 18 and the compound
    /// assignments from 19: persisted codes, which never change.
    pub const fn code(self) -> u32 {
        match self {
            BinaryOp::LogicOp(LogicOp::And) => 0,
            BinaryOp::LogicOp(LogicOp::Or) => 1,
            BinaryOp::ArithOp(op) => 2 + op.code(),
            BinaryOp::CmpOp(CmpOp::Eq { negated }) => 12 + negated as u32,
            BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => {
                14 + 2 * ordering as u32 + strict as u32
            }
            BinaryOp::Assignment { op: None } => 18,
            BinaryOp::Assignment { op: Some(op) } => 19 + op.code(),
        }
    }

    /// The operator [`BinaryOp::code`] gave `code`.
    pub const fn from_code(code: u32) -> Option<BinaryOp> {
        Some(match code {
            0 => BinaryOp::LogicOp(LogicOp::And),
            1 => BinaryOp::LogicOp(LogicOp::Or),
            2..12 => BinaryOp::ArithOp(ArithOp::ALL[code as usize - 2]),
            12 | 13 => BinaryOp::CmpOp(CmpOp::Eq { negated: code == 13 }),
            14..18 => BinaryOp::CmpOp(CmpOp::Ord {
                ordering: if code >= 16 { Ordering::Greater } else { Ordering::Less },
                strict: code % 2 == 1,
            }),
            18 => BinaryOp::Assignment { op: None },
            19..29 => BinaryOp::Assignment { op: Some(ArithOp::ALL[code as usize - 19]) },
            _ => return None,
        })
    }
}

impl fmt::Display for LogicOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = match self {
            LogicOp::And => "&&",
            LogicOp::Or => "||",
        };
        f.write_str(res)
    }
}

impl fmt::Display for ArithOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = match self {
            ArithOp::Add => "+",
            ArithOp::Mul => "*",
            ArithOp::Sub => "-",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
            ArithOp::Shl => "<<",
            ArithOp::Shr => ">>",
            ArithOp::BitXor => "^",
            ArithOp::BitOr => "|",
            ArithOp::BitAnd => "&",
        };
        f.write_str(res)
    }
}

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = match self {
            CmpOp::Eq { negated: false } => "==",
            CmpOp::Eq { negated: true } => "!=",
            CmpOp::Ord { ordering: Ordering::Less, strict: false } => "<=",
            CmpOp::Ord { ordering: Ordering::Less, strict: true } => "<",
            CmpOp::Ord { ordering: Ordering::Greater, strict: false } => ">=",
            CmpOp::Ord { ordering: Ordering::Greater, strict: true } => ">",
        };
        f.write_str(res)
    }
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryOp::LogicOp(op) => fmt::Display::fmt(op, f),
            BinaryOp::ArithOp(op) => fmt::Display::fmt(op, f),
            BinaryOp::CmpOp(op) => fmt::Display::fmt(op, f),
            BinaryOp::Assignment { op } => {
                if let Some(op) = op {
                    fmt::Display::fmt(op, f)?;
                }
                f.write_str("=")?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hir-ty's `binary_code`, as it was.
    fn infer_code(op: BinaryOp) -> u32 {
        let arith = |op: ArithOp| ArithOp::ALL.iter().position(|it| *it == op).unwrap() as u32;
        match op {
            BinaryOp::LogicOp(LogicOp::And) => 0,
            BinaryOp::LogicOp(LogicOp::Or) => 1,
            BinaryOp::ArithOp(op) => 2 + arith(op),
            BinaryOp::CmpOp(CmpOp::Eq { negated }) => 12 + u32::from(negated),
            BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => {
                14 + 2 * u32::from(ordering == Ordering::Greater) + u32::from(strict)
            }
            BinaryOp::Assignment { op: None } => 18,
            BinaryOp::Assignment { op: Some(op) } => 19 + arith(op),
        }
    }

    /// hir-def's `binary_op_word`, as it was.
    fn column_word(op: Option<BinaryOp>) -> u32 {
        match op {
            None => 0,
            Some(BinaryOp::LogicOp(LogicOp::And)) => 1,
            Some(BinaryOp::LogicOp(LogicOp::Or)) => 2,
            Some(BinaryOp::ArithOp(op)) => 3 + op as u32,
            Some(BinaryOp::CmpOp(CmpOp::Eq { negated })) => 13 + negated as u32,
            Some(BinaryOp::CmpOp(CmpOp::Ord { ordering, strict })) => {
                15 + 2 * ordering as u32 + strict as u32
            }
            Some(BinaryOp::Assignment { op: None }) => 19,
            Some(BinaryOp::Assignment { op: Some(op) }) => 20 + op as u32,
        }
    }

    #[test]
    fn codes_are_the_ones_persisted() {
        for code in 0..BinaryOp::CODES {
            let op = BinaryOp::from_code(code).expect("every code below CODES is an operator");
            assert_eq!(op.code(), code);
            assert_eq!(infer_code(op), code, "hir-ty's code of {op:?}");
            assert_eq!(column_word(Some(op)), code + 1, "hir-def's word of {op:?}");
        }
        assert_eq!(BinaryOp::from_code(BinaryOp::CODES), None);
        for (code, op) in ArithOp::ALL.into_iter().enumerate() {
            assert_eq!(op.code() as usize, code);
        }
    }
}
