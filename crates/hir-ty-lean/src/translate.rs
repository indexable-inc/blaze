//! The v0 functional translation: structured MIR ([`crate::body`]) to a Lean model, the way
//! Aeneas translates LLBC.
//!
//! - **A local is a Lean variable**, rebound by `let` each time it is assigned, so at every
//!   point the variable in scope holds the local's value. A write of a part of a local
//!   (`x.f = v`, `(*p).f = v`, `xs[i] = v`) rebinds the local to an updated copy.
//! - **A reference is the value.** A `&mut` parameter comes in as a value and goes out with
//!   the result: `fn f(x: &mut T) -> R` is `f (x : T) : R × T` (just `T` when `R` is `()`).
//!   A call lends by writing what comes back into the place lent.
//! - **A branch or a labelled block nothing jumps out of is an expression** of the locals it
//!   changes that are live after it: `let (a, b) ← if c then .. else ..`. One that something
//!   jumps out of (a `return`, a `break` of an outer label) takes what follows it into the
//!   arms that go on.
//! - **A loop is a recursive function** of the locals live at its head, `f.loop_N`, taking
//!   `fuel : Nat` and recursing on it: out of fuel is `Result.div`. So a function with a loop
//!   (or calling one) takes `fuel` too, and what is proved of it is partial correctness. A
//!   `continue` is the call; a `break` gives what the block around the loop gives.
//! - **What can fail is in `Result`**: checked arithmetic, division, an index, a panic, a
//!   loop. A function with none of these is a plain Lean function (the plan's "Effects make
//!   models simpler", decided here from the body: no fallible operation is left in it).
//!
//! A local that holds no binding and is used once, by the next statement, is written where it
//! is used; a local never used is not written. Nothing else is simplified: the model follows
//! the MIR statement by statement.

use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use crate::{
    NotTranslated,
    body::{
        AdtKind, BinOp, Body, Callee, Cond, Elem, IntTy, Local, OpTy, Operand, Place,
        Rvalue, Stmt, Ty, Variant,
    },
    lean::{self, Binder, Decl, DoElem, Infix, Pat, Term, app, atom, infix, name},
    structured::{End, Node},
};

type Nodes = [Node<Stmt, Cond>];
type Set = BTreeSet<Local>;
type Refused<T> = Result<T, NotTranslated>;

/// How a function's model is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    /// It takes `fuel : Nat` first: it has a loop, or calls a function that does.
    pub fuel: bool,
    /// It gives a `Result`.
    pub monadic: bool,
    /// Its `&mut` parameters, by position: their final values follow its result.
    pub mut_params: Vec<usize>,
    /// It returns `()`, which the model leaves out when it gives `&mut` values back.
    pub returns_unit: bool,
}

/// A function's model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// The types, the loops, then the function.
    pub decls: Vec<Decl>,
    pub shape: Shape,
    /// The function's parameters, without `fuel`.
    pub params: Vec<Binder>,
    /// What the model gives, without `Result`.
    pub result: Term,
}

/// The Lean type of `ty`.
pub fn lean_ty(ty: &Ty) -> Term {
    match ty {
        Ty::Bool => name("Bool"),
        Ty::Int(int) => name(int.lean()),
        Ty::Unit | Ty::Never => name("Unit"),
        Ty::Tuple(fields) => product(fields.iter().map(lean_ty).collect()),
        Ty::Adt(adt) => name(adt.clone()),
        Ty::Option(inner) => app("Option", vec![lean_ty(inner)]),
        Ty::Result(ok, err) => app("core.result.Result", vec![lean_ty(ok), lean_ty(err)]),
        Ty::Ref(inner) | Ty::RefMut(inner) => lean_ty(inner),
        Ty::Slice(elem) => app("Slice", vec![lean_ty(elem)]),
        Ty::Array(elem, len) => app("Arr", vec![lean_ty(elem), atom(len.to_string())]),
    }
}

/// `A × B × C`; `Unit` of none.
fn product(mut types: Vec<Term>) -> Term {
    let Some(mut out) = types.pop() else { return name("Unit") };
    while let Some(ty) = types.pop() {
        out = infix(Infix::Prod, ty, out);
    }
    out
}

fn tuple(mut terms: Vec<Term>) -> Term {
    match terms.len() {
        0 => atom("()"),
        1 => terms.remove(0),
        _ => Term::Tuple(terms),
    }
}

fn tuple_pat(mut pats: Vec<Pat>) -> Pat {
    match pats.len() {
        0 => Pat::Wild,
        1 => pats.remove(0),
        _ => Pat::Tuple(pats),
    }
}

/// An integer constant: `(5 : U64)`; a negative one by `Scalar.lit`, which needs no `Neg`.
fn int_lit(ty: IntTy, bits: u128) -> Term {
    let text = ty.decimal(bits);
    if text.starts_with('-') {
        app("Scalar.lit", vec![atom(format!(".{}", ty.lean())), atom(text)])
    } else {
        Term::Typed(Box::new(atom(text)), Box::new(name(ty.lean())))
    }
}

