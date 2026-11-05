//! Expression syntax tree, parser and evaluator.
//!
//! Expressions are stored in an arena so a [`ExprRef`] can be embedded in
//! fixups and symbol definitions without boxing or lifetimes. Evaluation
//! yields a [`Value`], which is *relocatable*: it may carry a symbol reference
//! that only becomes a number once addresses are assigned (or never, if it has
//! to be handed to the linker as a relocation).

use crate::cursor::Cursor;
use crate::diag::{DiagBag, Diagnostic};
use crate::intern::{Interner, Name};
use crate::lexer::{Dialect, LocalDir, Punct, TokKind};
use crate::source::Span;
use crate::symbol::{SymbolId, SymbolTable, SymbolValue};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ExprRef(u32);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UnOp {
    Neg,
    Not,
    LogicalNot,
    Plus,
    /// CC-RL and CC-RH `HIGH`, and `>` in the 8-bit dialect: bits 8 to 15.
    High,
    /// `LOW`, and `<` in the 8-bit dialect: bits 0 to 7.
    Low,
    /// ca65's `^`: bits 16 to 23, the bank byte of a 24-bit address.
    Bank,
    /// `HIGHW`: bits 16 to 31.
    HighW,
    /// `LOWW`: bits 0 to 15.
    LowW,
    /// CC-RH `HIGHW1`: bits 16 to 31 plus bit 15, the high half that pairs
    /// with a sign-extended `LOWW`.
    HighW1,
}

impl UnOp {
    fn symbol(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "~",
            UnOp::LogicalNot => "!",
            UnOp::Plus => "+",
            UnOp::High => "HIGH",
            UnOp::Low => "LOW",
            UnOp::Bank => "^",
            UnOp::HighW => "HIGHW",
            UnOp::LowW => "LOWW",
            UnOp::HighW1 => "HIGHW1",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    And,
    Or,
    Xor,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    LogicalAnd,
    LogicalOr,
    /// CC-RL's `>>`: a logical shift of the value's 32 bits.
    Shr32,
    /// CC-RH's `>>`: an arithmetic shift of the value's 32 bits.
    Sar32,
    /// NASM's `/`, which divides the 64-bit values as unsigned; its signed
    /// `//` is [`BinOp::Div`].
    DivU,
    /// NASM's `%`, the unsigned remainder; `%%` is [`BinOp::Rem`].
    RemU,
    /// NASM's `>>>`: an arithmetic shift of the 64-bit value.
    Sar,
    /// NASM's `^^`: 1 if exactly one side is non-zero.
    LogicalXor,
    /// NASM's `<=>`: -1, 0 or 1 as the left side is less than, equal to or
    /// greater than the right, signed.
    Compare,
    /// The `.` of a bit-addressing target: bit `b` of the byte at `a`, as
    /// the single 8-bit address the MCS-51 bit instructions take. `20H.3` is
    /// 03H and `P1.3` is 93H; see [`eval_bit_address`]. Only the backends
    /// that ask for it see this operator, and it splits a whole operand
    /// rather than binding by precedence, since it binds looser
    /// than anything else can.
    BitAddr,
}

impl BinOp {
    fn precedence(self, dialect: Dialect) -> u8 {
        use BinOp::*;
        match dialect {
            // CC-RL Table 5.5 (page 430) and CC-RH Table 5.4 (page 384). The
            // two agree except on whether `+` binds tighter than `&`.
            Dialect::CcRl | Dialect::CcRh => {
                let additive_first = dialect == Dialect::CcRl;
                match self {
                    LogicalOr | LogicalAnd => 1,
                    Eq | Ne | Lt | Gt | Le | Ge => 2,
                    And | Or | Xor if additive_first => 3,
                    Add | Sub if additive_first => 4,
                    Add | Sub => 3,
                    And | Or | Xor => 4,
                    Mul | Div | Rem | Shl | Shr | Shr32 | Sar32 => 5,
                    // NASM's operators, which no Renesas lexer produces.
                    DivU | RemU | Sar | LogicalXor | Compare => 5,
                    BitAddr => 6,
                }
            }
            // The NASM manual's §3.5, lowest first: comparisons bind looser
            // than the bitwise operators, unlike C.
            Dialect::Nasm => match self {
                LogicalOr => 1,
                LogicalXor => 2,
                LogicalAnd => 3,
                Eq | Ne | Lt | Gt | Le | Ge | Compare => 4,
                Or => 5,
                Xor => 6,
                And => 7,
                Shl | Shr | Sar | Shr32 | Sar32 => 8,
                Add | Sub => 9,
                Mul | Div | Rem | DivU | RemU => 10,
                BitAddr => 11,
            },
            // CC-RX Table 5.11 (R20UT3248EJ0115 page 462), which has no
            // logical operators and puts the shifts below `+`.
            Dialect::CcRx => match self {
                LogicalOr | LogicalAnd | LogicalXor | Eq | Ne | Lt | Gt | Le | Ge | Compare => 1,
                Or | Xor => 2,
                And => 3,
                Shl | Shr | Shr32 | Sar32 | Sar => 4,
                Add | Sub => 5,
                Mul | Div | Rem | DivU | RemU => 6,
                BitAddr => 7,
            },
            _ => match self {
                LogicalOr => 1,
                LogicalAnd | LogicalXor => 2,
                Or | Xor => 3,
                And => 4,
                Eq | Ne | Lt | Gt | Le | Ge | Compare => 5,
                Shl | Shr | Shr32 | Sar32 | Sar => 6,
                Add | Sub => 7,
                Mul | Div | Rem | DivU | RemU => 8,
                BitAddr => 9,
            },
        }
    }

