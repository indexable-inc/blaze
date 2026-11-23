//! The `.solve` test language's parser (the grammar is the lane brief's, kept in solve.md,
//! "Tests"): one line per declaration, directive or goal, read into a syntax tree that
//! [`super::world`] lowers into the world's portable tables or the case's arena.

use std::string::{String, ToString};
use std::vec::Vec;
use std::boxed::Box;
use std::format;

use crate::term::{FloatTy, IntTy, Mutbl, TypingMode, UintTy};

/// A parse or lowering error: the line (1-based, in its file) and what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseError {
    pub line: u32,
    pub what: String,
}

impl core::fmt::Display for CaseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

pub type Parsed<T> = Result<T, CaseError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    Ident(String),
    Num(u64),
    /// One or two punctuation characters (`::`, `->`, `==`, `=>`, `<:`).
    P(&'static str),
}

const PUNCT2: [&str; 5] = ["::", "->", "==", "=>", "<:"];
const PUNCT1: [&str; 23] = ["<", ">", "(", ")", "[", "]", "{", "}", ",", ";", ":", "=", "!", "?", "&", "*", "'", "@", "#", "+", "|", "_", "-"];

/// Strips a comment: `#` at the line's start or after whitespace.
pub fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

pub fn lex(line: u32, s: &str) -> Parsed<Vec<Tok>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    'outer: while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || (c == b'_' && i + 1 < b.len() && (b[i + 1].is_ascii_alphanumeric() || b[i + 1] == b'_')) {
            let st = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(s[st..i].to_string()));
            continue;
        }
        if c.is_ascii_digit() {
            let st = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n = s[st..i].parse::<u64>().map_err(|_| CaseError { line, what: format!("bad number {}", &s[st..i]) })?;
            out.push(Tok::Num(n));
            continue;
        }
        for p in PUNCT2 {
            if s[i..].starts_with(p) {
                out.push(Tok::P(p));
                i += 2;
                continue 'outer;
            }
        }
        for p in PUNCT1 {
            if p.as_bytes()[0] == c {
                out.push(Tok::P(p));
                i += 1;
                continue 'outer;
            }
        }
        return Err(CaseError { line, what: format!("unexpected character {:?}", c as char) });
    }
    Ok(out)
}