/// A computation, before it is known whether it is in `Result`.
#[derive(Clone, Debug)]
enum M {
    Ret(Term),
    /// `Result.fail Error.<this>`.
    Fail(&'static str),
    /// A term that is the computation's `Result`.
    Tail(Term),
    Let { pat: Pat, value: Rhs, body: Box<M> },
    If { cond: Term, then: Box<M>, otherwise: Box<M> },
    Match { on: Term, arms: Vec<(Pat, M)> },
}

#[derive(Clone, Debug)]
enum Rhs {
    Pure(Term),
    /// A term in `Result`.
    Act(Term),
    Sub(Box<M>),
}

fn is_pure(m: &M) -> bool {
    match m {
        M::Ret(_) => true,
        M::Fail(_) | M::Tail(_) => false,
        M::Let { value, body, .. } => {
            let value = match value {
                Rhs::Pure(_) => true,
                Rhs::Act(_) => false,
                Rhs::Sub(sub) => is_pure(sub),
            };
            value && is_pure(body)
        }
        M::If { then, otherwise, .. } => is_pure(then) && is_pure(otherwise),
        M::Match { arms, .. } => arms.iter().all(|(_, arm)| is_pure(arm)),
    }
}

fn binds(pat: &Pat, x: &str) -> bool {
    match pat {
        Pat::Name(it) => it == x,
        Pat::Tuple(all) | Pat::Ctor(_, all) => all.iter().any(|it| binds(it, x)),
        Pat::Wild | Pat::Atom(_) => false,
    }
}

/// How often the variable `x` is read in `term`. The terms a body is made of bind nothing.
fn occurs(term: &Term, x: &str) -> usize {
    match term {
        Term::Name(it) => usize::from(it == x),
        Term::Atom(_) => 0,
        Term::App(function, args) => {
            occurs(function, x) + args.iter().map(|it| occurs(it, x)).sum::<usize>()
        }
        Term::Proj(inner, _) | Term::BoolNot(inner) | Term::Not(inner) => occurs(inner, x),
        Term::Tuple(all) => all.iter().map(|it| occurs(it, x)).sum(),
        Term::Typed(value, _) => occurs(value, x),
        Term::Update(base, fields) => {
            occurs(base, x) + fields.iter().map(|(_, it)| occurs(it, x)).sum::<usize>()
        }
        Term::Structure(fields) => fields.iter().map(|(_, it)| occurs(it, x)).sum(),
        Term::Infix(_, left, right) => occurs(left, x) + occurs(right, x),
        Term::If(a, b, c) => occurs(a, x) + occurs(b, x) + occurs(c, x),
        // Not made by a body's expressions; counted as many so nothing is put into them.
        Term::Match(..) | Term::Let(..) | Term::Do(_) | Term::Forall(..) => 2,
    }
}

fn substitute(term: &mut Term, x: &str, with: &Term) {
    match term {
        Term::Name(it) => {
            if it == x {
                *term = with.clone();
            }
        }
        Term::Atom(_) => {}
        Term::App(function, args) => {
            substitute(function, x, with);
            args.iter_mut().for_each(|it| substitute(it, x, with));
        }
        Term::Proj(inner, _) | Term::BoolNot(inner) | Term::Not(inner) => {
            substitute(inner, x, with);
        }
        Term::Tuple(all) => all.iter_mut().for_each(|it| substitute(it, x, with)),
        Term::Typed(value, _) => substitute(value, x, with),
        Term::Update(base, fields) => {
            substitute(base, x, with);
            fields.iter_mut().for_each(|(_, it)| substitute(it, x, with));
        }
        Term::Structure(fields) => fields.iter_mut().for_each(|(_, it)| substitute(it, x, with)),
        Term::Infix(_, left, right) => {
            substitute(left, x, with);
            substitute(right, x, with);
        }
        Term::If(a, b, c) => {
            substitute(a, x, with);
            substitute(b, x, with);
            substitute(c, x, with);
        }
        Term::Match(..) | Term::Let(..) | Term::Do(_) | Term::Forall(..) => {}
    }
}

/// How often `x` is read in `m`, up to where it is bound again.
fn reads(m: &M, x: &str) -> usize {
    match m {
        M::Ret(term) | M::Tail(term) => occurs(term, x),
        M::Fail(_) => 0,
        M::Let { pat, value, body } => {
            let value = match value {
                Rhs::Pure(term) | Rhs::Act(term) => occurs(term, x),
                Rhs::Sub(sub) => reads(sub, x),
            };
            value + if binds(pat, x) { 0 } else { reads(body, x) }
        }
        M::If { cond, then, otherwise } => occurs(cond, x) + reads(then, x) + reads(otherwise, x),
        M::Match { on, arms } => {
            occurs(on, x)
                + arms
                    .iter()
                    .map(|(pat, arm)| if binds(pat, x) { 0 } else { reads(arm, x) })
                    .sum::<usize>()
        }
    }
}

/// The term `m` evaluates first, if it is one.
fn head(m: &mut M) -> Option<&mut Term> {
    match m {
        M::Ret(term) | M::Tail(term) => Some(term),
        M::Let { value: Rhs::Pure(term) | Rhs::Act(term), .. } => Some(term),
        M::If { cond, .. } => Some(cond),
        M::Match { on, .. } => Some(on),
        M::Fail(_) | M::Let { value: Rhs::Sub(_), .. } => None,
    }
}

fn pat_term(pat: &Pat) -> Option<Term> {
    match pat {
        Pat::Name(it) => Some(name(it.clone())),
        Pat::Tuple(all) => Some(Term::Tuple(all.iter().map(pat_term).collect::<Option<_>>()?)),
        _ => None,
    }
}

/// The simplifications the module names; `temps` are the variables that hold no binding.
fn simplify(m: M, temps: &BTreeSet<String>) -> M {
    match m {
        M::Let { pat, value, body } => {
            let mut body = simplify(*body, temps);
            let value = match value {
                Rhs::Sub(sub) => match simplify(*sub, temps) {
                    M::Ret(term) => Rhs::Pure(term),
                    M::Tail(term) => Rhs::Act(term),
                    sub => Rhs::Sub(Box::new(sub)),
                },
                other => other,
            };
            let nothing = match &value {
                Rhs::Pure(_) => true,
                Rhs::Sub(sub) => is_pure(sub),
                Rhs::Act(_) => false,
            };
            if nothing && matches!(pat, Pat::Wild) {
                return body;
            }
            if let (Pat::Name(x), Rhs::Pure(term)) = (&pat, &value) {
                let count = reads(&body, x);
                if count == 0 {
                    return body;
                }
                if count == 1
                    && temps.contains(x)
                    && let Some(first) = head(&mut body)
                    && occurs(first, x) == 1
                {
                    substitute(first, x, term);
                    return body;
                }
            }
            // `let p ← e; ok p` is `e`.
            if let M::Ret(result) = &body
                && pat_term(&pat).as_ref() == Some(result)
            {
                match value {
                    Rhs::Act(term) => return M::Tail(term),
                    Rhs::Sub(sub) => return *sub,
                    Rhs::Pure(term) => return M::Ret(term),
                }
            }
            M::Let { pat, value, body: Box::new(body) }
        }
        M::If { cond, then, otherwise } => M::If {
            cond,
            then: Box::new(simplify(*then, temps)),
            otherwise: Box::new(simplify(*otherwise, temps)),
        },
        M::Match { on, arms } => M::Match {
            on,
            arms: arms.into_iter().map(|(pat, arm)| (pat, simplify(arm, temps))).collect(),
        },
        other => other,
    }
}

/// `m` as a Lean term: in `Result` when `monadic`.
fn emit(m: &M, monadic: bool) -> Term {
    let mut elems = Vec::new();
    let mut at = m;
    while let M::Let { pat, value, body } = at {
        elems.push(match value {
            Rhs::Pure(term) => DoElem::Let(pat.clone(), term.clone()),
            Rhs::Act(term) => DoElem::Bind(pat.clone(), term.clone()),
            Rhs::Sub(sub) if is_pure(sub) => DoElem::Let(pat.clone(), emit(sub, false)),
            Rhs::Sub(sub) => DoElem::Bind(pat.clone(), emit(sub, true)),
        });
        at = body;
    }
    let last = match at {
        M::Ret(term) if monadic => app("Result.ok", vec![term.clone()]),
        M::Ret(term) | M::Tail(term) => term.clone(),
        M::Fail(error) => app("Result.fail", vec![name(format!("Error.{error}"))]),
        M::If { cond, then, otherwise } => Term::If(
            Box::new(cond.clone()),
            Box::new(emit(then, monadic)),
            Box::new(emit(otherwise, monadic)),
        ),
        M::Match { on, arms } => Term::Match(
            Box::new(on.clone()),
            arms.iter().map(|(pat, arm)| (pat.clone(), emit(arm, monadic))).collect(),
        ),
        M::Let { .. } => unreachable!("the loop above ends at what is not a `let`"),
    };
    if elems.iter().any(|it| matches!(it, DoElem::Bind(..))) {
        elems.push(DoElem::Term(last));
        return Term::Do(elems);
    }
    elems.into_iter().rev().fold(last, |body, elem| match elem {
        DoElem::Let(pat, value) => Term::Let(pat, Box::new(value), Box::new(body)),
        DoElem::Bind(..) | DoElem::Term(_) => unreachable!("only pure `let`s are left"),
    })
}

/// Whether a jump in `nodes` leaves them: a `return`, or a jump to a label `depth` or more
/// levels out.
fn escapes(nodes: &Nodes, depth: u32) -> bool {
    nodes.iter().any(|node| match node {
        Node::Return => true,
        Node::Break(to) | Node::Continue(to) => *to >= depth,
        Node::Branch { arms, otherwise, .. } => {
            arms.iter().any(|(_, arm)| escapes(arm, depth)) || escapes(otherwise, depth)
        }
        Node::Block(body) | Node::Loop(body) => escapes(body, depth + 1),
        Node::Do(_) | Node::End(_) => false,
    })
}

fn changed(nodes: &Nodes, out: &mut Set) {
    for node in nodes {
        match node {
            Node::Do(statement) => statement.changes(out),
            Node::Branch { arms, otherwise, .. } => {
                arms.iter().for_each(|(_, arm)| changed(arm, out));
                changed(otherwise, out);
            }
            Node::Block(body) | Node::Loop(body) => changed(body, out),
            Node::Break(_) | Node::Continue(_) | Node::Return | Node::End(_) => {}
        }
    }
}

/// What comes after the code being translated.
#[derive(Clone)]
enum Then<'a> {
    /// The function returns.
    Return,
    /// The code is an expression of these locals.
    Yield(Vec<Local>),
    /// These nodes, under the labels in scope there (`depth` of them), then `then`.
    Seq { rest: &'a Nodes, then: Rc<Then<'a>>, live: Rc<Set>, depth: usize },
    /// The loop's body ended: the loop runs again.
    Again { name: String, params: Vec<Local> },
}

