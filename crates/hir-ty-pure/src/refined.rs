//! Refined values (docs/v2/plan/design/refined-values.md): what `#[blaze::invariant(..)]` on a
//! struct and a function's native `requires` and `ensures` say, read into one small language
//! of linear comparisons over integers and measures ([`Pred`]), and the sites of a body that
//! do not show what they must ([`of_body`]).
//!
//! The prover ([`crate::reach`]) shows a clause where one is needed and assumes it where one
//! holds (`reach::obligations`). This module only reads clauses and says why one is refused.
//!
//! The language, the decidable fragment the design states: integer constants (and `iN::MAX`,
//! `uN::MIN` and the like), parameters, the return, a struct's fields one level down (`self.f`,
//! `self.0`), `.len()` of any of these (a measure), `+`, `-`, `*` by a constant, `/` of
//! constants, the six comparisons, `&&`, `||`, `!`, `true` and `false`. A comparison is of
//! mathematical integers: `self.x * 2 <= i32::MAX` does not overflow. Anything else is not
//! checkable, and an invariant written with it is refused.

use base_db::Db;
use hir_def::{
    AdtId, AttrDefId, DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    StructId, VariantId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore, SpecClauseKind, pretty::print_expr_hir},
    hir::{
        ArithOp, BinaryOp, BindingId, CmpOp, Expr, ExprId, Literal, LogicOp, Ordering, Pat, UnaryOp,
    },
    item_tree::RawVisibility,
    resolver::{HasResolver as _, Resolver, ValueNs},
    signatures::{EnumSignature, FunctionSignature, StructSignature, UnionSignature},
};
use hir_ty::{db::HirDatabase as _, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;

use crate::reach::{self, Rel};

// The language.

/// A value a clause names.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum Var {
    /// The value an invariant is of (`self` in `#[blaze::invariant(..)]`).
    This,
    /// The function's parameter of this index, `self` first.
    Arg(u32),
    /// The function's return, named in `-> (r: T)`.
    Ret,
    /// Field `.1` (its index) of the struct value `.0` leads to.
    Field(Box<Var>, u32),
    /// The length of what `.0` leads to: a slice's, an array's, a `str`'s in bytes, a `Vec`'s
    /// or a `String`'s. A measure: a pure, total, ghost value of the place.
    Len(Box<Var>),
}

/// The exact integer `sum of coefficient * var, plus constant`, each var once.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Lin {
    pub(crate) parts: Vec<(Var, i128)>,
    pub(crate) constant: i128,
}

impl Lin {
    fn constant(value: i128) -> Lin {
        Lin { parts: Vec::new(), constant: value }
    }

    fn var(var: Var) -> Lin {
        Lin { parts: vec![(var, 1)], constant: 0 } // alloc: one part
    }

    /// `self + scale * other`; none when a coefficient or the constant leaves `i128`.
    fn add(&self, other: &Lin, scale: i128) -> Option<Lin> {
        let mut parts = self.parts.clone(); // clone: the sum starts from the left side
        for (var, coefficient) in &other.parts {
            let by = coefficient.checked_mul(scale)?;
            match parts.iter_mut().find(|it| it.0 == *var) {
                Some(it) => it.1 = it.1.checked_add(by)?,
                None => parts.push((var.clone(), by)), // clone: the part's own copy
            }
        }
        parts.retain(|it| it.1 != 0);
        parts.sort();
        Some(Lin { parts, constant: self.constant.checked_add(other.constant.checked_mul(scale)?)? })
    }

    fn scale(&self, by: i128) -> Option<Lin> {
        Lin::constant(0).add(self, by)
    }

    fn as_constant(&self) -> Option<i128> {
        self.parts.is_empty().then_some(self.constant)
    }
}

/// What a clause says.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Pred {
    /// Every part holds; `All([])` is `true`.
    All(Vec<Pred>),
    /// Some part holds; `Any([])` is `false`.
    Any(Vec<Pred>),
    /// `a rel b`.
    Cmp(Rel, Lin, Lin),
}

/// A clause as written, and what it says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Clause {
    /// The clause's source, for messages.
    pub text: Box<str>,
    pub(crate) pred: Pred,
}

/// A refusal of a clause, a type or a site: what is wrong, and what to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refusal {
    pub message: String,
    pub note: Option<String>,
}

