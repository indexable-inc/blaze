//! An NFA-based parser, which is porting from rustc mbe parsing code
//!
//! See <https://github.com/rust-lang/rust/blob/70b18bc2cbac4712020019f5bf57c00905373205/compiler/rustc_expand/src/mbe/macro_parser.rs>
//! Here is a quick intro to how the parser works, copied from rustc:
//!
//! A 'position' is a dot in the middle of a matcher, usually represented as a
//! dot. For example `· a $( a )* a b` is a position, as is `a $( · a )* a b`.
//!
//! The parser walks through the input a character at a time, maintaining a list
//! of threads consistent with the current position in the input string: `cur_items`.
//!
//! As it processes them, it fills up `eof_items` with threads that would be valid if
//! the macro invocation is now over, `bb_items` with threads that are waiting on
//! a Rust non-terminal like `$e:expr`, and `next_items` with threads that are waiting
//! on a particular token. Most of the logic concerns moving the · through the
//! repetitions indicated by Kleene stars. The rules for moving the · without
//! consuming any input are called epsilon transitions. It only advances or calls
//! out to the real Rust parser when no `cur_items` threads remain.
//!
//! Example:
//!
//! ```text, ignore
//! Start parsing a a a a b against [· a $( a )* a b].
//!
//! Remaining input: a a a a b
//! next: [· a $( a )* a b]
//!
//! - - - Advance over an a. - - -
//!
//! Remaining input: a a a b
//! cur: [a · $( a )* a b]
//! Descend/Skip (first item).
//! next: [a $( · a )* a b]  [a $( a )* · a b].
//!
//! - - - Advance over an a. - - -
//!
//! Remaining input: a a b
//! cur: [a $( a · )* a b]  [a $( a )* a · b]
//! Follow epsilon transition: Finish/Repeat (first item)
//! next: [a $( a )* · a b]  [a $( · a )* a b]  [a $( a )* a · b]
//!
//! - - - Advance over an a. - - - (this looks exactly like the last step)
//!
//! Remaining input: a b
//! cur: [a $( a · )* a b]  [a $( a )* a · b]
//! Follow epsilon transition: Finish/Repeat (first item)
//! next: [a $( a )* · a b]  [a $( · a )* a b]  [a $( a )* a · b]
//!
//! - - - Advance over an a. - - - (this looks exactly like the last step)
//!
//! Remaining input: b
//! cur: [a $( a · )* a b]  [a $( a )* a · b]
//! Follow epsilon transition: Finish/Repeat (first item)
//! next: [a $( a )* · a b]  [a $( · a )* a b]  [a $( a )* a · b]
//!
//! - - - Advance over a b. - - -
//!
//! Remaining input: ''
//! eof: [a $( a )* a b ·]
//! ```
//!
//! The data, for speed. A rule's pattern is flattened once, when the macro is defined, into a
//! [`Matcher`] (rustc's `MatcherLoc`): the dot of a position is an index into its `locs`, so
//! descending into a group or a repetition pushes no frame. A position ([`Pos`]) is 24 bytes,
//! `Copy`, kept in the [`Buffers`] reused for every step and every rule of a call. Its
//! bindings are cons cells in the buffers, never changed once pushed, so forking a position
//! copies two `u32`s (rustc shares an `Rc<Vec<NamedMatch>>` and copies it on write; the old
//! code here copied or linked a `Vec` per fork and an `Rc` per binding). The parent of a
//! repetition is one copy in the buffers, not a boxed clone per exit. Only a rule's first error
//! is built. The trees the transcriber reads are built from the cells once, when the match is
//! over ([`Buffers::build`]).

use base_db::Db;
use arrayvec::ArrayVec;
use blaze_mem::frame::Frame;
use intern::sym;
use tt::{
    DelimSpan, DelimiterKind, MAX_GLUED_PUNCT_LEN,
    iter::{TtElement, TtIter},
};

mod failure;

pub(super) use self::failure::match_failure;
use self::failure::Tracker;
use super::{Binding, Bindings, Fragment, NONE, TokensOrigin};
use crate::{
    ExpandError, ExpandErrorKind, ExpandResult, MetaTemplate, ValueResult, expect_fragment,
    parser::{Binder, ExprKind, MetaVarKind, NO_BINDER, Op, RepeatKind, Separator},
};

/// A rule's pattern, flattened once when the macro is defined. Every list is indexed by `u32`;
/// the leaves a location compares with sit in side tables by kind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Matcher {
    locs: Box<[Loc]>,
    reps: Box<[Rep]>,
    /// The binders each repetition declares, in pattern order (`Rep::vars_start`).
    rep_vars: Box<[Binder]>,
    literals: Box<[tt::Literal]>,
    idents: Box<[tt::Ident]>,
    puncts: Box<[ArrayVec<tt::Punct, MAX_GLUED_PUNCT_LEN>]>,
    separators: Box<[Separator]>,
    /// How many binders the rule has ([`crate::parser::resolve_binders`]).
    binders: u32,
    /// Each `Var` location's declaration, by location, for what rustc says of a failed match.
    var_decls: Box<[(u32, failure::VarDecl)]>,
    /// Each visible group's `Open` and `Close` locations and delimiter, for the same.
    groups: Box<[failure::Group]>,
}

/// A place the dot of a position can be at. 8 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loc {
    /// `Matcher::literals[_]`.
    Literal(u32),
    /// `Matcher::idents[_]`.
    Ident(u32),
    /// `Matcher::puncts[_]`.
    Punct(u32),
    Var {
        binder: Binder,
        kind: Option<MetaVarKind>,
    },
    /// A group of the pattern, which needs a group of the input with this delimiter. A visible
    /// group's `Open`, contents and `Close` follow it; an invisible group's contents alone.
    Subtree(DelimiterKind),
    Open,
    Close,
    /// A repetition, `Matcher::reps[_]`; its body follows.
    RepStart(u32),
    /// The end of the body of `Matcher::reps[_]`.
    RepEnd(u32),
    /// A `${..}` expression, which a pattern cannot have.
    MetaExpr,
    Eof,
}

