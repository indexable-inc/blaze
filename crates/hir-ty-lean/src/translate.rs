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
        Rvalue, Shared, Stmt, Ty, Variant,
    },
    lean::{Binder, Decl, DoElem, Infix, Pat, Term, app, atom, infix, name},
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
    /// It gives a `Result`, or a `Threads.Prog` when it is `shared`.
    pub monadic: bool,
    /// It touches the memory threads share (it calls an atomic's operation, a function of
    /// `blaze_core::world::threads`, or a shared function): it gives a `Threads.Prog`, a program
    /// of operations on that memory. A shared function is monadic.
    pub shared: bool,
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
        Ty::Vec(elem) => app("Vec", vec![lean_ty(elem)]),
        Ty::Array(elem, len) => app("Arr", vec![lean_ty(elem), atom(len.to_string())]),
        Ty::TryFromIntError => name("core.num.TryFromIntError"),
        Ty::PhantomData => name("core.marker.PhantomData"),
        Ty::Duration => name("core.time.Duration"),
        Ty::Ordering => name("core.cmp.Ordering"),
        Ty::Range { elem, inclusive: false } => app("core.ops.Range", vec![lean_ty(elem)]),
        Ty::Range { elem, inclusive: true } => app("core.ops.RangeInclusive", vec![lean_ty(elem)]),
        Ty::Str => name("Str"),
        Ty::Opaque(opaque) => name(opaque.lean()),
        Ty::Shared(shared) => match shared {
            Shared::Atomic(int) => app("core.sync.atomic.Atomic", vec![atom(format!(".{}", int.lean()))]),
            Shared::AtomicBool => name("core.sync.atomic.AtomicBool"),
            Shared::Ordering => name("core.sync.atomic.Ordering"),
            Shared::Step => name("Threads.Step"),
            Shared::Woke => name("Threads.Woke"),
            Shared::Ended(inner) => app("Threads.Ended", vec![lean_ty(inner)]),
        },
    }
}

/// What a computation may do besides give its value, in order: nothing, fail (a `Result`), or
/// also operate on the memory threads share (a `Threads.Prog`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Eff {
    Pure,
    Result,
    Prog,
}

/// `Threads.Prog.done r`: the program that does nothing and ends with the `Result` `r`. The one
/// spelling of a `Result` made a `Threads.Prog`, so no model relies on a coercion.
fn done(result: Term) -> Term {
    app("Threads.Prog.done", vec![result])
}

/// Whether `nodes` call a function that touches the memory threads share: an atomic's
/// operation, `blaze_core::world::threads`'s, or a function whose shape in `shapes` is shared
/// (its callees are emitted before it, so their shapes are known).
fn shares(nodes: &Nodes, shapes: &BTreeMap<String, Shape>) -> bool {
    nodes.iter().any(|node| match node {
        Node::Do(Stmt::Call { callee, .. }) => match callee {
            Callee::Shared { .. } | Callee::Two { .. } => true,
            Callee::Function { name } => shapes.get(name).is_some_and(|it| it.shared),
            Callee::Model { .. } => false,
        },
        Node::Do(_) => false,
        Node::Branch { arms, otherwise, .. } => {
            arms.iter().any(|(_, arm)| shares(arm, shapes)) || shares(otherwise, shapes)
        }
        Node::Block(body) | Node::Loop(body) => shares(body, shapes),
        Node::Break(_) | Node::Continue(_) | Node::Return | Node::End(_) => false,
    })
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
    /// A term that is the computation's `Threads.Prog`.
    ProgTail(Term),
    Let { pat: Pat, value: Rhs, body: Box<M> },
    If { cond: Term, then: Box<M>, otherwise: Box<M> },
    /// `of`: the constructors of the type of `on`, each as a pattern's head names it.
    Match { on: Term, arms: Vec<(Pat, M)>, of: Vec<String> },
}

#[derive(Clone, Debug)]
enum Rhs {
    Pure(Term),
    /// A term in `Result`.
    Act(Term),
    /// A term in `Threads.Prog`.
    Prog(Term),
    Sub(Box<M>),
}