    #[rustfmt::skip]
    fn symbol(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+", Sub => "-", Mul => "*", Div => "/", Rem => "%",
            Shl => "<<", Shr | Shr32 | Sar32 => ">>", And => "&", Or => "|", Xor => "^",
            Eq => "==", Ne => "!=", Lt => "<", Gt => ">", Le => "<=", Ge => ">=",
            LogicalAnd => "&&", LogicalOr => "||",
            DivU => "/", RemU => "%", Sar => ">>>", LogicalXor => "^^", Compare => "<=>",
            BitAddr => ".",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExprKind {
    Int(u64),
    /// A named symbol, resolved at evaluation time.
    Sym(Name),
    /// An already-resolved symbol. Positional references (`.`, `1f`) are
    /// rewritten into this once the statement they appear in is parsed.
    SymId(SymbolId),
    /// `1f` / `2b`: nearest numeric local label in the given direction.
    LocalRef(u32, LocalDir),
    /// The current location counter (`.` in GAS, `$` in NASM).
    Here,
    /// The start of the current section (`$$` in NASM).
    SectionStart,
    Unary(UnOp, ExprRef),
    Binary(BinOp, ExprRef, ExprRef),
    /// A relocation modifier such as `foo@PLT`, `foo@GOTPCREL` or `:lo12:foo`.
    /// The architecture interprets the name.
    Modifier(Name, ExprRef),
}

#[derive(Clone, Debug)]
pub struct ExprNode {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Default)]
pub struct ExprArena {
    pub(crate) nodes: Vec<ExprNode>,
}

impl ExprArena {
    pub fn new() -> ExprArena {
        ExprArena::default()
    }

    pub fn alloc(&mut self, kind: ExprKind, span: Span) -> ExprRef {
        let r = ExprRef(self.nodes.len() as u32);
        self.nodes.push(ExprNode { kind, span });
        r
    }

    pub fn get(&self, r: ExprRef) -> &ExprNode {
        &self.nodes[r.0 as usize]
    }

    /// Replaces what a node is, keeping where it was written.
    pub(crate) fn set_kind(&mut self, r: ExprRef, kind: ExprKind) {
        self.nodes[r.0 as usize].kind = kind;
    }

    pub fn span(&self, r: ExprRef) -> Span {
        self.nodes[r.0 as usize].span
    }

    /// Convenience for building a constant.
    pub fn int(&mut self, v: u64, span: Span) -> ExprRef {
        self.alloc(ExprKind::Int(v), span)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// The result of evaluating an expression.
///
/// `plus - minus + addend`. When both symbol slots are empty the value is a
/// plain constant; when only `plus` is set it is a relocatable address; when
/// both are set it is a difference, which collapses to a constant if the two
/// symbols end up in the same section.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Value {
    pub addend: i64,
    pub plus: Option<SymbolId>,
    pub minus: Option<SymbolId>,
}

impl Value {
    pub fn abs(v: i64) -> Value {
        Value {
            addend: v,
            plus: None,
            minus: None,
        }
    }

    pub fn sym(s: SymbolId, addend: i64) -> Value {
        Value {
            addend,
            plus: Some(s),
            minus: None,
        }
    }

    pub fn is_absolute(&self) -> bool {
        self.plus.is_none() && self.minus.is_none()
    }

    /// The constant value, if this needs no relocation.
    pub fn as_abs(&self) -> Option<i64> {
        self.is_absolute().then_some(self.addend)
    }
}

/// What an expression evaluator needs from the assembler.
pub trait EvalCtx {
    /// Resolves a name to a value, interning the symbol if it is new.
    fn lookup_symbol(&mut self, name: Name, span: Span) -> Result<Value, EvalError>;
    /// Resolves an already-identified symbol.
    fn symbol_value(&mut self, id: SymbolId, span: Span) -> Result<Value, EvalError>;
    /// The current location counter.
    fn here(&mut self, span: Span) -> Result<Value, EvalError>;
    /// The start of the current section.
    fn section_start(&mut self, span: Span) -> Result<Value, EvalError>;
    /// Resolves `1f` / `2b`.
    fn local_ref(&mut self, n: u32, dir: LocalDir, span: Span) -> Result<Value, EvalError>;
    /// Handles `expr@MODIFIER`. Most contexts reject these; fixup lowering
    /// peels them off before evaluating.
    fn modifier(&mut self, name: Name, inner: Value, span: Span) -> Result<Value, EvalError>;
}

#[derive(Clone, Debug)]
pub struct EvalError {
    pub span: Span,
    pub msg: String,
}

impl EvalError {
    pub fn new(span: Span, msg: impl Into<String>) -> EvalError {
        EvalError {
            span,
            msg: msg.into(),
        }
    }