const _: () = assert!(size_of::<Loc>() <= 8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rep {
    /// The first location of the body.
    body: u32,
    /// The location after the repetition's `RepEnd`.
    after: u32,
    vars_start: u32,
    vars_len: u32,
    /// `Matcher::separators[_]`, or [`NONE`].
    separator: u32,
    kind: RepeatKind,
}

impl Matcher {
    /// `pattern` with its binders resolved ([`crate::parser::resolve_binders`]), `binders` of them.
    pub(crate) fn new(pattern: &MetaTemplate, binders: u32) -> Matcher {
        let mut builder = MatcherBuilder::default();
        builder.flatten(pattern);
        builder.locs.push(Loc::Eof);
        let MatcherBuilder {
            locs,
            reps,
            rep_vars,
            literals,
            idents,
            puncts,
            separators,
            var_decls,
            groups,
        } = builder;
        Matcher {
            locs: locs.into_boxed_slice(),
            reps: reps.into_boxed_slice(),
            rep_vars: rep_vars.into_boxed_slice(),
            literals: literals.into_boxed_slice(),
            idents: idents.into_boxed_slice(),
            puncts: puncts.into_boxed_slice(),
            separators: separators.into_boxed_slice(),
            binders,
            var_decls: var_decls.into_boxed_slice(),
            groups: groups.into_boxed_slice(),
        }
    }

    fn rep_vars(&self, rep: Rep) -> &[Binder] {
        &self.rep_vars[rep.vars_start as usize..(rep.vars_start + rep.vars_len) as usize]
    }
}

#[derive(Default)]
struct MatcherBuilder {
    locs: Vec<Loc>,
    reps: Vec<Rep>,
    rep_vars: Vec<Binder>,
    literals: Vec<tt::Literal>,
    idents: Vec<tt::Ident>,
    puncts: Vec<ArrayVec<tt::Punct, MAX_GLUED_PUNCT_LEN>>,
    separators: Vec<Separator>,
    var_decls: Vec<(u32, failure::VarDecl)>,
    groups: Vec<failure::Group>,
}

impl MatcherBuilder {
    fn flatten(&mut self, template: &MetaTemplate) {
        for op in template.iter() {
            match op {
                Op::Literal(it) => {
                    self.locs.push(Loc::Literal(self.literals.len() as u32));
                    // clone: a leaf: a symbol, a span and a flag or two, no heap. Built once per rule, when the macro is defined.
                    self.literals.push(it.clone());
                }
                Op::Ident(it) => {
                    self.locs.push(Loc::Ident(self.idents.len() as u32));
                    // clone: a leaf: a symbol, a span and a flag or two, no heap. Built once per rule, when the macro is defined.
                    self.idents.push(it.clone());
                }
                Op::Punct(it) => {
                    self.locs.push(Loc::Punct(self.puncts.len() as u32));
                    // clone: a glued punct, inline. Built once per rule, when the macro is defined.
                    self.puncts.push((**it).clone());
                }
                Op::Var { binder, kind, name, spec, decl, .. } => {
                    debug_assert_ne!(*binder, NO_BINDER, "a pattern's metavariable is unresolved");
                    let decl = failure::VarDecl { name: *name.sym(), spec: *spec, span: *decl };
                    self.var_decls.push((self.locs.len() as u32, decl));
                    self.locs.push(Loc::Var { binder: *binder, kind: *kind });
                }
                Op::Subtree { tokens, delimiter } => {
                    let visible = delimiter.kind != DelimiterKind::Invisible;
                    self.locs.push(Loc::Subtree(delimiter.kind));
                    let open = self.locs.len() as u32;
                    if visible {
                        self.locs.push(Loc::Open);
                    }
                    self.flatten(tokens);
                    if visible {
                        let group = failure::Group { open, close: self.locs.len() as u32, delimiter: *delimiter };
                        self.groups.push(group);
                        self.locs.push(Loc::Close);
                    }
                }
                Op::Repeat { tokens, kind, separator } => {
                    let rep = self.reps.len();
                    let vars_start = self.rep_vars.len() as u32;
                    collect_vars(tokens, &mut self.rep_vars);
                    let vars_len = self.rep_vars.len() as u32 - vars_start;
                    let separator = match separator {
                        Some(separator) => {
                            // clone: a separator leaf. Built once per rule, when the macro is defined.
                            self.separators.push((**separator).clone());
                            (self.separators.len() - 1) as u32
                        }
                        None => NONE,
                    };
                    self.reps.push(Rep {
                        body: 0,
                        after: 0,
                        vars_start,
                        vars_len,
                        separator,
                        kind: *kind,
                    });
                    self.locs.push(Loc::RepStart(rep as u32));
                    let body = self.locs.len() as u32;
                    self.flatten(tokens);
                    self.locs.push(Loc::RepEnd(rep as u32));
                    self.reps[rep].body = body;
                    self.reps[rep].after = self.locs.len() as u32;
                }
                Op::Ignore { .. }
                | Op::Index { .. }
                | Op::Count { .. }
                | Op::Len { .. }
                | Op::Concat { .. } => self.locs.push(Loc::MetaExpr),
            }
        }
    }
}

/// The metavariables of a repetition's body, which a repetition matched zero times binds as
/// empty.
fn collect_vars(pattern: &MetaTemplate, vars: &mut Vec<Binder>) {
    for op in pattern.iter() {
        match op {
            Op::Var { binder, .. } => vars.push(*binder),
            Op::Subtree { tokens, .. } | Op::Repeat { tokens, .. } => collect_vars(tokens, vars),
            Op::Literal(_)
            | Op::Ident(_)
            | Op::Punct(_)
            | Op::Ignore { .. }
            | Op::Index { .. }
            | Op::Count { .. }
            | Op::Len { .. }
            | Op::Concat { .. } => {}
        }
    }
}

#[derive(Debug)]
pub(super) struct Match<'t> {
    pub(super) bindings: Bindings<'t>,
    /// We currently just keep the first error.
    pub(super) err: Option<MatchError>,
    /// How many top-level token trees were left to match.
    pub(super) unmatched_tts: usize,
    /// The number of bound variables
    pub(super) bound_count: usize,
}