/// What blaze reads of an expression, before it is a [`Pred`] or a [`Lin`].
enum Ast {
    Int(i128),
    Bool(bool),
    Var(Var),
    Neg(Box<Ast>),
    Not(Box<Ast>),
    Bin(Op, Box<Ast>, Box<Ast>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Or,
    And,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    Add,
    Sub,
    Mul,
    Div,
}

/// `ast` as a condition, negated when `negated`: `!` pushed down to the comparisons.
fn pred_of(ast: &Ast, negated: bool) -> Option<Pred> {
    Some(match ast {
        Ast::Bool(value) if *value != negated => Pred::All(Vec::new()),
        Ast::Bool(_) => Pred::Any(Vec::new()),
        Ast::Not(inner) => return pred_of(inner, !negated),
        Ast::Bin(op @ (Op::And | Op::Or), a, b) => {
            let (a, b) = (pred_of(a, negated)?, pred_of(b, negated)?);
            // alloc: the two sides
            if (*op == Op::And) != negated { Pred::All(vec![a, b]) } else { Pred::Any(vec![a, b]) }
        }
        Ast::Bin(op, a, b) => {
            let (a, b) = (lin_of(a)?, lin_of(b)?);
            let (rel, a, b) = match op {
                Op::Lt => (Rel::Lt, a, b),
                Op::Le => (Rel::Le, a, b),
                Op::Gt => (Rel::Lt, b, a),
                Op::Ge => (Rel::Le, b, a),
                Op::Eq => (Rel::Eq, a, b),
                Op::Ne => (Rel::Ne, a, b),
                _ => return None,
            };
            match (negated, rel) {
                (false, rel) => Pred::Cmp(rel, a, b),
                (true, Rel::Lt) => Pred::Cmp(Rel::Le, b, a),
                (true, Rel::Le) => Pred::Cmp(Rel::Lt, b, a),
                (true, Rel::Eq) => Pred::Cmp(Rel::Ne, a, b),
                (true, Rel::Ne) => Pred::Cmp(Rel::Eq, a, b),
            }
        }
        Ast::Int(_) | Ast::Var(_) | Ast::Neg(_) => return None,
    })
}

/// `ast` as an exact integer: linear, a product only by a constant, a quotient only of
/// constants (rounded toward 0, as Rust's `/`).
fn lin_of(ast: &Ast) -> Option<Lin> {
    match ast {
        Ast::Int(value) => Some(Lin::constant(*value)),
        Ast::Var(var) => Some(Lin::var(var.clone())), // clone: the form's own copy
        Ast::Neg(inner) => lin_of(inner)?.scale(-1),
        Ast::Bin(Op::Add, a, b) => lin_of(a)?.add(&lin_of(b)?, 1),
        Ast::Bin(Op::Sub, a, b) => lin_of(a)?.add(&lin_of(b)?, -1),
        Ast::Bin(Op::Mul, a, b) => {
            let (a, b) = (lin_of(a)?, lin_of(b)?);
            match (a.as_constant(), b.as_constant()) {
                (Some(by), _) => b.scale(by),
                (_, Some(by)) => a.scale(by),
                _ => None,
            }
        }
        Ast::Bin(Op::Div, a, b) => {
            let (a, b) = (lin_of(a)?.as_constant()?, lin_of(b)?.as_constant()?);
            a.checked_div(b).map(Lin::constant)
        }
        Ast::Bool(_) | Ast::Not(_) | Ast::Bin(..) => None,
    }
}

/// The least and greatest value of the integer type spelled `name` (`i32`, `usize`, `u30`).
fn type_bounds(name: &str, word: u32) -> Option<(i128, i128)> {
    let (signed, bits) = match name {
        "usize" => (false, word),
        "isize" => (true, word),
        _ => {
            let signed = match name.as_bytes().first()? {
                b'u' => false,
                b'i' => true,
                _ => return None,
            };
            let bits: u32 = name[1..].parse().ok()?;
            (signed, bits)
        }
    };
    Some(match (signed, bits) {
        (_, 0) | (_, 129..) => return None,
        (false, 128) => (0, i128::MAX),
        (true, 128) => (i128::MIN, i128::MAX),
        (false, bits) => (0, (1i128 << bits) - 1),
        (true, bits) => (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1),
    })
}

/// `iN::MAX`, `uN::MIN` and the like, as written.
fn builtin_constant(text: &str, word: u32) -> Option<i128> {
    let (ty, which) = text.split_once("::")?;
    let (least, most) = type_bounds(ty.trim(), word)?;
    match which.trim() {
        "MAX" => Some(most),
        "MIN" => Some(least),
        _ => None,
    }
}

// The attribute's text.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token<'t> {
    Int(i128),
    Word(&'t str),
    Punct(&'static str),
}

/// The tokens of a condition; none for a character the language has no use for.
fn lex(text: &str) -> Option<Vec<Token<'_>>> {
    const PUNCTS: [&str; 17] =
        ["::", "<=", ">=", "==", "!=", "&&", "||", "<", ">", "+", "-", "*", "/", "!", "(", ")", "."];
    let bytes = text.as_bytes();
    let mut tokens = Vec::new(); // alloc: a condition's tokens, a few dozen at most
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        if c.is_ascii_whitespace() {
            at += 1;
        } else if c.is_ascii_digit() {
            // Digits and `_`, then an integer type's name as a suffix.
            let mut value: i128 = 0;
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'_') {
                if bytes[at] != b'_' {
                    value = value.checked_mul(10)?.checked_add(i128::from(bytes[at] - b'0'))?;
                }
                at += 1;
            }
            let start = at;
            while at < bytes.len() && bytes[at].is_ascii_alphanumeric() {
                at += 1;
            }
            if start != at {
                let (least, most) = type_bounds(&text[start..at], 128)?;
                if value < least || most < value {
                    return None;
                }
            }
            tokens.push(Token::Int(value));
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            tokens.push(Token::Word(&text[start..at]));
        } else {
            let punct = PUNCTS.iter().copied().find(|it| text[at..].starts_with(*it))?;
            tokens.push(Token::Punct(punct));
            at += punct.len();
        }
    }
    Some(tokens)
}