    pub fn into_diagnostic(self) -> Diagnostic {
        Diagnostic::error(self.span, self.msg)
    }
}

pub fn eval(arena: &ExprArena, r: ExprRef, cx: &mut dyn EvalCtx) -> Result<Value, EvalError> {
    let node = arena.get(r);
    let span = node.span;
    match &node.kind {
        ExprKind::Int(v) => Ok(Value::abs(*v as i64)),
        ExprKind::Sym(n) => cx.lookup_symbol(*n, span),
        ExprKind::SymId(id) => cx.symbol_value(*id, span),
        ExprKind::LocalRef(n, dir) => cx.local_ref(*n, *dir, span),
        ExprKind::Here => cx.here(span),
        ExprKind::SectionStart => cx.section_start(span),
        ExprKind::Modifier(name, inner) => {
            let v = eval(arena, *inner, cx)?;
            cx.modifier(*name, v, span)
        }
        ExprKind::Unary(op, inner) => {
            let v = eval(arena, *inner, cx)?;
            let Some(a) = v.as_abs() else {
                if *op == UnOp::Plus {
                    return Ok(v);
                }
                return Err(EvalError::new(
                    span,
                    format!("operand of `{}` must be an absolute value", op.symbol()),
                ));
            };
            Ok(Value::abs(match op {
                UnOp::Neg => a.wrapping_neg(),
                UnOp::Not => !a,
                UnOp::LogicalNot => (a == 0) as i64,
                UnOp::Plus => a,
                UnOp::High => (a >> 8) & 0xff,
                UnOp::Low => a & 0xff,
                UnOp::Bank => (a >> 16) & 0xff,
                UnOp::HighW => (a >> 16) & 0xffff,
                UnOp::LowW => a & 0xffff,
                // Wraps to 0 when the high half is 0xffff and bit 15 is set
                // (CC-RH §5.1.8, page 416).
                UnOp::HighW1 => (((a >> 16) & 0xffff) + ((a >> 15) & 1)) & 0xffff,
            }))
        }
        ExprKind::Binary(op, l, r2) => {
            let lv = eval(arena, *l, cx)?;
            let rv = eval(arena, *r2, cx)?;
            eval_binary(*op, lv, rv, span)
        }
    }
}

pub(crate) fn eval_binary(op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, EvalError> {
    use BinOp::*;

    // Addition and subtraction are the only operators that may keep a symbol.
    match op {
        Add => {
            return match (l.plus, l.minus, r.plus, r.minus) {
                // Cancel `a - b` against `+ b`.
                (lp, Some(lm), Some(rp), None) if lm == rp => Ok(Value {
                    addend: l.addend.wrapping_add(r.addend),
                    plus: lp,
                    minus: None,
                }),
                (Some(lp), None, _, Some(rm)) if lp == rm => Ok(Value {
                    addend: l.addend.wrapping_add(r.addend),
                    plus: r.plus,
                    minus: None,
                }),
                (lp, lm, rp, rm) => {
                    let (plus, minus) = match (lp, rp) {
                        (Some(_), Some(_)) => {
                            return Err(EvalError::new(span, "cannot add two relocatable symbols"));
                        }
                        (a, b) => (
                            a.or(b),
                            match (lm, rm) {
                                (Some(_), Some(_)) => {
                                    return Err(EvalError::new(
                                        span,
                                        "cannot subtract two relocatable symbols here",
                                    ));
                                }
                                (a, b) => a.or(b),
                            },
                        ),
                    };
                    Ok(Value {
                        addend: l.addend.wrapping_add(r.addend),
                        plus,
                        minus,
                    })
                }
            };
        }
        Sub => {
            return match (l.plus, l.minus, r.plus, r.minus) {
                // sym - sym: a difference, resolvable if both land in the
                // same section.
                (Some(lp), None, Some(rp), None) => {
                    if lp == rp {
                        Ok(Value::abs(l.addend.wrapping_sub(r.addend)))
                    } else {
                        Ok(Value {
                            addend: l.addend.wrapping_sub(r.addend),
                            plus: Some(lp),
                            minus: Some(rp),
                        })
                    }
                }
                (lp, lm, None, None) => Ok(Value {
                    addend: l.addend.wrapping_sub(r.addend),
                    plus: lp,
                    minus: lm,
                }),
                _ => Err(EvalError::new(
                    span,
                    "unsupported combination of relocatable values in `-`",
                )),
            };
        }
        _ => {}
    }

    let (Some(a), Some(b)) = (l.as_abs(), r.as_abs()) else {
        return Err(EvalError::new(
            span,
            format!("operands of `{}` must be absolute values", op.symbol()),
        ));
    };

    let v = match op {
        Add | Sub => unreachable!("handled above"),
        Mul => a.wrapping_mul(b),
        Div => {
            if b == 0 {
                return Err(EvalError::new(span, "division by zero"));
            }
            a.wrapping_div(b)
        }
        Rem => {
            if b == 0 {
                return Err(EvalError::new(span, "remainder by zero"));
            }
            a.wrapping_rem(b)
        }
        // Shift counts of 64 or more produce 0, matching GAS rather than
        // panicking or wrapping the count around.
        Shl => {
            if (b as u64) >= 64 {
                0
            } else {
                ((a as u64) << b) as i64
            }
        }
        Shr => {
            if (b as u64) >= 64 {
                0
            } else {
                ((a as u64) >> b) as i64
            }
        }
        // Both Renesas assemblers evaluate in 32 bits and give 0 for a count
        // over 31 (CC-RL §5.1.8, page 456; CC-RH §5.1.6, page 408). CC-RH
        // shifts the sign bit in, which as a 64-bit value is the sign-extended
        // result.
        Shr32 => {
            if (b as u64) > 31 {
                0
            } else {
                ((a as u32) >> b) as i64
            }
        }
        Sar32 => {
            if (b as u64) > 31 {
                0
            } else {
                ((a as i32) >> b) as i64
            }
        }
        DivU => {
            if b == 0 {
                return Err(EvalError::new(span, "division by zero"));
            }
            ((a as u64) / (b as u64)) as i64
        }
        RemU => {
            if b == 0 {
                return Err(EvalError::new(span, "remainder by zero"));
            }
            ((a as u64) % (b as u64)) as i64
        }
        Sar => a >> (b as u64).min(63),
        LogicalXor => ((a != 0) != (b != 0)) as i64,
        // NASM 2.16.03 computes -1 for "less" and then takes -1 as its
        // marker for an unknown value, which ends up as 0; `3 <=> 5` is 0
        // there, and so it is here.
        Compare => (a.cmp(&b) as i64).max(0),
        And => a & b,
        Or => a | b,
        Xor => a ^ b,
        Eq => (a == b) as i64,
        Ne => (a != b) as i64,
        Lt => (a < b) as i64,
        Gt => (a > b) as i64,
        Le => (a <= b) as i64,
        Ge => (a >= b) as i64,
        LogicalAnd => (a != 0 && b != 0) as i64,
        LogicalOr => (a != 0 || b != 0) as i64,
        BitAddr => eval_bit_address(a, b, span)?,
    };
    Ok(Value::abs(v))
}

/// The MCS-51 bit address of bit `bit` of the byte at `base`.
///
/// The machine has two bit-addressable areas and numbers their bits in one
/// 8-bit space: the 16 bytes of internal RAM at 20H to 2FH hold bits 00H to
/// 7FH, and every SFR whose address is a multiple of 8 holds the eight bits
/// at its own address. So `20H.3` is 03H and `P1.3`, P1 being 90H, is 93H.
/// This is what the Macro Assembler AS computes for `A.B` on a byte that has
/// bit addresses. A byte that has none is refused here, where AS warns about
/// 40H to 7FH and 81H to FFH off a multiple of 8, and assembles a bit of some
/// other byte, and says nothing about 30H to 3FH, whose "bits" it numbers 80H
/// to FFH, over the SFRs' own.
pub fn eval_bit_address(base: i64, bit: i64, span: Span) -> Result<i64, EvalError> {
    if !(0..=7).contains(&bit) {
        return Err(EvalError::new(
            span,
            format!("a bit number must be 0 to 7, not {bit}"),
        ));
    }
    match base {
        0x20..=0x2f => Ok((base - 0x20) * 8 + bit),
        0x80..=0xff if base % 8 == 0 => Ok(base + bit),
        _ => Err(EvalError::new(
            span,
            format!(
                "{base:#x} is not bit addressable: only 20H to 2FH and the special \
                 function registers at a multiple of 8 are"
            ),
        )),
    }
}

/// Evaluates against a finished symbol table, without recording uses.
///
/// This is the read-only counterpart of the assembler's own evaluator: it
/// resolves `.set` chains, which is what lets an immediate written as a named
/// constant still pick the shortest encoding.
pub struct SymbolEnv<'a> {
    pub exprs: &'a ExprArena,
    pub symbols: &'a SymbolTable,
    depth: u32,
}

impl<'a> SymbolEnv<'a> {
    pub fn new(exprs: &'a ExprArena, symbols: &'a SymbolTable) -> SymbolEnv<'a> {
        SymbolEnv {
            exprs,
            symbols,
            depth: 0,
        }
    }