impl<'t> Match<'t> {
    pub(super) fn new_in(frame: &'t Frame) -> Self {
        Match { bindings: Bindings::new_in(frame), err: None, unmatched_tts: 0, bound_count: 0 }
    }

    fn clear(&mut self) {
        self.bindings.clear();
        self.err = None;
        self.unmatched_tts = 0;
        self.bound_count = 0;
    }

    fn add_err(&mut self, err: impl Into<MatchError>) {
        if self.err.is_none() {
            self.err = Some(err.into());
        }
    }

    /// As [`Match::add_err`], building the error only when it is the first.
    fn add_err_with(&mut self, err: impl FnOnce() -> ExpandError) {
        if self.err.is_none() {
            self.err = Some(MatchError::Built(err()));
        }
    }
}

/// A match's first error, as what makes it: one a fragment's token makes is built into an
/// [`ExpandError`] only for the match the expander reports, not for each match a later token or
/// rule makes moot (`$($t:tt)*` fails one more `tt` at the end of every input it matches).
#[derive(Debug)]
pub(super) enum MatchError {
    Built(ExpandError),
    /// A fragment's binding error, `what` the fragment it expected.
    Expected { span: span::Span, what: &'static str },
    /// A fragment whose parse took tokens and then failed: the parser's error, where it was,
    /// which says more than the token the fragment started at.
    Fragment(ExpandError),
}

impl MatchError {
    pub(super) fn build(self) -> ExpandError {
        match self {
            MatchError::Built(it) | MatchError::Fragment(it) => it,
            MatchError::Expected { span, what } => ExpandError::binding_error(span, what),
        }
    }
}

/// A fragment's parse, its error a [`MatchError::Fragment`] when the parse took tokens first.
fn fragment_parsed(
    parsed: ExpandResult<tt::TokenTreesView<'_>>,
) -> ValueResult<tt::TokenTreesView<'_>, MatchError> {
    let inside = !parsed.value.is_empty();
    parsed.map_err(|err| match inside {
        true => MatchError::Fragment(err),
        false => MatchError::Built(err),
    })
}

impl From<ExpandError> for MatchError {
    fn from(it: ExpandError) -> Self {
        MatchError::Built(it)
    }
}

/// A matcher position: where its dot is, the repetition it is in and what it bound. 24 bytes,
/// copied on every fork.
#[derive(Clone, Copy, Debug)]
struct Pos {
    /// An index into `Matcher::locs`.
    dot: u32,
    /// The position its repetition was entered from, `Buffers::ups[_]`, or [`NONE`] outside
    /// repetitions.
    up: u32,
    /// What it bound in the current iteration of its repetition (or outside any), the newest
    /// of a list of `Buffers::entries`.
    cur: u32,
    /// The finished iterations of its repetition, the newest of a list of `Buffers::iterations`.
    iterations: u32,
    /// A matched separator, glued punctuation or fragment the next step goes on after,
    /// `Buffers::metas[_]`, or [`NONE`].
    meta: u32,
    sep_matched: bool,
    /// Is error occurred in this state, will `poised` to "parent"
    is_error: bool,
}

const _: () = assert!(size_of::<Pos>() == 24);

impl Pos {
    fn start() -> Pos {
        Pos {
            dot: 0,
            up: NONE,
            cur: NONE,
            iterations: NONE,
            meta: NONE,
            sep_matched: false,
            is_error: false,
        }
    }
}

/// One binding, a cons cell: what it binds and the binding before it. 20 bytes.
#[derive(Clone, Copy, Debug)]
struct Entry {
    prev: u32,
    /// [`NO_BINDER`] for `Nested`, which binds the names of its iterations.
    binder: Binder,
    kind: EntryKind,
}

#[derive(Clone, Copy, Debug)]
enum EntryKind {
    /// In a repetition matched zero times.
    Empty,
    /// A fragment that may be empty (`vis`) and is.
    Optional,
    /// `Buffers::fragments[_]`.
    Fragment(u32),
    Missing(MetaVarKind),
    /// What a repetition bound: its finished iterations (`Buffers::iterations`) and the last.
    Nested { iterations: u32, last: u32 },
}

const _: () = assert!(size_of::<Entry>() == 20);

/// A finished iteration of a repetition: its bindings and the iteration before it.
#[derive(Clone, Copy, Debug)]
struct Iteration {
    prev: u32,
    head: u32,
}

/// Where the input goes on after a token a position matched over more than one step's worth of
/// tokens: a separator, glued punctuation or a fragment the black-box parser took.
#[derive(Debug)]
struct Meta<'t> {
    iter: TtIter<'t>,
    /// `Buffers::fragments[_]` for a fragment, else [`NONE`].
    fragment: u32,
}

/// A list of a `Nested` binding's iterations while [`Buffers::build`] makes it.
#[derive(Clone, Copy, Debug)]
struct List {
    head: u32,
    tail: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug)]
struct Link {
    node: u32,
    next: u32,
}

/// Everything a match allocates, reused for each rule of a call and each step of a match. The
/// positions' lists (`ups`, `entries`, `iterations`, `fragments`, `metas`) only grow during a
/// match, and what is pushed is never changed, so any number of positions share them.
#[derive(Debug)]
pub(super) struct Buffers<'t> {
    /// Where every list below lives: the frame of the call's [`expand_rules`].
    ///
    /// [`expand_rules`]: super::expand_rules
    frame: &'t Frame,
    cur_items: Vec<Pos, &'t Frame>,
    next_items: Vec<Pos, &'t Frame>,
    bb_items: Vec<Pos, &'t Frame>,
    eof_items: Vec<Pos, &'t Frame>,
    error_items: Vec<Pos, &'t Frame>,
    ups: Vec<Pos, &'t Frame>,
    entries: Vec<Entry, &'t Frame>,
    iterations: Vec<Iteration, &'t Frame>,
    fragments: Vec<Fragment<'t>, &'t Frame>,
    metas: Vec<Meta<'t>, &'t Frame>,
    /// The input levels around the one being matched, innermost last.
    stack: Vec<TtIter<'t>, &'t Frame>,
    /// `Buffers::build`'s scratch, empty (or all [`NONE`]) between builds.
    order: Vec<u32, &'t Frame>,
    /// Per level of repetition and binder, its binding at that level while it is being built.
    slots: Vec<u32, &'t Frame>,
    /// Per level, the binders with a slot set.
    touched: Vec<Vec<Binder, &'t Frame>, &'t Frame>,
    lists: Vec<List, &'t Frame>,
    links: Vec<Link, &'t Frame>,
}