enum Label<'a> {
    /// A `Break` of it does `then`, before which `live` are live.
    Block { then: Then<'a>, live: Set },
    Loop { name: String, params: Vec<Local>, live: Set },
}

impl Label<'_> {
    fn live(&self) -> &Set {
        match self {
            Label::Block { live, .. } | Label::Loop { live, .. } => live,
        }
    }
}

struct Translate<'a> {
    body: &'a Body,
    shapes: &'a BTreeMap<String, Shape>,
    /// Each local's Lean variable.
    names: Vec<String>,
    temps: BTreeSet<String>,
    labels: Vec<Label<'a>>,
    /// The labels below this index are outside the loop function being written: a jump to a
    /// loop among them would make the two loops call each other.
    boundary: usize,
    /// The type of what the code being translated computes.
    result: Term,
    loops: Vec<Decl>,
    loop_count: usize,
    fresh: usize,
    /// The fields a `match` arm in scope binds: of this variant of the enum at this place.
    bound: Vec<(Place, u32, Vec<String>)>,
    uses_fuel: bool,
    /// What `return` gives: the result unless it is `()`, then each `&mut` parameter.
    returned: Vec<Local>,
}

impl<'a> Translate<'a> {
    fn refuse(&self, what: impl Into<String>, at: &str) -> NotTranslated {
        NotTranslated {
            function: self.body.display.clone(),
            what: what.into(),
            at: if at.is_empty() { format!("fn {}", self.body.display) } else { at.to_owned() },
        }
    }

    fn var(&self, local: Local) -> Term {
        name(self.names[local as usize].clone())
    }

    fn fresh(&mut self, prefix: &str) -> String {
        loop {
            let candidate = format!("{prefix}{}", self.fresh);
            self.fresh += 1;
            if !self.names.contains(&candidate) {
                self.temps.insert(candidate.clone());
                return candidate;
            }
        }
    }

    fn vars(&self, locals: &[Local]) -> Term {
        tuple(locals.iter().map(|it| self.var(*it)).collect())
    }

    fn types(&self, locals: &[Local]) -> Term {
        product(locals.iter().map(|it| lean_ty(&self.body.locals[*it as usize].ty)).collect())
    }

    /// The locals live before `nodes`, `out` being live after them and `labels` at each label
    /// in scope (innermost last).
    fn live(&self, nodes: &Nodes, out: &Set, labels: &mut Vec<Set>) -> Set {
        let mut live = out.clone();
        for node in nodes.iter().rev() {
            match node {
                Node::Do(statement) => {
                    if let Some(written) = statement.writes() {
                        live.remove(&written);
                    }
                    statement.reads(&mut live);
                }
                Node::Branch { on, arms, otherwise } => {
                    let mut before = self.live(otherwise, &live, labels);
                    for (_, arm) in arms {
                        before.extend(self.live(arm, &live, labels));
                    }
                    match on {
                        Cond::Operand(Operand::Place(place), _) | Cond::Discriminant(place, _) => {
                            place.reads(&mut before);
                        }
                        Cond::Operand(..) => {}
                    }
                    live = before;
                }
                Node::Block(body) => {
                    labels.push(live.clone());
                    live = self.live(body, &live, labels);
                    labels.pop();
                }
                Node::Loop(body) => {
                    // Grows until it is the set live at the head; the locals bound it.
                    let mut head = Set::new();
                    loop {
                        labels.push(head.clone());
                        let mut next = self.live(body, &head, labels);
                        labels.pop();
                        next.extend(head.iter().copied());
                        if next == head {
                            break;
                        }
                        head = next;
                    }
                    live = head;
                }
                Node::Break(to) | Node::Continue(to) => {
                    live = match labels.len().checked_sub(1 + *to as usize) {
                        Some(at) => labels[at].clone(),
                        None => Set::new(),
                    };
                }
                Node::Return => live = self.returned.iter().copied().collect(),
                Node::End(_) => live = Set::new(),
            }
        }
        live
    }