    /// Evaluates `e`, or returns `None` if anything in it is still unknown.
    pub fn value(&mut self, e: ExprRef) -> Option<Value> {
        let exprs = self.exprs;
        eval(exprs, e, self).ok()
    }

    /// Evaluates `e` to a plain number, or `None` if it is not one yet.
    pub fn constant(&mut self, e: ExprRef) -> Option<i64> {
        self.value(e)?.as_abs()
    }
}

impl EvalCtx for SymbolEnv<'_> {
    fn lookup_symbol(&mut self, name: Name, span: Span) -> Result<Value, EvalError> {
        match self.symbols.lookup(name) {
            Some(id) => self.symbol_value(id, span),
            None => Err(EvalError::new(span, "undefined symbol")),
        }
    }

    fn symbol_value(&mut self, id: SymbolId, span: Span) -> Result<Value, EvalError> {
        match self.symbols.get(id).value {
            SymbolValue::Expr(e) => {
                if self.depth > 64 {
                    return Err(EvalError::new(span, "symbol definition is circular"));
                }
                self.depth += 1;
                let exprs = self.exprs;
                let v = eval(exprs, e, self);
                self.depth -= 1;
                v
            }
            _ => Ok(Value::sym(id, 0)),
        }
    }

    fn here(&mut self, span: Span) -> Result<Value, EvalError> {
        Err(EvalError::new(span, "`.` cannot be used here"))
    }

    fn section_start(&mut self, span: Span) -> Result<Value, EvalError> {
        Err(EvalError::new(span, "`$$` is not supported yet"))
    }

    fn local_ref(&mut self, n: u32, _: LocalDir, span: Span) -> Result<Value, EvalError> {
        Err(EvalError::new(
            span,
            format!("local label `{n}` is not resolved yet"),
        ))
    }

    fn modifier(&mut self, _name: Name, inner: Value, _span: Span) -> Result<Value, EvalError> {
        Ok(inner)
    }
}

/// Evaluates an expression that must not mention any symbol.
///
/// Used where a width has to be chosen before addresses are known: an
/// immediate or displacement that folds to a constant can pick the shortest
/// encoding, while anything symbolic falls back to the widest one.
#[allow(dead_code)]
pub fn const_fold(arena: &ExprArena, r: ExprRef) -> Option<i64> {
    struct NoSymbols;
    impl EvalCtx for NoSymbols {
        fn lookup_symbol(&mut self, _: Name, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "not a constant"))
        }
        fn symbol_value(&mut self, _: SymbolId, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "not a constant"))
        }
        fn here(&mut self, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "not a constant"))
        }
        fn section_start(&mut self, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "not a constant"))
        }
        fn local_ref(&mut self, _: u32, _: LocalDir, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "not a constant"))
        }
        fn modifier(&mut self, _: Name, _: Value, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "not a constant"))
        }
    }
    eval(arena, r, &mut NoSymbols).ok().and_then(|v| v.as_abs())
}

/// Rewrites every `Here` and `LocalRef` node added since `from` into a plain
/// symbol reference.
///
/// Both depend on *where in the file* they appear, so they are bound as soon
/// as the statement containing them is parsed rather than at evaluation time,
/// when that position is long gone.
pub fn bind_positional(
    arena: &mut ExprArena,
    from: usize,
    mut resolve: impl FnMut(&ExprKind, Span) -> Option<ExprKind>,
) {
    for i in from..arena.nodes.len() {
        let span = arena.nodes[i].span;
        if !matches!(
            arena.nodes[i].kind,
            ExprKind::Here | ExprKind::LocalRef(..) | ExprKind::SectionStart
        ) {
            continue;
        }
        if let Some(new) = resolve(&arena.nodes[i].kind, span) {
            arena.nodes[i].kind = new;
        }
    }
}

/// Parses expressions out of a statement's token stream.
pub struct ExprParser<'a> {
    pub arena: &'a mut ExprArena,
    pub interner: &'a mut Interner,
    pub diags: &'a mut DiagBag,
    /// In NASM, `$` is the location counter; in GAS it introduces an immediate
    /// and must not be consumed here.
    pub dollar_is_here: bool,
    /// In Motorola source `*` in operand position is the location counter.
    pub star_is_here: bool,
    /// Decides operator precedence and the dialect's own operators, such as
    /// CC-RL's `HIGH` and `LOWW`.
    pub dialect: Dialect,
    /// Whether `A.B` after a term is a bit address, as it is on the MCS-51;
    /// see [`BinOp::BitAddr`]. Only a backend that has bit addressing asks
    /// for it, so `.` keeps its usual meaning everywhere else.
    pub bit_dot: bool,
    /// The string literals, where a quoted string can stand for a number: in
    /// NASM, `'ab'` is `0x6261`.
    pub strings: Option<&'a crate::lexer::LitPool>,
    /// Relocation modifiers written as a suffix in parentheses after the term
    /// they apply to, `sym(GOT)`, which is how ARM's GNU as spells what the
    /// rest of the GNU syntax spells `sym@GOT`. Empty everywhere the caller
    /// does not allow one, so that a `(` after a symbol stays whatever else
    /// it was; see [`ExprParser::paren_modifier`].
    pub paren_modifiers: &'static [&'static str],
    /// Whether an `@` modifier is spelled in upper case and nothing else; see
    /// [`crate::arch::Architecture::uppercase_modifiers`].
    pub upper_modifiers: bool,
}