impl<'t> Buffers<'t> {
    pub(super) fn new_in(frame: &'t Frame) -> Self {
        Buffers {
            frame,
            cur_items: Vec::new_in(frame),
            next_items: Vec::new_in(frame),
            bb_items: Vec::new_in(frame),
            eof_items: Vec::new_in(frame),
            error_items: Vec::new_in(frame),
            ups: Vec::new_in(frame),
            entries: Vec::new_in(frame),
            iterations: Vec::new_in(frame),
            fragments: Vec::new_in(frame),
            metas: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            order: Vec::new_in(frame),
            slots: Vec::new_in(frame),
            touched: Vec::new_in(frame),
            lists: Vec::new_in(frame),
            links: Vec::new_in(frame),
        }
    }

    fn clear(&mut self) {
        self.cur_items.clear();
        self.next_items.clear();
        self.bb_items.clear();
        self.eof_items.clear();
        self.error_items.clear();
        self.ups.clear();
        self.entries.clear();
        self.iterations.clear();
        self.fragments.clear();
        self.metas.clear();
        self.stack.clear();
    }

    fn push(&mut self, head: &mut u32, binder: Binder, kind: EntryKind) {
        self.entries.push(Entry { prev: *head, binder, kind });
        *head = (self.entries.len() - 1) as u32;
    }

    /// Ends the current iteration of `item`'s repetition.
    fn push_iteration(&mut self, item: &mut Pos) {
        self.iterations.push(Iteration { prev: item.iterations, head: item.cur });
        item.iterations = (self.iterations.len() - 1) as u32;
        item.cur = NONE;
    }

    fn push_fragment(&mut self, fragment: Fragment<'t>) -> u32 {
        self.fragments.push(fragment);
        (self.fragments.len() - 1) as u32
    }

    fn push_meta(&mut self, iter: TtIter<'t>, fragment: u32) -> u32 {
        self.metas.push(Meta { iter, fragment });
        (self.metas.len() - 1) as u32
    }

    /// Makes the trees of the bindings `head` into `out`. `head` is a position's `cur`: what it
    /// bound outside repetitions, or in the current iteration of the one it is in.
    ///
    /// The cells are read oldest first. A later binding of a name replaces an earlier one at
    /// its level. A `Nested` cell adds, for every name its iterations bind, the iteration's
    /// binding to the name's list at this level, after empty lists for the iterations that
    /// bound nothing to it; a name already bound to a leaf at this level keeps the leaf.
    fn build(&mut self, matcher: &Matcher, head: u32, out: &mut Bindings<'t>) {
        let binders = matcher.binders as usize;
        out.clear();
        self.build_level(head, 0, binders, out);
        out.roots.resize(binders, NONE);
        for &binder in &self.touched[0] {
            out.roots[binder as usize] = std::mem::replace(&mut self.slots[binder as usize], NONE);
        }
        self.touched[0].clear();

        // The lists into CSR ranges of `children`.
        for node in 0..out.nodes.len() {
            if let Binding::Nested { start: list, .. } = out.nodes[node] {
                let List { head, len, .. } = self.lists[list as usize];
                let start = out.children.len() as u32;
                let mut at = head;
                while at != NONE {
                    let link = self.links[at as usize];
                    out.children.push(link.node);
                    at = link.next;
                }
                out.nodes[node] = Binding::Nested { start, len };
            }
        }
        self.lists.clear();
        self.links.clear();
    }

    /// The bindings `head` into the slots of `level`.
    fn build_level(&mut self, head: u32, level: usize, binders: usize, out: &mut Bindings<'t>) {
        if self.touched.len() <= level {
            let frame = self.frame;
            self.touched.resize_with(level + 1, || Vec::new_in(frame));
        }
        if self.slots.len() < (level + 1) * binders {
            self.slots.resize((level + 1) * binders, NONE);
        }
        let base = self.order.len();
        let mut at = head;
        while at != NONE {
            self.order.push(at);
            at = self.entries[at as usize].prev;
        }
        // `order[base..]` is newest first; deeper calls push above it and truncate back.
        let mut i = self.order.len();
        while i > base {
            i -= 1;
            let entry = self.entries[self.order[i] as usize];
            let node = match entry.kind {
                EntryKind::Empty => out.push_node(Binding::Empty),
                EntryKind::Optional => {
                    let fragment = out.push_fragment(Fragment::Empty);
                    out.push_node(Binding::Fragment(fragment))
                }
                EntryKind::Fragment(fragment) => {
                    // clone: bindings hold views only; the matcher makes no owned fragment.
                    let fragment = out.push_fragment(self.fragments[fragment as usize].clone());
                    out.push_node(Binding::Fragment(fragment))
                }
                EntryKind::Missing(kind) => out.push_node(Binding::Missing(kind)),
                EntryKind::Nested { iterations, last } => {
                    self.build_nested(iterations, last, level, binders, out);
                    continue;
                }
            };
            let slot = &mut self.slots[level * binders + entry.binder as usize];
            if *slot == NONE {
                self.touched[level].push(entry.binder);
            }
            *slot = node;
        }
        self.order.truncate(base);
    }

