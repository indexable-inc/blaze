//! The mutation operators and the concrete mutations a site admits.
//!
//! Each operator is a typed, deterministic rewrite of one function's checked MIR. It works on a
//! [`Site`] the front end described with the alternatives the type checker already accepts, so
//! every mutation compiles by construction: [`mutations`] never makes one the front end would
//! refuse.
//!
//! The yields and equivalence rates in [`Info`] cite the literature where it measured them;
//! the rest are marked estimates, to be replaced by blaze's own counts (`report::Score` per
//! operator, over reth, tempo and blaze).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use crate::site::{ArithOp, CmpOp, Site, SiteKind, TyClass};

/// One operator: the rule name a diagnostic carries is [`Operator::rule`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Operator {
    /// The function's body returns a default of its type.
    ReturnDefault,
    /// The function's body returns one of its arguments of the return type.
    ReturnArg,
    /// One field of the returned struct is replaced by its default.
    ReturnField,
    /// A branch's condition is negated.
    NegateCondition,
    /// A comparison's boundary moves: `<` and `<=`, `>` and `>=`.
    CmpBoundary,
    /// `==` and `!=` swap.
    CmpEquality,
    /// An arithmetic or bitwise operator is replaced by its neighbour.
    ArithNeighbour,
    /// A constant is replaced by zero or an adjacent value.
    ConstAdjacent,
    /// A statement whose effect is a write or a call is deleted.
    DeleteEffect,
    /// One arm's body is replaced by another arm's.
    ArmBody,
    /// Two arguments of the same type at a call swap.
    SwapArgs,
    /// An early return is skipped.
    SkipEarlyReturn,
    /// Another variant of the same enum (another error) is constructed.
    OtherVariant,
}

/// What an operator is for, how much it finds, and how often it is equivalent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    pub why: &'static str,
    /// How often a mutant of it, shown, leads to a test worth writing.
    pub yields: &'static str,
    /// How often a mutant of it is equivalent after this crate's rules.
    pub equivalent: &'static str,
}

impl Operator {
    pub const ALL: [Operator; 13] = [
        Operator::ReturnDefault,
        Operator::ReturnArg,
        Operator::ReturnField,
        Operator::NegateCondition,
        Operator::CmpBoundary,
        Operator::CmpEquality,
        Operator::ArithNeighbour,
        Operator::ConstAdjacent,
        Operator::DeleteEffect,
        Operator::ArmBody,
        Operator::SwapArgs,
        Operator::SkipEarlyReturn,
        Operator::OtherVariant,
    ];