impl<'a> ExprParser<'a> {
    pub fn parse(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let mut e = self.parse_bp(cur, 0)?;
        // On a bit-addressing target a `.` splits the whole operand into a
        // byte and a bit number, as AS splits an operand at its last `.`: so
        // `20H+1.3` is bit 3 of 21H, and `P1.1+2` bit 3 of P1.
        if self.bit_dot && cur.check_punct(Punct::Dot) {
            cur.advance();
            let bit = self.parse_bp(cur, 0)?;
            let span = self.arena.span(e).to(self.arena.span(bit));
            e = self
                .arena
                .alloc(ExprKind::Binary(BinOp::BitAddr, e, bit), span);
        }
        if self.dialect == Dialect::Nasm {
            return self.nasm_wrt(cur, e);
        }
        Some(e)
    }

    /// NASM's `expr wrt ..plt`, which applies to the whole expression before
    /// it, and names the relocation the way `@PLT` does in GNU syntax: it
    /// becomes the same [`ExprKind::Modifier`], named without the dots. The
    /// segment form, `wrt seg`, only means something to 16-bit object formats
    /// rsasm does not write, and is refused.
    fn nasm_wrt(&mut self, cur: &mut Cursor<'_>, e: ExprRef) -> Option<ExprRef> {
        let tok = cur.peek();
        let Some(n) = tok.ident() else {
            return Some(e);
        };
        if !self.interner.get(n).eq_ignore_ascii_case("wrt") {
            return Some(e);
        }
        cur.advance();
        let target = cur.peek();
        let name = target
            .ident()
            .map(|t| self.interner.get(t).to_ascii_lowercase())
            .and_then(|t| t.strip_prefix("..").map(str::to_string));
        let Some(name) = name else {
            self.diags.error(
                tok.span.to(target.span),
                "expected a special symbol such as `..plt` or `..got` after `wrt`",
            );
            return None;
        };
        cur.advance();
        let span = self.arena.span(e).to(target.span);
        let name = self.interner.intern(&name);
        Some(self.arena.alloc(ExprKind::Modifier(name, e), span))
    }