    fn live_here(&self, nodes: &Nodes, out: &Set) -> Set {
        let mut labels: Vec<Set> = self.labels.iter().map(|it| it.live().clone()).collect();
        self.live(nodes, out, &mut labels)
    }

    fn ty_of(&self, place: &Place, upto: usize) -> Ty {
        let mut ty = self.body.locals[place.local as usize].ty.clone();
        for elem in &place.elems[..upto] {
            ty = self.elem_ty(&ty, elem);
        }
        ty
    }

    fn elem_ty(&self, ty: &Ty, elem: &Elem) -> Ty {
        let ty = if matches!(elem, Elem::Deref) { ty } else { ty.value() };
        match (elem, ty) {
            (Elem::Deref, Ty::Ref(inner) | Ty::RefMut(inner)) => (**inner).clone(),
            (Elem::TupleField { index, .. }, Ty::Tuple(fields)) => fields[*index as usize].clone(),
            (Elem::Field(field), Ty::Adt(adt)) => match self.body.adt(adt).map(|it| &it.kind) {
                Some(AdtKind::Struct(fields)) => fields
                    .iter()
                    .find(|it| it.name == *field)
                    .map_or(Ty::Never, |it| it.ty.clone()),
                _ => Ty::Never,
            },
            (Elem::VariantField { variant, index }, ty) => self
                .body
                .variants(ty)
                .and_then(|all| all.get(*variant as usize).cloned())
                .and_then(|it| it.fields.get(*index as usize).cloned())
                .map_or(Ty::Never, |it| it.ty),
            (Elem::Index(_) | Elem::ConstIndex(_), Ty::Slice(elem) | Ty::Array(elem, _)) => {
                (**elem).clone()
            }
            _ => Ty::Never,
        }
    }

    /// Field `index` of the `arity`-tuple `base`: Lean's tuples nest to the right.
    fn tuple_field(base: Term, index: u32, arity: u32) -> Term {
        let mut term = base;
        for _ in 0..index {
            term = Term::Proj(Box::new(term), "2".to_owned());
        }
        if index + 1 < arity { Term::Proj(Box::new(term), "1".to_owned()) } else { term }
    }

    fn index_term(&self, elem: &Elem) -> Term {
        match elem {
            Elem::Index(by) => self.var(*by),
            Elem::ConstIndex(at) => int_lit(IntTy::Usize, *at as u128),
            _ => unreachable!("asked of an index"),
        }
    }