    fn build_nested(
        &mut self,
        iterations: u32,
        last: u32,
        level: usize,
        binders: usize,
        out: &mut Bindings<'t>,
    ) {
        let base = self.order.len();
        let mut at = iterations;
        while at != NONE {
            let iteration = self.iterations[at as usize];
            self.order.push(iteration.head);
            at = iteration.prev;
        }
        // `order[base..base + count - 1]` holds the finished iterations newest first.
        let count = self.order.len() - base + 1;
        for idx in 0..count {
            let head = if idx + 1 == count { last } else { self.order[base + count - 2 - idx] };
            self.build_level(head, level + 1, binders, out);
            let inner = (level + 1) * binders;
            let mut touched =
                std::mem::replace(&mut self.touched[level + 1], Vec::new_in(self.frame));
            for &binder in &touched {
                let value = std::mem::replace(&mut self.slots[inner + binder as usize], NONE);
                let slot = level * binders + binder as usize;
                let mut target = self.slots[slot];
                if target == NONE {
                    let list = self.new_list();
                    target = out.push_node(Binding::Nested { start: list, len: 0 });
                    self.slots[slot] = target;
                    self.touched[level].push(binder);
                }
                if let Binding::Nested { start: list, .. } = out.nodes[target as usize] {
                    // insert empty nested bindings before this one
                    while (self.lists[list as usize].len as usize) < idx {
                        let empty = self.new_list();
                        let empty = out.push_node(Binding::Nested { start: empty, len: 0 });
                        self.append(list, empty);
                    }
                    self.append(list, value);
                }
            }
            touched.clear();
            self.touched[level + 1] = touched;
        }
        self.order.truncate(base);
    }

    fn new_list(&mut self) -> u32 {
        self.lists.push(List { head: NONE, tail: NONE, len: 0 });
        (self.lists.len() - 1) as u32
    }

    fn append(&mut self, list: u32, node: u32) {
        let link = self.links.len() as u32;
        self.links.push(Link { node, next: NONE });
        let list = &mut self.lists[list as usize];
        if list.len == 0 {
            list.head = link;
        } else {
            self.links[list.tail as usize].next = link;
        }
        list.tail = link;
        list.len += 1;
    }
}

impl<'t> Bindings<'t> {
    fn push_node(&mut self, binding: Binding) -> u32 {
        self.nodes.push(binding);
        (self.nodes.len() - 1) as u32
    }

    fn push_fragment(&mut self, fragment: Fragment<'t>) -> u32 {
        self.fragments.push(fragment);
        (self.fragments.len() - 1) as u32
    }
}

/// Matching errors are added to the `Match`.
pub(super) fn match_<'t>(
    db: &Db,
    matcher: &Matcher,
    input: &'t tt::TopSubtree,
    buffers: &mut Buffers<'t>,
    res: &mut Match<'t>,
) {
    match_loop(db, matcher, input, buffers, res, None);
    let bindings = &res.bindings;
    res.bound_count = bindings
        .roots
        .iter()
        .filter(|&&node| node != NONE)
        .map(|&node| bindings.bound(bindings.nodes[node as usize]))
        .sum();
}

/// The span of the token at the dot, or of the end of the input.
fn token_span(token: &Option<TtElement<'_>>, delim_span: DelimSpan) -> span::Span {
    token.as_ref().map_or(delim_span.close, TtElement::first_span)
}