/// A Pratt parser of an invariant's condition, over its struct's field names.
struct Parser<'t, 'f> {
    tokens: Vec<Token<'t>>,
    at: usize,
    word: u32,
    field: &'f dyn Fn(&str) -> Option<u32>,
}

impl<'t> Parser<'t, '_> {
    fn peek(&self) -> Option<Token<'t>> {
        self.tokens.get(self.at).copied()
    }

    fn eat(&mut self, punct: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Punct(it)) if it == punct);
        self.at += usize::from(found);
        found
    }

    /// The operator next, with its binding power.
    fn infix(&self) -> Option<(Op, u8)> {
        let Some(Token::Punct(punct)) = self.peek() else { return None };
        Some(match punct {
            "||" => (Op::Or, 1),
            "&&" => (Op::And, 2),
            "<" => (Op::Lt, 3),
            "<=" => (Op::Le, 3),
            ">" => (Op::Gt, 3),
            ">=" => (Op::Ge, 3),
            "==" => (Op::Eq, 3),
            "!=" => (Op::Ne, 3),
            "+" => (Op::Add, 4),
            "-" => (Op::Sub, 4),
            "*" => (Op::Mul, 5),
            "/" => (Op::Div, 5),
            _ => return None,
        })
    }

    fn expr(&mut self, least: u8) -> Option<Ast> {
        let mut left = self.prefix()?;
        while let Some((op, power)) = self.infix() {
            if power < least {
                break;
            }
            self.at += 1;
            // Comparisons do not chain: `a < b < c` is not Rust.
            let right = self.expr(power + 1)?;
            if power == 3 && self.infix().is_some_and(|(_, next)| next == 3) {
                return None;
            }
            left = Ast::Bin(op, Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn prefix(&mut self) -> Option<Ast> {
        let token = self.peek()?;
        self.at += 1;
        match token {
            Token::Int(value) => Some(Ast::Int(value)),
            Token::Punct("-") => Some(Ast::Neg(Box::new(self.expr(6)?))),
            Token::Punct("!") => Some(Ast::Not(Box::new(self.expr(6)?))),
            Token::Punct("(") => {
                let inner = self.expr(0)?;
                self.eat(")").then_some(inner)
            }
            Token::Word("true") => Some(Ast::Bool(true)),
            Token::Word("false") => Some(Ast::Bool(false)),
            Token::Word("self") => {
                if !self.eat(".") {
                    return None;
                }
                let field = match self.peek()? {
                    Token::Int(index) => u32::try_from(index).ok()?,
                    Token::Word(name) => (self.field)(name)?,
                    Token::Punct(_) => return None,
                };
                self.at += 1;
                let mut var = Var::Field(Box::new(Var::This), field);
                if self.eat(".") {
                    if self.peek()? != Token::Word("len") {
                        return None;
                    }
                    self.at += 1;
                    if !(self.eat("(") && self.eat(")")) {
                        return None;
                    }
                    var = Var::Len(Box::new(var));
                }
                Some(Ast::Var(var))
            }
            Token::Word(ty) => {
                if !self.eat("::") {
                    return None;
                }
                let Token::Word(which) = self.peek()? else { return None };
                self.at += 1;
                // alloc: the path's text, once per constant
                builtin_constant(&format!("{ty}::{which}"), self.word).map(Ast::Int)
            }
            Token::Punct(_) => None,
        }
    }
}

/// The condition of `#[blaze::invariant(text)]`, over fields `field` names.
fn parse_invariant(text: &str, word: u32, field: &dyn Fn(&str) -> Option<u32>) -> Option<Pred> {
    let mut parser = Parser { tokens: lex(text)?, at: 0, word, field };
    let ast = parser.expr(0)?;
    if parser.at != parser.tokens.len() {
        return None;
    }
    pred_of(&ast, false)
}

// Invariants.

/// What a struct's `#[blaze::invariant(..)]`s say.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Invariant {
    /// The conditions each value built must be shown to keep.
    pub clauses: Vec<Clause>,
    /// The conditions its fields' types already hold, which no site needs to show.
    pub implied: Vec<Box<str>>,
    /// Why the invariants are refused, when they are: then nothing is shown or assumed of them,
    /// and the type's declaration carries the error.
    pub refused: Option<Refusal>,
}

/// The name of `adt`, for messages.
pub fn adt_name(db: &Db, adt: AdtId) -> String {
    match adt {
        AdtId::StructId(it) => StructSignature::of(db, it).name.as_str().to_owned(),
        AdtId::EnumId(it) => EnumSignature::of(db, it).name.as_str().to_owned(),
        AdtId::UnionId(it) => UnionSignature::of(db, it).name.as_str().to_owned(),
    } // alloc: a message's part
}

/// The invariants of `adt`: none when it states none (one attribute walk, kept).
pub fn invariant_of(db: &Db, adt: AdtId) -> Option<&Invariant> {
    if AttrFlags::blaze_invariants(db, AttrDefId::AdtId(adt)).is_empty() {
        return None;
    }
    Some(invariant(db, adt))
}

/// How many bits a `usize` has where `krate` is built for; 64 when the target is not known,
/// which only spells `usize::MAX` in a clause (a refusal says the target is unknown elsewhere).
fn word_of(db: &Db, krate: base_db::Crate) -> u32 {
    db.target_data_layout(krate).map_or(64, |it| it.pointer_size().bits() as u32)
}

#[blaze_query::tracked(returns(ref))]
fn invariant(db: &Db, adt: AdtId) -> Invariant {
    let texts = AttrFlags::blaze_invariants(db, AttrDefId::AdtId(adt));
    let name = adt_name(db, adt);
    let refused = |message: String, note: String| Invariant {
        refused: Some(Refusal { message, note: Some(note) }),
        ..Invariant::default()
    };
    let AdtId::StructId(it) = adt else {
        let kind = if matches!(adt, AdtId::EnumId(_)) { "an enum" } else { "a union" };
        // alloc: a message
        return refused(
            format!("`#[blaze::invariant(..)]` is on `{name}`, {kind}: an invariant is shown where a struct is built"),
            "state it on a struct, and hold that struct in the variant or field".to_owned(),
        );
    };
    // Which crates are checked is decided by what their modules declare ([`in_play`]): a
    // struct in a block would be assumed and never shown.
    if it.module(db).is_block_module(db) {
        // alloc: a message
        return refused(
            format!("`#[blaze::invariant(..)]` is on `{name}`, which is declared in a block"),
            format!("declare `{name}` in a module, where blaze finds the types whose invariants it shows"),
        );
    }
    let fields = it.fields(db).fields();
    for (_, field) in fields.iter() {
        if !matches!(field.visibility, RawVisibility::PubSelf(_)) {
            let field = field.name.as_str();
            // alloc: a message
            return refused(
                format!(
                    "`{name}.{field}` is not private, so code outside `{name}`'s module can build or change a `{name}` without `#[blaze::invariant({})]` being shown",
                    texts[0]
                ),
                format!("make `{field}` private, and build and change `{name}` through functions of its module"),
            );
        }
    }
    let krate = it.krate(db);
    let word = word_of(db, krate);
    let by_name = |wanted: &str| {
        fields.iter().position(|(_, field)| field.name.as_str() == wanted).map(|at| at as u32)
    };
    let types = db.field_types(VariantId::StructId(it));
    let mut found = Invariant::default();
    for text in texts {
        let Some(pred) = parse_invariant(text, word, &by_name) else {
            // alloc: a message
            return refused(
                format!("not checkable: `#[blaze::invariant({text})]`"),
                "state it as a narrower type or a simpler bound: blaze proves comparisons of integer fields, `self.f.len()`, constants, `+`, `-`, `*` by a constant, `&&`, `||` and `!`"
                    .to_owned(),
            );
        };
        let range = |var: &Var| -> Option<(i128, i128)> {
            match var {
                Var::Field(base, at) if **base == Var::This => {
                    let ty = types.as_slice().get(*at as usize)?.ty().skip_binder();
                    reach::int_range(ty, word).filter(|_| !matches!(ty.kind(), TyKind::Bool))
                }
                Var::Len(_) => Some((0, type_bounds("usize", word)?.1)),
                _ => None,
            }
        };
        if implied(&pred, &range) {
            found.implied.push(text.clone()); // clone: the query's own copy
        } else {
            found.clauses.push(Clause { text: text.clone(), pred }); // clone: as above
        }
    }
    found
}

/// Whether `pred` holds for every value the ranges `range` gives its values allow.
fn implied(pred: &Pred, range: &dyn Fn(&Var) -> Option<(i128, i128)>) -> bool {
    match pred {
        Pred::All(parts) => parts.iter().all(|it| implied(it, range)),
        Pred::Any(parts) => parts.iter().any(|it| implied(it, range)),
        Pred::Cmp(rel, a, b) => {
            let Some(difference) = a.add(b, -1) else { return false };
            let (mut low, mut high) = (Some(difference.constant), Some(difference.constant));
            for (var, coefficient) in &difference.parts {
                let Some((least, most)) = range(var) else { return false };
                let (down, up) = if *coefficient > 0 { (least, most) } else { (most, least) };
                low = low.and_then(|it| it.checked_add(coefficient.checked_mul(down)?));
                high = high.and_then(|it| it.checked_add(coefficient.checked_mul(up)?));
            }
            let (Some(low), Some(high)) = (low, high) else { return false };
            match rel {
                Rel::Lt => high < 0,
                Rel::Le => high <= 0,
                Rel::Eq => low == 0 && high == 0,
                Rel::Ne => high < 0 || 0 < low,
            }
        }
    }
}

// Contracts.

/// The clauses of a function's native contract (`requires`, `ensures`) that blaze proves at
/// each call and at each return. A clause outside the language is in neither: it is the
/// contracts design's, checked at run time in dev and test builds (docs/v2/plan/design/contracts.md).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Contract {
    pub requires: Vec<Clause>,
    pub ensures: Vec<Clause>,
}