fn is_pure(m: &M) -> bool {
    match m {
        M::Ret(_) => true,
        M::Fail(_) | M::Tail(_) | M::ProgTail(_) => false,
        M::Let { value, body, .. } => {
            let value = match value {
                Rhs::Pure(_) => true,
                Rhs::Act(_) | Rhs::Prog(_) => false,
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
        Term::Tuple(all) | Term::List(all) => all.iter().map(|it| occurs(it, x)).sum(),
        Term::Typed(value, _) => occurs(value, x),
        Term::Update(base, fields) => {
            occurs(base, x) + fields.iter().map(|(_, it)| occurs(it, x)).sum::<usize>()
        }
        Term::Structure(fields) => fields.iter().map(|(_, it)| occurs(it, x)).sum(),
        Term::Infix(_, left, right) => occurs(left, x) + occurs(right, x),
        Term::If(a, b, c) => occurs(a, x) + occurs(b, x) + occurs(c, x),
        // Not made by a body's expressions; counted as many so nothing is put into them.
        Term::Match(..) | Term::Let(..) | Term::Do(_) | Term::Forall(..) | Term::Exists(..) => 2,
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
        Term::Tuple(all) | Term::List(all) => {
            all.iter_mut().for_each(|it| substitute(it, x, with));
        }
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
        Term::Match(..) | Term::Let(..) | Term::Do(_) | Term::Forall(..) | Term::Exists(..) => {}
    }
}

/// How often `x` is read in `m`, up to where it is bound again. A tail several arms go on to
/// ([`Translate::joins`]) reads, where it is called, what it reads itself: it is written in the
/// scope of the code that calls it, so a binding it reads is live at each call.
fn reads(m: &M, x: &str, joins: &[M]) -> usize {
    reads_through(m, x, joins, &mut Vec::new())
}

/// [`reads`], `open` the tails whose reads are being counted (a tail that calls one of them
/// adds nothing more).
fn reads_through(m: &M, x: &str, joins: &[M], open: &mut Vec<usize>) -> usize {
    let mut term_reads = |term: &Term, open: &mut Vec<usize>| {
        occurs(term, x)
            + match join_of(term) {
                Some(id) if !open.contains(&id) => {
                    open.push(id);
                    let tail = joins.get(id).map_or(0, |tail| reads_through(tail, x, joins, open));
                    open.pop();
                    tail
                }
                _ => 0,
            }
    };
    match m {
        M::Ret(term) | M::Tail(term) | M::ProgTail(term) => term_reads(term, open),
        M::Fail(_) => 0,
        M::Let { pat, value, body } => {
            let value = match value {
                Rhs::Pure(term) | Rhs::Act(term) | Rhs::Prog(term) => term_reads(term, open),
                Rhs::Sub(sub) => reads_through(sub, x, joins, open),
            };
            value + if binds(pat, x) { 0 } else { reads_through(body, x, joins, open) }
        }
        M::If { cond, then, otherwise } => {
            occurs(cond, x) + reads_through(then, x, joins, open) + reads_through(otherwise, x, joins, open)
        }
        M::Match { on, arms, .. } => {
            occurs(on, x)
                + arms
                    .iter()
                    .map(|(pat, arm)| if binds(pat, x) { 0 } else { reads_through(arm, x, joins, open) })
                    .sum::<usize>()
        }
    }
}

/// The term `m` evaluates first, if it is one.
fn head(m: &mut M) -> Option<&mut Term> {
    match m {
        M::Ret(term) | M::Tail(term) | M::ProgTail(term) => Some(term),
        M::Let { value: Rhs::Pure(term) | Rhs::Act(term) | Rhs::Prog(term), .. } => Some(term),
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

/// The names a pattern of names and tuples binds; `None` for any other pattern.
fn pat_names(pat: &Pat) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    fn walk(pat: &Pat, out: &mut BTreeSet<String>) -> bool {
        match pat {
            Pat::Name(it) => out.insert(it.clone()),
            Pat::Tuple(all) => all.iter().all(|it| walk(it, out)),
            _ => false,
        }
    }
    walk(pat, &mut out).then_some(out)
}

/// Whether `m` is `let`s that bind only `names` and `temps`, then `ok` of `pat` as a term.
fn spine_gives(m: &M, pat: &Pat, names: &BTreeSet<String>, temps: &BTreeSet<String>) -> bool {
    match m {
        M::Let { pat: bound, body, .. } => {
            pat_names(bound).is_some_and(|all| all.iter().all(|it| names.contains(it) || temps.contains(it)))
                && spine_gives(body, pat, names, temps)
        }
        M::Ret(term) => pat_term(pat).as_ref() == Some(term),
        _ => false,
    }
}

/// The `let`s of `m`, a spine [`spine_gives`] accepted, then `end` in place of its result.
fn replace_end(m: M, end: M) -> M {
    match m {
        M::Let { pat, value, body } => M::Let { pat, value, body: Box::new(replace_end(*body, end)) },
        _ => end,
    }
}

/// The simplifications the module names; `temps` are the variables that hold no binding,
/// `joins` the tails the code's placeholders stand for ([`Translate::joins`]).
fn simplify(m: M, temps: &BTreeSet<String>, joins: &[M]) -> M {
    match m {
        M::Let { pat, value, body } => {
            let mut body = simplify(*body, temps, joins);
            let value = match value {
                Rhs::Sub(sub) => match simplify(*sub, temps, joins) {
                    M::Ret(term) => Rhs::Pure(term),
                    M::Tail(term) => Rhs::Act(term),
                    M::ProgTail(term) => Rhs::Prog(term),
                    sub => Rhs::Sub(Box::new(sub)),
                },
                other => other,
            };
            let nothing = match &value {
                Rhs::Pure(_) => true,
                Rhs::Sub(sub) => is_pure(sub),
                Rhs::Act(_) | Rhs::Prog(_) => false,
            };
            if nothing && matches!(pat, Pat::Wild) {
                return body;
            }
            if let (Pat::Name(x), Rhs::Pure(term)) = (&pat, &value) {
                let count = reads(&body, x, joins);
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
            // `let (a, b) ← (let a := x; let b := y; ok (a, b)); body` is
            // `let a := x; let b := y; body`: each name the inner `let`s bind is one of the
            // pattern's, given its last value there, or a temporary nothing else reads.
            if let Rhs::Sub(sub) = &value
                && let Some(names) = pat_names(&pat)
                && spine_gives(sub, &pat, &names, temps)
            {
                let Rhs::Sub(sub) = value else { unreachable!("matched above") };
                return replace_end(*sub, body);
            }
            // `let p ← e; ok p` is `e`.
            if let M::Ret(result) = &body
                && pat_term(&pat).as_ref() == Some(result)
            {
                match value {
                    Rhs::Act(term) => return M::Tail(term),
                    Rhs::Prog(term) => return M::ProgTail(term),
                    Rhs::Sub(sub) => return *sub,
                    Rhs::Pure(term) => return M::Ret(term),
                }
            }
            M::Let { pat, value, body: Box::new(body) }
        }
        M::If { cond, then, otherwise } => M::If {
            cond,
            then: Box::new(simplify(*then, temps, joins)),
            otherwise: Box::new(simplify(*otherwise, temps, joins)),
        },
        M::Match { on, arms, of } => {
            let mut arms: Vec<(Pat, M)> =
                arms.into_iter().map(|(pat, arm)| (pat, simplify(arm, temps, joins))).collect();
            // `match x with | A => a | _ => match x with | B => b | A => c` is
            // `match x with | A => a | B => b`: the `_` arm binds nothing, so the inner `x` is
            // the outer one, and an inner arm for a constructor an outer arm took is dead there.
            // MIR tests an or-pattern's other alternatives again, so this is the match it
            // wrote.
            let heads: Option<Vec<String>> =
                arms.split_last().and_then(|(_, before)| before.iter().map(|(pat, _)| pat_head(pat)).collect());
            // When the inner match gives a value the rest of the `_` arm binds (an or-pattern's
            // bindings, joined), each live inner arm gets its own copy of that rest, as any
            // code after a join does; the arm for a constructor the outer match took goes.
            let live = |heads: &[String], pat: &Pat| pat_head(pat).is_none_or(|it| !heads.contains(&it));
            match (heads, arms.last()) {
                (Some(heads), Some((Pat::Wild, M::Match { on: inner, .. }))) if *inner == on => {
                    let Some((_, M::Match { arms: inner, .. })) = arms.pop() else {
                        unreachable!("matched above")
                    };
                    arms.extend(inner.into_iter().filter(|(pat, _)| live(&heads, pat)));
                }
                (
                    Some(heads),
                    Some((Pat::Wild, M::Let { value: Rhs::Sub(sub), .. })),
                ) if matches!(&**sub, M::Match { on: inner, arms, .. }
                        if *inner == on && arms.iter().any(|(pat, _)| !live(&heads, pat))) =>
                {
                    let Some((_, M::Let { pat: bound, value: Rhs::Sub(sub), body })) = arms.pop() else {
                        unreachable!("matched above")
                    };
                    let M::Match { arms: inner, .. } = *sub else { unreachable!("matched above") };
                    for (pat, arm) in inner.into_iter().filter(|(pat, _)| live(&heads, pat)) {
                        let rest = M::Let { pat: bound.clone(), value: Rhs::Sub(Box::new(arm)), body: body.clone() };
                        arms.push((pat, simplify(rest, temps, joins)));
                    }
                }
                _ => {}
            }
            // Joined, the arms may take every constructor of the type (the inner match's own
            // `_` arm was for the variants the outer arms took): a `_` arm after them is never
            // reached, and Lean refuses it as redundant.
            let taken: BTreeSet<String> = arms.iter().filter_map(|(pat, _)| pat_head(pat)).collect();
            if !of.is_empty() && of.iter().all(|it| taken.contains(it)) {
                arms.retain(|(pat, _)| !matches!(pat, Pat::Wild));
            }
            M::Match { on, arms, of }
        }
        other => other,
    }
}

/// The constructor or literal a pattern tests, when it tests one.
fn pat_head(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ctor(head, _) => Some(head.clone()),
        Pat::Atom(head) => Some(head.clone()),
        Pat::Wild | Pat::Name(_) | Pat::Tuple(_) => None,
    }
}

/// `m` as a Lean term at `level`: a plain value, in `Result`, or in `Threads.Prog`. In a
/// `Threads.Prog`, a `Result` bound by `←` is lifted by `MonadLift Result Prog`; one that ends
/// the computation, and every end, is made a program by [`done`].
fn emit(m: &M, level: Eff) -> Term {
    let inner = if level == Eff::Prog { Eff::Prog } else { Eff::Result };
    let mut elems = Vec::new();
    let mut at = m;
    while let M::Let { pat, value, body } = at {
        elems.push(match value {
            Rhs::Pure(term) => DoElem::Let(pat.clone(), term.clone()),
            Rhs::Act(term) | Rhs::Prog(term) => DoElem::Bind(pat.clone(), term.clone()),
            Rhs::Sub(sub) if is_pure(sub) => DoElem::Let(pat.clone(), emit(sub, Eff::Pure)),
            Rhs::Sub(sub) => DoElem::Bind(pat.clone(), emit(sub, inner)),
        });
        at = body;
    }
    let fail = |error: &str| app("Result.fail", vec![name(format!("Error.{error}"))]);
    let last = match (at, level) {
        (M::Ret(term), Eff::Pure) => term.clone(),
        (M::Ret(term), Eff::Result) => app("Result.ok", vec![term.clone()]),
        (M::Ret(term), Eff::Prog) => done(app("Result.ok", vec![term.clone()])),
        (M::Tail(term), Eff::Prog) => done(term.clone()),
        (M::Tail(term) | M::ProgTail(term), _) => term.clone(),
        (M::Fail(error), Eff::Prog) => done(fail(error)),
        (M::Fail(error), _) => fail(error),
        (M::If { cond, then, otherwise }, _) => Term::If(
            Box::new(cond.clone()),
            Box::new(emit(then, level)),
            Box::new(emit(otherwise, level)),
        ),
        (M::Match { on, arms, .. }, _) => Term::Match(
            Box::new(on.clone()),
            arms.iter().map(|(pat, arm)| (pat.clone(), emit(arm, level))).collect(),
        ),
        (M::Let { .. }, _) => unreachable!("the loop above ends at what is not a `let`"),
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
    /// The tail [`Translate::joins`] holds at this index, which several arms go on to: its
    /// placeholder ([`join_call`]).
    Join { id: usize },
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
    /// The variants the enum at a place can still be, in the `_` arm of a `match` on it: those
    /// no other arm has.
    narrowed: Vec<(Place, Vec<u32>)>,
    uses_fuel: bool,
    /// What `return` gives: the result unless it is `()`, then each `&mut` parameter.
    returned: Vec<Local>,
    /// The loops whose functions are shared ([`shares`]): each gives a `Threads.Prog`.
    prog_loops: BTreeSet<String>,
    /// Each variable's Lean type: the locals' and the translation's own ([`Translate::fresh`]).
    var_types: BTreeMap<String, Term>,
    /// Each loop's type as a function of its parameters after `fuel`: what a program point
    /// that goes on with the loop takes in its place ([`Points`]).
    loop_types: BTreeMap<String, Term>,
    /// The definition being written (the function, or the loop whose body it is) gives a
    /// `Threads.Prog`: its program points are named, and a tail several arms go on to is
    /// written once.
    prog_level: bool,
    /// The tails several arms go on to, each written once ([`Then::Join`]), by number. The
    /// table only grows: a number is taken only with its tail, never given back or reused, and
    /// an entry is only ever replaced by a fuller tail ([`push`]), so every [`join_call`] made
    /// from a number finds its tail, whatever order the code around it is written in.
    joins: Vec<M>,
    /// The tails a loop's program points went on to ([`Points::join`]).
    reached: BTreeSet<usize>,
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

    /// A new variable of the translation's own, holding a value of type `ty`.
    fn fresh(&mut self, prefix: &str, ty: &Ty) -> String {
        loop {
            let candidate = format!("{prefix}{}", self.fresh);
            self.fresh += 1;
            if !self.names.contains(&candidate) {
                self.temps.insert(candidate.clone());
                self.var_types.insert(candidate.clone(), lean_ty(ty));
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
            (Elem::Field(field), Ty::Range { elem, .. }) => match field.as_str() {
                "exhausted" => Ty::Bool,
                _ => (**elem).clone(),
            },
            (Elem::VariantField { variant, index }, ty) => self
                .body
                .variants(ty)
                .and_then(|all| all.get(*variant as usize).cloned())
                .and_then(|it| it.fields.get(*index as usize).cloned())
                .map_or(Ty::Never, |it| it.ty),
            (
                Elem::Index(_) | Elem::ConstIndex(_),
                Ty::Slice(elem) | Ty::Vec(elem) | Ty::Array(elem, _),
            ) => {
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
                    let x = self.fresh("x", &self.elem_ty(&ty, elem));
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
                    let x = self.fresh("x", &inner_ty);
                    let read = app(&format!("{kind}.index"), vec![base.clone(), by.clone()]);
                    lets.push((Pat::Name(x.clone()), Rhs::Act(read)));
                    self.updated(name(x), &inner_ty, rest, value, lets, at)?
                };
                let x = self.fresh("x", ty);
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
                    let x = self.fresh("x", &self.ty_of(place, place.elems.len()));
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
        self.forget(place.local);
        Ok(())
    }

    /// What `match` arms bound of a place that reads `local` (its root, or an index in it) is
    /// no longer known: `local` was given a new value.
    fn forget(&mut self, local: Local) {
        self.bound.retain(|(of, _, _)| of.local != local && !of.indexed_by(local));
        self.narrowed.retain(|(of, _)| of.local != local && !of.indexed_by(local));
    }

    /// Whether `nodes` read a field of an enum's variant at a place that neither a `match` in
    /// them nor one in scope ([`Translate::bound`]) tests: read where they are, it would be
    /// refused ([`Translate::read`]).
    fn reads_unbound_field(&self, nodes: &Nodes) -> bool {
        fn unbound(place: &Place, matched: &[&Place], bound: &[(Place, u32, Vec<String>)]) -> bool {
            place.elems.iter().enumerate().any(|(upto, elem)| {
                matches!(elem, Elem::VariantField { .. }) && {
                    let prefix = Place { local: place.local, elems: place.elems[..upto].to_vec() };
                    !matched.iter().any(|it| **it == prefix) && !bound.iter().any(|(of, _, _)| *of == prefix)
                }
            })
        }
        fn walk<'n>(nodes: &'n Nodes, matched: &mut Vec<&'n Place>, bound: &[(Place, u32, Vec<String>)]) -> bool {
            nodes.iter().any(|node| match node {
                Node::Do(statement) => statement.read_places().iter().any(|it| unbound(it, matched, bound)),
                Node::Branch { on, arms, otherwise } => {
                    let tested = match on {
                        Cond::Operand(Operand::Place(place), _) | Cond::Discriminant(place, _) => Some(place),
                        Cond::Operand(..) => None,
                    };
                    if tested.is_some_and(|it| unbound(it, matched, bound)) {
                        return true;
                    }
                    let matches = match on {
                        Cond::Discriminant(place, _) => Some(place),
                        Cond::Operand(..) => None,
                    };
                    matched.extend(matches);
                    let reads = arms.iter().any(|(_, arm)| walk(arm, matched, bound)) || walk(otherwise, matched, bound);
                    if matches.is_some() {
                        matched.pop();
                    }
                    reads
                }
                Node::Block(body) | Node::Loop(body) => walk(body, matched, bound),
                Node::Break(_) | Node::Continue(_) | Node::Return | Node::End(_) => false,
            })
        }
        walk(nodes, &mut Vec::new(), &self.bound)
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
            Operand::PhantomData => name("core.marker.PhantomData.mk"),
            Operand::Str(bytes) => app(
                "Str.ofBytes",
                vec![Term::List(bytes.iter().map(|it| atom(it.to_string())).collect())],
            ),
            Operand::Duration { secs, nanos } => {
                app("core.time.Duration.ofLit", vec![atom(secs.to_string()), atom(nanos.to_string())])
            }
            Operand::Ordering(variant) => {
                let (variant, _) = crate::body::ORDERING[*variant as usize];
                name(format!("core.cmp.Ordering.{variant}"))
            }
            Operand::Variant { ty, variant } => {
                let Some(found) = self.body.variants(ty).and_then(|all| all.get(*variant as usize).cloned())
                else {
                    return Err(self.refuse("a constant of a variant its enum does not have", at));
                };
                name(self.constructor(ty, &found))
            }
        })
    }

    /// The Lean constructor of `variant` of the enum type `ty`.
    fn constructor(&self, ty: &Ty, variant: &Variant) -> String {
        match ty {
            Ty::Adt(adt) => format!("{adt}.{}", variant.name),
            Ty::Option(_) => format!("Option.{}", variant.name),
            Ty::Ordering => format!("core.cmp.Ordering.{}", variant.name),
            Ty::Shared(Shared::Ordering) => format!("core.sync.atomic.Ordering.{}", variant.name),
            Ty::Shared(Shared::Woke) => format!("Threads.Woke.{}", variant.name),
            Ty::Shared(Shared::Ended(_)) => format!("Threads.Ended.{}", variant.name),
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
                    terms.push(self.operand(of, lets, at)?);
                }
                Rhs::Pure(app("Arr.ofList", vec![Term::List(terms)]))
            }
            Rvalue::Repeat(of, len) => Rhs::Pure(app(
                "Arr.replicate",
                vec![atom(len.to_string()), self.operand(of, lets, at)?],
            )),
            Rvalue::Range { inclusive, start, end } => {
                let new = if *inclusive { "core.ops.RangeInclusive.new" } else { "core.ops.Range.new" };
                let (start, end) = (self.operand(start, lets, at)?, self.operand(end, lets, at)?);
                Rhs::Pure(app(new, vec![start, end]))
            }
            Rvalue::Step { second, pick } => {
                let (second, pick) = (self.operand(second, lets, at)?, self.operand(pick, lets, at)?);
                Rhs::Pure(app("Threads.Step.mk", vec![second, pick]))
            }
            Rvalue::EmptyVec(elem) => Rhs::Pure(Term::Typed(
                Box::new(name("Vec.new")),
                Box::new(app("Vec", vec![lean_ty(elem)])),
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
            Stmt::Call { dest, callee: Callee::Model { name, monadic, ty, lends_first }, args, at } => {
                let mut terms = Vec::with_capacity(args.len() + 1);
                if let Some(ty) = ty {
                    terms.push(atom(format!(".{}", ty.lean())));
                }
                for arg in args {
                    terms.push(self.operand(arg, lets, at)?);
                }
                let call = app(name, terms);
                if *lends_first {
                    // A method that returns `()` gives back only what it was lent.
                    let unit = matches!(self.ty_of(dest, dest.elems.len()), Ty::Unit);
                    let eff = if *monadic { Eff::Result } else { Eff::Pure };
                    return self.call(call, eff, unit, &[0], dest, args, lets, at);
                }
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
                let eff = match (shape.shared, shape.monadic) {
                    (true, _) => Eff::Prog,
                    (false, true) => Eff::Result,
                    (false, false) => Eff::Pure,
                };
                self.call(
                    app(callee, terms),
                    eff,
                    shape.returns_unit,
                    &shape.mut_params,
                    dest,
                    args,
                    lets,
                    at,
                )
            }
            Stmt::Call { dest, callee: Callee::Shared { name: callee }, args, at } => {
                let mut terms = Vec::with_capacity(args.len());
                for arg in args {
                    terms.push(self.operand(arg, lets, at)?);
                }
                self.write(dest, Rhs::Prog(app(callee, terms)), lets, at)
            }
            Stmt::Call { dest, callee: Callee::Two { first, second }, args, at } => {
                let [schedule, first_env, second_env] = args.as_slice() else {
                    return Err(self.refuse("a call of `two` not given a schedule and two closures", at));
                };
                let schedule = self.operand(schedule, lets, at)?;
                let shapes: &'a BTreeMap<String, Shape> = self.shapes;
                let mut threads = vec![schedule];
                for (callee, env) in [(first, first_env), (second, second_env)] {
                    let Some(shape) = shapes.get(callee) else {
                        return Err(self.refuse(format!("a call of `{callee}`, whose model is not known"), at));
                    };
                    if !shape.mut_params.is_empty() {
                        return Err(self.refuse(
                            format!(
                                "a closure given to `blaze_core::world::threads::two` whose call \
                                 writes its captures back (`{callee}` takes its closure by `&mut`; a \
                                 thread shares nothing with its caller but the memory; rule \
                                 SharedLend)"
                            ),
                            at,
                        ));
                    }
                    let mut terms = Vec::with_capacity(2);
                    if shape.fuel {
                        self.uses_fuel = true;
                        terms.push(name("fuel"));
                    }
                    terms.push(self.operand(env, lets, at)?);
                    let call = app(callee, terms);
                    // Each thread is a program: a closure that is not shared is one that does no
                    // operation and ends as its call does.
                    threads.push(match (shape.shared, shape.monadic) {
                        (true, _) => call,
                        (false, true) => done(call),
                        (false, false) => done(app("Result.ok", vec![call])),
                    });
                }
                self.write(dest, Rhs::Prog(app("Threads.two_of", threads)), lets, at)
            }
        }
    }

    /// Binds what `call` gives back, in what `eff` says it is in: its result (none when it
    /// `returns_unit`), then the new value of each `&mut` argument in `mut_params`, written to
    /// `dest` and the places lent.
    #[expect(clippy::too_many_arguments, reason = "one call's whole shape")]
    fn call(
        &mut self,
        call: Term,
        eff: Eff,
        returns_unit: bool,
        mut_params: &[usize],
        dest: &Place,
        args: &[Operand],
        lets: &mut Vec<(Pat, Rhs)>,
        at: &str,
    ) -> Refused<()> {
        let mut pats = Vec::new();
        let mut writes: Vec<(&Place, String)> = Vec::new();
        let takes = |this: &mut Self, place: &Place| -> (Pat, Option<String>) {
            if place.elems.iter().all(|it| matches!(it, Elem::Deref)) {
                (Pat::Name(this.names[place.local as usize].clone()), None)
            } else {
                let x = this.fresh("x", &this.ty_of(place, place.elems.len()));
                (Pat::Name(x.clone()), Some(x))
            }
        };
        if !returns_unit {
            let (pat, later) = takes(self, dest);
            pats.push(pat);
            if let Some(x) = later {
                writes.push((dest, x));
            }
        }
        for &param in mut_params {
            let Some(Operand::Place(lent)) = args.get(param) else {
                return Err(self.refuse("a `&mut` argument that is not a place", at));
            };
            let (pat, later) = takes(self, lent);
            pats.push(pat);
            if let Some(x) = later {
                writes.push((lent, x));
            }
        }
        let value = match eff {
            Eff::Pure => Rhs::Pure(call),
            Eff::Result => Rhs::Act(call),
            Eff::Prog => Rhs::Prog(call),
        };
        lets.push((tuple_pat(pats), value));
        for (place, x) in writes {
            self.write(place, Rhs::Pure(name(x)), lets, at)?;
        }
        for &param in mut_params {
            if let Some(Operand::Place(lent)) = args.get(param) {
                self.forget(lent.local);
            }
        }
        if returns_unit {
            self.write(dest, Rhs::Pure(atom("()")), lets, at)?;
        }
        self.forget(dest.local);
        Ok(())
    }

    fn call_loop(&mut self, loop_name: &str, params: &[Local]) -> M {
        self.uses_fuel = true;
        let mut args = vec![name("fuel")];
        args.extend(params.iter().map(|it| self.var(*it)));
        match self.prog_loops.contains(loop_name) {
            true => M::ProgTail(app(loop_name, args)),
            false => M::Tail(app(loop_name, args)),
        }
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
            Then::Join { id } => Ok(join_call(*id)),
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
                // The code after reads a field of an enum's variant that no `match` arm in
                // scope binds: a `return` or a `break` with a value from inside an arm of a
                // loop's `match` that uses the arm's binding (`loop { match r { Ok(v) => return
                // v, .. } }`), whose code MIR's structure puts after the loop. That code is
                // translated at each way out, inside the arm that binds the field, as it is
                // when the node leaves further; never as an expression after the node, where
                // the binding is out of scope.
                if leaves && let Some(shared) = self.shared_tail(node, after, then, live, &live_after)? {
                    return Ok(shared);
                }
                if leaves || self.reads_unbound_field(after) {
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
                // Inside an arm of a `match` on the same place, nothing having written it (or
                // an index it reads) since, the variants it can be are known: that arm's, or,
                // in a `_` arm, those no other arm has. Where each of them takes the same arm,
                // the test takes it. MIR tests an or-pattern twice, to choose the arm and then
                // to bind what the alternative that matched binds, so this is the second test.
                // Arms equal for two variants read no field of either: a field read names its
                // variant.
                let possible = match self.bound.iter().rev().find(|(of, _, _)| of == place) {
                    Some((_, which, _)) => Some(vec![*which]),
                    None => self.narrowed.iter().rev().find(|(of, _)| of == place).map(|it| it.1.clone()),
                };
                if let Some(possible) = possible {
                    let arm_of = |which: u32| {
                        let discriminant = variants[which as usize].discriminant;
                        arms.iter().find(|(value, _)| *value == discriminant).map_or(otherwise, |(_, arm)| arm)
                    };
                    if let Some((&first, rest)) = possible.split_first()
                        && rest.iter().all(|it| arm_of(*it) == arm_of(first))
                    {
                        return self.seq(arm_of(first), then, live);
                    }
                }
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
                        variant.fields.iter().map(|it| self.fresh("v", &it.ty)).collect();
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
                    let outer_narrowed = self.narrowed.clone();
                    let pat = match left.as_slice() {
                        [which] => {
                            let variant = &variants[*which];
                            let fields: Vec<String> =
                                variant.fields.iter().map(|it| self.fresh("v", &it.ty)).collect();
                            let pat = Pat::Ctor(
                                self.constructor(ty, variant),
                                fields.iter().cloned().map(Pat::Name).collect(),
                            );
                            self.bound.push((place.clone(), *which as u32, fields));
                            pat
                        }
                        _ => {
                            let left = left.iter().map(|it| *it as u32).collect();
                            self.narrowed.push((place.clone(), left));
                            Pat::Wild
                        }
                    };
                    let rest = self.seq(otherwise, then, live);
                    self.bound = outer;
                    self.narrowed = outer_narrowed;
                    out.push((pat, rest?));
                }
                let of = variants.iter().map(|it| self.constructor(ty, it)).collect();
                Ok(Self::with(lets, M::Match { on, arms: out, of }))
            }
            _ => unreachable!("asked of a branch or a block"),
        }
    }

    /// In a program, a branch that something jumps out of, with the code after it (`after`,
    /// then `then`) written once where its arms go on to it, not once per arm: translated before
    /// the arms, it is a tail the arms call ([`Then::Join`]), or, where it is one call or one
    /// end or only one arm reaches it, put in place of that call. `None` where it cannot be
    /// written once: outside a program, or where it reads a field that only an arm's `match`
    /// binds.
    fn shared_tail(
        &mut self,
        node: &'a Node<Stmt, Cond>,
        after: &'a Nodes,
        then: &Then<'a>,
        live: &Set,
        live_after: &Set,
    ) -> Refused<Option<M>> {
        if !self.prog_level {
            return Ok(None);
        }
        // After the branch, what an arm writes is no longer what a `match` around it bound.
        let (bound, narrowed) = (self.bound.clone(), self.narrowed.clone());
        let mut written = Set::new();
        changed(std::slice::from_ref(node), &mut written);
        for local in &written {
            self.forget(*local);
        }
        if self.reads_unbound_field(after) {
            (self.bound, self.narrowed) = (bound, narrowed);
            return Ok(None);
        }
        // The tail first: a loop in an arm whose way out goes on to it makes its program points
        // when the loop is written, from the tail.
        let before = (self.loops.len(), self.loop_count, self.prog_loops.clone());
        let rest = self.seq(after, then, live);
        (self.bound, self.narrowed) = (bound.clone(), narrowed.clone());
        let rest = simplify(rest?, &self.temps, &self.joins);
        let id = self.joins.len();
        self.joins.push(rest.clone());
        let m = self.inside(node, &Then::Join { id }, live_after)?;
        let calls = joins_in(&m, id);
        if calls == 0 && !self.reached.contains(&id) {
            // Nothing goes on: the tail is not part of the model, and the branch is written
            // the plain way ([`Then::Seq`]), its loops numbered as if the tail had never been
            // written. The tail stays in the table under its own number, which nothing calls.
            let (loops, loop_count, prog_loops) = before;
            self.loops.truncate(loops);
            self.loop_count = loop_count;
            self.prog_loops = prog_loops;
            (self.bound, self.narrowed) = (bound, narrowed);
            return Ok(None);
        }
        // Only a loop in an arm goes on to it: the loop has the tail's point.
        if calls == 0 {
            return Ok(Some(m));
        }
        if calls == 1 || single(&rest) {
            return Ok(Some(put_join(m, id, &rest)));
        }
        Ok(Some(m))
    }

    /// Every name a variable of the model can have: the locals', the translation's own, and
    /// `fuel`.
    fn known(&self) -> BTreeSet<String> {
        self.var_types.keys().cloned().chain(["fuel".to_owned()]).collect()
    }

    /// The program points of the body `m` of a definition that gives a `Threads.Prog` (the
    /// function `owner`, or its loop `owner`): `m` with the code after each call that is
    /// itself shared made a definition of its own ([`Points`]), and those definitions, each
    /// before those that call it. `cycle`: the loops a point takes as parameters ([`Points`]);
    /// in `m`, which is the loop's own body, each is that loop at the body's `fuel`.
    fn points(&mut self, m: M, owner: &str, cycle: &BTreeSet<String>) -> Refused<(M, Vec<Decl>)> {
        let mut known = self.known();
        let mut loops = BTreeMap::new();
        for it in cycle {
            let mut binder = it.rsplit('.').next().unwrap_or(it).to_owned();
            while known.contains(&binder) {
                binder.push('_');
            }
            let Some(ty) = self.loop_types.get(it) else {
                return Err(self.refuse(
                    format!("a program point that goes on with the loop `{it}`, whose type is not known"),
                    "",
                ));
            };
            loops.insert(it.clone(), (binder, ty.clone()));
        }
        let m = prepass(m, &mut self.joins);
        let m = simplify(m, &self.temps, &self.joins);
        known.extend(loops.values().map(|(binder, _)| binder.clone()));
        let mut points = Points {
            known,
            loops: loops.clone(),
            joins: &self.joins,
            reached: &mut self.reached,
            joined: BTreeMap::new(),
            made: Vec::new(),
        };
        let m = points.cut(m);
        let (mut m, decls) = match points.finish(m, owner, &self.result, &self.var_types) {
            Ok(it) => it,
            Err(what) => return Err(self.refuse(what, "")),
        };
        for (it, (binder, _)) in &loops {
            let at = app(it, vec![name("fuel")]);
            visit_m(&mut m, &mut |term| substitute(term, binder, &at));
        }
        Ok((m, decls))
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
        // Shared when its body touches the memory threads share; known before the body is
        // translated, whose `continue`s call the loop.
        let shared = shares(body, self.shapes);
        if shared {
            self.prog_loops.insert(loop_name.clone());
        }
        let (level, monad, out_of_fuel) = match shared {
            true => (Eff::Prog, "Threads.Prog", done(name("Result.div"))),
            false => (Eff::Result, "Result", name("Result.div")),
        };
        let returns = app(monad, vec![self.result.clone()]);
        let loop_type = params.iter().rev().fold(returns.clone(), |to, it| {
            infix(Infix::Implies, lean_ty(&self.body.locals[*it as usize].ty), to)
        });
        self.loop_types.insert(loop_name.clone(), loop_type);
        let outer_level = std::mem::replace(&mut self.prog_level, shared);
        let depth = self.labels.len();
        let outer_boundary = std::mem::replace(&mut self.boundary, depth);
        let outer_bound = std::mem::take(&mut self.bound);
        let outer_narrowed = std::mem::take(&mut self.narrowed);
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
        self.narrowed = outer_narrowed;
        self.prog_level = outer_level;
        let m = simplify(m?, &self.temps, &self.joins);
        let m = match shared {
            true => {
                // The loops a point of this one goes on with are those still being written
                // (this one, the loops around it) and the loops written that call one of them:
                // each is a parameter of the point, so the points call no loop that calls them
                // back, and the loops alone recurse on their fuel.
                let mut cycle: BTreeSet<String> = self
                    .labels
                    .iter()
                    .filter_map(|it| match it {
                        Label::Loop { name, .. } => Some(name.clone()),
                        Label::Block { .. } => None,
                    })
                    .chain([loop_name.clone()])
                    .collect();
                loop {
                    let reaching: Vec<String> = self
                        .loops
                        .iter()
                        .filter_map(|it| match it {
                            Decl::Def { name, body, .. }
                                if self.loop_types.contains_key(name)
                                    && !cycle.contains(name)
                                    && cycle.iter().any(|it| names_in(body, it)) =>
                            {
                                Some(name.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    if reaching.is_empty() {
                        break;
                    }
                    cycle.extend(reaching);
                }
                let (m, points) = self.points(m, &loop_name, &cycle)?;
                self.loops.extend(points);
                m
            }
            false => m,
        };
        let mut binders = vec![Binder { name: "fuel".to_owned(), ty: name("Nat") }];
        binders.extend(params.iter().map(|it| Binder {
            name: self.names[*it as usize].clone(),
            ty: lean_ty(&self.body.locals[*it as usize].ty),
        }));
        // A loop inside this one was declared while its body was translated: before it, and so
        // were this one's program points.
        self.loops.push(Decl::Def {
            name: loop_name.clone(),
            params: binders,
            ty: returns,
            body: Term::Match(
                Box::new(name("fuel")),
                vec![
                    (Pat::Atom("0".to_owned()), out_of_fuel),
                    (Pat::Atom("fuel + 1".to_owned()), emit(&m, level)),
                ],
            ),
        });
        Ok(self.call_loop(&loop_name, &params))
    }
}

/// The declaration of the struct or enum `adt`: the one declaration of its module
/// ([`crate::type_module`]).
pub(crate) fn type_decl(adt: &crate::body::AdtDecl) -> Decl {
    let fields = |fields: &[crate::body::Field]| -> Vec<Binder> {
        fields.iter().map(|it| Binder { name: it.name.clone(), ty: lean_ty(&it.ty) }).collect()
    };
    match &adt.kind {
        AdtKind::Struct(all) => Decl::Structure { name: adt.name.clone(), fields: fields(all) },
        AdtKind::Enum(variants) => Decl::Inductive {
            name: adt.name.clone(),
            constructors: variants.iter().map(|it| (it.name.clone(), fields(&it.fields))).collect(),
        },
    }
}

/// The first component of every global name a model can refer to besides its crate's: the
/// library's namespaces and types. A variable of one of these names would shadow it, so
/// `Slice.len v` would read a field of the variable `Slice`.
const GLOBAL_ROOTS: &[&str] = &[
    "Blaze", "Scalar", "ScalarTy", "Slice", "Array", "Vec", "Option", "Result", "Error", "core", "Str", "Threads",
    "Bool", "Nat", "Int", "List", "Prop", "Unit", "U8", "U16", "U32", "U64", "U128", "Usize",
    "I8", "I16", "I32", "I64", "I128", "Isize",
];

/// Each local's Lean variable: its binding's name, told apart by the local's number where two
/// locals share one, or where it is the name of the crate (the root of its global names, as
/// `queue` is of `queue.Queue.wf`) or of a library namespace; `ret` for the return slot;
/// `t{n}` for a temporary.
fn local_names(body: &Body) -> (Vec<String>, BTreeSet<String>) {
    let krate = body.name.split('.').next().unwrap_or(&body.name);
    let mut taken: BTreeSet<String> =
        ["fuel", krate].into_iter().chain(GLOBAL_ROOTS.iter().copied()).map(str::to_owned).collect();
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
    // A `&mut` parameter is given back, and so is a closure that lends (its environment holds
    // a capture by `&mut`), taken by value: what it changes is the caller's.
    let lends = |ty: &Ty| match ty {
        Ty::RefMut(_) => true,
        Ty::Adt(name) => body.adts.iter().any(|it| it.name == *name && it.lends()),
        _ => false,
    };
    for (at, &param) in body.params.iter().enumerate() {
        if lends(&body.locals[param as usize].ty) {
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
        narrowed: Vec::new(),
        uses_fuel: false,
        returned,
        prog_loops: BTreeSet::new(),
        var_types: BTreeMap::new(),
        loop_types: BTreeMap::new(),
        prog_level: false,
        joins: Vec::new(),
        reached: BTreeSet::new(),
    };
    translate.var_types = body
        .locals
        .iter()
        .zip(&translate.names)
        .map(|(local, named)| (named.clone(), lean_ty(&local.ty)))
        .collect();
    // A shared function gives a `Threads.Prog`: it is monadic whatever is left of its body.
    let shared = shares(&body.nodes, shapes);
    translate.prog_level = shared;
    translate.result = translate.types(&translate.returned);
    let live: Set = translate.returned.iter().copied().collect();
    let m = translate.seq(&body.nodes, &Then::Return, &live)?;
    let m = simplify(m, &translate.temps, &translate.joins);
    let (m, points) = match shared {
        true => translate.points(m, &body.name, &BTreeSet::new())?,
        false => (m, Vec::new()),
    };
    let monadic = shared || !is_pure(&m);
    let level = match (shared, monadic) {
        (true, _) => Eff::Prog,
        (false, true) => Eff::Result,
        (false, false) => Eff::Pure,
    };
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
    let ty = match level {
        Eff::Prog => app("Threads.Prog", vec![result.clone()]),
        Eff::Result => app("Result", vec![result.clone()]),
        Eff::Pure => result.clone(),
    };
    // The types are not declared here: each has a module of its own, which this one imports.
    let mut decls = mutual_loops(std::mem::take(&mut translate.loops));
    decls.extend(points);
    decls.push(Decl::Def { name: body.name.clone(), params: binders, ty, body: emit(&m, level) });
    let mut params = params;
    renumber(&mut decls, &mut params, &generated_names(body, &translate.names, &translate.temps));
    Ok(Model {
        decls,
        shape: Shape { fuel: translate.uses_fuel, monadic, shared, mut_params, returns_unit },
        params,
        result,
    })
}

/// The loops' functions, in the order they were declared (each after the loops inside it), with
/// those that call each other put in one `mutual` block. A loop calls one around it when its
/// exit goes on with the outer loop (`while i < n { ..; while j < i { .. } i += 1; }`, where
/// the inner loop's exit is the outer loop's next turn): the two are mutually recursive. Each
/// recurses on its own `fuel`, matched as `fuel + 1`, and every call it makes, of itself or of
/// another loop of the block, passes that smaller `fuel`, so Lean accepts the block by
/// structural recursion on `Nat` as it does one loop. A block stands where its last loop was
/// declared (the outermost, which the loops inside it all come before); a loop that calls no
/// loop calling it back stays as it was.
fn mutual_loops(loops: Vec<Decl>) -> Vec<Decl> {
    let names: Vec<String> = loops
        .iter()
        .map(|it| match it {
            Decl::Def { name, .. } => name.clone(),
            _ => unreachable!("a loop is a definition"),
        })
        .collect();
    // `calls[a][b]`: loop `a`'s body names loop `b`; closed transitively below.
    let mut calls: Vec<Vec<bool>> = loops
        .iter()
        .map(|it| {
            let Decl::Def { body, .. } = it else { unreachable!("a loop is a definition") };
            names.iter().map(|callee| names_in(body, callee)).collect()
        })
        .collect();
    let count = names.len();
    for via in 0..count {
        for from in 0..count {
            if calls[from][via] {
                for to in 0..count {
                    if calls[via][to] {
                        calls[from][to] = true;
                    }
                }
            }
        }
    }
    // Each loop's block: the loops it calls that call it back, itself among them.
    let block_of = |at: usize| -> Vec<usize> {
        (0..count).filter(|other| *other == at || (calls[at][*other] && calls[*other][at])).collect()
    };
    let mut slots: Vec<Option<Decl>> = loops.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(count);
    for at in 0..count {
        let block = block_of(at);
        if block.len() == 1 {
            if let Some(decl) = slots[at].take() {
                out.push(decl);
            }
            continue;
        }
        if block.last() != Some(&at) {
            continue;
        }
        let members: Vec<Decl> = block.iter().filter_map(|it| slots[*it].take()).collect();
        out.push(Decl::Mutual(members));
    }
    out
}

/// Whether `term` names `name` anywhere (in a `do` block or a `match` arm too, which
/// [`occurs`] counts as many without looking).
fn names_in(term: &Term, name: &str) -> bool {
    let mut found = false;
    let mut probe = term.clone();
    visit_term(&mut probe, &mut |it: &mut String| found |= it == name);
    found
}

/// What a generated variable's new name is made of: its kind's prefix and a number.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Generated {
    /// A temporary (`t`): a local with no binding.
    Temp,
    /// A local whose binding's name another local, a library root or the crate already has
    /// (`<binding>_`).
    Suffixed(String),
    /// A variable of the translation's own ([`Translate::fresh`]): `x` for a part read or
    /// written, `v` for a field a `match` arm binds.
    Fresh(String),
}

/// The variables of `body`'s model whose names [`local_names`] and [`Translate::fresh`] made
/// from MIR's numbering (a local's number, a counter), each with its kind; the binding names
/// and `ret` are not among them.
fn generated_names(body: &Body, names: &[String], temps: &BTreeSet<String>) -> BTreeMap<String, Generated> {
    let mut out = BTreeMap::new();
    for (at, (local, named)) in body.locals.iter().zip(names).enumerate() {
        let kind = match &local.name {
            Some(binding) if binding == named => continue,
            Some(binding) => Generated::Suffixed(binding.clone()),
            None if at == 0 && named == "ret" => continue,
            None if at == 0 => Generated::Suffixed("ret".to_owned()),
            None => Generated::Temp,
        };
        out.insert(named.clone(), kind);
    }
    for fresh in temps {
        if !out.contains_key(fresh) && !names.contains(fresh) {
            let prefix = fresh.trim_end_matches(|it: char| it.is_ascii_digit()).to_owned();
            out.insert(fresh.clone(), Generated::Fresh(prefix));
        }
    }
    out
}

/// Renames the `generated` variables of a model (its declarations, the function's last, and
/// its parameters) by the order each is first bound in the model as printed: temporaries
/// `t0`, `t1`, .., a suffixed binding `<binding>_1`, `<binding>_2`, .. per binding, and the
/// translation's own `x0`, .. and `v0`, .. per prefix. So the model of one function is the
/// same text whatever MIR numbered its locals (the standard library's MIR may have one more
/// temporary than the fixture's `core`). Each variable is renamed everywhere it occurs, to a
/// name no other variable or name of the model has: the model is alpha-equivalent to the one
/// before, shadowing included.
fn renumber(decls: &mut [Decl], params: &mut [Binder], generated: &BTreeMap<String, Generated>) {
    if generated.is_empty() {
        return;
    }
    // Every name that stays: a candidate equal to one is skipped.
    let mut taken: BTreeSet<String> = GLOBAL_ROOTS.iter().map(|it| (*it).to_owned()).collect();
    taken.insert("fuel".to_owned());
    for decl in decls.iter_mut() {
        visit_decl(decl, &mut |it: &mut String| {
            if !generated.contains_key(it.as_str()) {
                taken.insert(it.clone());
            }
        });
    }
    let mut renamed: BTreeMap<String, String> = BTreeMap::new();
    let mut counters: BTreeMap<Generated, usize> = BTreeMap::new();
    let mut rename = |it: &mut String| {
        let Some(kind) = generated.get(it.as_str()) else { return };
        if let Some(new) = renamed.get(it.as_str()) {
            *it = new.clone();
            return;
        }
        let counter = counters.entry(kind.clone()).or_insert(match kind {
            Generated::Suffixed(_) => 1,
            Generated::Temp | Generated::Fresh(_) => 0,
        });
        let new = loop {
            let candidate = match kind {
                Generated::Temp => format!("t{counter}"),
                Generated::Suffixed(binding) => format!("{binding}_{counter}"),
                Generated::Fresh(prefix) => format!("{prefix}{counter}"),
            };
            *counter += 1;
            if !taken.contains(&candidate) {
                break candidate;
            }
        };
        taken.insert(new.clone());
        renamed.insert(std::mem::replace(it, new.clone()), new);
    };
    for decl in decls.iter_mut() {
        visit_decl(decl, &mut rename);
    }
    for param in params.iter_mut() {
        rename(&mut param.name);
    }
}

/// Calls `f` on each variable name of `decl` (a binder's, a pattern's, a name's), in the order
/// the printer writes them; constructors, fields and declarations' own names are not variables.
fn visit_decl(decl: &mut Decl, f: &mut impl FnMut(&mut String)) {
    match decl {
        Decl::Def { params, ty, body, .. } => {
            visit_binders(params, f);
            visit_term(ty, f);
            visit_term(body, f);
        }
        Decl::Mutual(decls) => decls.iter_mut().for_each(|it| visit_decl(it, f)),
        Decl::Structure { .. }
        | Decl::Inductive { .. }
        | Decl::Theorem { .. }
        | Decl::PrintAxioms(_)
        | Decl::DeriveDecidableEq(_)
        | Decl::Opaque { .. } => {}
    }
}

fn visit_binders(binders: &mut [Binder], f: &mut impl FnMut(&mut String)) {
    for binder in binders {
        f(&mut binder.name);
        visit_term(&mut binder.ty, f);
    }
}

fn visit_pat(pat: &mut Pat, f: &mut impl FnMut(&mut String)) {
    match pat {
        Pat::Name(it) => f(it),
        Pat::Tuple(all) | Pat::Ctor(_, all) => all.iter_mut().for_each(|it| visit_pat(it, f)),
        Pat::Wild | Pat::Atom(_) => {}
    }
}

fn visit_term(term: &mut Term, f: &mut impl FnMut(&mut String)) {
    match term {
        Term::Name(it) => f(it),
        Term::Atom(_) => {}
        Term::App(head, args) => {
            visit_term(head, f);
            args.iter_mut().for_each(|it| visit_term(it, f));
        }
        Term::Proj(of, _) | Term::BoolNot(of) | Term::Not(of) => visit_term(of, f),
        Term::Tuple(all) | Term::List(all) => all.iter_mut().for_each(|it| visit_term(it, f)),
        Term::Typed(of, ty) => {
            visit_term(of, f);
            visit_term(ty, f);
        }
        Term::Update(of, fields) => {
            visit_term(of, f);
            fields.iter_mut().for_each(|(_, it)| visit_term(it, f));
        }
        Term::Structure(fields) => fields.iter_mut().for_each(|(_, it)| visit_term(it, f)),
        Term::Infix(_, left, right) => {
            visit_term(left, f);
            visit_term(right, f);
        }
        Term::If(test, then, otherwise) => {
            visit_term(test, f);
            visit_term(then, f);
            visit_term(otherwise, f);
        }
        Term::Match(on, arms) => {
            visit_term(on, f);
            for (pat, arm) in arms {
                visit_pat(pat, f);
                visit_term(arm, f);
            }
        }
        Term::Let(pat, value, rest) => {
            visit_pat(pat, f);
            visit_term(value, f);
            visit_term(rest, f);
        }
        Term::Do(elems) => {
            for elem in elems {
                match elem {
                    DoElem::Bind(pat, value) | DoElem::Let(pat, value) => {
                        visit_pat(pat, f);
                        visit_term(value, f);
                    }
                    DoElem::Term(it) => visit_term(it, f),
                }
            }
        }
        Term::Forall(binders, of) | Term::Exists(binders, of) => {
            visit_binders(binders, f);
            visit_term(of, f);
        }
    }
}

/// The prefix of a program point's name before it is numbered ([`Points::finish`]).
const POINT: &str = "\u{0}point";
/// The prefix of the name of a tail several arms go on to ([`Translate::joins`]).
const JOIN: &str = "\u{0}join";

/// The call of the tail `id`: a placeholder, which [`put_join`] replaces by the tail or
/// [`Points::cut`] by the call of the tail's program point, over the variables it reads.
fn join_call(id: usize) -> M {
    M::ProgTail(name(format!("{JOIN}{id}")))
}

/// The placeholder's number, where `term` is the placeholder of a tail.
fn join_of(term: &Term) -> Option<usize> {
    let head = match term {
        Term::App(head, _) => &**head,
        other => other,
    };
    match head {
        Term::Name(it) => it.strip_prefix(JOIN).and_then(|it| it.parse().ok()),
        _ => None,
    }
}

/// Calls `f` on each term of `m`, in the order they are printed.
fn visit_m(m: &mut M, f: &mut impl FnMut(&mut Term)) {
    match m {
        M::Ret(term) | M::Tail(term) | M::ProgTail(term) => f(term),
        M::Fail(_) => {}
        M::Let { value, body, .. } => {
            match value {
                Rhs::Pure(term) | Rhs::Act(term) | Rhs::Prog(term) => f(term),
                Rhs::Sub(sub) => visit_m(sub, f),
            }
            visit_m(body, f);
        }
        M::If { cond, then, otherwise } => {
            f(cond);
            visit_m(then, f);
            visit_m(otherwise, f);
        }
        M::Match { on, arms, .. } => {
            f(on);
            arms.iter_mut().for_each(|(_, arm)| visit_m(arm, f));
        }
    }
}

/// How many times `m` goes on to the tail `id`.
fn joins_in(m: &M, id: usize) -> usize {
    let mut count = 0;
    visit_m(&mut m.clone(), &mut |term| count += usize::from(join_of(term) == Some(id)));
    count
}

/// `m` with each call of the tail `id` replaced by the tail, `rest`: it reads the variables
/// its call is given, which hold there what they hold where it was written.
fn put_join(m: M, id: usize, rest: &M) -> M {
    match m {
        M::ProgTail(term) if join_of(&term) == Some(id) => rest.clone(),
        M::Let { pat, value, body } => M::Let {
            pat,
            value: match value {
                Rhs::Sub(sub) => Rhs::Sub(Box::new(put_join(*sub, id, rest))),
                other => other,
            },
            body: Box::new(put_join(*body, id, rest)),
        },
        M::If { cond, then, otherwise } => M::If {
            cond,
            then: Box::new(put_join(*then, id, rest)),
            otherwise: Box::new(put_join(*otherwise, id, rest)),
        },
        M::Match { on, arms, of } => M::Match {
            on,
            arms: arms.into_iter().map(|(pat, arm)| (pat, put_join(arm, id, rest))).collect(),
            of,
        },
        leaf => leaf,
    }
}

/// Whether `m` is one term: an end, or one call. The code after a shared call that is one term
/// is not made a program point: it already has a name, or is where the program ends.
fn single(m: &M) -> bool {
    matches!(m, M::Ret(_) | M::Fail(_) | M::Tail(_) | M::ProgTail(_))
}

/// Whether `m` operates on the memory threads share: a call that does, or a tail that is one.
fn in_prog(m: &M) -> bool {
    match m {
        M::ProgTail(_) => true,
        M::Ret(_) | M::Tail(_) | M::Fail(_) => false,
        M::Let { value, body, .. } => {
            let value = match value {
                Rhs::Prog(_) => true,
                Rhs::Sub(sub) => in_prog(sub),
                Rhs::Pure(_) | Rhs::Act(_) => false,
            };
            value || in_prog(body)
        }
        M::If { then, otherwise, .. } => in_prog(then) || in_prog(otherwise),
        M::Match { arms, .. } => arms.iter().any(|(_, arm)| in_prog(arm)),
    }
}

/// How many ways `m` ends with a value (not a failure).
fn ends(m: &M) -> usize {
    match m {
        M::Ret(_) | M::Tail(_) | M::ProgTail(_) => 1,
        M::Fail(_) => 0,
        M::Let { body, .. } => ends(body),
        M::If { then, otherwise, .. } => ends(then) + ends(otherwise),
        M::Match { arms, .. } => arms.iter().map(|(_, arm)| ends(arm)).sum(),
    }
}

/// `m`, whose value `pat` binds, then `rest`: `rest` at each end of `m`.
fn push(m: M, pat: &Pat, rest: &M, joins: &mut Vec<M>, pushed: &mut BTreeSet<usize>) -> M {
    let then = |value: Rhs| M::Let { pat: pat.clone(), value, body: Box::new(rest.clone()) };
    match m {
        // A tail the arms share takes what follows once, where it is written.
        M::ProgTail(term) if let Some(id) = join_of(&term) => {
            if pushed.insert(id) {
                // The tail stays in the table while what follows is pushed into it.
                let tail = push(joins[id].clone(), pat, rest, joins, pushed);
                joins[id] = prepass(tail, joins);
            }
            M::ProgTail(term)
        }
        M::Ret(term) => then(Rhs::Pure(term)),
        M::Tail(term) => then(Rhs::Act(term)),
        M::ProgTail(term) => then(Rhs::Prog(term)),
        M::Fail(error) => M::Fail(error),
        M::Let { pat: bound, value, body } => {
            M::Let { pat: bound, value, body: Box::new(push(*body, pat, rest, joins, pushed)) }
        }
        M::If { cond, then, otherwise } => M::If {
            cond,
            then: Box::new(push(*then, pat, rest, joins, pushed)),
            otherwise: Box::new(push(*otherwise, pat, rest, joins, pushed)),
        },
        M::Match { on, arms, of } => M::Match {
            on,
            arms: arms.into_iter().map(|(bound, arm)| (bound, push(arm, pat, rest, joins, pushed))).collect(),
            of,
        },
    }
}

/// A program with no shared operation inside a value it binds: each branch that operates on the
/// shared memory and gives a value (`let x ← if c then do ..; op .. else ..; rest`) takes what
/// follows it into its arms (a join point bound in place would be where a thread stands, with
/// no name), and a tail its arms share takes it once. What follows is copied where it is one
/// term or only one arm reaches it, and is otherwise a tail the arms call
/// ([`Translate::joins`]).
fn prepass(m: M, joins: &mut Vec<M>) -> M {
    match m {
        M::Let { pat, value: Rhs::Sub(sub), body } if in_prog(&sub) => bind_sub(*sub, &pat, *body, joins),
        M::Let { pat, value: Rhs::Prog(term), body } if join_of(&term).is_some() => {
            bind_sub(M::ProgTail(term), &pat, *body, joins)
        }
        M::Let { pat, value, body } => M::Let { pat, value, body: Box::new(prepass(*body, joins)) },
        M::If { cond, then, otherwise } => M::If {
            cond,
            then: Box::new(prepass(*then, joins)),
            otherwise: Box::new(prepass(*otherwise, joins)),
        },
        M::Match { on, arms, of } => M::Match {
            on,
            arms: arms.into_iter().map(|(pat, arm)| (pat, prepass(arm, joins))).collect(),
            of,
        },
        leaf => leaf,
    }
}

/// `let pat ← sub; body`, `body` pushed into `sub`'s ends ([`prepass`]).
fn bind_sub(sub: M, pat: &Pat, body: M, joins: &mut Vec<M>) -> M {
    let body = prepass(body, joins);
    let rest = match single(&body) || ends(&sub) <= 1 {
        true => body,
        false => {
            let id = joins.len();
            joins.push(body);
            join_call(id)
        }
    };
    let sub = push(sub, pat, &rest, joins, &mut BTreeSet::new());
    prepass(sub, joins)
}

/// The names a pattern binds, in order.
fn pat_vars(pat: &Pat, out: &mut Vec<String>) {
    match pat {
        Pat::Name(it) => out.push(it.clone()),
        Pat::Tuple(all) | Pat::Ctor(_, all) => all.iter().for_each(|it| pat_vars(it, out)),
        Pat::Wild | Pat::Atom(_) => {}
    }
}

/// The variables among `known` that `term` reads and does not bind, each once, in the order
/// they are first read, `bound` being bound around it.
fn term_vars(term: &Term, known: &BTreeSet<String>, bound: &mut Vec<String>, out: &mut Vec<String>) {
    let scoped = |pats: &[&Pat], bound: &mut Vec<String>, inner: &mut dyn FnMut(&mut Vec<String>)| {
        let depth = bound.len();
        pats.iter().for_each(|it| pat_vars(it, bound));
        inner(bound);
        bound.truncate(depth);
    };
    match term {
        Term::Name(it) => {
            if known.contains(it) && !bound.contains(it) && !out.contains(it) {
                out.push(it.clone());
            }
        }
        Term::Atom(_) => {}
        Term::App(head, args) => {
            term_vars(head, known, bound, out);
            args.iter().for_each(|it| term_vars(it, known, bound, out));
        }
        Term::Proj(of, _) | Term::BoolNot(of) | Term::Not(of) => term_vars(of, known, bound, out),
        Term::Tuple(all) | Term::List(all) => all.iter().for_each(|it| term_vars(it, known, bound, out)),
        Term::Typed(of, ty) => {
            term_vars(of, known, bound, out);
            term_vars(ty, known, bound, out);
        }
        Term::Update(of, fields) => {
            term_vars(of, known, bound, out);
            fields.iter().for_each(|(_, it)| term_vars(it, known, bound, out));
        }
        Term::Structure(fields) => fields.iter().for_each(|(_, it)| term_vars(it, known, bound, out)),
        Term::Infix(_, left, right) => {
            term_vars(left, known, bound, out);
            term_vars(right, known, bound, out);
        }
        Term::If(test, then, otherwise) => {
            term_vars(test, known, bound, out);
            term_vars(then, known, bound, out);
            term_vars(otherwise, known, bound, out);
        }
        Term::Match(on, arms) => {
            term_vars(on, known, bound, out);
            for (pat, arm) in arms {
                scoped(&[pat], bound, &mut |bound| term_vars(arm, known, bound, out));
            }
        }
        Term::Let(pat, value, rest) => {
            term_vars(value, known, bound, out);
            scoped(&[pat], bound, &mut |bound| term_vars(rest, known, bound, out));
        }
        Term::Do(elems) => {
            let depth = bound.len();
            for elem in elems {
                match elem {
                    DoElem::Bind(pat, value) | DoElem::Let(pat, value) => {
                        term_vars(value, known, bound, out);
                        pat_vars(pat, bound);
                    }
                    DoElem::Term(it) => term_vars(it, known, bound, out),
                }
            }
            bound.truncate(depth);
        }
        Term::Forall(binders, of) | Term::Exists(binders, of) => {
            let depth = bound.len();
            bound.extend(binders.iter().map(|it| it.name.clone()));
            term_vars(of, known, bound, out);
            bound.truncate(depth);
        }
    }
}

/// [`term_vars`] of a computation.
fn m_vars(m: &M, known: &BTreeSet<String>, bound: &mut Vec<String>, out: &mut Vec<String>) {
    match m {
        M::Ret(term) | M::Tail(term) | M::ProgTail(term) => term_vars(term, known, bound, out),
        M::Fail(_) => {}
        M::Let { pat, value, body } => {
            match value {
                Rhs::Pure(term) | Rhs::Act(term) | Rhs::Prog(term) => term_vars(term, known, bound, out),
                Rhs::Sub(sub) => m_vars(sub, known, bound, out),
            }
            let depth = bound.len();
            pat_vars(pat, bound);
            m_vars(body, known, bound, out);
            bound.truncate(depth);
        }
        M::If { cond, then, otherwise } => {
            term_vars(cond, known, bound, out);
            m_vars(then, known, bound, out);
            m_vars(otherwise, known, bound, out);
        }
        M::Match { on, arms, .. } => {
            term_vars(on, known, bound, out);
            for (pat, arm) in arms {
                let depth = bound.len();
                pat_vars(pat, bound);
                m_vars(arm, known, bound, out);
                bound.truncate(depth);
            }
        }
    }
}

/// The variables `m` reads that it does not bind, in the order it first reads them.
fn free_vars(m: &M, known: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    m_vars(m, known, &mut Vec::new(), &mut out);
    out
}

/// `term` with each call of a loop of `loops` at `fuel` a call of the parameter that stands for
/// it.
fn unloop(term: &mut Term, loops: &BTreeMap<String, (String, Term)>) {
    if let Term::App(head, args) = term
        && let Term::Name(callee) = &**head
        && let Some((binder, _)) = loops.get(callee)
        && args.first() == Some(&name("fuel"))
    {
        let rest: Vec<Term> = args.drain(1..).collect();
        *term = app(binder, rest);
    }
    match term {
        Term::Name(_) | Term::Atom(_) => {}
        Term::App(head, args) => {
            unloop(head, loops);
            args.iter_mut().for_each(|it| unloop(it, loops));
        }
        Term::Proj(of, _) | Term::BoolNot(of) | Term::Not(of) => unloop(of, loops),
        Term::Tuple(all) | Term::List(all) => all.iter_mut().for_each(|it| unloop(it, loops)),
        Term::Typed(of, _) => unloop(of, loops),
        Term::Update(of, fields) => {
            unloop(of, loops);
            fields.iter_mut().for_each(|(_, it)| unloop(it, loops));
        }
        Term::Structure(fields) => fields.iter_mut().for_each(|(_, it)| unloop(it, loops)),
        Term::Infix(_, left, right) => {
            unloop(left, loops);
            unloop(right, loops);
        }
        Term::If(test, then, otherwise) => {
            unloop(test, loops);
            unloop(then, loops);
            unloop(otherwise, loops);
        }
        Term::Match(on, arms) => {
            unloop(on, loops);
            arms.iter_mut().for_each(|(_, arm)| unloop(arm, loops));
        }
        Term::Let(_, value, rest) => {
            unloop(value, loops);
            unloop(rest, loops);
        }
        Term::Do(elems) => elems.iter_mut().for_each(|it| match it {
            DoElem::Bind(_, value) | DoElem::Let(_, value) | DoElem::Term(value) => unloop(value, loops),
        }),
        Term::Forall(_, of) | Term::Exists(_, of) => unloop(of, loops),
    }
}

/// The program points of a definition that gives a `Threads.Prog`: where a thread stands
/// between two operations on the memory threads share, each a definition a proof about threads
/// names (docs/v2/plan/design/proofs-for-sans-io.md, section 8).
///
/// The code after each call that is itself shared (an atomic operation, `Threads.wait`, `wake`,
/// `two_of`, a call of a shared function) is a definition of its own, `<owner>.at_<n>`, over the
/// variables it reads, in the order it first reads them (`fuel` first and the loops it goes on
/// with next, where it reads them), then the call's result where it reads it; the call is
/// followed by the call of that definition. Code after a shared call that is one term (an end,
/// one call) is not a point: it already has a name. Control flow between two shared calls stays
/// inside the point that holds it. A tail several arms go on to is a point the arms call
/// ([`Translate::joins`]).
///
/// A point inside a loop's body that goes on with the loop (or with a loop that calls it back)
/// takes that loop as a parameter, named as the loop's last component, which the loop's own body
/// gives as the loop at its smaller `fuel`: the points call no loop, so each loop alone recurses
/// structurally on its fuel, and the model is the same program as before, definition for
/// definition (`f.loop_0.at_1 (f.loop_0 fuel) ..` unfolds to the code it names).
///
/// Points are numbered from 1 in the order a walk of the definition's text, entering each point
/// where it is first called, meets them, and each is declared before every point that calls it.
struct Points<'j> {
    /// Every variable name: the locals', the translation's own, `fuel` and the loops' parameters.
    known: BTreeSet<String>,
    /// Each loop a point takes as a parameter: the parameter's name and type.
    loops: BTreeMap<String, (String, Term)>,
    joins: &'j [M],
    /// The tails these points go on to, recorded for [`Translate::shared_tail`].
    reached: &'j mut BTreeSet<usize>,
    /// The call of each tail made a point.
    joined: BTreeMap<usize, M>,
    /// The points made: each one's parameters and body.
    made: Vec<(Vec<String>, M)>,
}

impl Points<'_> {
    fn cut(&mut self, m: M) -> M {
        match m {
            M::Let { pat, value: Rhs::Prog(call), body } => {
                let body = self.cut(*body);
                let body = if single(&body) { body } else { self.point(body, &pat) };
                M::Let { pat, value: Rhs::Prog(call), body: Box::new(body) }
            }
            M::Let { pat, value, body } => M::Let { pat, value, body: Box::new(self.cut(*body)) },
            M::If { cond, then, otherwise } => M::If {
                cond,
                then: Box::new(self.cut(*then)),
                otherwise: Box::new(self.cut(*otherwise)),
            },
            M::Match { on, arms, of } => M::Match {
                on,
                arms: arms.into_iter().map(|(pat, arm)| (pat, self.cut(arm))).collect(),
                of,
            },
            M::ProgTail(term) => match join_of(&term) {
                Some(id) => self.join(id),
                None => M::ProgTail(term),
            },
            leaf => leaf,
        }
    }

    /// The call of the tail `id`: of its point, made once.
    fn join(&mut self, id: usize) -> M {
        if let Some(call) = self.joined.get(&id) {
            return call.clone();
        }
        self.reached.insert(id);
        // A loop in an arm that goes on to the tail has its own copy of the tail's point.
        let m = self.cut(self.joins[id].clone());
        let call = if single(&m) { m } else { self.point(m, &Pat::Wild) };
        self.joined.insert(id, call.clone());
        call
    }

    /// `body` a point, after a call whose result `pat` binds: the call of the point.
    fn point(&mut self, mut body: M, pat: &Pat) -> M {
        visit_m(&mut body, &mut |term| unloop(term, &self.loops));
        let used = free_vars(&body, &self.known);
        let mut results = Vec::new();
        pat_vars(pat, &mut results);
        results.retain(|it| used.contains(it));
        let binders: Vec<&String> = self.loops.values().map(|(binder, _)| binder).collect();
        let mut params: Vec<String> = Vec::with_capacity(used.len());
        if used.iter().any(|it| it == "fuel") {
            params.push("fuel".to_owned());
        }
        params.extend(binders.iter().filter(|it| used.contains(it)).map(|it| (*it).clone()));
        params.extend(
            used.iter()
                .filter(|it| *it != "fuel" && !binders.contains(it) && !results.contains(it))
                .cloned(),
        );
        params.extend(results);
        let id = self.made.len();
        let call = app(&format!("{POINT}{id}"), params.iter().map(|it| name(it.clone())).collect());
        self.made.push((params, body));
        M::ProgTail(call)
    }

    /// The points in `m`, in order, each entered where first met: `number` of each, and the
    /// order they are declared in (each after those it calls).
    fn order(&self, m: &M, number: &mut [Option<usize>], declared: &mut Vec<usize>, next: &mut usize) {
        let mut met = Vec::new();
        visit_m(&mut m.clone(), &mut |term| {
            visit_term(term, &mut |it: &mut String| {
                if let Some(id) = it.strip_prefix(POINT).and_then(|it| it.parse::<usize>().ok()) {
                    met.push(id);
                }
            });
        });
        for id in met {
            if number[id].is_some() {
                continue;
            }
            number[id] = Some(*next);
            *next += 1;
            self.order(&self.made[id].1, number, declared, next);
            declared.push(id);
        }
    }

    /// The points of `owner`, numbered and named, its body `m` calling them by name: `m`, and
    /// the points' definitions, each before those that call it.
    fn finish(
        self,
        mut m: M,
        owner: &str,
        result: &Term,
        types: &BTreeMap<String, Term>,
    ) -> Result<(M, Vec<Decl>), String> {
        let mut number = vec![None; self.made.len()];
        let mut declared = Vec::new();
        self.order(&m, &mut number, &mut declared, &mut 1);
        let names: Vec<Option<String>> =
            number.iter().map(|it| it.map(|n| format!("{owner}.at_{n}"))).collect();
        let mut rename = |term: &mut Term| {
            visit_term(term, &mut |it: &mut String| {
                if let Some(id) = it.strip_prefix(POINT).and_then(|it| it.parse::<usize>().ok())
                    && let Some(Some(named)) = names.get(id)
                {
                    *it = named.clone();
                }
            });
        };
        visit_m(&mut m, &mut rename);
        let mut made = self.made;
        let mut decls = Vec::with_capacity(declared.len());
        for id in declared {
            let (params, body) = &mut made[id];
            visit_m(body, &mut rename);
            let mut binders = Vec::with_capacity(params.len());
            for param in params.iter() {
                let ty = match param.as_str() {
                    "fuel" => name("Nat"),
                    _ => match self.loops.values().find(|(binder, _)| binder == param) {
                        Some((_, ty)) => ty.clone(),
                        None => match types.get(param) {
                            Some(ty) => ty.clone(),
                            None => {
                                return Err(format!(
                                    "a variable `{param}` that a program point of `{owner}` reads, \
                                     whose type is not known"
                                ));
                            }
                        },
                    },
                };
                binders.push(Binder { name: param.clone(), ty });
            }
            let Some(named) = names[id].clone() else { unreachable!("a declared point is numbered") };
            decls.push(Decl::Def {
                name: named,
                params: binders,
                ty: app("Threads.Prog", vec![result.clone()]),
                body: emit(body, Eff::Prog),
            });
        }
        Ok((m, decls))
    }
}