/// Process the matcher positions of `cur_items` until it is empty. In the process, this will
/// produce more items in `next_items`, `eof_items`, and `bb_items`.
///
/// For more info about how this happens, see the module-level doc comments and the inline
/// comments of this function.
///
/// # Parameters
///
/// - `src`: the current token of the parser.
/// - `res`: the match result to store errors
/// - `a`: the [`Buffers`], which hold the sets of items: `cur_items`, the set of current items
///   to be processed. This should be empty by the end of a successful execution of this function. `next_items`,
///   the set of newly generated items, used to replenish `cur_items` in `match_loop`;
///   `eof_items`, the set of items that would be valid if this was the EOF; `bb_items`, the set
///   of items that are waiting for the black-box parser; `error_items`, the set of items in
///   errors, used for error-resilient parsing. And `stack`, the "parent" frames of the input.
#[inline]
fn match_loop_inner<'t>(
    db: &Db,
    matcher: &Matcher,
    src: &TtIter<'t>,
    res: &mut Match<'t>,
    a: &mut Buffers<'t>,
    delim_span: DelimSpan,
    mut track: Option<&mut Tracker>,
) {
    let is_delim_closed = src.is_empty() && !a.stack.is_empty();
    // The token at the dot, decoded once for every position, and the punctuation glued from it.
    let token = src.peek();
    let mut glued: Option<Result<(ArrayVec<tt::Punct, MAX_GLUED_PUNCT_LEN>, TtIter<'t>), ()>> =
        None;
    let mut glued_meta = NONE;

    macro_rules! try_push {
        ($items: ident, $it:expr) => {{
            let it = $it;
            if it.is_error {
                a.error_items.push(it);
            } else {
                a.$items.push(it);
            }
        }};
    }

    while let Some(mut item) = a.cur_items.pop() {
        if let Some(track) = track.as_deref_mut() {
            track.before_loc(matcher, item.dot, item.sep_matched, a.cur_items.is_empty());
        }
        match matcher.locs[item.dot as usize] {
            // At the end of the matcher, not in a repetition: being at the end of a matcher
            // means that we have reached the potential end of the input.
            Loc::Eof => try_push!(eof_items, item),
            // We are at the end of the matcher of a repetition.
            Loc::RepEnd(rep) => {
                let rep = matcher.reps[rep as usize];
                if !item.sep_matched {
                    // Get the `up` matcher, add matches from this repetition to its own and
                    // move its "dot" past the repetition.
                    let mut new_pos = a.ups[item.up as usize];
                    a.push(
                        &mut new_pos.cur,
                        NO_BINDER,
                        EntryKind::Nested { iterations: item.iterations, last: item.cur },
                    );
                    new_pos.dot = rep.after;
                    new_pos.is_error = new_pos.is_error || item.is_error;
                    a.cur_items.push(new_pos);
                }

                // Check if we need a separator.
                if rep.separator != NONE && !item.sep_matched {
                    // clone: a cursor, one view.
                    let mut fork = src.clone();
                    if expect_separator(&mut fork, &matcher.separators[rep.separator as usize]) {
                        // The dot stays at the end; the next step starts the next iteration.
                        item.meta = a.push_meta(fork, NONE);
                        item.sep_matched = true;
                        try_push!(next_items, item);
                    }
                }
                // We don't need a separator. Move the "dot" back to the beginning of the matcher
                // and try to match again UNLESS we are only allowed to have _one_ repetition.
                else if rep.kind != RepeatKind::ZeroOrOne {
                    item.dot = rep.body;
                    item.sep_matched = false;
                    a.push_iteration(&mut item);
                    a.cur_items.push(item);
                }
            }
            Loc::RepStart(rep) => {
                let rep = matcher.reps[rep as usize];
                if matches!(rep.kind, RepeatKind::ZeroOrMore | RepeatKind::ZeroOrOne) {
                    let mut new_item = item;
                    new_item.dot = rep.after;
                    for &binder in matcher.rep_vars(rep) {
                        a.push(&mut new_item.cur, binder, EntryKind::Empty);
                    }
                    a.cur_items.push(new_item);
                }
                let up = a.ups.len() as u32;
                a.ups.push(item);
                a.cur_items.push(Pos { dot: rep.body, up, ..Pos::start() });
            }
            Loc::Subtree(kind) => {
                if let Some(TtElement::Subtree(subtree, _)) = &token
                    && subtree.delimiter.kind == kind
                {
                    item.dot += 1;
                    a.cur_items.push(item);
                }
            }
            Loc::Var { binder, kind } => {
                if let Some(kind) = kind {
                    // clone: a cursor, one view.
                    let mut fork = src.clone();
                    let match_res = match_meta_var(db, kind, &mut fork, delim_span);
                    match match_res.err {
                        None => {
                            // Some meta variables are optional (e.g. vis)
                            if !match_res.value.is_empty() {
                                let fragment = a.push_fragment(match_res.value);
                                item.meta = a.push_meta(fork, fragment);
                                try_push!(bb_items, item);
                            } else {
                                a.push(&mut item.cur, binder, EntryKind::Optional);
                                item.dot += 1;
                                a.cur_items.push(item);
                            }
                        }
                        Some(err) => {
                            res.add_err(err);
                            let entry = if !match_res.value.is_empty() {
                                EntryKind::Fragment(a.push_fragment(match_res.value))
                            } else {
                                EntryKind::Missing(kind)
                            };
                            a.push(&mut item.cur, binder, entry);
                            item.is_error = true;
                            a.error_items.push(item);
                        }
                    }
                }
            }
            Loc::Literal(lhs) => {
                let lhs = &matcher.literals[lhs as usize];
                if let Some(TtElement::Leaf(rhs)) = &token {
                    if matches!(rhs, tt::Leaf::Literal(it) if it.text_and_suffix == lhs.text_and_suffix)
                    {
                        item.dot += 1;
                    } else {
                        res.add_err_with(|| {
                            ExpandError::new(*rhs.span(), ExpandErrorKind::UnexpectedToken)
                        });
                        item.is_error = true;
                    }
                } else {
                    res.add_err_with(|| {
                        ExpandError::binding_error(
                            token_span(&token, delim_span),
                            // alloc: an error.
                            format!("expected literal: `{lhs}`"),
                        )
                    });
                    item.is_error = true;
                }
                try_push!(next_items, item);
            }
            Loc::Ident(lhs) => {
                let lhs = &matcher.idents[lhs as usize];
                if let Some(TtElement::Leaf(rhs)) = &token {
                    if matches!(rhs, tt::Leaf::Ident(it) if it.sym == lhs.sym && it.is_raw == lhs.is_raw)
                    {
                        item.dot += 1;
                    } else {
                        res.add_err_with(|| {
                            ExpandError::new(*rhs.span(), ExpandErrorKind::UnexpectedToken)
                        });
                        item.is_error = true;
                    }
                } else {
                    res.add_err_with(|| {
                        ExpandError::binding_error(
                            token_span(&token, delim_span),
                            // alloc: an error.
                            format!("expected ident: `{lhs}`"),
                        )
                    });
                    item.is_error = true;
                }
                try_push!(next_items, item);
            }
            Loc::Punct(lhs) => {
                let lhs = &matcher.puncts[lhs as usize];
                let glued = glued.get_or_insert_with(|| {
                    // clone: a cursor, one view.
                    let mut fork = src.clone();
                    fork.expect_glued_punct().map(|rhs| (rhs, fork))
                });
                match glued {
                    Ok((rhs, fork)) => {
                        if lhs.iter().map(|it| it.char).eq(rhs.iter().map(|it| it.char)) {
                            // The fork is the same for every position matching here.
                            if glued_meta == NONE {
                                // clone: a cursor, one view.
                                glued_meta = a.push_meta(fork.clone(), NONE);
                            }
                            item.meta = glued_meta;
                            item.dot += 1;
                            a.next_items.push(item);
                            continue;
                        }
                        let rhs = &*rhs;
                        res.add_err_with(|| {
                            if rhs[0].char == '\'' {
                                // If the first punct token is a single quote, that's a part of
                                // a lifetime ident, not a punct.
                                ExpandError::new(
                                    rhs.get(1).map_or(rhs[0].span, |it| it.span),
                                    ExpandErrorKind::UnexpectedToken,
                                )
                            } else {
                                let lhs = lhs.iter().map(|it| it.char).collect::<String>();
                                ExpandError::binding_error(
                                    rhs[0].span,
                                    // alloc: an error.
                                    format!("expected punct: `{lhs}`"),
                                )
                            }
                        });
                    }
                    Err(()) => res.add_err_with(|| {
                        ExpandError::new(
                            token_span(&token, delim_span),
                            ExpandErrorKind::UnexpectedToken,
                        )
                    }),
                }
                item.is_error = true;
                a.error_items.push(item);
            }
            Loc::MetaExpr => {
                unreachable!("`MetaTemplate::parse` refuses `${{..}}` in a matcher");
            }
            Loc::Open => {
                if matches!(token, Some(TtElement::Subtree(..))) {
                    item.dot += 1;
                    try_push!(next_items, item);
                }
            }
            Loc::Close => {
                if is_delim_closed {
                    item.dot += 1;
                    try_push!(next_items, item);
                }
            }
        }
    }
}

fn match_loop<'t>(
    db: &Db,
    matcher: &Matcher,
    input: &'t tt::TopSubtree,
    a: &mut Buffers<'t>,
    res: &mut Match<'t>,
    mut track: Option<&mut Tracker>,
) {
    let span = input.top_subtree().delimiter.delim_span();
    let mut src = input.iter();
    a.clear();
    res.clear();
    let mut error_recover_item = None;
    if let Some(track) = track.as_deref_mut() {
        track.start(span);
    }

    a.cur_items.push(Pos::start());

    loop {
        a.bb_items.clear();
        a.eof_items.clear();
        a.error_items.clear();

        assert!(a.next_items.is_empty());

        match_loop_inner(db, matcher, &src, res, a, span, track.as_deref_mut());
        assert!(a.cur_items.is_empty());

        if let Some(item) = a.error_items.pop() {
            error_recover_item = Some(item.cur);
        } else if let Some(state) = a.eof_items.first() {
            error_recover_item = Some(state.cur);
        }

        // We need to do some post processing after the `match_loop_inner`.
        // If we reached the EOF, check that there is EXACTLY ONE possible matcher. Otherwise,
        // either the parse is ambiguous (which should never happen) or there is a syntax error.
        if src.is_empty() && a.stack.is_empty() {
            if let &[state] = a.eof_items.as_slice() {
                // remove all errors, because it is the correct answer !
                res.clear();
                a.build(matcher, state.cur, &mut res.bindings);
            } else {
                // Error recovery
                if let Some(item) = error_recover_item {
                    a.build(matcher, item, &mut res.bindings);
                }
                res.add_err(ExpandError::new(span.open, ExpandErrorKind::UnexpectedToken));
                // More than one is rustc's ambiguity error, not a failure.
                if a.eof_items.is_empty()
                    && let Some(track) = track
                {
                    track.fail(db, None);
                }
            }
            return;
        }

        // If there are no possible next positions AND we aren't waiting for the black-box parser,
        // then there is a syntax error.
        //
        // Another possibility is that we need to call out to parse some rust nonterminal
        // (black-box) parser. However, if there is not EXACTLY ONE of these, something is wrong.
        let has_leftover_tokens = (a.bb_items.is_empty() && a.next_items.is_empty())
            || !(a.bb_items.is_empty() || a.next_items.is_empty())
            || a.bb_items.len() > 1;
        if has_leftover_tokens {
            res.unmatched_tts += src.remaining().len();
            res.add_err(ExpandError::new(span.open, ExpandErrorKind::LeftoverTokens));
            // Positions waiting on more than one thing are rustc's ambiguity error.
            if a.bb_items.is_empty()
                && a.next_items.is_empty()
                && let Some(track) = track
            {
                track.fail(db, Some(&src));
            }

            if let Some(item) = error_recover_item {
                a.build(matcher, item, &mut res.bindings);
            }
            return;
        }
        // Dump all possible `next_items` into `cur_items` for the next iteration.
        else if !a.next_items.is_empty() {
            let meta = a.next_items[0].meta;
            if meta != NONE {
                // We've matched a possibly "glued" punct or a separator. The match must be the
                // same for all items. We already bumped, so no need to call `.next()` like in
                // the other branch.
                // clone: a cursor, one view.
                src = a.metas[meta as usize].iter.clone();
                for item in &mut a.next_items {
                    item.meta = NONE;
                }
                if let Some(track) = track.as_deref_mut() {
                    track.bump(1);
                }
            } else {
                let next = src.next();
                if let Some(track) = track.as_deref_mut() {
                    track.step(next.as_ref());
                }
                match next {
                    Some(TtElement::Subtree(_, subtree_iter)) => {
                        // clone: a cursor, one view.
                        a.stack.push(src.clone());
                        src = subtree_iter;
                    }
                    None => {
                        if let Some(iter) = a.stack.pop() {
                            src = iter;
                        }
                    }
                    _ => (),
                }
            }
            // Now process the next token
            a.cur_items.append(&mut a.next_items);
        }
        // Finally, we have the case where we need to call the black-box parser to get some
        // nonterminal.
        else {
            assert!(a.bb_items.len() == 1);
            let mut item = a.bb_items.pop().unwrap();

            let Loc::Var { binder, .. } = matcher.locs[item.dot as usize] else { unreachable!() };
            let meta = std::mem::replace(&mut item.meta, NONE);
            let Meta { iter, fragment } = &a.metas[meta as usize];
            // clone: a cursor, one view.
            let (iter, fragment) = (iter.clone(), *fragment);
            let entry =
                if fragment != NONE { EntryKind::Fragment(fragment) } else { EntryKind::Optional };
            a.push(&mut item.cur, binder, entry);
            if let Some(track) = track.as_deref_mut() {
                track.bump(failure::tokens(iter.from_savepoint(src.savepoint())));
            }
            src = iter;
            item.dot += 1;
            a.cur_items.push(item);
        }
        assert!(!a.cur_items.is_empty());
    }
}