/// The contract of `func`: none when its crate has no `#![feature(blaze_verify)]` or it states
/// no clause blaze proves.
pub fn contract_of(db: &Db, func: FunctionId) -> Option<&Contract> {
    contract(db, func)
}

#[blaze_query::tracked(returns(as_ref))]
fn contract(db: &Db, func: FunctionId) -> Option<Contract> {
    let krate = func.krate(db);
    if !hir_def::nameres::crate_def_map(db, krate).features().blaze_verify {
        return None;
    }
    let def = DefWithBodyId::FunctionId(func);
    let body = Body::of(db, def);
    let store = &body.store;
    if store.spec_clauses().is_empty() {
        return None;
    }
    // The parameters' bindings, `self` first, as the MIR's parameter locals are.
    let mut params = Vec::new(); // alloc: one per parameter, few
    if let Some(it) = &body.self_param {
        params.push(Some(it.formal));
    }
    for param in body.params.iter() {
        params.push(match store.pat(param.formal) {
            Pat::Bind { id, subpat: None } => Some(id),
            _ => None,
        });
    }
    // The struct `self` is, for `self.f`.
    let this = match func.lookup(db).container {
        ItemContainerId::ImplId(imp) => match db.impl_self_ty(imp).skip_binder().kind() {
            TyKind::Adt(adt, _) => match adt.def_id() {
                AdtId::StructId(it) => Some(it),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    };
    let mut reader = Reader {
        db,
        def,
        store,
        resolver: def.resolver(db),
        params,
        ret: None,
        this,
        word: word_of(db, krate),
    };
    let mut contract = Contract::default();
    for &(group, kind) in store.spec_clauses() {
        let (items, into) = match kind {
            SpecClauseKind::Requires => (group, &mut contract.requires),
            SpecClauseKind::Ensures => match store.expr(group) {
                Expr::Closure { args: &[ret], body: items, .. } => {
                    reader.ret = match store.pat(ret) {
                        Pat::Bind { id, subpat: None } => Some(id),
                        _ => None,
                    };
                    (items, &mut contract.ensures)
                }
                _ => {
                    reader.ret = None;
                    (group, &mut contract.ensures)
                }
            },
            SpecClauseKind::Recommends
            | SpecClauseKind::Decreases
            | SpecClauseKind::Invariant
            | SpecClauseKind::LoopEnsures
            | SpecClauseKind::LoopDecreases => continue,
        };
        let Expr::Tuple { exprs } = store.expr(items) else { continue };
        for &item in exprs {
            let Some(pred) = reader.ast(item).and_then(|it| pred_of(&it, false)) else { continue };
            // alloc: the clause's text, for messages
            let text = print_expr_hir(db, store, def.into(), item, syntax::Edition::CURRENT);
            into.push(Clause { text: text.into_boxed_str(), pred });
        }
    }
    let empty = contract.requires.is_empty() && contract.ensures.is_empty();
    (!empty).then_some(contract)
}

/// Reads a clause's HIR into an [`Ast`].
struct Reader<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    store: &'db ExpressionStore,
    resolver: Resolver<'db>,
    params: Vec<Option<BindingId>>,
    ret: Option<BindingId>,
    this: Option<StructId>,
    word: u32,
}

impl Reader<'_> {
    fn ast(&mut self, e: ExprId) -> Option<Ast> {
        Some(match self.store.expr(e) {
            Expr::Literal(Literal::Int(value, _)) => Ast::Int(*value),
            Expr::Literal(Literal::Uint(value, _)) => Ast::Int(i128::try_from(*value).ok()?),
            Expr::Literal(Literal::Bool(value)) => Ast::Bool(*value),
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => Ast::Neg(Box::new(self.ast(expr)?)),
            Expr::UnaryOp { expr, op: UnaryOp::Not } => Ast::Not(Box::new(self.ast(expr)?)),
            // `*r` of a reference parameter is what it leads to, which a measure reads anyway.
            Expr::UnaryOp { expr, op: UnaryOp::Deref } => self.ast(expr)?,
            Expr::BinaryOp { lhs, rhs, op: Some(op) } => {
                let op = match op {
                    BinaryOp::LogicOp(LogicOp::And) => Op::And,
                    BinaryOp::LogicOp(LogicOp::Or) => Op::Or,
                    BinaryOp::ArithOp(ArithOp::Add) => Op::Add,
                    BinaryOp::ArithOp(ArithOp::Sub) => Op::Sub,
                    BinaryOp::ArithOp(ArithOp::Mul) => Op::Mul,
                    BinaryOp::ArithOp(ArithOp::Div) => Op::Div,
                    BinaryOp::CmpOp(CmpOp::Eq { negated: false }) => Op::Eq,
                    BinaryOp::CmpOp(CmpOp::Eq { negated: true }) => Op::Ne,
                    BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less, strict: true }) => Op::Lt,
                    BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less, strict: false }) => Op::Le,
                    BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: true }) => {
                        Op::Gt
                    }
                    BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: false }) => {
                        Op::Ge
                    }
                    _ => return None,
                };
                Ast::Bin(op, Box::new(self.ast(lhs)?), Box::new(self.ast(rhs)?))
            }
            Expr::MethodCall { receiver, method_name, args: &[], .. }
                if method_name.as_str() == "len" =>
            {
                Ast::Var(Var::Len(Box::new(self.var(receiver)?)))
            }
            Expr::Path(_) | Expr::Field { .. } => match self.var(e) {
                Some(var) => Ast::Var(var),
                None => {
                    let text = print_expr_hir(self.db, self.store, self.def.into(), e, syntax::Edition::CURRENT);
                    Ast::Int(builtin_constant(&text, self.word)?)
                }
            },
            _ => return None,
        })
    }

    /// The value `e` names: a parameter, the return, or a field of `self`.
    fn var(&mut self, e: ExprId) -> Option<Var> {
        match self.store.expr(e) {
            Expr::Path(path) => {
                let guard = self.resolver.update_to_inner_scope(self.db, self.def, e);
                let hygiene = self.store.expr_path_hygiene(e);
                let found = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
                self.resolver.reset_to_guard(guard);
                let Some(ValueNs::LocalBinding(binding)) = found else { return None };
                if self.ret == Some(binding) {
                    return Some(Var::Ret);
                }
                let at = self.params.iter().position(|it| *it == Some(binding))?;
                Some(Var::Arg(at as u32))
            }
            Expr::Field { expr, name } => {
                let base = self.var(expr)?;
                // Fields of `self` only: the one struct a clause can name fields of unread.
                if base != Var::Arg(0) {
                    return None;
                }
                let this = self.this?;
                let fields = this.fields(self.db).fields();
                let at = fields.iter().position(|(_, field)| field.name.as_str() == name.as_str())?;
                Some(Var::Field(Box::new(base), at as u32))
            }
            Expr::UnaryOp { expr, op: UnaryOp::Deref } => self.var(expr),
            _ => None,
        }
    }
}