    /// The diagnostic's rule name.
    pub fn rule(self) -> &'static str {
        match self {
            Operator::ReturnDefault => "mutants::return_default",
            Operator::ReturnArg => "mutants::return_arg",
            Operator::ReturnField => "mutants::return_field",
            Operator::NegateCondition => "mutants::negate_condition",
            Operator::CmpBoundary => "mutants::cmp_boundary",
            Operator::CmpEquality => "mutants::cmp_equality",
            Operator::ArithNeighbour => "mutants::arith_neighbour",
            Operator::ConstAdjacent => "mutants::const_adjacent",
            Operator::DeleteEffect => "mutants::delete_effect",
            Operator::ArmBody => "mutants::arm_body",
            Operator::SwapArgs => "mutants::swap_args",
            Operator::SkipEarlyReturn => "mutants::skip_early_return",
            Operator::OtherVariant => "mutants::other_variant",
        }
    }

    /// The operator a rule name names: what `#[blaze::equivalent_mutant(rule = ..)]` is read by.
    pub fn of_rule(rule: &[u8]) -> Option<Operator> {
        Operator::ALL.into_iter().find(|op| op.rule().as_bytes() == rule)
    }

    /// The extreme operators: one mutant replaces the whole body's result, so a survivor means
    /// the function is pseudo-tested (its tests run it and check nothing of what it returns).
    pub fn extreme(self) -> bool {
        matches!(self, Operator::ReturnDefault | Operator::ReturnArg | Operator::ReturnField)
    }

    /// How telling a survivor of it is, for picking one mutant per line: the higher, the more
    /// precise the test it asks for. An estimate, ordered by how directly the asked-for test
    /// follows from the diagnostic.
    pub fn telling(self) -> u8 {
        match self {
            Operator::ReturnField => 12,
            Operator::CmpBoundary => 11,
            Operator::CmpEquality => 10,
            Operator::NegateCondition => 9,
            Operator::OtherVariant => 8,
            Operator::DeleteEffect => 7,
            Operator::SkipEarlyReturn => 6,
            Operator::ArithNeighbour => 5,
            Operator::SwapArgs => 4,
            Operator::ArmBody => 3,
            Operator::ConstAdjacent => 2,
            Operator::ReturnArg => 1,
            Operator::ReturnDefault => 0,
        }
    }

    /// Its number in a record: stable, never reused.
    pub fn code(self) -> u8 {
        match self {
            Operator::ReturnDefault => 1,
            Operator::ReturnArg => 2,
            Operator::ReturnField => 3,
            Operator::NegateCondition => 4,
            Operator::CmpBoundary => 5,
            Operator::CmpEquality => 6,
            Operator::ArithNeighbour => 7,
            Operator::ConstAdjacent => 8,
            Operator::DeleteEffect => 9,
            Operator::ArmBody => 10,
            Operator::SwapArgs => 11,
            Operator::SkipEarlyReturn => 12,
            Operator::OtherVariant => 13,
        }
    }

    pub fn info(self) -> Info {
        match self {
            Operator::ReturnDefault => Info {
                why: "a function whose result no test checks is pseudo-tested: run, never asserted; the one mutant per function that finds it",
                yields: "highest per mutant: pseudo-tested methods were found in every one of 21 Java projects studied (Vera-Pérez et al., EMSE 2019, after Niedermayr et al., 2016); the share per project is not checked here",
                equivalent: "low: equivalent only when the default is the only value the function returns (estimate 2%)",
            },
            Operator::ReturnArg => Info {
                why: "a function that returns an argument unchanged passes tests that only call it with values it leaves alone",
                yields: "high, as extreme mutation (estimate)",
                equivalent: "higher than the default's: identity-like functions (estimate 5%)",
            },
            Operator::ReturnField => Info {
                why: "a test that asserts on part of a result (one field, `is_ok()`, a length) does not kill it: the gap LLM-written tests most often leave",
                yields: "high where results are structs (estimate); the failing test it asks for names the field",
                equivalent: "low: equivalent only when the field always holds its default (estimate 3%)",
            },
            Operator::NegateCondition => Info {
                why: "a branch only one side of which is tested",
                yields: "high: the classic decision mutation (estimate)",
                equivalent: "low, under 2% (estimate): a negated condition changes every path it decides",
            },
            Operator::CmpBoundary => Info {
                why: "off-by-one: no test sits on the boundary",
                yields: "high: relational-operator replacement is among the most productive at Google (Petrović et al., TSE 2021, reports per-operator productivity; numbers not restated here)",
                equivalent: "the most frequent source of equivalents (a boundary no input reaches); estimate 10% after the dead-value rule",
            },
            Operator::CmpEquality => Info {
                why: "a test that checks only one of equal and unequal inputs",
                yields: "high (estimate)",
                equivalent: "low (estimate 2%)",
            },
            Operator::ArithNeighbour => Info {
                why: "a computed value no test checks with operands where the operators differ",
                yields: "medium (estimate)",
                equivalent: "medium: `x + 0`, `x * 1` and masks that make two operators agree (estimate 8%); trivial compiler equivalence finds 7% of all C mutants and 5.7% of Java's equivalent (Kintis et al., TSE 2018)",
            },
            Operator::ConstAdjacent => Info {
                why: "a constant no test depends on: a limit, an offset, a seed",
                yields: "lower: many constants are tuning (estimate)",
                equivalent: "medium, after the absorbed-constant rule (estimate 10%)",
            },
            Operator::DeleteEffect => Info {
                why: "a write or a call with effects no test observes",
                yields: "high for state machines and stores (estimate)",
                equivalent: "low once pure calls with unused results and dead writes are ruled equivalent (estimate 4%)",
            },
            Operator::ArmBody => Info {
                why: "two arms no test tells apart",
                yields: "medium (estimate)",
                equivalent: "medium: arms that already do the same (estimate 8%)",
            },
            Operator::SwapArgs => Info {
                why: "arguments of one type passed in the wrong order, the bug a type cannot catch",
                yields: "medium (estimate)",
                equivalent: "medium: commutative callees (estimate 10%)",
            },
            Operator::SkipEarlyReturn => Info {
                why: "a guard no test takes",
                yields: "high for validation code (estimate)",
                equivalent: "low (estimate 3%)",
            },
            Operator::OtherVariant => Info {
                why: "a test that checks `is_err()` or `is_some()` and not which error or value",
                yields: "high where errors are typed (estimate)",
                equivalent: "low (estimate 2%)",
            },
        }
    }
}