fn match_meta_var<'t>(
    db: &Db,
    kind: MetaVarKind,
    input: &mut TtIter<'t>,
    delim_span: DelimSpan,
) -> ValueResult<Fragment<'t>, MatchError> {
    let fragment = match kind {
        MetaVarKind::Path => {
            return fragment_parsed(expect_fragment(db, input, parser::PrefixEntryPoint::Path, delim_span))
                .map(Fragment::Path);
        }
        MetaVarKind::Ty => {
            return fragment_parsed(expect_fragment(db, input, parser::PrefixEntryPoint::Ty, delim_span))
                .map(Fragment::Ty);
        }
        MetaVarKind::Expr(expr) => {
            // `expr_2021` should not match underscores, let expressions, or inline const.
            // The latter two are for [backwards compatibility][0].
            // And `expr` also should not contain let expressions but may contain the other two
            // since `Edition2024`.
            // HACK: Macro expansion should not be done using "rollback and try another alternative".
            // rustc [explicitly checks the next token][1].
            // [0]: https://github.com/rust-lang/rust/issues/86730
            // [1]: https://github.com/rust-lang/rust/blob/f0c4da499/compiler/rustc_expand/src/mbe/macro_parser.rs#L576
            match input.peek() {
                Some(TtElement::Leaf(tt::Leaf::Ident(it))) => {
                    let is_err = if it.is_raw.no() && matches!(expr, ExprKind::Expr2021) {
                        it.sym == sym::underscore || it.sym == sym::let_ || it.sym == sym::const_
                    } else {
                        it.sym == sym::let_
                    };
                    if is_err {
                        return ValueResult::only_err(MatchError::Built(ExpandError::new(
                            it.span,
                            ExpandErrorKind::NoMatchingRule,
                        )));
                    }
                }
                _ => {}
            };
            return fragment_parsed(expect_fragment(db, input, parser::PrefixEntryPoint::Expr, delim_span))
                .map(Fragment::Expr);
        }
        MetaVarKind::Ident | MetaVarKind::Tt | MetaVarKind::Lifetime | MetaVarKind::Literal => {
            let span = input.next_span();
            let savepoint = input.savepoint();
            let mut negative = false;
            let (found, what) = match kind {
                MetaVarKind::Ident => (input.expect_ident().map(drop), "expected ident"),
                MetaVarKind::Tt => (expect_tt(input), "expected token tree"),
                MetaVarKind::Lifetime => (expect_lifetime(input).map(drop), "expected lifetime"),
                MetaVarKind::Literal => {
                    negative = eat_char(input, '-');
                    (expect_literal(input), "expected literal")
                }
                _ => unreachable!(),
            };
            let err = found
                .err()
                .map(|()| MatchError::Expected { span: span.unwrap_or(delim_span.close), what });
            let value = match err {
                Some(_) => Fragment::Empty,
                // A sign and a number are one operand where they go, as an expression is.
                None if negative => Fragment::Expr(input.from_savepoint(savepoint)),
                None => {
                    let tree = input.from_savepoint(savepoint);
                    // A fragment taken as a token tree is still that fragment where it goes.
                    let origin = match kind {
                        MetaVarKind::Tt if is_fragment(tree) => TokensOrigin::Ast,
                        _ => TokensOrigin::Raw,
                    };
                    Fragment::Tokens { tree, origin }
                }
            };
            return ValueResult { value, err };
        }
        MetaVarKind::Pat => (parser::PrefixEntryPoint::PatTop, TokensOrigin::Ast),
        MetaVarKind::PatParam => (parser::PrefixEntryPoint::Pat, TokensOrigin::Ast),
        MetaVarKind::Stmt => (parser::PrefixEntryPoint::Stmt, TokensOrigin::Grouped),
        MetaVarKind::Block => (parser::PrefixEntryPoint::Block, TokensOrigin::Grouped),
        MetaVarKind::Meta => (parser::PrefixEntryPoint::MetaItem, TokensOrigin::Ast),
        MetaVarKind::Item => (parser::PrefixEntryPoint::Item, TokensOrigin::Ast),
        MetaVarKind::Vis => (parser::PrefixEntryPoint::Vis, TokensOrigin::Ast),
    };
    let (entry_point, origin) = fragment;
    fragment_parsed(expect_fragment(db, input, entry_point, delim_span))
        .map(|tree| Fragment::Tokens { tree, origin })
}