// Sites.

/// Whether a body of `krate` can have anything to show: `krate`, or a crate it depends on,
/// has `#![feature(blaze_verify)]` or a struct with `#[blaze::invariant(..)]` (only a crate
/// that sees one can build, change or call one). Once per crate: the opt-in's whole cost for
/// a program that states none.
#[blaze_query::tracked(returns(copy))]
pub fn in_play(db: &Db, krate: base_db::Crate) -> bool {
    states_here(db, krate) || krate.data(db).dependencies.iter().any(|dep| in_play(db, dep.crate_id))
}

/// Whether `krate` itself has `#![feature(blaze_verify)]` or a module-level struct with
/// `#[blaze::invariant(..)]`.
#[blaze_query::tracked(returns(copy))]
fn states_here(db: &Db, krate: base_db::Crate) -> bool {
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    if def_map.features().blaze_verify {
        return true;
    }
    def_map.modules(db).any(|(_, module)| {
        module.scope(db).declarations().any(|it| match it {
            hir_def::ModuleDefId::AdtId(adt @ AdtId::StructId(_)) => {
                !AttrFlags::blaze_invariants(db, AttrDefId::AdtId(adt)).is_empty()
            }
            _ => false,
        })
    })
}

/// What a body did not show: each site, at the expression it is written at where it has one.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Sites {
    pub found: Box<[(Option<ExprId>, Refusal)]>,
}