/// One concrete rewrite of a site.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Mutation {
    ReturnDefault,
    ReturnArg { arg: u8 },
    ReturnField { field: u16 },
    NegateCondition,
    Compare { to: CmpOp },
    Arith { to: ArithOp },
    Const { to: i128 },
    DeleteEffect,
    ArmBody { arm: u16, from: u16 },
    SwapArgs { a: u8, b: u8 },
    SkipEarlyReturn,
    Variant { to: u16 },
}

impl Mutation {
    /// The operator it is of. A comparison's is decided by what it replaces.
    pub fn operator(&self, site: &Site) -> Operator {
        match self {
            Mutation::ReturnDefault => Operator::ReturnDefault,
            Mutation::ReturnArg { .. } => Operator::ReturnArg,
            Mutation::ReturnField { .. } => Operator::ReturnField,
            Mutation::NegateCondition => Operator::NegateCondition,
            Mutation::Compare { to } => match (&site.kind, to) {
                (SiteKind::Compare { op: CmpOp::Eq | CmpOp::Ne, .. }, _) => Operator::CmpEquality,
                _ => Operator::CmpBoundary,
            },
            Mutation::Arith { .. } => Operator::ArithNeighbour,
            Mutation::Const { .. } => Operator::ConstAdjacent,
            Mutation::DeleteEffect => Operator::DeleteEffect,
            Mutation::ArmBody { .. } => Operator::ArmBody,
            Mutation::SwapArgs { .. } => Operator::SwapArgs,
            Mutation::SkipEarlyReturn => Operator::SkipEarlyReturn,
            Mutation::Variant { .. } => Operator::OtherVariant,
        }
    }

    /// Its bytes in a key and a record: a tag, then its fields, fixed width, little-endian.
    pub fn encode(&self, out: &mut Vec<u8, Global>) {
        match *self {
            Mutation::ReturnDefault => out.push(1),
            Mutation::ReturnArg { arg } => out.extend_from_slice(&[2, arg]),
            Mutation::ReturnField { field } => {
                out.push(3);
                out.extend_from_slice(&field.to_le_bytes());
            }
            Mutation::NegateCondition => out.push(4),
            Mutation::Compare { to } => out.extend_from_slice(&[5, cmp_code(to)]),
            Mutation::Arith { to } => out.extend_from_slice(&[6, arith_code(to)]),
            Mutation::Const { to } => {
                out.push(7);
                out.extend_from_slice(&to.to_le_bytes());
            }
            Mutation::DeleteEffect => out.push(8),
            Mutation::ArmBody { arm, from } => {
                out.push(9);
                out.extend_from_slice(&arm.to_le_bytes());
                out.extend_from_slice(&from.to_le_bytes());
            }
            Mutation::SwapArgs { a, b } => out.extend_from_slice(&[10, a, b]),
            Mutation::SkipEarlyReturn => out.push(11),
            Mutation::Variant { to } => {
                out.push(12);
                out.extend_from_slice(&to.to_le_bytes());
            }
        }
    }