/// Whether `tree` is what an earlier expansion put in for one of its metavariables.
fn is_fragment(tree: tt::TokenTreesView<'_>) -> bool {
    tree.try_into_subtree()
        .is_some_and(|it| it.top_subtree().delimiter.kind == tt::DelimiterKind::Invisible)
}

/// A literal, which may be one that an earlier expansion took as an expression.
fn expect_literal(input: &mut TtIter<'_>) -> Result<(), ()> {
    // clone: a cursor, one view.
    let mut fork = input.clone();
    if let Ok((subtree, mut inner)) = fork.expect_subtree()
        && subtree.delimiter.kind == tt::DelimiterKind::Invisible
    {
        inner.expect_literal()?;
        if inner.next().is_some() {
            return Err(());
        }
        *input = fork;
        return Ok(());
    }
    input.expect_literal().map(drop)
}

fn expect_separator(iter: &mut TtIter<'_>, separator: &Separator) -> bool {
    // clone: a cursor, one view.
    let mut fork = iter.clone();
    let ok = match separator {
        Separator::Ident(lhs) => match fork.expect_ident_or_underscore() {
            Ok(rhs) => rhs.sym == lhs.sym,
            Err(_) => false,
        },
        Separator::Literal(lhs) => match fork.expect_literal() {
            Ok(rhs) => match rhs {
                tt::Leaf::Literal(rhs) => rhs.text_and_suffix == lhs.text_and_suffix,
                tt::Leaf::Ident(rhs) => rhs.sym == lhs.text_and_suffix,
                tt::Leaf::Punct(_) => false,
            },
            Err(_) => false,
        },
        Separator::Puncts(lhs) => match fork.expect_glued_punct() {
            Ok(rhs) => {
                let lhs = lhs.iter().map(|it| it.char);
                let rhs = rhs.iter().map(|it| it.char);
                lhs.eq(rhs)
            }
            Err(_) => false,
        },
        Separator::Lifetime(_punct, ident) => match expect_lifetime(&mut fork) {
            Ok(lifetime) => lifetime.sym == ident.sym,
            Err(_) => false,
        },
    };
    if ok {
        *iter = fork;
    }
    ok
}

fn expect_tt(iter: &mut TtIter<'_>) -> Result<(), ()> {
    if let Some(TtElement::Leaf(tt::Leaf::Punct(punct))) = iter.peek() {
        if punct.char == '\'' {
            expect_lifetime(iter)?;
        } else {
            iter.expect_glued_punct()?;
        }
    } else {
        iter.next().ok_or(())?;
    }
    Ok(())
}

fn expect_lifetime(iter: &mut TtIter<'_>) -> Result<tt::Ident, ()> {
    let punct = iter.expect_single_punct()?;
    if punct.char != '\'' {
        return Err(());
    }
    iter.expect_ident_or_underscore()
}

/// Takes the next token if it is the punctuation `c`, and says whether it was.
fn eat_char(iter: &mut TtIter<'_>, c: char) -> bool {
    let found = matches!(iter.peek(), Some(TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char, .. }))) if char == c);
    if found {
        iter.next();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budgets the module's docs state.
    #[test]
    fn sizes() {
        assert!(size_of::<Loc>() <= 8);
        assert_eq!(size_of::<Pos>(), 24);
        assert_eq!(size_of::<Entry>(), 20);
        assert_eq!(size_of::<Binding>(), 12);
    }
}