    fn container(&self, ty: &Ty) -> &'static str {
        if matches!(ty.value(), Ty::Array(..)) { "Arr" } else { "Slice" }
    }

    /// The value at `place`; an index in it is read by a `let` put in `lets`.
    fn read(&mut self, place: &Place, lets: &mut Vec<(Pat, Rhs)>, at: &str) -> Refused<Term> {
        let mut term = self.var(place.local);
        let mut ty = self.body.locals[place.local as usize].ty.clone();
        for (upto, elem) in place.elems.iter().enumerate() {
            term = match elem {
                Elem::Deref => term,
                Elem::TupleField { index, arity } => Self::tuple_field(term, *index, *arity),
                Elem::Field(field) => Term::Proj(Box::new(term), field.clone()),
                Elem::VariantField { variant, index } => {
                    let prefix = Place { local: place.local, elems: place.elems[..upto].to_vec() };
                    let found = self
                        .bound
                        .iter()
                        .rev()
                        .find(|(of, which, _)| *of == prefix && which == variant)
                        .and_then(|(_, _, fields)| fields.get(*index as usize));
                    match found {
                        Some(field) => name(field.clone()),
                        None => {
                            return Err(self.refuse(
                                "a field of an enum's variant read outside the `match` arm of \
                                 that variant, or after the enum was assigned",
                                at,
                            ));
                        }
                    }
                }
                Elem::Index(_) | Elem::ConstIndex(_) => {
                    let read = format!("{}.index", self.container(&ty));
                    let x = self.fresh("x");
                    let value = app(&read, vec![term, self.index_term(elem)]);
                    lets.push((Pat::Name(x.clone()), Rhs::Act(value)));
                    name(x)
                }
            };
            ty = self.elem_ty(&ty, elem);
        }
        Ok(term)
    }

    /// `base` (of type `ty`) with `value` at `elems` in it.
    fn updated(
        &mut self,
        base: Term,
        ty: &Ty,
        elems: &[Elem],
        value: Term,
        lets: &mut Vec<(Pat, Rhs)>,
        at: &str,
    ) -> Refused<Term> {
        let Some((elem, rest)) = elems.split_first() else { return Ok(value) };
        let inner_ty = self.elem_ty(ty, elem);
        Ok(match elem {
            Elem::Deref => self.updated(base, &inner_ty, rest, value, lets, at)?,
            Elem::Field(field) => {
                let inner = Term::Proj(Box::new(base.clone()), field.clone());
                let inner = self.updated(inner, &inner_ty, rest, value, lets, at)?;
                Term::Update(Box::new(base), vec![(field.clone(), inner)])
            }
            Elem::TupleField { index, arity } => {
                let mut fields = Vec::with_capacity(*arity as usize);
                for field in 0..*arity {
                    let inner = Self::tuple_field(base.clone(), field, *arity);
                    fields.push(if field == *index {
                        self.updated(inner, &inner_ty, rest, value.clone(), lets, at)?
                    } else {
                        inner
                    });
                }
                tuple(fields)
            }
            Elem::Index(_) | Elem::ConstIndex(_) => {
                let kind = self.container(ty);
                let by = self.index_term(elem);
                let value = if rest.is_empty() {
                    value
                } else {
                    let x = self.fresh("x");
                    let read = app(&format!("{kind}.index"), vec![base.clone(), by.clone()]);
                    lets.push((Pat::Name(x.clone()), Rhs::Act(read)));
                    self.updated(name(x), &inner_ty, rest, value, lets, at)?
                };
                let x = self.fresh("x");
                let write = app(&format!("{kind}.update"), vec![base, by, value]);
                lets.push((Pat::Name(x.clone()), Rhs::Act(write)));
                name(x)
            }
            Elem::VariantField { .. } => {
                return Err(self.refuse("an assignment to a field of an enum's variant", at));
            }
        })
    }

    /// Binds `place` to `value`.
    fn write(
        &mut self,
        place: &Place,
        value: Rhs,
        lets: &mut Vec<(Pat, Rhs)>,
        at: &str,
    ) -> Refused<()> {
        let root = Pat::Name(self.names[place.local as usize].clone());
        if place.elems.iter().all(|it| matches!(it, Elem::Deref)) {
            lets.push((root, value));
        } else {
            let value = match value {
                Rhs::Pure(term) => term,
                other => {
                    let x = self.fresh("x");
                    lets.push((Pat::Name(x.clone()), other));
                    name(x)
                }
            };
            let ty = self.body.locals[place.local as usize].ty.clone();
            let whole =
                self.updated(self.var(place.local), &ty, &place.elems, value, lets, at)?;
            lets.push((root, Rhs::Pure(whole)));
        }
        // What a `match` arm bound of the local is no longer what it holds.
        self.bound.retain(|(of, _, _)| of.local != place.local);
        Ok(())
    }

    fn operand(
        &mut self,
        operand: &Operand,
        lets: &mut Vec<(Pat, Rhs)>,
        at: &str,
    ) -> Refused<Term> {
        Ok(match operand {
            Operand::Place(place) => self.read(place, lets, at)?,
            Operand::Int(ty, bits) => int_lit(*ty, *bits),
            Operand::Bool(value) => atom(if *value { "true" } else { "false" }),
            Operand::Unit => atom("()"),
        })
    }

    /// The Lean constructor of `variant` of the enum type `ty`.
    fn constructor(&self, ty: &Ty, variant: &Variant) -> String {
        match ty {
            Ty::Adt(adt) => format!("{adt}.{}", variant.name),
            Ty::Option(_) => format!("Option.{}", variant.name),
            _ => format!("core.result.Result.{}", variant.name),
        }
    }

    fn rvalue(&mut self, value: &Rvalue, lets: &mut Vec<(Pat, Rhs)>, at: &str) -> Refused<Rhs> {
        Ok(match value {
            Rvalue::Use(of) => Rhs::Pure(self.operand(of, lets, at)?),
            Rvalue::Bin { op, ty: OpTy::Int(_), checked, left, right } => {
                let args = vec![self.operand(left, lets, at)?, self.operand(right, lets, at)?];
                let (function, fallible) = match (op, checked) {
                    (BinOp::Add, true) => ("Scalar.add", true),
                    (BinOp::Sub, true) => ("Scalar.sub", true),
                    (BinOp::Mul, true) => ("Scalar.mul", true),
                    (BinOp::Shl, true) => ("Scalar.shl", true),
                    (BinOp::Shr, true) => ("Scalar.shr", true),
                    (BinOp::Add, false) => ("Scalar.wrapping_add", false),
                    (BinOp::Sub, false) => ("Scalar.wrapping_sub", false),
                    (BinOp::Mul, false) => ("Scalar.wrapping_mul", false),
                    (BinOp::Shl, false) => ("Scalar.wrapping_shl", false),
                    (BinOp::Shr, false) => ("Scalar.wrapping_shr", false),
                    (BinOp::Div, _) => ("Scalar.div", true),
                    (BinOp::Rem, _) => ("Scalar.rem", true),
                    (BinOp::BitXor, _) => ("Scalar.xor", false),
                    (BinOp::BitAnd, _) => ("Scalar.and", false),
                    (BinOp::BitOr, _) => ("Scalar.or", false),
                    (BinOp::Eq, _) => ("Scalar.eq", false),
                    (BinOp::Ne, _) => ("Scalar.ne", false),
                    (BinOp::Lt, _) => ("Scalar.lt", false),
                    (BinOp::Le, _) => ("Scalar.le", false),
                    (BinOp::Gt, _) => ("Scalar.gt", false),
                    (BinOp::Ge, _) => ("Scalar.ge", false),
                };
                let term = app(function, args);
                if fallible { Rhs::Act(term) } else { Rhs::Pure(term) }
            }
            Rvalue::Bin { op, ty: OpTy::Bool, left, right, .. } => {
                let left = self.operand(left, lets, at)?;
                let right = self.operand(right, lets, at)?;
                let op = match op {
                    BinOp::BitAnd => Infix::BoolAnd,
                    BinOp::BitOr => Infix::BoolOr,
                    BinOp::BitXor | BinOp::Ne => Infix::BoolNe,
                    BinOp::Eq => Infix::BoolEq,
                    other => {
                        return Err(self.refuse(format!("`{other:?}` on `bool`s"), at));
                    }
                };
                Rhs::Pure(infix(op, left, right))
            }
            Rvalue::Not { ty: OpTy::Bool, of } => {
                Rhs::Pure(Term::BoolNot(Box::new(self.operand(of, lets, at)?)))
            }
            Rvalue::Not { ty: OpTy::Int(_), of } => {
                Rhs::Pure(app("Scalar.not", vec![self.operand(of, lets, at)?]))
            }
            Rvalue::Neg { checked, of, .. } => {
                let of = self.operand(of, lets, at)?;
                if *checked {
                    Rhs::Act(app("Scalar.neg", vec![of]))
                } else {
                    Rhs::Pure(app("Scalar.wrapping_neg", vec![of]))
                }
            }
            Rvalue::Cast { from, to, of } => {
                let to = atom(format!(".{}", to.lean()));
                let of = self.operand(of, lets, at)?;
                Rhs::Pure(match from {
                    OpTy::Int(_) => app("Scalar.cast", vec![to, of]),
                    OpTy::Bool => app("Scalar.ofBool", vec![to, of]),
                })
            }
            Rvalue::Len(place) => Rhs::Pure(app("Slice.len", vec![self.read(place, lets, at)?])),
            Rvalue::Discriminant(..) => {
                return Err(self.refuse("a discriminant read that no `match` follows", at));
            }
            Rvalue::Tuple(all) => {
                let mut terms = Vec::with_capacity(all.len());
                for of in all {
                    terms.push(self.operand(of, lets, at)?);
                }
                Rhs::Pure(tuple(terms))
            }
            Rvalue::Struct { adt, fields } => {
                let mut terms = Vec::with_capacity(fields.len());
                for (field, of) in fields {
                    terms.push((field.clone(), self.operand(of, lets, at)?));
                }
                Rhs::Pure(Term::Typed(
                    Box::new(Term::Structure(terms)),
                    Box::new(name(adt.clone())),
                ))
            }
            Rvalue::Variant { ty, variant, args } => {
                let Some(found) =
                    self.body.variants(ty).and_then(|all| all.get(*variant as usize).cloned())
                else {
                    return Err(self.refuse("a variant its enum does not have", at));
                };
                let mut terms = Vec::with_capacity(args.len());
                for of in args {
                    terms.push(self.operand(of, lets, at)?);
                }
                let value = app(&self.constructor(ty, &found), terms);
                Rhs::Pure(Term::Typed(Box::new(value), Box::new(lean_ty(ty))))
            }
            Rvalue::Array(all) => {
                let mut terms = Vec::with_capacity(all.len());
                for of in all {
                    terms.push(lean::inline(&self.operand(of, lets, at)?));
                }
                Rhs::Pure(app("Arr.ofList", vec![atom(format!("[{}]", terms.join(", ")))]))
            }
            Rvalue::Repeat(of, len) => Rhs::Pure(app(
                "Arr.replicate",
                vec![atom(len.to_string()), self.operand(of, lets, at)?],
            )),
            Rvalue::WithOverflow { .. } => {
                return Err(self.refuse(
                    "an overflowing operation whose check and result were not found after it",
                    at,
                ));
            }
        })
    }

    fn statement(&mut self, statement: &Stmt, lets: &mut Vec<(Pat, Rhs)>) -> Refused<()> {
        match statement {
            Stmt::Assign { place, value, at } => {
                let value = self.rvalue(value, lets, at)?;
                self.write(place, value, lets, at)
            }
            Stmt::Assert { cond, expected, at, .. } => {
                let cond = self.operand(cond, lets, at)?;
                let cond = if *expected { cond } else { Term::BoolNot(Box::new(cond)) };
                lets.push((Pat::Wild, Rhs::Act(app("massert", vec![cond]))));
                Ok(())
            }
            Stmt::Call { dest, callee: Callee::Model { name, monadic }, args, at } => {
                let mut terms = Vec::with_capacity(args.len());
                for arg in args {
                    terms.push(self.operand(arg, lets, at)?);
                }
                let call = app(name, terms);
                self.write(dest, if *monadic { Rhs::Act(call) } else { Rhs::Pure(call) }, lets, at)
            }
            Stmt::Call { dest, callee: Callee::Function { name: callee }, args, at } => {
                let shapes: &'a BTreeMap<String, Shape> = self.shapes;
                let Some(shape) = shapes.get(callee) else {
                    return Err(self.refuse(format!("a call of `{callee}`, whose model is not known"), at));
                };
                let mut terms = Vec::with_capacity(args.len() + 1);
                if shape.fuel {
                    self.uses_fuel = true;
                    terms.push(name("fuel"));
                }
                for arg in args {
                    terms.push(self.operand(arg, lets, at)?);
                }
                // What comes back: the result, then each `&mut` argument's new value.
                let mut pats = Vec::new();
                let mut writes: Vec<(&Place, String)> = Vec::new();
                let takes = |this: &mut Self, place: &Place| -> (Pat, Option<String>) {
                    if place.elems.iter().all(|it| matches!(it, Elem::Deref)) {
                        (Pat::Name(this.names[place.local as usize].clone()), None)
                    } else {
                        let x = this.fresh("x");
                        (Pat::Name(x.clone()), Some(x))
                    }
                };
                if !shape.returns_unit {
                    let (pat, later) = takes(self, dest);
                    pats.push(pat);
                    if let Some(x) = later {
                        writes.push((dest, x));
                    }
                }
                for &param in &shape.mut_params {
                    let Some(Operand::Place(lent)) = args.get(param) else {
                        return Err(self.refuse("a `&mut` argument that is not a place", at));
                    };
                    let (pat, later) = takes(self, lent);
                    pats.push(pat);
                    if let Some(x) = later {
                        writes.push((lent, x));
                    }
                }
                let call = app(callee, terms);
                let value = if shape.monadic { Rhs::Act(call) } else { Rhs::Pure(call) };
                lets.push((tuple_pat(pats), value));
                for (place, x) in writes {
                    self.write(place, Rhs::Pure(name(x)), lets, at)?;
                }
                for &param in &shape.mut_params {
                    if let Some(Operand::Place(lent)) = args.get(param) {
                        self.bound.retain(|(of, _, _)| of.local != lent.local);
                    }
                }
                if shape.returns_unit {
                    self.write(dest, Rhs::Pure(atom("()")), lets, at)?;
                }
                self.bound.retain(|(of, _, _)| of.local != dest.local);
                Ok(())
            }
        }
    }

    fn call_loop(&mut self, loop_name: &str, params: &[Local]) -> M {
        self.uses_fuel = true;
        let mut args = vec![name("fuel")];
        args.extend(params.iter().map(|it| self.var(*it)));
        M::Tail(app(loop_name, args))
    }

    /// What `then` is, the code before it having ended.
    fn then(&mut self, then: &Then<'a>) -> Refused<M> {
        match then {
            Then::Return => Ok(M::Ret(self.vars(&self.returned))),
            Then::Yield(locals) => Ok(M::Ret(self.vars(locals))),
            Then::Seq { rest, then, live, depth } => {
                let inner = self.labels.split_off(*depth);
                let out = self.seq(*rest, then, live);
                self.labels.extend(inner);
                out
            }
            Then::Again { name, params } => Ok(self.call_loop(name, params)),
        }
    }

    fn jump(&mut self, to: u32) -> Refused<M> {
        let Some(at) = self.labels.len().checked_sub(1 + to as usize) else {
            return Err(self.refuse("a jump past the outermost label", ""));
        };
        match &self.labels[at] {
            Label::Loop { name, params, .. } => {
                if at < self.boundary {
                    return Err(self.refuse(
                        "a jump from inside a loop to a loop around it that is not a plain \
                         `break` of the inner loop (the two loops would be mutually recursive)",
                        "",
                    ));
                }
                let (name, params) = (name.clone(), params.clone());
                Ok(self.call_loop(&name, &params))
            }
            Label::Block { then, .. } => {
                let then = then.clone();
                let inner = self.labels.split_off(at);
                let out = self.then(&then);
                self.labels.extend(inner);
                out
            }
        }
    }

    /// `nodes`, then `then`, before which `live` are live.
    fn seq(&mut self, nodes: &'a Nodes, then: &Then<'a>, live: &Set) -> Refused<M> {
        let mut lets: Vec<(Pat, Rhs)> = Vec::new();
        let mut at = 0;
        while let Some(Node::Do(statement)) = nodes.get(at) {
            self.statement(statement, &mut lets)?;
            at += 1;
        }
        let rest = &nodes[at.min(nodes.len())..];
        let tail = match rest.split_first() {
            None => self.then(then)?,
            Some((node, after)) => self.node(node, after, then, live)?,
        };
        Ok(lets
            .into_iter()
            .rev()
            .fold(tail, |body, (pat, value)| M::Let { pat, value, body: Box::new(body) }))
    }

    /// `node`, then `after`, then `then`.
    fn node(
        &mut self,
        node: &'a Node<Stmt, Cond>,
        after: &'a Nodes,
        then: &Then<'a>,
        live: &Set,
    ) -> Refused<M> {
        match node {
            Node::Do(_) => unreachable!("`seq` takes the statements"),
            Node::Break(to) | Node::Continue(to) => self.jump(*to),
            Node::Return => Ok(M::Ret(self.vars(&self.returned))),
            Node::End(End::Panic) => Ok(M::Fail("panic")),
            Node::End(End::Unreachable) => Ok(M::Fail("unreachable")),
            Node::Loop(body) => self.loop_(body),
            Node::Branch { .. } | Node::Block(_) => {
                let live_after = self.live_here(after, live);
                let leaves = match node {
                    Node::Block(body) => escapes(body, 1),
                    _ => escapes(std::slice::from_ref(node), 0),
                };
                if leaves {
                    let then = Then::Seq {
                        rest: after,
                        then: Rc::new(then.clone()),
                        live: Rc::new(live.clone()),
                        depth: self.labels.len(),
                    };
                    return self.inside(node, &then, &live_after);
                }
                // An expression of what it changes that is read later.
                let mut locals = Set::new();
                changed(std::slice::from_ref(node), &mut locals);
                let locals: Vec<Local> = locals.intersection(&live_after).copied().collect();
                let ty = self.types(&locals);
                let outer = std::mem::replace(&mut self.result, ty);
                let yielded: Set = locals.iter().copied().collect();
                let value = self.inside(node, &Then::Yield(locals.clone()), &yielded);
                self.result = outer;
                let value = value?;
                let pat =
                    tuple_pat(locals.iter().map(|it| Pat::Name(self.names[*it as usize].clone())).collect());
                let body = self.seq(after, then, live)?;
                Ok(M::Let { pat, value: Rhs::Sub(Box::new(value)), body: Box::new(body) })
            }
        }
    }

    /// A branch or a block, each way out of it going on to `then`.
    fn inside(&mut self, node: &'a Node<Stmt, Cond>, then: &Then<'a>, live: &Set) -> Refused<M> {
        match node {
            Node::Block(body) => {
                self.labels.push(Label::Block { then: then.clone(), live: live.clone() });
                let out = self.seq(body, then, live);
                self.labels.pop();
                out
            }
            Node::Branch { on: Cond::Operand(on, OpTy::Bool), arms, otherwise } => {
                let mut lets = Vec::new();
                let cond = self.operand(on, &mut lets, "")?;
                let m = match arms.as_slice() {
                    [(0, arm)] => M::If {
                        cond,
                        then: Box::new(self.seq(otherwise, then, live)?),
                        otherwise: Box::new(self.seq(arm, then, live)?),
                    },
                    [(1, arm)] => M::If {
                        cond,
                        then: Box::new(self.seq(arm, then, live)?),
                        otherwise: Box::new(self.seq(otherwise, then, live)?),
                    },
                    _ => return Err(self.refuse("a switch on a `bool` that is not an `if`", "")),
                };
                Ok(Self::with(lets, m))
            }
            Node::Branch { on: Cond::Operand(on, OpTy::Int(ty)), arms, otherwise } => {
                let mut lets = Vec::new();
                let tested = self.operand(on, &mut lets, "")?;
                let mut m = self.seq(otherwise, then, live)?;
                for (value, arm) in arms.iter().rev() {
                    m = M::If {
                        cond: app("Scalar.eq", vec![tested.clone(), int_lit(*ty, *value)]),
                        then: Box::new(self.seq(arm, then, live)?),
                        otherwise: Box::new(m),
                    };
                }
                Ok(Self::with(lets, m))
            }
            Node::Branch { on: Cond::Discriminant(place, ty), arms, otherwise } => {
                let Some(variants) = self.body.variants(ty) else {
                    return Err(self.refuse("a `match` on a value that is not an enum", ""));
                };
                let mut lets = Vec::new();
                let on = self.read(place, &mut lets, "")?;
                let mut out = Vec::with_capacity(arms.len() + 1);
                for (value, arm) in arms {
                    let Some(which) = variants.iter().position(|it| it.discriminant == *value)
                    else {
                        return Err(self.refuse(
                            format!("a `match` arm for the discriminant {value}, which no variant has"),
                            "",
                        ));
                    };
                    let variant = &variants[which];
                    let fields: Vec<String> =
                        variant.fields.iter().map(|_| self.fresh("v")).collect();
                    let pat = Pat::Ctor(
                        self.constructor(ty, variant),
                        fields.iter().cloned().map(Pat::Name).collect(),
                    );
                    let outer = self.bound.clone();
                    self.bound.push((place.clone(), which as u32, fields));
                    let arm = self.seq(arm, then, live);
                    self.bound = outer;
                    out.push((pat, arm?));
                }
                // With every variant matched, the rest is never reached: an enum's value is
                // one of its variants. With one left, the rest is that variant's arm.
                let left: Vec<usize> = (0..variants.len())
                    .filter(|which| {
                        !arms.iter().any(|(value, _)| variants[*which].discriminant == *value)
                    })
                    .collect();
                if !left.is_empty() {
                    let outer = self.bound.clone();
                    let pat = match left.as_slice() {
                        [which] => {
                            let variant = &variants[*which];
                            let fields: Vec<String> =
                                variant.fields.iter().map(|_| self.fresh("v")).collect();
                            let pat = Pat::Ctor(
                                self.constructor(ty, variant),
                                fields.iter().cloned().map(Pat::Name).collect(),
                            );
                            self.bound.push((place.clone(), *which as u32, fields));
                            pat
                        }
                        _ => Pat::Wild,
                    };
                    let rest = self.seq(otherwise, then, live);
                    self.bound = outer;
                    out.push((pat, rest?));
                }
                Ok(Self::with(lets, M::Match { on, arms: out }))
            }
            _ => unreachable!("asked of a branch or a block"),
        }
    }

    fn with(lets: Vec<(Pat, Rhs)>, m: M) -> M {
        lets.into_iter()
            .rev()
            .fold(m, |body, (pat, value)| M::Let { pat, value, body: Box::new(body) })
    }

    /// A loop: its function, and the call that enters it.
    fn loop_(&mut self, body: &'a Nodes) -> Refused<M> {
        // What is live at the head, the labels around the loop deciding what its exits need.
        let head: Set = {
            let node = [Node::Loop(body.to_vec())];
            self.live_here(&node, &Set::new())
        };
        let params: Vec<Local> = head.iter().copied().collect();
        let loop_name = format!("{}.loop_{}", self.body.name, self.loop_count);
        self.loop_count += 1;
        let depth = self.labels.len();
        let outer_boundary = std::mem::replace(&mut self.boundary, depth);
        let outer_bound = std::mem::take(&mut self.bound);
        self.labels.push(Label::Loop {
            name: loop_name.clone(),
            params: params.clone(),
            live: head.clone(),
        });
        let again = Then::Again { name: loop_name.clone(), params: params.clone() };
        let m = self.seq(body, &again, &head);
        self.labels.pop();
        self.boundary = outer_boundary;
        self.bound = outer_bound;
        let m = simplify(m?, &self.temps);
        let mut binders = vec![Binder { name: "fuel".to_owned(), ty: name("Nat") }];
        binders.extend(params.iter().map(|it| Binder {
            name: self.names[*it as usize].clone(),
            ty: lean_ty(&self.body.locals[*it as usize].ty),
        }));
        // A loop inside this one was declared while its body was translated: before it.
        self.loops.push(Decl::Def {
            name: loop_name.clone(),
            params: binders,
            ty: app("Result", vec![self.result.clone()]),
            body: Term::Match(
                Box::new(name("fuel")),
                vec![
                    (Pat::Atom("0".to_owned()), name("Result.div")),
                    (Pat::Atom("fuel + 1".to_owned()), emit(&m, true)),
                ],
            ),
        });
        Ok(self.call_loop(&loop_name, &params))
    }
}