/// The sites of the body of `def` that build, change, pass or return a value without showing
/// what its type's invariant or a contract needs, once per definition. A body with no such
/// site costs one scan of its MIR. The body's closures are not walked: a closure that builds
/// a value of a type with an invariant is a next step of the design.
#[blaze_query::tracked(returns(ref))]
pub fn of_body(db: &Db, def: DefWithBodyId) -> Sites {
    let found = match reach::obligations_of(db, def) {
        Ok(found) => found,
        Err(why) => {
            // alloc: a message
            let message = format!(
                "this body builds, changes or passes a value whose invariant or contract blaze must show, and it is not analysed: {why}"
            );
            let note = "move what needs showing into a function of its own".to_owned(); // alloc: a note
            return Sites { found: Box::new([(None, Refusal { message, note: Some(note) })]) };
        }
    };
    // alloc: the sites, the query's value
    let mut sites: Vec<(Option<ExprId>, Refusal)> = found
        .into_iter()
        .map(|it| {
            let at = match it.at {
                hir_ty_mir::mir::MirSpan::ExprId(expr) => Some(expr),
                _ => None,
            };
            (at, Refusal { message: it.message, note: it.note })
        })
        .collect();
    sites.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.message.cmp(&b.1.message)));
    sites.dedup();
    Sites { found: sites.into_boxed_slice() }
}

/// The name of `func`, for messages.
pub(crate) fn fn_name(db: &Db, func: FunctionId) -> String {
    FunctionSignature::of(db, func).name.as_str().to_owned() // alloc: a message's part
}