    fn parse_bp(&mut self, cur: &mut Cursor<'_>, min_prec: u8) -> Option<ExprRef> {
        let mut lhs = self.parse_prefix(cur)?;
        // A modifier binds to the term it follows, so `foo@GOT+4` is the
        // symbol's GOT entry plus four.
        lhs = self.parse_postfix(cur, lhs);
        while let Some(op) = peek_binop(cur, self.dialect) {
            let prec = op.precedence(self.dialect);
            if prec < min_prec {
                break;
            }
            cur.advance();
            // All operators here are left-associative.
            let rhs = self.parse_bp(cur, prec + 1)?;
            let span = self.arena.span(lhs).to(self.arena.span(rhs));
            lhs = self.arena.alloc(ExprKind::Binary(op, lhs, rhs), span);
            // GNU as, and llvm-mc with it, make a true comparison -1 rather
            // than 1 (`!`, `&&` and `||` still give 1); NASM, the Renesas
            // assemblers and the 8-bit ones give 1.
            if self.dialect == Dialect::Gas
                && matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge
                )
            {
                lhs = self.arena.alloc(ExprKind::Unary(UnOp::Neg, lhs), span);
            }
        }
        Some(self.parse_postfix(cur, lhs))
    }

    /// Handles `expr@MODIFIER`, the ELF relocation-modifier syntax, and the
    /// `expr(MODIFIER)` ARM writes instead.
    fn parse_postfix(&mut self, cur: &mut Cursor<'_>, mut e: ExprRef) -> ExprRef {
        if let Some(wrapped) = self.paren_modifier(cur, e) {
            e = wrapped;
        }
        while cur.check_punct(Punct::At) {
            let at = cur.advance();
            let tok = cur.peek();
            let name = match tok.kind {
                TokKind::Ident(n) => {
                    cur.advance();
                    let text = self.interner_get(n);
                    // Where the target spells its modifiers in upper case,
                    // any other spelling is not one; see
                    // `Architecture::uppercase_modifiers`.
                    if self.upper_modifiers && text != text.to_ascii_uppercase() {
                        self.diags.error(
                            at.span.to(tok.span),
                            format!(
                                "`@{text}` is not a relocation modifier; this target \
                                 spells them in upper case"
                            ),
                        );
                        break;
                    }
                    self.interner.intern_lower(&text)
                }
                _ => {
                    self.diags
                        .error(at.span.to(tok.span), "expected a relocation name after `@`");
                    break;
                }
            };
            let span = self.arena.span(e).to(tok.span);
            e = self.arena.alloc(ExprKind::Modifier(name, e), span);
        }
        e
    }

    /// Handles `sym(GOT)`, the relocation suffix ARM's GNU as reads in place
    /// of `sym@GOT`; see [`ExprParser::paren_modifiers`]. `None` leaves the
    /// cursor alone, which is what a `(` that starts something else needs.
    ///
    /// GNU as takes the suffix only after a symbol, and only spelled wholly
    /// in lower or wholly in upper case, since its table holds the two
    /// spellings and nothing else. A `(name)` that is neither is the error
    /// `s_arm_elf_cons` gives rather than a symbol followed by a group.
    fn paren_modifier(&mut self, cur: &mut Cursor<'_>, e: ExprRef) -> Option<ExprRef> {
        if self.paren_modifiers.is_empty()
            || !matches!(self.arena.get(e).kind, ExprKind::Sym(_))
            || !cur.peek().is_punct(Punct::LParen)
        {
            return None;
        }
        let name = cur.nth(1).ident()?;
        if !cur.nth(2).is_punct(Punct::RParen) {
            return None;
        }
        let span = cur.peek().span.to(cur.nth(2).span);
        let text = self.interner.get(name).to_string();
        let found = self
            .paren_modifiers
            .iter()
            .find(|m| text == **m || text == m.to_ascii_uppercase());
        cur.advance();
        cur.advance();
        cur.advance();
        let Some(found) = found else {
            self.diags
                .error(span, format!("unrecognized relocation suffix `({text})`"));
            return None;
        };
        let found = self.interner.intern(found);
        Some(
            self.arena
                .alloc(ExprKind::Modifier(found, e), self.arena.span(e).to(span)),
        )
    }

    fn interner_get(&self, n: Name) -> String {
        self.interner.get(n).to_string()
    }

    /// The CC-RL/CC-RH operator a word names, when it is followed by a term:
    /// the byte and word separators (CC-RL §5.1.9-5.1.10, pages 458-466;
    /// CC-RH §5.1.7-5.1.8, pages 410-416), which are reserved words and
    /// apply to the one term after them, like any unary operator.
    ///
    /// The operators that only the Renesas linker can evaluate — `STARTOF`,
    /// `SIZEOF`, the mirror-area `MIRHW`/`MIRLW`/`SMRLW`, and the bit-symbol
    /// `DATAPOS`/`BITPOS` — are refused here with the reason, rather than
    /// being read as a call to a symbol of that name. Returns `None` after
    /// reporting one of those. CC-RX has no separators, but its `SIZEOF` and
    /// `TOPOF` (R20UT3248EJ0115 Table 5.7, page 461) are the linker's too.
    fn renesas_operator(&mut self, n: Name, span: Span) -> Option<Option<UnOp>> {
        let word = self.interner.get(n).to_ascii_uppercase();
        let cc = self.dialect.is_cc();
        let op = match word.as_str() {
            "HIGH" if cc => UnOp::High,
            "LOW" if cc => UnOp::Low,
            "HIGHW" if cc => UnOp::HighW,
            "LOWW" if cc => UnOp::LowW,
            "HIGHW1" if self.dialect == Dialect::CcRh => UnOp::HighW1,
            "STARTOF" | "SIZEOF" | "TOPOF" => {
                self.diags.error(
                    span,
                    format!(
                        "`{word}` is evaluated by the Renesas optimizing linker, and no ELF \
                         relocation can carry it; use a linker-defined symbol instead"
                    ),
                );
                return None;
            }
            "MIRHW" | "MIRLW" | "SMRLW" if self.dialect == Dialect::CcRl => {
                self.diags.error(
                    span,
                    format!(
                        "`{word}` needs the device's mirror area, which only the Renesas \
                         toolchain knows; it is not supported"
                    ),
                );
                return None;
            }
            "DATAPOS" | "BITPOS" if self.dialect == Dialect::CcRl => {
                self.diags.error(
                    span,
                    format!("`{word}` takes a bit symbol, and bit symbols are not supported"),
                );
                return None;
            }
            _ => return Some(None),
        };
        Some(Some(op))
    }

    fn parse_prefix(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let tok = cur.peek();
        let unop = match tok.kind {
            TokKind::Punct(Punct::Minus) => Some(UnOp::Neg),
            TokKind::Punct(Punct::Tilde) => Some(UnOp::Not),
            // CC-RH's `!` is the bitwise NOT (§5.1.4, page 394).
            TokKind::Punct(Punct::Bang) if self.dialect == Dialect::CcRh => Some(UnOp::Not),
            TokKind::Punct(Punct::Bang) => Some(UnOp::LogicalNot),
            TokKind::Punct(Punct::Plus) => Some(UnOp::Plus),
            // ca65's byte selectors, which vasm and AS read too. In operand
            // position they cannot be comparisons.
            TokKind::Punct(Punct::Lt) if self.dialect == Dialect::EightBit => Some(UnOp::Low),
            TokKind::Punct(Punct::Gt) if self.dialect == Dialect::EightBit => Some(UnOp::High),
            TokKind::Punct(Punct::Caret) if self.dialect == Dialect::EightBit => Some(UnOp::Bank),
            TokKind::Ident(n)
                if (self.dialect.is_cc() || self.dialect == Dialect::CcRx)
                    && starts_term(cur.nth(1).kind) =>
            {
                self.renesas_operator(n, tok.span)?
            }
            // CC-RX's `?+` and `?-`, the temporary labels after and before
            // (R20UT3248EJ0115 page 497).
            TokKind::Punct(Punct::Question)
                if self.dialect == Dialect::CcRx
                    && (cur.nth(1).is_punct(Punct::Plus) || cur.nth(1).is_punct(Punct::Minus)) =>
            {
                cur.advance();
                let sign = cur.advance();
                let dir = if sign.is_punct(Punct::Plus) {
                    LocalDir::Forward
                } else {
                    LocalDir::Backward
                };
                let kind = ExprKind::LocalRef(crate::parser::CCRX_TEMPORARY_LABEL, dir);
                return Some(self.arena.alloc(kind, tok.span.to(sign.span)));
            }
            _ => None,
        };
        if let Some(op) = unop {
            cur.advance();
            // Unary binds tighter than every binary operator.
            let inner = self.parse_prefix(cur)?;
            let inner = self.parse_postfix(cur, inner);
            let span = tok.span.to(self.arena.span(inner));
            return Some(self.arena.alloc(ExprKind::Unary(op, inner), span));
        }

        match tok.kind {
            TokKind::Int(v) => {
                cur.advance();
                Some(self.arena.alloc(ExprKind::Int(v), tok.span))
            }
            // A NASM string used as a number packs its first eight bytes
            // little-endian, so `'ab'` is 0x6261.
            TokKind::Str(i) if self.dialect == Dialect::Nasm && self.strings.is_some() => {
                cur.advance();
                let bytes = self.strings.map_or(&[][..], |p| p.get(i));
                let v = bytes
                    .iter()
                    .take(8)
                    .rev()
                    .fold(0u64, |v, &b| (v << 8) | b as u64);
                Some(self.arena.alloc(ExprKind::Int(v), tok.span))
            }
            // The lexer leaves malformed numbers unreported because only the
            // consumer can tell whether they are errors. In an expression they
            // are.
            TokKind::BadNumber(text) => {
                cur.advance();
                let msg = crate::lexer::explain_bad_number(self.interner.get(text));
                self.diags.error(tok.span, msg);
                None
            }
            TokKind::Ident(n) if self.dialect == Dialect::CcRx => {
                cur.advance();
                let word = self.interner.get(n);
                // A macro expansion has already put the count in place of
                // these; anywhere else they are 0 (R20UT3248EJ0115 page
                // 490).
                if word.eq_ignore_ascii_case("..macpara") || word.eq_ignore_ascii_case("..macrep") {
                    return Some(self.arena.alloc(ExprKind::Int(0), tok.span));
                }
                if [".len", ".instr", ".substr"]
                    .iter()
                    .any(|f| word.eq_ignore_ascii_case(f))
                {
                    self.diags.error(
                        tok.span,
                        format!(
                            "the string function `{}` is not supported",
                            word.to_ascii_uppercase()
                        ),
                    );
                    return None;
                }
                Some(self.arena.alloc(ExprKind::Sym(n), tok.span))
            }
            // ca65's function spellings of the byte selectors.
            TokKind::Ident(n)
                if self.dialect == Dialect::EightBit && cur.nth(1).is_punct(Punct::LParen) =>
            {
                let op = match self.interner.get(n).to_ascii_lowercase().as_str() {
                    ".lobyte" => Some(UnOp::Low),
                    ".hibyte" => Some(UnOp::High),
                    ".bankbyte" => Some(UnOp::Bank),
                    ".loword" => Some(UnOp::LowW),
                    _ => None,
                };
                cur.advance();
                let Some(op) = op else {
                    return Some(self.arena.alloc(ExprKind::Sym(n), tok.span));
                };
                let inner = self.parse_prefix(cur)?;
                let span = tok.span.to(self.arena.span(inner));
                Some(self.arena.alloc(ExprKind::Unary(op, inner), span))
            }
            TokKind::Ident(n) => {
                cur.advance();
                // Where `@` is part of a name, GNU syntax still means a
                // relocation modifier when what follows the last `@` names
                // one: `foo@IMGREL`, but not `__xmm@0f0e0d0c`.
                if self.dialect == Dialect::Gas
                    && let Some((base, modifier)) = split_modifier(self.interner.get(n))
                {
                    let base = self.interner.intern(&base);
                    let modifier = self.interner.intern(&modifier);
                    let sym = self.arena.alloc(ExprKind::Sym(base), tok.span);
                    return Some(
                        self.arena
                            .alloc(ExprKind::Modifier(modifier, sym), tok.span),
                    );
                }
                Some(self.arena.alloc(ExprKind::Sym(n), tok.span))
            }
            TokKind::LocalRef(n, dir) => {
                cur.advance();
                Some(self.arena.alloc(ExprKind::LocalRef(n, dir), tok.span))
            }
            TokKind::Punct(Punct::Dot) => {
                cur.advance();
                Some(self.arena.alloc(ExprKind::Here, tok.span))
            }
            // Only reachable in operand position: a `*` between two operands
            // was already taken as multiplication by the binary-operator loop.
            TokKind::Punct(Punct::Star) if self.star_is_here => {
                cur.advance();
                Some(self.arena.alloc(ExprKind::Here, tok.span))
            }
            TokKind::Punct(Punct::Dollar) if self.dollar_is_here => {
                cur.advance();
                if cur.check_punct(Punct::Dollar) {
                    let t2 = cur.advance();
                    return Some(
                        self.arena
                            .alloc(ExprKind::SectionStart, tok.span.to(t2.span)),
                    );
                }
                Some(self.arena.alloc(ExprKind::Here, tok.span))
            }
            TokKind::Punct(Punct::LParen) => {
                cur.advance();
                let inner = self.parse_bp(cur, 0)?;
                if cur.eat_punct(Punct::RParen).is_none() {
                    self.diags.emit(
                        Diagnostic::error(cur.peek().span, "expected `)`")
                            .with_note(tok.span, "to match this `(`"),
                    );
                    return None;
                }
                Some(inner)
            }
            _ => {
                let what = describe(cur, tok.kind);
                self.diags
                    .error(tok.span, format!("expected an expression, found {what}"));
                None
            }
        }
    }
}