/// A region as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lt {
    Static,
    Named(String),
    Erased,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TyA {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(Vec<TyA>),
    Ref(Lt, Box<TyA>, Mutbl),
    Ptr(Box<TyA>, Mutbl),
    Slice(Box<TyA>),
    Array(Box<TyA>, Box<TyA>),
    /// A name with args: a param, an ADT, a foreign type.
    Path(String, Vec<TyA>),
    FnPtr(Vec<TyA>, Box<TyA>),
    FnItem(String, Vec<TyA>),
    /// `closure#N<kind, sig, upvars>`.
    Closure(u64, Box<TyA>, Box<TyA>, Box<TyA>),
    /// A closure kind word (`fn`, `fnmut`, `fnonce`).
    Kind(u64),
    Dyn(Vec<BoundA>, Lt),
    /// `<self as Trait<args>>::Name<own>`, rigid when written `rigid <..>`.
    Proj { self_ty: Box<TyA>, trait_: String, args: Vec<TyA>, name: String, own: Vec<TyA>, rigid: bool },
    Opaque(String, Vec<TyA>, bool),
    /// `?N`, `?iN`, `?fN`, `?K`.
    Var(String),
    Placeholder(u32),
    Const(u64),
    Error,
    Region(Lt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundA {
    pub negative: bool,
    /// `?Sized`.
    pub maybe: bool,
    pub trait_: String,
    pub args: Vec<TyA>,
    pub assoc: Vec<(String, TyA)>,
    /// `Name: B + ..` (an associated item constraint with bounds, RFC 2289): each bound on
    /// `<self as Trait<args>>::Name`, nested as written.
    pub assoc_bounds: Vec<(String, Vec<BoundA>)>,
}

/// A bound's arguments: the written ones, the `Name = T` bindings and the `Name: Bounds`
/// constraints, each list in writing order.
type BoundArgs = (Vec<TyA>, Vec<(String, TyA)>, Vec<(String, Vec<BoundA>)>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredA {
    Bound(TyA, BoundA),
    /// `alias == term`.
    Eq(TyA, TyA),
    /// `normalizes(alias) == term`.
    Normalizes(TyA, TyA),
    Wf(TyA),
    Sub(TyA, TyA),
    ForAll(Vec<String>, Box<PredA>),
    /// `const NAME(T, ..)`: a `where const` condition at the arguments. The test world answers
    /// it by the name, whatever the arguments: `yes` true, `no` false, `err` an evaluation
    /// error; any other name is a fact the world lacks (`Missing`).
    Const(String, Vec<TyA>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamA {
    pub name: String,
    pub bounds: Vec<BoundA>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub extern_: bool,
    pub lang: Option<String>,
    pub auto: bool,
    pub marker: bool,
    pub coinductive: bool,
    pub fundamental: bool,
    pub unsafe_: bool,
    pub const_: bool,
    pub reservation: bool,
    /// `dyn_undecided trait T`: a trait whose dyn compatibility the world did not decide
    /// (`TraitFlags::DYN_UNDECIDED` in place of `DYN_COMPATIBLE`).
    pub dyn_undecided: bool,
    /// `auto_copy_pub struct S`: Blaze-edition transparent all-`pub` struct (`AdtFlags::AUTO_COPY_PUB`).
    pub auto_copy_pub: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitItemA {
    pub lang: Option<String>,
    pub is_const: bool,
    pub name: String,
    pub params: Vec<ParamA>,
    pub bounds: Vec<BoundA>,
    /// A const's type.
    pub ty: Option<TyA>,
    pub default: Option<TyA>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplItemA {
    pub is_default: bool,
    pub is_const: bool,
    pub name: String,
    pub params: Vec<ParamA>,
    pub value: TyA,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeclA {
    /// `wheres`: the trait's `where` list (`trait T: S where P; P`), which is not elaborated
    /// (only `supers` and their associated item constraints are implied by `X: T`).
    Trait { attrs: Attrs, name: String, params: Vec<ParamA>, supers: Vec<BoundA>, wheres: Vec<PredA>, items: Vec<TraitItemA> },
    Adt { attrs: Attrs, name: String, params: Vec<ParamA>, is_enum: bool, variants: Vec<Vec<TyA>> },
    Impl { attrs: Attrs, label: Option<String>, params: Vec<ParamA>, negative: bool, trait_: String, args: Vec<TyA>, self_ty: TyA, wheres: Vec<PredA>, items: Vec<ImplItemA> },
    Fn { attrs: Attrs, name: String, params: Vec<ParamA>, inputs: Vec<TyA>, output: TyA },
    Opaque { name: String, params: Vec<ParamA>, bounds: Vec<BoundA>, hidden: Option<TyA> },
    Foreign { attrs: Attrs, name: String },
}

/// What a goal is expected to answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    Yes(Vec<(String, TyA)>),
    No,
    Maybe,
    Overflow,
    /// The goal stops: a `dyn` principal's dyn compatibility is undecided
    /// (`Unsupported::DynUndecided`), never an answer.
    DynUndecided,
    Overlap,
    Disjoint,
    MayOverlap,
    Ok,
    NotLocal,
    Uncovered(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckA {
    Goal(PredA),
    Overlap(String, String),
    Orphan(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalA {
    pub line: u32,
    pub check: CheckA,
    pub expect: Expect,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaseA {
    pub name: String,
    pub rule: String,
    pub line: u32,
    pub decls: Vec<(u32, DeclA)>,
    pub params: Vec<String>,
    pub env: Vec<PredA>,
    /// Its `mode` line's typing mode; `None`: no such line (the runner checks it in analysis).
    pub mode: Option<TypingMode>,
    pub goals: Vec<GoalA>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileA {
    pub decls: Vec<(u32, DeclA)>,
    pub cases: Vec<CaseA>,
}

/// Parses a whole file.
pub fn parse_file(text: &str) -> Parsed<FileA> {
    let mut file = FileA::default();
    for (n, raw) in text.lines().enumerate() {
        let line = n as u32 + 1;
        let s = strip_comment(raw).trim();
        if s.is_empty() {
            continue;
        }
        // A case line's rule reference is free text after `=`.
        let lexed = match s.strip_prefix("case ") {
            Some(_) => s.split_once('=').map_or(s, |(head, _)| head).to_string() + " =",
            None => s.to_string(),
        };
        let toks = lex(line, &lexed)?;
        let mut p = P { t: &toks, i: 0, line };
        let first = match &toks[0] {
            Tok::Ident(w) => w.as_str(),
            _ => "",
        };
        match first {
            "case" => {
                p.i = 1;
                let name = p.ident()?;
                p.expect("=")?;
                let rule = s.split_once('=').map(|(_, r)| r.trim().to_string()).unwrap_or_default();
                file.cases.push(CaseA { name, rule, line, ..CaseA::default() });
            }
            "params" | "env" | "mode" | "goal" | "overlap" | "orphan" => {
                let Some(case) = file.cases.last_mut() else {
                    return Err(p.err("a directive outside a case"));
                };
                p.i = 1;
                match first {
                    "params" => {
                        while !p.done() {
                            case.params.push(p.ident()?);
                        }
                    }
                    "env" => loop {
                        case.env.push(p.pred()?);
                        if !p.eat(";") {
                            break;
                        }
                        if p.done() {
                            break;
                        }
                    },
                    "mode" => {
                        let m = p.ident()?;
                        case.mode = Some(match m.as_str() {
                            "coherence" => TypingMode::Coherence,
                            "post_analysis" => TypingMode::PostAnalysis,
                            _ => return Err(p.err("mode is `coherence` or `post_analysis`")),
                        });
                    }
                    "goal" => {
                        let pred = p.pred()?;
                        p.expect("=>")?;
                        let expect = p.expect_()?;
                        case.goals.push(GoalA { line, check: CheckA::Goal(pred), expect });
                    }
                    "overlap" => {
                        p.expect("@")?;
                        let a = p.ident()?;
                        p.expect("@")?;
                        let b = p.ident()?;
                        p.expect("=>")?;
                        let expect = p.expect_()?;
                        case.goals.push(GoalA { line, check: CheckA::Overlap(a, b), expect });
                    }
                    _ => {
                        p.expect("@")?;
                        let a = p.ident()?;
                        p.expect("=>")?;
                        let expect = p.expect_()?;
                        case.goals.push(GoalA { line, check: CheckA::Orphan(a), expect });
                    }
                }
                if !p.done() {
                    return Err(p.err("trailing tokens"));
                }
            }
            _ => {
                let d = p.decl()?;
                if !p.done() {
                    return Err(p.err("trailing tokens after a declaration"));
                }
                match file.cases.last_mut() {
                    Some(c) => c.decls.push((line, d)),
                    None => file.decls.push((line, d)),
                }
            }
        }
    }
    Ok(file)
}

struct P<'t> {
    t: &'t [Tok],
    i: usize,
    line: u32,
}

impl P<'_> {
    fn err(&self, what: &str) -> CaseError {
        CaseError { line: self.line, what: format!("{what} (at token {}: {:?})", self.i, self.t.get(self.i)) }
    }

    fn done(&self) -> bool {
        self.i >= self.t.len()
    }

    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn peek_at(&self, k: usize) -> Option<&Tok> {
        self.t.get(self.i + k)
    }

    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Some(Tok::P(q)) if *q == p)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == w)
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: &str) -> Parsed<()> {
        if self.eat(p) { Ok(()) } else { Err(self.err(&format!("expected `{p}`"))) }
    }

    fn ident(&mut self) -> Parsed<String> {
        match self.peek() {
            Some(Tok::Ident(w)) => {
                let w = w.clone();
                self.i += 1;
                Ok(w)
            }
            _ => Err(self.err("expected a name")),
        }
    }

    fn num(&mut self) -> Parsed<u64> {
        match self.peek() {
            Some(Tok::Num(n)) => {
                let n = *n;
                self.i += 1;
                Ok(n)
            }
            _ => Err(self.err("expected a number")),
        }
    }

    fn expect_(&mut self) -> Parsed<Expect> {
        let w = self.ident()?;
        Ok(match w.as_str() {
            "yes" => {
                let mut binds = Vec::new();
                while self.eat("?") {
                    let name = self.var_name()?;
                    self.expect("=")?;
                    let t = self.ty()?;
                    binds.push((name, t));
                    if !self.eat(",") {
                        break;
                    }
                }
                Expect::Yes(binds)
            }
            "no" => Expect::No,
            "maybe" => Expect::Maybe,
            "overflow" => Expect::Overflow,
            "dyn_undecided" => Expect::DynUndecided,
            "overlap" => Expect::Overlap,
            "disjoint" => Expect::Disjoint,
            "may_overlap" => Expect::MayOverlap,
            "ok" => Expect::Ok,
            "not_local" => Expect::NotLocal,
            "uncovered" => {
                self.expect("(")?;
                let n = self.num()?;
                self.expect(")")?;
                Expect::Uncovered(n as u32)
            }
            _ => return Err(self.err("unknown expectation")),
        })
    }

    /// After `?`: `0`, `i0`, `f0`, `K`.
    fn var_name(&mut self) -> Parsed<String> {
        match self.peek() {
            Some(Tok::Num(n)) => {
                let s = n.to_string();
                self.i += 1;
                Ok(s)
            }
            Some(Tok::Ident(w)) => {
                let s = w.clone();
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.err("expected a variable")),
        }
    }

    fn attrs(&mut self) -> Parsed<Attrs> {
        let mut a = Attrs::default();
        loop {
            match self.peek() {
                Some(Tok::Ident(w)) => match w.as_str() {
                    "extern" => a.extern_ = true,
                    "auto" => a.auto = true,
                    "marker" => a.marker = true,
                    "coinductive" => a.coinductive = true,
                    "fundamental" => a.fundamental = true,
                    "unsafe" => a.unsafe_ = true,
                    "const" => a.const_ = true,
                    "reservation" => a.reservation = true,
                    "dyn_undecided" => a.dyn_undecided = true,
                    "auto_copy_pub" => a.auto_copy_pub = true,
                    "lang" => {
                        self.i += 1;
                        self.expect("(")?;
                        a.lang = Some(self.ident()?);
                        self.expect(")")?;
                        continue;
                    }
                    _ => return Ok(a),
                },
                _ => return Ok(a),
            }
            self.i += 1;
        }
    }

    fn generics(&mut self) -> Parsed<Vec<ParamA>> {
        let mut out = Vec::new();
        if !self.eat("<") {
            return Ok(out);
        }
        while !self.eat(">") {
            let name = self.ident()?;
            let bounds = if self.eat(":") { self.bounds()? } else { Vec::new() };
            out.push(ParamA { name, bounds });
            if !self.eat(",") {
                self.expect(">")?;
                break;
            }
        }
        Ok(out)
    }

    fn decl(&mut self) -> Parsed<DeclA> {
        let attrs = self.attrs()?;
        let kw = self.ident()?;
        match kw.as_str() {
            "trait" => {
                let name = self.ident()?;
                let params = self.generics()?;
                let supers = if self.eat(":") { self.bounds()? } else { Vec::new() };
                let mut wheres = Vec::new();
                if self.eat_word("where") {
                    loop {
                        wheres.push(self.pred()?);
                        if !self.eat(";") || self.done() || self.is("{") {
                            break;
                        }
                    }
                }
                let mut items = Vec::new();
                if self.eat("{") {
                    while !self.eat("}") {
                        let lang = if self.eat_word("lang") {
                            self.expect("(")?;
                            let l = self.ident()?;
                            self.expect(")")?;
                            Some(l)
                        } else {
                            None
                        };
                        let is_const = if self.eat_word("const") {
                            true
                        } else if self.eat_word("type") {
                            false
                        } else {
                            return Err(self.err("expected `type` or `const`"));
                        };
                        let name = self.ident()?;
                        let params = self.generics()?;
                        let mut bounds = Vec::new();
                        let mut ty = None;
                        if self.eat(":") {
                            if is_const {
                                ty = Some(self.ty()?);
                            } else {
                                bounds = self.bounds()?;
                            }
                        }
                        let default = if self.eat("=") { Some(self.ty()?) } else { None };
                        self.expect(";")?;
                        items.push(TraitItemA { lang, is_const, name, params, bounds, ty, default });
                    }
                }
                Ok(DeclA::Trait { attrs, name, params, supers, wheres, items })
            }
            "struct" | "enum" => {
                let is_enum = kw == "enum";
                let name = self.ident()?;
                let params = self.generics()?;
                let mut variants = Vec::new();
                if is_enum {
                    self.expect("{")?;
                    let mut cur = Vec::new();
                    loop {
                        if self.eat("}") {
                            variants.push(cur);
                            break;
                        }
                        if self.eat("|") {
                            variants.push(core::mem::take(&mut cur));
                            continue;
                        }
                        cur.push(self.ty()?);
                        self.eat(",");
                    }
                } else {
                    let mut fields = Vec::new();
                    if self.eat("(") {
                        while !self.eat(")") {
                            fields.push(self.ty()?);
                            if !self.eat(",") {
                                self.expect(")")?;
                                break;
                            }
                        }
                    }
                    variants.push(fields);
                }
                Ok(DeclA::Adt { attrs, name, params, is_enum, variants })
            }
            "impl" => {
                let label = if self.eat("@") { Some(self.ident()?) } else { None };
                let params = self.generics()?;
                let negative = self.eat("!");
                let trait_ = self.ident()?;
                let (args, assoc) = self.args()?;
                if !assoc.is_empty() {
                    return Err(self.err("an impl's trait takes no associated bindings"));
                }
                if !self.eat_word("for") {
                    return Err(self.err("expected `for`"));
                }
                let self_ty = self.ty()?;
                let mut wheres = Vec::new();
                if self.eat_word("where") {
                    loop {
                        wheres.push(self.pred()?);
                        if !self.eat(";") || self.done() || self.is("{") {
                            break;
                        }
                    }
                }
                let mut items = Vec::new();
                if self.eat("{") {
                    while !self.eat("}") {
                        let is_default = self.eat_word("default");
                        let is_const = if self.eat_word("const") {
                            true
                        } else if self.eat_word("type") {
                            false
                        } else {
                            return Err(self.err("expected `type` or `const`"));
                        };
                        let name = self.ident()?;
                        let params = self.generics()?;
                        if is_const {
                            self.expect(":")?;
                            self.ty()?;
                        }
                        self.expect("=")?;
                        let value = self.ty()?;
                        self.expect(";")?;
                        items.push(ImplItemA { is_default, is_const, name, params, value });
                    }
                }
                Ok(DeclA::Impl { attrs, label, params, negative, trait_, args, self_ty, wheres, items })
            }
            "fn" => {
                let name = self.ident()?;
                let params = self.generics()?;
                self.expect("(")?;
                let mut inputs = Vec::new();
                while !self.eat(")") {
                    inputs.push(self.ty()?);
                    if !self.eat(",") {
                        self.expect(")")?;
                        break;
                    }
                }
                let output = if self.eat("->") { self.ty()? } else { TyA::Tuple(Vec::new()) };
                Ok(DeclA::Fn { attrs, name, params, inputs, output })
            }
            "opaque" => {
                let name = self.ident()?;
                let params = self.generics()?;
                self.expect(":")?;
                let bounds = self.bounds()?;
                let hidden = if self.eat("=") { Some(self.ty()?) } else { None };
                Ok(DeclA::Opaque { name, params, bounds, hidden })
            }
            "foreign" => Ok(DeclA::Foreign { attrs, name: self.ident()? }),
            _ => Err(self.err("unknown declaration")),
        }
    }

    /// `B + B + ..` (stops before anything else).
    fn bounds(&mut self) -> Parsed<Vec<BoundA>> {
        let mut out = Vec::new();
        loop {
            out.push(self.bound()?);
            if !self.eat("+") {
                return Ok(out);
            }
        }
    }

    fn bound(&mut self) -> Parsed<BoundA> {
        if self.eat("?") {
            let t = self.ident()?;
            if t != "Sized" {
                return Err(self.err("only `?Sized`"));
            }
            return Ok(BoundA { negative: false, maybe: true, trait_: t, args: Vec::new(), assoc: Vec::new(), assoc_bounds: Vec::new() });
        }
        let negative = self.eat("!");
        let trait_ = self.ident()?;
        let (args, assoc, assoc_bounds) = self.bound_args()?;
        Ok(BoundA { negative, maybe: false, trait_, args, assoc, assoc_bounds })
    }

    /// `<A, B, Name = T>` or nothing (no `Name: Bounds`: that is a bound's only).
    fn args(&mut self) -> Parsed<(Vec<TyA>, Vec<(String, TyA)>)> {
        let (args, assoc, bounds) = self.bound_args()?;
        if !bounds.is_empty() {
            return Err(self.err("an associated item constraint with bounds (`Name: B`) outside a bound"));
        }
        Ok((args, assoc))
    }

    /// `<A, B, Name = T, Name: B + B>` or nothing: a bound's arguments.
    fn bound_args(&mut self) -> Parsed<BoundArgs> {
        let mut args = Vec::new();
        let mut assoc = Vec::new();
        let mut assoc_bounds = Vec::new();
        if !self.eat("<") {
            return Ok((args, assoc, assoc_bounds));
        }
        while !self.eat(">") {
            let named = matches!(self.peek(), Some(Tok::Ident(_)));
            let binding = named && matches!(self.peek_at(1), Some(Tok::P("=")));
            let constraint = named && matches!(self.peek_at(1), Some(Tok::P(":")));
            if binding {
                let n = self.ident()?;
                self.expect("=")?;
                assoc.push((n, self.ty()?));
            } else if constraint {
                let n = self.ident()?;
                self.expect(":")?;
                assoc_bounds.push((n, self.bounds()?));
            } else {
                args.push(self.ty()?);
            }
            if !self.eat(",") {
                self.expect(">")?;
                break;
            }
        }
        Ok((args, assoc, assoc_bounds))
    }

    fn ty_args(&mut self) -> Parsed<Vec<TyA>> {
        let (a, b) = self.args()?;
        if !b.is_empty() {
            return Err(self.err("associated bindings outside a bound"));
        }
        Ok(a)
    }

    fn lifetime(&mut self) -> Parsed<Lt> {
        let n = self.ident()?;
        Ok(if n == "static" { Lt::Static } else { Lt::Named(n) })
    }

    pub fn ty(&mut self) -> Parsed<TyA> {
        enum Frame {
            Ref(Lt, Mutbl),
            Ptr(Mutbl),
            Rigid,
            Tuple { start: usize },
            Bracket,
            FnPtrIn { start: usize },
            FnPtrOut { start: usize },
        }
        let mut stack: [Option<Frame>; 64] = [const { None }; 64];
        let mut sp = 0usize;
        let mut buf: Vec<TyA> = Default::default();
        let push_frame = |stack: &mut [Option<Frame>; 64], sp: &mut usize, f: Frame| -> Result<(), ()> {
            let Some(slot) = stack.get_mut(*sp) else {
                return Err(());
            };
            *slot = Some(f);
            *sp += 1;
            Ok(())
        };
        'parse: loop {
            if self.eat("&") {
                let lt = if self.eat("'") { self.lifetime()? } else { Lt::Erased };
                let m = if self.eat_word("mut") { Mutbl::Mut } else { Mutbl::Not };
                push_frame(&mut stack, &mut sp, Frame::Ref(lt, m)).map_err(|()| self.err("type nesting depth exceeded"))?;
                continue;
            }
            if self.eat("*") {
                let m = if self.eat_word("mut") {
                    Mutbl::Mut
                } else if self.eat_word("const") {
                    Mutbl::Not
                } else {
                    return Err(self.err("expected `const` or `mut`"));
                };
                push_frame(&mut stack, &mut sp, Frame::Ptr(m)).map_err(|()| self.err("type nesting depth exceeded"))?;
                continue;
            }
            let mut val = if self.eat("'") {
                TyA::Region(self.lifetime()?)
            } else if self.eat("(") {
                if self.eat(")") {
                    TyA::Tuple(Default::default())
                } else {
                    let start = buf.len();
                    push_frame(&mut stack, &mut sp, Frame::Tuple { start }).map_err(|()| self.err("type nesting depth exceeded"))?;
                    continue;
                }
            } else if self.eat("[") {
                push_frame(&mut stack, &mut sp, Frame::Bracket).map_err(|()| self.err("type nesting depth exceeded"))?;
                continue;
            } else if self.eat("{") {
                let n = self.num()?;
                self.expect("}")?;
                TyA::Const(n)
            } else if self.eat("?") {
                TyA::Var(self.var_name()?)
            } else if self.eat("!") {
                if let Some(Tok::Num(_)) = self.peek() {
                    let n = u32::try_from(self.num()?).map_err(|_| self.err("placeholder index out of range"))?;
                    TyA::Placeholder(n)
                } else {
                    TyA::Never
                }
            } else if self.eat("_") {
                TyA::Error
            } else if self.eat("<") {
                self.proj(false)?
            } else {
                let w = self.ident()?;
                match w.as_str() {
                    "rigid" => {
                        if self.eat("<") {
                            self.proj(true)?
                        } else {
                            push_frame(&mut stack, &mut sp, Frame::Rigid).map_err(|()| self.err("type nesting depth exceeded"))?;
                            continue;
                        }
                    }
                    "bool" => TyA::Bool,
                    "char" => TyA::Char,
                    "str" => TyA::Str,
                    "i8" => TyA::Int(IntTy::I8),
                    "i16" => TyA::Int(IntTy::I16),
                    "i32" => TyA::Int(IntTy::I32),
                    "i64" => TyA::Int(IntTy::I64),
                    "i128" => TyA::Int(IntTy::I128),
                    "isize" => TyA::Int(IntTy::Isize),
                    "u8" => TyA::Uint(UintTy::U8),
                    "u16" => TyA::Uint(UintTy::U16),
                    "u32" => TyA::Uint(UintTy::U32),
                    "u64" => TyA::Uint(UintTy::U64),
                    "u128" => TyA::Uint(UintTy::U128),
                    "usize" => TyA::Uint(UintTy::Usize),
                    "f16" => TyA::Float(FloatTy::F16),
                    "f32" => TyA::Float(FloatTy::F32),
                    "f64" => TyA::Float(FloatTy::F64),
                    "f128" => TyA::Float(FloatTy::F128),
                    "fnmut" => TyA::Kind(crate::term::CLOSURE_FN_MUT),
                    "fnonce" => TyA::Kind(crate::term::CLOSURE_FN_ONCE),
                    "fn" => {
                        if self.eat("#") {
                            let n = self.ident()?;
                            let a = self.ty_args()?;
                            TyA::FnItem(n, a)
                        } else if !self.eat("(") {
                            TyA::Kind(crate::term::CLOSURE_FN)
                        } else if self.eat(")") {
                            if self.eat("->") {
                                let start = buf.len();
                                push_frame(&mut stack, &mut sp, Frame::FnPtrOut { start }).map_err(|()| self.err("type nesting depth exceeded"))?;
                                continue;
                            }
                            TyA::FnPtr(Default::default(), Box::new(TyA::Tuple(Default::default())))
                        } else {
                            let start = buf.len();
                            push_frame(&mut stack, &mut sp, Frame::FnPtrIn { start }).map_err(|()| self.err("type nesting depth exceeded"))?;
                            continue;
                        }
                    }
                    "closure" => {
                        self.expect("#")?;
                        let n = self.num()?;
                        let a = self.ty_args()?;
                        let [k, s, u]: [TyA; 3] = a.try_into().map_err(|_| self.err("closure#N<kind, sig, upvars>"))?;
                        TyA::Closure(n, Box::new(k), Box::new(s), Box::new(u))
                    }
                    "opaque" => {
                        self.expect("#")?;
                        let n = self.ident()?;
                        let a = self.ty_args()?;
                        TyA::Opaque(n, a, false)
                    }
                    "dyn" => {
                        let mut bs = Vec::new();
                        let mut lt = Lt::Static;
                        loop {
                            if self.eat("'") {
                                lt = self.lifetime()?;
                            } else {
                                bs.push(self.bound()?);
                            }
                            if !self.eat("+") {
                                break;
                            }
                        }
                        TyA::Dyn(bs, lt)
                    }
                    _ => {
                        let a = self.ty_args()?;
                        TyA::Path(w, a)
                    }
                }
            };
            while sp > 0 {
                sp -= 1;
                let Some(frame) = stack.get_mut(sp).and_then(Option::take) else { break };
                match frame {
                    Frame::Ref(lt, m) => val = TyA::Ref(lt, Box::new(val), m),
                    Frame::Ptr(m) => val = TyA::Ptr(Box::new(val), m),
                    Frame::Rigid => {
                        val = match val {
                            TyA::Opaque(n, a, _) => TyA::Opaque(n, a, true),
                            _ => return Err(self.err("`rigid` takes an alias")),
                        };
                    }
                    Frame::Tuple { start } => {
                        buf.push(val);
                        let trailing = self.eat(",");
                        if trailing && !self.eat(")") {
                            push_frame(&mut stack, &mut sp, Frame::Tuple { start }).map_err(|()| self.err("type nesting depth exceeded"))?;
                            continue 'parse;
                        }
                        if !trailing {
                            self.expect(")")?;
                        }
                        let mut items = buf.split_off(start);
                        val = if items.len() == 1 && !trailing { items.remove(0) } else { TyA::Tuple(items) };
                    }
                    Frame::Bracket => {
                        if self.eat(";") {
                            let n = if self.eat("{") {
                                let n = self.num()?;
                                self.expect("}")?;
                                n
                            } else {
                                self.num()?
                            };
                            self.expect("]")?;
                            val = TyA::Array(Box::new(val), Box::new(TyA::Const(n)));
                        } else {
                            self.expect("]")?;
                            val = TyA::Slice(Box::new(val));
                        }
                    }
                    Frame::FnPtrIn { start } => {
                        buf.push(val);
                        let comma = self.eat(",");
                        if comma && !self.eat(")") {
                            push_frame(&mut stack, &mut sp, Frame::FnPtrIn { start }).map_err(|()| self.err("type nesting depth exceeded"))?;
                            continue 'parse;
                        }
                        if !comma {
                            self.expect(")")?;
                        }
                        if self.eat("->") {
                            push_frame(&mut stack, &mut sp, Frame::FnPtrOut { start }).map_err(|()| self.err("type nesting depth exceeded"))?;
                            continue 'parse;
                        }
                        let ins = buf.split_off(start);
                        val = TyA::FnPtr(ins, Box::new(TyA::Tuple(Default::default())));
                    }
                    Frame::FnPtrOut { start } => {
                        let ins = buf.split_off(start);
                        val = TyA::FnPtr(ins, Box::new(val));
                    }
                }
            }
            return Ok(val);
        }
    }

    /// After `<`: `Ty as Trait<args>>::Name<own>`.
    fn proj(&mut self, rigid: bool) -> Parsed<TyA> {
        let self_ty = self.ty()?;
        if !self.eat_word("as") {
            return Err(self.err("expected `as`"));
        }
        let trait_ = self.ident()?;
        let args = self.ty_args()?;
        self.expect(">")?;
        self.expect("::")?;
        let name = self.ident()?;
        let own = self.ty_args()?;
        Ok(TyA::Proj { self_ty: Box::new(self_ty), trait_, args, name, own, rigid })
    }

    pub fn pred(&mut self) -> Parsed<PredA> {
        let mut binders = [const { None }; 16];
        let mut nb = 0usize;
        while self.eat_word("for") {
            let Some(slot) = binders.get_mut(nb) else {
                return Err(self.err("nested `for<..>` binder depth exceeded"));
            };
            self.expect("<")?;
            let mut names = Vec::new();
            while !self.eat(">") {
                names.push(self.ident()?);
                if !self.eat(",") {
                    self.expect(">")?;
                    break;
                }
            }
            *slot = Some(names);
            nb += 1;
        }
        let mut p = if self.is_word("wf") && matches!(self.peek_at(1), Some(Tok::P("("))) {
            self.i += 2;
            let t = self.ty()?;
            self.expect(")")?;
            PredA::Wf(t)
        } else if self.eat_word("const") {
            let name = self.ident()?;
            self.expect("(")?;
            let mut args = Vec::new();
            while !self.eat(")") {
                args.push(self.ty()?);
                if !self.eat(",") {
                    self.expect(")")?;
                    break;
                }
            }
            PredA::Const(name, args)
        } else if self.is_word("normalizes") && matches!(self.peek_at(1), Some(Tok::P("("))) {
            self.i += 2;
            let a = self.ty()?;
            self.expect(")")?;
            self.expect("==")?;
            PredA::Normalizes(a, self.ty()?)
        } else {
            let a = self.ty()?;
            if self.eat("==") {
                PredA::Eq(a, self.ty()?)
            } else if self.eat("<:") {
                PredA::Sub(a, self.ty()?)
            } else {
                self.expect(":")?;
                PredA::Bound(a, self.bound()?)
            }
        };
        while nb > 0 {
            nb -= 1;
            if let Some(names) = binders.get_mut(nb).and_then(Option::take) {
                p = PredA::ForAll(names, Box::new(p));
            }
        }
        Ok(p)
    }
}