    /// The mutation `bytes` start with, and how many bytes it took; `None` for bytes no
    /// mutation encodes to.
    pub fn decode(bytes: &[u8]) -> Option<(Mutation, usize)> {
        let (&tag, rest) = bytes.split_first()?;
        let u16_at = |at: usize| -> Option<u16> {
            let raw = rest.get(at..at + 2)?;
            Some(u16::from_le_bytes([raw[0], raw[1]]))
        };
        Some(match tag {
            1 => (Mutation::ReturnDefault, 1),
            2 => (Mutation::ReturnArg { arg: *rest.first()? }, 2),
            3 => (Mutation::ReturnField { field: u16_at(0)? }, 3),
            4 => (Mutation::NegateCondition, 1),
            5 => (Mutation::Compare { to: cmp_of(*rest.first()?)? }, 2),
            6 => (Mutation::Arith { to: arith_of(*rest.first()?)? }, 2),
            7 => {
                let raw: [u8; 16] = rest.get(..16)?.try_into().ok()?;
                (Mutation::Const { to: i128::from_le_bytes(raw) }, 17)
            }
            8 => (Mutation::DeleteEffect, 1),
            9 => (Mutation::ArmBody { arm: u16_at(0)?, from: u16_at(2)? }, 5),
            10 => (Mutation::SwapArgs { a: *rest.first()?, b: *rest.get(1)? }, 3),
            11 => (Mutation::SkipEarlyReturn, 1),
            12 => (Mutation::Variant { to: u16_at(0)? }, 3),
            _ => return None,
        })
    }
}

fn cmp_code(op: CmpOp) -> u8 {
    match op {
        CmpOp::Lt => 0,
        CmpOp::Le => 1,
        CmpOp::Gt => 2,
        CmpOp::Ge => 3,
        CmpOp::Eq => 4,
        CmpOp::Ne => 5,
    }
}

fn cmp_of(code: u8) -> Option<CmpOp> {
    Some(match code {
        0 => CmpOp::Lt,
        1 => CmpOp::Le,
        2 => CmpOp::Gt,
        3 => CmpOp::Ge,
        4 => CmpOp::Eq,
        5 => CmpOp::Ne,
        _ => return None,
    })
}

fn arith_code(op: ArithOp) -> u8 {
    match op {
        ArithOp::Add => 0,
        ArithOp::Sub => 1,
        ArithOp::Mul => 2,
        ArithOp::Div => 3,
        ArithOp::Rem => 4,
        ArithOp::BitAnd => 5,
        ArithOp::BitOr => 6,
        ArithOp::BitXor => 7,
        ArithOp::Shl => 8,
        ArithOp::Shr => 9,
    }
}

fn arith_of(code: u8) -> Option<ArithOp> {
    Some(match code {
        0 => ArithOp::Add,
        1 => ArithOp::Sub,
        2 => ArithOp::Mul,
        3 => ArithOp::Div,
        4 => ArithOp::Rem,
        5 => ArithOp::BitAnd,
        6 => ArithOp::BitOr,
        7 => ArithOp::BitXor,
        8 => ArithOp::Shl,
        9 => ArithOp::Shr,
        _ => return None,
    })
}

/// The boundary partner of a relational comparison; `None` for `==` and `!=`.
pub fn boundary(op: CmpOp) -> Option<CmpOp> {
    match op {
        CmpOp::Lt => Some(CmpOp::Le),
        CmpOp::Le => Some(CmpOp::Lt),
        CmpOp::Gt => Some(CmpOp::Ge),
        CmpOp::Ge => Some(CmpOp::Gt),
        CmpOp::Eq | CmpOp::Ne => None,
    }
}

/// The neighbour an arithmetic operator is replaced by, on operands of `ty`; `None` when the
/// neighbour is not defined there. No neighbour introduces a division (a new divide-by-zero
/// check would be a second change): `*` goes to `+`, `/` to `*`, `%` to `/` (the same zero check).
pub fn neighbour(op: ArithOp, ty: TyClass) -> Option<ArithOp> {
    let to = match op {
        ArithOp::Add => ArithOp::Sub,
        ArithOp::Sub => ArithOp::Add,
        ArithOp::Mul => ArithOp::Add,
        ArithOp::Div => ArithOp::Mul,
        ArithOp::Rem => ArithOp::Div,
        ArithOp::BitAnd => ArithOp::BitOr,
        ArithOp::BitOr => ArithOp::BitAnd,
        ArithOp::BitXor => ArithOp::BitAnd,
        ArithOp::Shl => ArithOp::Shr,
        ArithOp::Shr => ArithOp::Shl,
    };
    (op.valid_on(ty) && to.valid_on(ty)).then_some(to)
}