fn describe(_cur: &Cursor<'_>, k: TokKind) -> String {
    match k {
        TokKind::Eof | TokKind::Eol => "end of statement".into(),
        TokKind::Punct(p) => format!("`{}`", p.as_str()),
        TokKind::Str(_) => "a string literal".into(),
        TokKind::BadNumber(_) => "a malformed number".into(),
        _ => "this token".into(),
    }
}

/// Whether a token can begin a term, which is what makes a CC-RL `LOWW` an
/// operator rather than a stray word.
fn starts_term(k: TokKind) -> bool {
    matches!(
        k,
        TokKind::Int(_)
            | TokKind::Ident(_)
            | TokKind::BadNumber(_)
            | TokKind::Punct(
                Punct::LParen | Punct::Minus | Punct::Plus | Punct::Tilde | Punct::Bang
            )
    )
}

fn peek_binop(cur: &Cursor<'_>, dialect: Dialect) -> Option<BinOp> {
    use BinOp::*;
    let TokKind::Punct(p) = cur.peek().kind else {
        return None;
    };
    let nasm = dialect == Dialect::Nasm;
    Some(match p {
        Punct::Plus => Add,
        Punct::Minus => Sub,
        Punct::Star => Mul,
        Punct::Slash if nasm => DivU,
        Punct::Percent if nasm => RemU,
        Punct::SlashSlash => Div,
        Punct::PercentPercent => Rem,
        Punct::Sar => Sar,
        Punct::CaretCaret => LogicalXor,
        Punct::Spaceship => Compare,
        // NASM spells equality `=` as well as `==`.
        Punct::Eq if nasm => Eq,
        Punct::Slash => Div,
        Punct::Percent => Rem,
        Punct::Shl => Shl,
        Punct::Shr if dialect == Dialect::CcRl => Shr32,
        Punct::Shr if dialect == Dialect::CcRh => Sar32,
        Punct::Shr => Shr,
        Punct::Amp => And,
        Punct::Pipe => Or,
        Punct::Caret => Xor,
        Punct::EqEq => Eq,
        // A single `=` compares in ca65, vasm and GNU as for the Z80. An
        // assignment was already told apart by the statement parser.
        Punct::Eq if dialect == Dialect::EightBit => Eq,
        Punct::Ne => Ne,
        Punct::Lt => Lt,
        Punct::Gt => Gt,
        Punct::Le => Le,
        Punct::Ge => Ge,
        Punct::AndAnd => LogicalAnd,
        Punct::OrOr => LogicalOr,
        _ => return None,
    })
}