/// The declarations of `body`'s types.
fn type_decls(body: &Body) -> Vec<Decl> {
    let fields = |fields: &[crate::body::Field]| -> Vec<Binder> {
        fields.iter().map(|it| Binder { name: it.name.clone(), ty: lean_ty(&it.ty) }).collect()
    };
    body.adts
        .iter()
        .map(|adt| match &adt.kind {
            AdtKind::Struct(all) => Decl::Structure { name: adt.name.clone(), fields: fields(all) },
            AdtKind::Enum(variants) => Decl::Inductive {
                name: adt.name.clone(),
                constructors: variants
                    .iter()
                    .map(|it| (it.name.clone(), fields(&it.fields)))
                    .collect(),
            },
        })
        .collect()
}

/// Each local's Lean variable: its binding's name, told apart by the local's number where two
/// locals share one; `ret` for the return slot; `t{n}` for a temporary.
fn local_names(body: &Body) -> (Vec<String>, BTreeSet<String>) {
    let mut taken: BTreeSet<String> = ["fuel".to_owned()].into();
    let mut names = Vec::with_capacity(body.locals.len());
    let mut temps = BTreeSet::new();
    for (at, local) in body.locals.iter().enumerate() {
        let mut candidate = match (&local.name, at) {
            (Some(binding), _) => binding.clone(),
            (None, 0) => "ret".to_owned(),
            (None, _) => format!("t{at}"),
        };
        if taken.contains(&candidate) {
            candidate = format!("{candidate}_{at}");
        }
        while taken.contains(&candidate) {
            candidate.push('_');
        }
        taken.insert(candidate.clone());
        if local.name.is_none() {
            temps.insert(candidate.clone());
        }
        names.push(candidate);
    }
    (names, temps)
}