/// The values of `ty`, as `i128`; `None` for a type with no integer values. An unsigned type of
/// more than 126 bits is cut at `i128::MAX`: its constants above that are not mutated.
pub fn range(ty: TyClass) -> Option<(i128, i128)> {
    match ty {
        TyClass::Bool => Some((0, 1)),
        TyClass::Int { signed: true, bits } => {
            let bits = u32::from(bits.clamp(1, 128));
            match bits {
                128 => Some((i128::MIN, i128::MAX)),
                _ => Some((-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)),
            }
        }
        TyClass::Int { signed: false, bits } => {
            let bits = u32::from(bits.clamp(1, 128));
            match bits >= 127 {
                true => Some((0, i128::MAX)),
                false => Some((0, (1i128 << bits) - 1)),
            }
        }
        TyClass::Float | TyClass::Char | TyClass::Other => None,
    }
}

/// The constants `value` of `ty` is replaced by, in order: zero, one more, one less; each in the
/// type's range, none equal to `value` or to another. A `bool` is flipped.
pub fn adjacent(value: i128, ty: TyClass, out: &mut Vec<i128, Global>) {
    out.clear();
    let Some((lo, hi)) = range(ty) else { return };
    if value < lo || value > hi {
        return;
    }
    if ty == TyClass::Bool {
        out.push(1 - value);
        return;
    }
    for candidate in [Some(0), value.checked_add(1), value.checked_sub(1)].into_iter().flatten() {
        if candidate != value && candidate >= lo && candidate <= hi && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
}

/// Every mutation `site` admits, in a fixed order. Each is well-typed by construction: it uses
/// only alternatives the site lists, or an operator defined on the site's type.
pub fn mutations(site: &Site, out: &mut Vec<Mutation, Global>) {
    out.clear();
    match &site.kind {
        SiteKind::Return { default, args, fields } => {
            if default.is_some() {
                out.push(Mutation::ReturnDefault);
            }
            for (arg, _) in args {
                out.push(Mutation::ReturnArg { arg: *arg });
            }
            for (field, _) in fields {
                out.push(Mutation::ReturnField { field: *field });
            }
        }
        SiteKind::Branch => out.push(Mutation::NegateCondition),
        SiteKind::Compare { op, .. } => match boundary(*op) {
            Some(to) => out.push(Mutation::Compare { to }),
            None => out.push(Mutation::Compare { to: if *op == CmpOp::Eq { CmpOp::Ne } else { CmpOp::Eq } }),
        },
        SiteKind::Arith { op, ty } => {
            if let Some(to) = neighbour(*op, *ty) {
                out.push(Mutation::Arith { to });
            }
        }
        SiteKind::Const { value, ty, .. } => {
            let mut values = Vec::new_in(Global);
            adjacent(*value, *ty, &mut values);
            for to in values {
                out.push(Mutation::Const { to });
            }
        }
        SiteKind::Effect(_) => out.push(Mutation::DeleteEffect),
        SiteKind::Arms { bodies, pairs } => {
            for &(arm, from) in pairs {
                if arm != from && usize::from(arm) < bodies.len() && usize::from(from) < bodies.len() {
                    out.push(Mutation::ArmBody { arm, from });
                }
            }
        }
        SiteKind::CallArgs { args, pairs } => {
            for &(a, b) in pairs {
                if a < b && usize::from(b) < args.len() {
                    out.push(Mutation::SwapArgs { a, b });
                }
            }
        }
        SiteKind::EarlyReturn => out.push(Mutation::SkipEarlyReturn),
        SiteKind::Construct { variant, alternatives } => {
            for (to, _) in alternatives {
                if to != variant {
                    out.push(Mutation::Variant { to: *to });
                }
            }
        }
    }
}

/// The text the mutation puts where `site.token` is.
pub fn replacement(site: &Site, mutation: &Mutation, out: &mut Vec<u8, Global>) {
    let token = token_text(site);
    match (&site.kind, mutation) {
        (SiteKind::Return { default: Some(default), .. }, Mutation::ReturnDefault) => out.extend_from_slice(default),
        (SiteKind::Return { args, .. }, Mutation::ReturnArg { arg }) => {
            if let Some((_, name)) = args.iter().find(|(at, _)| at == arg) {
                out.extend_from_slice(name);
            }
        }
        (SiteKind::Return { fields, .. }, Mutation::ReturnField { field }) => {
            out.extend_from_slice(token);
            out.extend_from_slice(b" with .");
            if let Some((_, name)) = fields.iter().find(|(at, _)| at == field) {
                out.extend_from_slice(name);
            }
            out.extend_from_slice(b" = Default::default()");
        }
        (_, Mutation::NegateCondition) => {
            out.extend_from_slice(b"!(");
            out.extend_from_slice(token);
            out.push(b')');
        }
        (_, Mutation::Compare { to }) => out.extend_from_slice(to.symbol()),
        (_, Mutation::Arith { to }) => out.extend_from_slice(to.symbol()),
        (SiteKind::Const { ty: TyClass::Bool, .. }, Mutation::Const { to }) => {
            out.extend_from_slice(if *to == 0 { &b"false"[..] } else { &b"true"[..] });
        }
        (_, Mutation::Const { to }) => decimal(*to, out),
        (_, Mutation::DeleteEffect) => {
            out.extend_from_slice(b"/* deleted: ");
            out.extend_from_slice(token);
            out.extend_from_slice(b" */");
        }
        (SiteKind::Arms { bodies, .. }, Mutation::ArmBody { from, .. }) => {
            if let Some(body) = bodies.get(usize::from(*from)) {
                out.extend_from_slice(body);
            }
        }
        (SiteKind::CallArgs { args, .. }, Mutation::SwapArgs { a, b }) => {
            for (at, arg) in args.iter().enumerate() {
                if at > 0 {
                    out.extend_from_slice(b", ");
                }
                let which = match at {
                    at if at == usize::from(*a) => usize::from(*b),
                    at if at == usize::from(*b) => usize::from(*a),
                    at => at,
                };
                out.extend_from_slice(args.get(which).unwrap_or(arg));
            }
        }
        (_, Mutation::SkipEarlyReturn) => {
            out.extend_from_slice(b"/* skipped: ");
            out.extend_from_slice(token);
            out.extend_from_slice(b" */");
        }
        (SiteKind::Construct { alternatives, .. }, Mutation::Variant { to }) => {
            if let Some((_, name)) = alternatives.iter().find(|(at, _)| at == to) {
                out.extend_from_slice(name);
            }
        }
        // A mutation of another site's kind: nothing [`mutations`] makes.
        _ => out.extend_from_slice(token),
    }
}

/// The site's line with the mutation in place of its token: what a diagnostic shows.
pub fn mutated_line(site: &Site, mutation: &Mutation, out: &mut Vec<u8, Global>) {
    let (start, end) = token_range(site);
    out.extend_from_slice(&site.text[..start]);
    replacement(site, mutation, out);
    out.extend_from_slice(&site.text[end..]);
}

/// The site's token as bytes of its text, clamped to the text.
pub fn token_text(site: &Site) -> &[u8] {
    let (start, end) = token_range(site);
    &site.text[start..end]
}

fn token_range(site: &Site) -> (usize, usize) {
    let len = site.text.len();
    let start = (site.token.0 as usize).min(len);
    let end = (site.token.1 as usize).clamp(start, len);
    (start, end)
}

/// `value` in decimal, into `out`.
pub fn decimal(value: i128, out: &mut Vec<u8, Global>) {
    if value < 0 {
        out.push(b'-');
    }
    let mut magnitude = value.unsigned_abs();
    let mut digits = [0u8; 40];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}