/// A name with a relocation modifier on the end, `foo@IMGREL`, as the base
/// name and the modifier lowercased, where the lexer keeps `@` inside names;
/// `None` if what follows the last `@` names no modifier, so that the whole
/// word is the name. The list is the modifiers some backend or object format
/// here knows.
fn split_modifier(name: &str) -> Option<(String, String)> {
    let (base, suffix) = name.rsplit_once('@')?;
    if base.is_empty() {
        return None;
    }
    let lower = suffix.to_ascii_lowercase();
    const KNOWN: &[&str] = &[
        "imgrel",
        "secrel32",
        "secrel",
        "secidx",
        "plt",
        "got",
        "gotoff",
        "gotpc",
        "gotpcrel",
        "tlsgd",
        "tlsld",
        "tlsldm",
        "dtpoff",
        "ntpoff",
        "tpoff",
        "gottpoff",
        "gotntpoff",
        "indntpoff",
        "size",
    ];
    KNOWN
        .contains(&lower.as_str())
        .then(|| (base.to_string(), lower))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::{Dialect, LexConfig, Lexer, LitPool};
    use crate::source::SourceMap;
    use crate::symbol::SymbolId;
    use std::collections::HashMap;

    struct TestCtx {
        syms: HashMap<String, Value>,
        names: HashMap<Name, String>,
        here: i64,
    }

    impl EvalCtx for TestCtx {
        fn lookup_symbol(&mut self, name: Name, span: Span) -> Result<Value, EvalError> {
            let s = self.names.get(&name).cloned().unwrap_or_default();
            self.syms
                .get(&s)
                .copied()
                .ok_or_else(|| EvalError::new(span, format!("undefined: {s}")))
        }
        fn symbol_value(&mut self, _: SymbolId, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, "no such symbol"))
        }
        fn here(&mut self, _: Span) -> Result<Value, EvalError> {
            Ok(Value::abs(self.here))
        }
        fn section_start(&mut self, _: Span) -> Result<Value, EvalError> {
            Ok(Value::abs(0))
        }
        fn local_ref(&mut self, n: u32, _: LocalDir, span: Span) -> Result<Value, EvalError> {
            Err(EvalError::new(span, format!("no local label {n}")))
        }
        fn modifier(&mut self, _: Name, inner: Value, _: Span) -> Result<Value, EvalError> {
            Ok(inner)
        }
    }

    fn eval_str(src: &str, syms: &[(&str, Value)]) -> Result<Value, String> {
        let mut sm = SourceMap::new();
        let f = sm.add("t.s", src);
        let mut interner = Interner::new();
        let mut diags = DiagBag::new();
        let mut toks = Vec::new();
        {
            let mut pool = LitPool::new();
            let mut lx = Lexer::new(&sm, f, LexConfig::for_dialect(Dialect::Gas));
            loop {
                let t = lx.next_token(&mut interner, &mut pool, &mut diags);
                if matches!(t.kind, TokKind::Eof | TokKind::Eol) {
                    break;
                }
                toks.push(t);
            }
        }
        let mut arena = ExprArena::new();
        let mut cur = Cursor::new(&toks);
        let r = {
            let mut p = ExprParser {
                arena: &mut arena,
                interner: &mut interner,
                diags: &mut diags,
                dollar_is_here: false,
                star_is_here: false,
                dialect: Dialect::Gas,
                bit_dot: false,
                strings: None,
                paren_modifiers: &[],
                upper_modifiers: false,
            };
            p.parse(&mut cur)
        };
        if diags.has_errors() {
            return Err(diags.render(&sm, false));
        }
        let r = r.ok_or_else(|| "no expression".to_string())?;
        let mut names = HashMap::new();
        let mut map = HashMap::new();
        for (n, v) in syms {
            if let Some(id) = interner.lookup(n) {
                names.insert(id, n.to_string());
            }
            map.insert(n.to_string(), *v);
        }
        let mut cx = TestCtx {
            syms: map,
            names,
            here: 0x1000,
        };
        eval(&arena, r, &mut cx).map_err(|e| e.msg)
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(eval_str("1 + 2 * 3", &[]).unwrap(), Value::abs(7));
        assert_eq!(eval_str("(1 + 2) * 3", &[]).unwrap(), Value::abs(9));
        assert_eq!(eval_str("1 << 4 | 3", &[]).unwrap(), Value::abs(19));
        assert_eq!(eval_str("-5 + 3", &[]).unwrap(), Value::abs(-2));
        assert_eq!(eval_str("~0 & 0xff", &[]).unwrap(), Value::abs(0xff));
        assert_eq!(eval_str("10 - 4 - 3", &[]).unwrap(), Value::abs(3));
        assert_eq!(eval_str("2 * 3 + 4 * 5", &[]).unwrap(), Value::abs(26));
        assert_eq!(eval_str("1 < 2 && 3 > 2", &[]).unwrap(), Value::abs(1));
    }

    #[test]
    fn location_counter() {
        assert_eq!(eval_str(". + 4", &[]).unwrap(), Value::abs(0x1004));
    }

    #[test]
    fn symbol_plus_constant_stays_relocatable() {
        let a = SymbolId(7);
        let v = eval_str("foo + 8", &[("foo", Value::sym(a, 0))]).unwrap();
        assert_eq!(
            v,
            Value {
                addend: 8,
                plus: Some(a),
                minus: None
            }
        );
    }

    #[test]
    fn difference_of_symbols() {
        let a = SymbolId(1);
        let b = SymbolId(2);
        let v = eval_str(
            "foo - bar",
            &[("foo", Value::sym(a, 4)), ("bar", Value::sym(b, 1))],
        )
        .unwrap();
        assert_eq!(
            v,
            Value {
                addend: 3,
                plus: Some(a),
                minus: Some(b)
            }
        );
        // Same symbol on both sides collapses to a constant.
        let v = eval_str(
            "foo - bar",
            &[("foo", Value::sym(a, 9)), ("bar", Value::sym(a, 2))],
        )
        .unwrap();
        assert_eq!(v, Value::abs(7));
    }

    #[test]
    fn rejects_nonsense_relocatable_arithmetic() {
        let a = SymbolId(1);
        let b = SymbolId(2);
        let e = eval_str(
            "foo + bar",
            &[("foo", Value::sym(a, 0)), ("bar", Value::sym(b, 0))],
        )
        .unwrap_err();
        assert!(e.contains("cannot add two relocatable"), "{e}");
        let e = eval_str("foo * 2", &[("foo", Value::sym(a, 0))]).unwrap_err();
        assert!(e.contains("absolute"), "{e}");
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert!(
            eval_str("1 / 0", &[])
                .unwrap_err()
                .contains("division by zero")
        );
    }

    #[test]
    fn oversized_shift_yields_zero() {
        assert_eq!(eval_str("1 << 64", &[]).unwrap(), Value::abs(0));
    }

    #[test]
    fn reports_missing_paren() {
        let e = eval_str("(1 + 2", &[]).unwrap_err();
        assert!(e.contains("expected `)`"), "{e}");
    }
}