/// `body`'s model. `shapes` has the shape of each function it calls, by Lean name.
pub fn translate(body: &Body, shapes: &BTreeMap<String, Shape>) -> Result<Model, NotTranslated> {
    let (names, temps) = local_names(body);
    let mut mut_params = Vec::new();
    let mut returned = Vec::new();
    let returns_unit = matches!(body.locals[0].ty, Ty::Unit | Ty::Never);
    if !returns_unit {
        returned.push(0);
    }
    for (at, &param) in body.params.iter().enumerate() {
        if matches!(body.locals[param as usize].ty, Ty::RefMut(_)) {
            mut_params.push(at);
            returned.push(param);
        }
    }
    let mut translate = Translate {
        body,
        shapes,
        names,
        temps,
        labels: Vec::new(),
        boundary: 0,
        result: name("Unit"),
        loops: Vec::new(),
        loop_count: 0,
        fresh: 0,
        bound: Vec::new(),
        uses_fuel: false,
        returned,
    };
    translate.result = translate.types(&translate.returned);
    let live: Set = translate.returned.iter().copied().collect();
    let m = translate.seq(&body.nodes, &Then::Return, &live)?;
    let m = simplify(m, &translate.temps);
    let monadic = !is_pure(&m);
    let params: Vec<Binder> = body
        .params
        .iter()
        .map(|it| Binder {
            name: translate.names[*it as usize].clone(),
            ty: lean_ty(&body.locals[*it as usize].ty),
        })
        .collect();
    let mut binders = Vec::with_capacity(params.len() + 1);
    if translate.uses_fuel {
        binders.push(Binder { name: "fuel".to_owned(), ty: name("Nat") });
    }
    binders.extend(params.iter().cloned());
    let result = translate.result.clone();
    let ty = if monadic { app("Result", vec![result.clone()]) } else { result.clone() };
    let mut decls = type_decls(body);
    decls.append(&mut translate.loops);
    decls.push(Decl::Def { name: body.name.clone(), params: binders, ty, body: emit(&m, monadic) });
    Ok(Model {
        decls,
        shape: Shape { fuel: translate.uses_fuel, monadic, mut_params, returns_unit },
        params,
        result,
    })
}
