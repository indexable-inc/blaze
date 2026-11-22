//! The matcher: an arm's locations run over the call's tokens as a set of positions advanced
//! one token at a time, as Rust's macro-by-example parser is specified (rustc's
//! `macro_parser.rs`), on explicit lists in the call's frame. A position's bindings are cons
//! cells shared with the positions it forked from, built into a tree only for the match kept.

use blaze_mem::frame::Frame;
use blaze_own::Vec;
use parser::PrefixEntryPoint;

use super::{
    Arm, At, Bound, CallError, DEPTH_BOUND, Engine, Frag, Loc, Msg, NONE, RepKind, SITE, Sep,
    failure::Tracker,
    input::{Joins, Level, View, same_puncts},
};
use crate::tokens::{Delim, Raw, TokenKind};

/// What a fragment was taken as, which says how the transcriber puts it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    /// Copied as it is.
    Raw,
    /// A whole syntax node: copied as it is.
    Ast,
    /// Put back in an invisible group (`stmt`, `block`).
    Grouped,
    /// Put back in an invisible group and parentheses: a list that would join with the tokens
    /// around it ([`super::input::Joins`]: an or-pattern, a type of bounds).
    Parenthesized,
}

/// Tokens `at..end` of the view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Toks {
    pub(crate) at: u32,
    pub(crate) end: u32,
}

impl Toks {
    pub(crate) fn is_empty(self) -> bool {
        self.at >= self.end
    }
}

/// A metavariable's value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fragment {
    Empty,
    Tokens { toks: Toks, origin: Origin },
    Expr(Toks),
    Path(Toks),
    Ty(Toks),
}

impl Fragment {
    pub(super) fn is_empty(self) -> bool {
        match self {
            Fragment::Empty => true,
            Fragment::Tokens { toks, .. }
            | Fragment::Expr(toks)
            | Fragment::Path(toks)
            | Fragment::Ty(toks) => toks.is_empty(),
        }
    }
}

/// A node of a match's bindings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Binding {
    /// A repetition matched no time.
    Empty,
    Fragment(u32),
    /// What a failed match bound: put back as a placeholder of the kind.
    Missing(Frag),
    /// A repetition's iterations: `Bindings::children[start..start + len]`.
    Nested { start: u32, len: u32 },
}

/// The bindings of one match: each binder's root node, or [`NONE`].
pub(crate) struct Bindings<'f> {
    pub(crate) roots: Vec<u32, &'f Frame>,
    pub(crate) nodes: Vec<Binding, &'f Frame>,
    pub(crate) fragments: Vec<Fragment, &'f Frame>,
    pub(crate) children: Vec<u32, &'f Frame>,
}

impl<'f> Bindings<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        Bindings {
            roots: Vec::new_in(frame),
            nodes: Vec::new_in(frame),
            fragments: Vec::new_in(frame),
            children: Vec::new_in(frame),
        }
    }

    fn clear(&mut self) {
        self.roots.clear();
        self.nodes.clear();
        self.fragments.clear();
        self.children.clear();
    }

    fn push_node(&mut self, binding: Binding) -> u32 {
        self.nodes.push(binding);
        crate::tokens::index(self.nodes.len() - 1)
    }

    fn push_fragment(&mut self, fragment: Fragment) -> u32 {
        self.fragments.push(fragment);
        crate::tokens::index(self.fragments.len() - 1)
    }

    pub(crate) fn root(&self, binder: u32) -> Option<Binding> {
        match self.roots.get(binder as usize).copied() {
            None | Some(NONE) => None,
            Some(node) => Some(self.nodes[node as usize]),
        }
    }

    pub(crate) fn child(&self, start: u32, idx: u32) -> Binding {
        self.nodes[self.children[(start + idx) as usize] as usize]
    }

    /// How many values a node binds.
    fn bound(&self, binding: Binding) -> usize {
        let Binding::Nested { start, len } = binding else { return 1 };
        let mut stack: Vec<(u32, u32, u32), &'f Frame> = Vec::new_in(*self.nodes.allocator());
        stack.push((start, 0, len));
        let mut total = 0usize;
        while let Some((s, idx, n)) = stack.last_mut() {
            if *idx >= *n {
                stack.pop();
                continue;
            }
            let cur = *idx;
            *idx = cur + 1;
            match self.child(*s, cur) {
                Binding::Nested { start, len } => stack.push((start, 0, len)),
                _ => total += 1,
            }
        }
        total
    }
}

impl super::linear::Values for Bindings<'_> {
    type Node = Binding;

    fn root(&self, binder: u32) -> Option<Binding> {
        Bindings::root(self, binder)
    }

    fn kind(&self, node: Binding) -> super::linear::NodeKind {
        use super::linear::NodeKind;
        match node {
            Binding::Empty => NodeKind::Empty,
            Binding::Fragment(fragment) => NodeKind::Fragment(self.fragments[fragment as usize]),
            Binding::Missing(kind) => NodeKind::Missing(kind),
            Binding::Nested { len, .. } => NodeKind::Nested(len),
        }
    }

    fn child(&self, node: Binding, idx: u32) -> Binding {
        let Binding::Nested { start, .. } = node else {
            unreachable!("an iteration is asked of a nested node")
        };
        Bindings::child(self, start, idx)
    }
}

/// A match's first error, as what makes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MatchError {
    Built(CallError),
    /// A fragment's error, `what` the fragment expected.
    Expected { at: At, what: &'static str },
    /// A fragment whose parse took tokens and then failed.
    Fragment(CallError),
}

impl MatchError {
    pub(crate) fn build(self) -> CallError {
        match self {
            MatchError::Built(error) | MatchError::Fragment(error) => error,
            MatchError::Expected { at, what } => CallError::Binding(at, Msg::Static(what)),
        }
    }
}

/// One match of an arm.
pub(crate) struct Match<'f> {
    pub(crate) bindings: Bindings<'f>,
    /// The first error.
    pub(crate) err: Option<MatchError>,
    /// How many tokens were left to match.
    pub(crate) unmatched: usize,
    /// How many values were bound.
    pub(crate) bound: usize,
}

impl<'f> Match<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        Match { bindings: Bindings::new_in(frame), err: None, unmatched: 0, bound: 0 }
    }

    fn clear(&mut self) {
        self.bindings.clear();
        self.err = None;
        self.unmatched = 0;
        self.bound = 0;
    }

    fn add_err(&mut self, err: MatchError) {
        if self.err.is_none() {
            self.err = Some(err);
        }
    }
}

/// A position in an arm's matcher.
#[derive(Clone, Copy, Debug)]
struct Pos {
    dot: u32,
    /// The position of the repetition this one is inside: `Buffers::ups[_]`.
    up: u32,
    /// Its bindings so far: `Buffers::entries[_]`.
    cur: u32,
    /// The finished iterations of its repetition: `Buffers::iterations[_]`.
    iterations: u32,
    /// Where the input goes on once the token this position waits on is taken.
    meta: u32,
    sep_matched: bool,
    is_error: bool,
}

impl Pos {
    fn start(dot: u32) -> Pos {
        Pos {
            dot,
            up: NONE,
            cur: NONE,
            iterations: NONE,
            meta: NONE,
            sep_matched: false,
            is_error: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum EntryKind {
    Empty,
    Optional,
    Fragment(u32),
    Missing(Frag),
    Nested { iterations: u32, last: u32 },
}

/// A binding a position made, over the ones it made before.
#[derive(Clone, Copy, Debug)]
struct Entry {
    prev: u32,
    binder: u32,
    kind: EntryKind,
}

#[derive(Clone, Copy, Debug)]
struct Iteration {
    prev: u32,
    head: u32,
}

/// The input after a token or fragment, and the fragment.
#[derive(Clone, Copy, Debug)]
struct Meta {
    at: u32,
    fragment: u32,
}

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

/// The matcher's lists, kept across the arms of a call.
pub(crate) struct Buffers<'f> {
    frame: &'f Frame,
    cur_items: Vec<Pos, &'f Frame>,
    next_items: Vec<Pos, &'f Frame>,
    bb_items: Vec<Pos, &'f Frame>,
    eof_items: Vec<Pos, &'f Frame>,
    error_items: Vec<Pos, &'f Frame>,
    ups: Vec<Pos, &'f Frame>,
    entries: Vec<Entry, &'f Frame>,
    iterations: Vec<Iteration, &'f Frame>,
    fragments: Vec<Fragment, &'f Frame>,
    metas: Vec<Meta, &'f Frame>,
    stack: Vec<Level, &'f Frame>,
    order: Vec<u32, &'f Frame>,
    slots: Vec<u32, &'f Frame>,
    touched: Vec<Vec<u32, &'f Frame>, &'f Frame>,
    lists: Vec<List, &'f Frame>,
    links: Vec<Link, &'f Frame>,
}

impl<'f> Buffers<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
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

    fn push(&mut self, head: &mut u32, binder: u32, kind: EntryKind) {
        self.entries.push(Entry { prev: *head, binder, kind });
        *head = crate::tokens::index(self.entries.len() - 1);
    }

    fn push_iteration(&mut self, item: &mut Pos) {
        self.iterations.push(Iteration { prev: item.iterations, head: item.cur });
        item.iterations = crate::tokens::index(self.iterations.len() - 1);
        item.cur = NONE;
    }

    fn push_fragment(&mut self, fragment: Fragment) -> u32 {
        self.fragments.push(fragment);
        crate::tokens::index(self.fragments.len() - 1)
    }

    fn push_meta(&mut self, at: u32, fragment: u32) -> u32 {
        self.metas.push(Meta { at, fragment });
        crate::tokens::index(self.metas.len() - 1)
    }

    /// The bindings of the position whose entries end at `head`.
    fn build(&mut self, binders: u32, head: u32, out: &mut Bindings<'f>) {
        let binders = binders as usize;
        out.clear();
        self.build_level(head, 0, binders, out);
        out.roots.resize(binders, NONE);
        let touched = core::mem::replace(&mut self.touched[0], Vec::new_in(self.frame));
        for &binder in touched.iter() {
            out.roots[binder as usize] = core::mem::replace(&mut self.slots[binder as usize], NONE);
        }
        self.touched[0] = touched;
        self.touched[0].clear();
        for node in 0..out.nodes.len() {
            if let Binding::Nested { start: list, .. } = out.nodes[node] {
                let List { head, len, .. } = self.lists[list as usize];
                let start = crate::tokens::index(out.children.len());
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

    /// The entries from `head` back, oldest first, into the slots of `level`: a later binding
    /// of a binder replaces an earlier one.
    fn build_level(&mut self, head: u32, level: usize, binders: usize, out: &mut Bindings<'f>) {
        if self.touched.len() <= level {
            let frame = self.frame;
            while self.touched.len() <= level {
                self.touched.push(Vec::new_in(frame));
            }
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
                    let fragment = out.push_fragment(self.fragments[fragment as usize]);
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

    /// A repetition's iterations, each built a level down and appended to its binders' lists
    /// at `level`; a binder bound in a later iteration only gets empty lists for the ones it
    /// missed.
    fn build_nested(
        &mut self,
        iterations: u32,
        last: u32,
        level: usize,
        binders: usize,
        out: &mut Bindings<'f>,
    ) {
        let base = self.order.len();
        let mut at = iterations;
        while at != NONE {
            let iteration = self.iterations[at as usize];
            self.order.push(iteration.head);
            at = iteration.prev;
        }
        let count = self.order.len() - base + 1;
        for idx in 0..count {
            let head = if idx + 1 == count { last } else { self.order[base + count - 2 - idx] };
            self.build_level(head, level + 1, binders, out);
            let inner = (level + 1) * binders;
            let mut touched =
                core::mem::replace(&mut self.touched[level + 1], Vec::new_in(self.frame));
            for &binder in touched.iter() {
                let value = core::mem::replace(&mut self.slots[inner + binder as usize], NONE);
                let slot = level * binders + binder as usize;
                let mut target = self.slots[slot];
                if target == NONE {
                    let list = self.new_list();
                    target = out.push_node(Binding::Nested { start: list, len: 0 });
                    self.slots[slot] = target;
                    self.touched[level].push(binder);
                }
                if let Binding::Nested { start: list, .. } = out.nodes[target as usize] {
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
        crate::tokens::index(self.lists.len() - 1)
    }

    fn append(&mut self, list: u32, node: u32) {
        let link = crate::tokens::index(self.links.len());
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

/// Matches `arm` against the call's tokens into `res`.
pub(crate) fn match_<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    arm: &Arm,
    view: &mut View<'_, 'f>,
    a: &mut Buffers<'f>,
    res: &mut Match<'f>,
    track: Option<&mut Tracker<'_>>,
) -> Result<(), Bound> {
    match_loop(engine, arm, view, a, res, track)?;
    let bindings = &res.bindings;
    res.bound = bindings
        .roots
        .iter()
        .filter(|&&node| node != NONE)
        .map(|&node| bindings.bound(bindings.nodes[node as usize]))
        .sum();
    Ok(())
}

/// The origin of the tree at the start of `level`, or the call site.
fn token_at(view: &View<'_, '_>, src: Level) -> At {
    match src.is_empty() {
        true => SITE,
        false => view.origin(src.at),
    }
}

/// Advances every position by the token at `src` without taking it: the matcher's epsilon
/// moves, and each position that waits on the token, a fragment or the end put in its list.
#[expect(clippy::too_many_arguments, reason = "the matcher's state, passed as its parts")]
fn match_loop_inner<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    _arm: &Arm,
    view: &mut View<'_, 'f>,
    src: Level,
    res: &mut Match<'f>,
    a: &mut Buffers<'f>,
    mut track: Option<&mut Tracker<'_>>,
) -> Result<(), Bound> {
    let program = engine.program;
    let def = engine.def;
    let is_delim_closed = src.is_empty() && !a.stack.is_empty();
    let token = (!src.is_empty()).then_some(src.at);
    // The punctuation glued at the token, once a step: its length (0 if it is no punctuation).
    let mut glued: Option<u8> = None;
    let mut glued_meta = NONE;

    macro_rules! try_push {
        ($items:ident, $it:expr) => {{
            let it = $it;
            if it.is_error {
                a.error_items.push(it);
            } else {
                a.$items.push(it);
            }
        }};
    }

    while let Some(mut item) = a.cur_items.pop() {
        engine.budget.step()?;
        if let Some(track) = track.as_deref_mut() {
            track.before_loc(program, item.dot, item.sep_matched, a.cur_items.is_empty());
        }
        match program.locs[item.dot as usize] {
            Loc::Eof => try_push!(eof_items, item),
            Loc::RepEnd(rep) => {
                let rep = program.reps[rep as usize];
                if !item.sep_matched {
                    let mut new_pos = a.ups[item.up as usize];
                    a.push(
                        &mut new_pos.cur,
                        NONE,
                        EntryKind::Nested { iterations: item.iterations, last: item.cur },
                    );
                    new_pos.dot = rep.after;
                    new_pos.is_error = new_pos.is_error || item.is_error;
                    a.cur_items.push(new_pos);
                }
                if rep.sep != NONE && !item.sep_matched {
                    let sep = program.seps[rep.sep as usize];
                    if let Some(after) = expect_separator(view, def, src, sep) {
                        item.meta = a.push_meta(after, NONE);
                        item.sep_matched = true;
                        try_push!(next_items, item);
                    }
                } else if rep.kind != RepKind::ZeroOrOne {
                    item.dot = rep.body;
                    item.sep_matched = false;
                    a.push_iteration(&mut item);
                    a.cur_items.push(item);
                }
            }
            Loc::RepStart(rep) => {
                let rep = program.reps[rep as usize];
                if matches!(rep.kind, RepKind::ZeroOrMore | RepKind::ZeroOrOne) {
                    let mut new_item = item;
                    new_item.dot = rep.after;
                    let vars = &program.rep_vars[rep.vars as usize..(rep.vars + rep.vars_len) as usize];
                    for &binder in vars {
                        a.push(&mut new_item.cur, binder, EntryKind::Empty);
                    }
                    a.cur_items.push(new_item);
                }
                let up = crate::tokens::index(a.ups.len());
                a.ups.push(item);
                a.cur_items.push(Pos { up, ..Pos::start(rep.body) });
            }
            Loc::Subtree(delim) => {
                if let Some(at) = token
                    && view.kind(at) == TokenKind::Open(delim)
                {
                    item.dot += 1;
                    a.cur_items.push(item);
                }
            }
            Loc::Var { binder, kind, .. } => {
                if let Some(kind) = kind {
                    let mut fork = src;
                    let (value, err) = match_meta_var(engine, view, kind, &mut fork);
                    match err {
                        None => {
                            if !value.is_empty() {
                                let fragment = a.push_fragment(value);
                                item.meta = a.push_meta(fork.at, fragment);
                                try_push!(bb_items, item);
                            } else {
                                a.push(&mut item.cur, binder, EntryKind::Optional);
                                item.dot += 1;
                                a.cur_items.push(item);
                            }
                        }
                        Some(err) => {
                            res.add_err(err);
                            let entry = match value.is_empty() {
                                false => EntryKind::Fragment(a.push_fragment(value)),
                                true => EntryKind::Missing(kind),
                            };
                            a.push(&mut item.cur, binder, entry);
                            item.is_error = true;
                            a.error_items.push(item);
                        }
                    }
                }
            }
            Loc::Literal(lhs) => {
                match token.map(|at| view.kind(at)) {
                    Some(TokenKind::Literal { kind, .. })
                        if matches!(def.kind(lhs), TokenKind::Literal { kind: k, .. } if k == kind)
                            && view.bytes(src.at) == def.bytes(lhs) =>
                    {
                        item.dot += 1;
                    }
                    Some(TokenKind::Literal { .. } | TokenKind::Ident { .. } | TokenKind::Punct { .. }) => {
                        res.add_err(MatchError::Built(CallError::UnexpectedToken(view.origin(src.at))));
                        item.is_error = true;
                    }
                    _ => {
                        let at = token_at(view, src);
                        res.add_err(MatchError::Built(CallError::Binding(at, Msg::ExpectedLiteral(lhs))));
                        item.is_error = true;
                    }
                }
                try_push!(next_items, item);
            }
            Loc::Ident(lhs) | Loc::Crate(lhs) => {
                let is_crate = matches!(program.locs[item.dot as usize], Loc::Crate(_));
                let (want_text, want_raw) = match is_crate {
                    true => (&b"$crate"[..], Raw::No),
                    false => match def.kind(lhs) {
                        TokenKind::Ident { raw, .. } => (def.bytes(lhs), raw),
                        _ => unreachable!("an identifier location is an identifier"),
                    },
                };
                match token.map(|at| view.kind(at)) {
                    Some(TokenKind::Ident { raw, .. })
                        if raw == want_raw && view.bytes(src.at) == want_text =>
                    {
                        item.dot += 1;
                    }
                    Some(TokenKind::Literal { .. } | TokenKind::Ident { .. } | TokenKind::Punct { .. }) => {
                        res.add_err(MatchError::Built(CallError::UnexpectedToken(view.origin(src.at))));
                        item.is_error = true;
                    }
                    _ => {
                        let at = token_at(view, src);
                        let msg = match is_crate {
                            true => Msg::ExpectedCrate,
                            false => Msg::ExpectedIdent(lhs),
                        };
                        res.add_err(MatchError::Built(CallError::Binding(at, msg)));
                        item.is_error = true;
                    }
                }
                try_push!(next_items, item);
            }
            Loc::Punct { at: lhs, len: lhs_len } => {
                let len = *glued.get_or_insert_with(|| match token {
                    Some(at) => view.glued(at, src.end),
                    None => 0,
                });
                if len > 0 {
                    let at = src.at;
                    if same_puncts(view, at, len, def, lhs, lhs_len) {
                        if glued_meta == NONE {
                            glued_meta = a.push_meta(at + u32::from(len), NONE);
                        }
                        item.meta = glued_meta;
                        item.dot += 1;
                        a.next_items.push(item);
                        continue;
                    }
                    let error = match view.char(at) == Some('\'') {
                        true => {
                            let second = if len > 1 { at + 1 } else { at };
                            CallError::UnexpectedToken(view.origin(second))
                        }
                        false => CallError::Binding(
                            view.origin(at),
                            Msg::ExpectedPunct { at: lhs, len: lhs_len },
                        ),
                    };
                    res.add_err(MatchError::Built(error));
                } else {
                    res.add_err(MatchError::Built(CallError::UnexpectedToken(token_at(view, src))));
                }
                item.is_error = true;
                a.error_items.push(item);
            }
            Loc::Open(_) => {
                if let Some(at) = token
                    && matches!(view.kind(at), TokenKind::Open(_))
                {
                    item.dot += 1;
                    try_push!(next_items, item);
                }
            }
            Loc::Close(_) => {
                if is_delim_closed {
                    item.dot += 1;
                    try_push!(next_items, item);
                }
            }
        }
    }
    Ok(())
}

/// The matcher's loop: one token of the input a step, until every position has ended or
/// failed.
fn match_loop<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    arm: &Arm,
    view: &mut View<'_, 'f>,
    a: &mut Buffers<'f>,
    res: &mut Match<'f>,
    mut track: Option<&mut Tracker<'_>>,
) -> Result<(), Bound> {
    let mut src = view.top();
    a.clear();
    res.clear();
    let mut error_recover_item = None;
    if let Some(track) = track.as_deref_mut() {
        track.start();
    }
    a.cur_items.push(Pos::start(arm.locs));
    loop {
        a.bb_items.clear();
        a.eof_items.clear();
        a.error_items.clear();
        match_loop_inner(engine, arm, view, src, res, a, track.as_deref_mut())?;
        if let Some(item) = a.error_items.pop() {
            error_recover_item = Some(item.cur);
        } else if let Some(state) = a.eof_items.first() {
            error_recover_item = Some(state.cur);
        }
        if src.is_empty() && a.stack.is_empty() {
            if let &[state] = &a.eof_items[..] {
                res.clear();
                a.build(arm.binders, state.cur, &mut res.bindings);
            } else {
                if let Some(item) = error_recover_item {
                    a.build(arm.binders, item, &mut res.bindings);
                }
                res.add_err(MatchError::Built(CallError::UnexpectedToken(SITE)));
                if a.eof_items.is_empty()
                    && let Some(track) = track
                {
                    track.fail(view, None);
                }
            }
            return Ok(());
        }
        let has_leftover_tokens = (a.bb_items.is_empty() && a.next_items.is_empty())
            || !(a.bb_items.is_empty() || a.next_items.is_empty())
            || a.bb_items.len() > 1;
        if has_leftover_tokens {
            res.unmatched += remaining(view, src);
            res.add_err(MatchError::Built(CallError::LeftoverTokens(SITE)));
            if a.bb_items.is_empty()
                && a.next_items.is_empty()
                && let Some(track) = track
            {
                track.fail(view, Some(src));
            }
            if let Some(item) = error_recover_item {
                a.build(arm.binders, item, &mut res.bindings);
            }
            return Ok(());
        } else if !a.next_items.is_empty() {
            let meta = a.next_items[0].meta;
            if meta != NONE {
                src.at = a.metas[meta as usize].at;
                for item in a.next_items.iter_mut() {
                    item.meta = NONE;
                }
                if let Some(track) = track.as_deref_mut() {
                    track.bump(1);
                }
            } else {
                let next = (!src.is_empty()).then_some(src.at);
                if let Some(track) = track.as_deref_mut() {
                    track.step(view, next);
                }
                match next {
                    Some(at) => match view.kind(at) {
                        TokenKind::Open(_) => {
                            let close = view.close_of(at);
                            a.stack.push(Level { at: close + 1, end: src.end });
                            if a.stack.len() > DEPTH_BOUND {
                                return Err(Bound(crate::error::RulesLimit::Depth));
                            }
                            src = view.inside(at);
                        }
                        _ => src.at += 1,
                    },
                    None => {
                        if let Some(level) = a.stack.pop() {
                            src = level;
                        }
                    }
                }
            }
            let mut next = core::mem::replace(&mut a.next_items, Vec::new_in(a.frame));
            a.cur_items.append(&mut next);
            a.next_items = next;
        } else {
            let mut item = a.bb_items.pop().expect("one position waits on a fragment");
            let Loc::Var { binder, .. } = engine.program.locs[item.dot as usize] else {
                unreachable!("a position waiting on a fragment is at a metavariable")
            };
            let meta = core::mem::replace(&mut item.meta, NONE);
            let Meta { at, fragment } = a.metas[meta as usize];
            let entry = match fragment != NONE {
                true => EntryKind::Fragment(fragment),
                false => EntryKind::Optional,
            };
            a.push(&mut item.cur, binder, entry);
            if let Some(track) = track.as_deref_mut() {
                track.bump(super::failure::tokens(view, src.at, at));
            }
            src.at = at;
            item.dot += 1;
            a.cur_items.push(item);
        }
    }
}

/// The tokens of `src` left, a group counted once with its inside and its close not at all.
fn remaining(view: &View<'_, '_>, src: Level) -> usize {
    (src.at..src.end).filter(|&at| !matches!(view.kind(at), TokenKind::Close(_))).count()
}

/// Takes a fragment of `kind` at `src`: its value and the first error.
pub(super) fn match_meta_var<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    view: &mut View<'_, 'f>,
    kind: Frag,
    src: &mut Level,
) -> (Fragment, Option<MatchError>) {
    let parsed = |engine: &mut Engine<'_, '_, 'f>, view: &mut View<'_, 'f>, src: &mut Level, entry| {
        let start = src.at;
        let (end, error, joins) = view.parse_fragment(engine, entry, *src);
        src.at = end;
        let toks = Toks { at: start, end };
        let error = error.map(|error| match toks.is_empty() {
            false => MatchError::Fragment(error),
            true => MatchError::Built(error),
        });
        (toks, error, joins)
    };
    let (entry, origin) = match kind {
        Frag::Path => {
            let (toks, error, _) = parsed(engine, view, src, PrefixEntryPoint::Path);
            return (Fragment::Path(toks), error);
        }
        Frag::Ty => {
            let (toks, error, joins) = parsed(engine, view, src, PrefixEntryPoint::Ty);
            return match joins {
                Joins::Yes => (Fragment::Tokens { toks, origin: Origin::Parenthesized }, error),
                Joins::No => (Fragment::Ty(toks), error),
            };
        }
        Frag::Expr | Frag::Expr2021 => {
            if !src.is_empty()
                && let TokenKind::Ident { raw, .. } = view.kind(src.at)
            {
                let text = view.bytes(src.at);
                let refused = match raw == Raw::No && kind == Frag::Expr2021 {
                    true => matches!(text, b"_" | b"let" | b"const"),
                    false => text == b"let",
                };
                if refused {
                    let error = CallError::NoMatchingRule(view.origin(src.at));
                    return (Fragment::Empty, Some(MatchError::Built(error)));
                }
            }
            let (toks, error, _) = parsed(engine, view, src, PrefixEntryPoint::Expr);
            return (Fragment::Expr(toks), error);
        }
        Frag::Ident | Frag::Tt | Frag::Lifetime | Frag::Literal => {
            let at = token_at(view, *src);
            let start = src.at;
            let mut negative = false;
            let (found, what) = match kind {
                Frag::Ident => (expect_ident(view, src, false).is_some(), "expected ident"),
                Frag::Tt => (expect_tt(view, src), "expected token tree"),
                Frag::Lifetime => (expect_lifetime(view, src).is_some(), "expected lifetime"),
                _ => {
                    negative = eat_char(view, src, '-');
                    (expect_literal_fragment(view, src), "expected literal")
                }
            };
            let toks = Toks { at: start, end: src.at };
            return match found {
                false => (Fragment::Empty, Some(MatchError::Expected { at, what })),
                true if negative => (Fragment::Expr(toks), None),
                true => {
                    let fragment = kind == Frag::Tt
                        && view.kind(toks.at) == TokenKind::Open(Delim::Invisible)
                        && view.close_of(toks.at) + 1 == toks.end;
                    let origin = if fragment { Origin::Ast } else { Origin::Raw };
                    (Fragment::Tokens { toks, origin }, None)
                }
            };
        }
        Frag::Pat => (PrefixEntryPoint::PatTop, Origin::Ast),
        Frag::PatParam => (PrefixEntryPoint::Pat, Origin::Ast),
        Frag::Stmt => (PrefixEntryPoint::Stmt, Origin::Grouped),
        Frag::Block => (PrefixEntryPoint::Block, Origin::Grouped),
        Frag::Meta => (PrefixEntryPoint::MetaItem, Origin::Ast),
        Frag::Item => (PrefixEntryPoint::Item, Origin::Ast),
        Frag::Vis => (PrefixEntryPoint::Vis, Origin::Ast),
    };
    let (toks, error, joins) = parsed(engine, view, src, entry);
    let origin = match joins {
        Joins::Yes => Origin::Parenthesized,
        Joins::No => origin,
    };
    (Fragment::Tokens { toks, origin }, error)
}

/// Takes the next tree of `src`, which must be a leaf.
fn expect_leaf(view: &View<'_, '_>, src: &mut Level) -> Option<u32> {
    if src.is_empty() {
        return None;
    }
    let at = src.at;
    src.at = view.after_tree(at);
    match view.kind(at) {
        TokenKind::Open(_) | TokenKind::Close(_) => None,
        _ => Some(at),
    }
}

/// An identifier (not `_`, unless `underscore`).
pub(crate) fn expect_ident(view: &View<'_, '_>, src: &mut Level, underscore: bool) -> Option<u32> {
    let at = expect_leaf(view, src)?;
    match view.kind(at) {
        TokenKind::Ident { .. } if underscore || view.bytes(at) != b"_" => Some(at),
        _ => None,
    }
}

/// A literal leaf, or `true` or `false`.
fn expect_literal(view: &View<'_, '_>, src: &mut Level) -> Option<u32> {
    let at = expect_leaf(view, src)?;
    match view.kind(at) {
        TokenKind::Literal { .. } => Some(at),
        TokenKind::Ident { .. } if matches!(view.bytes(at), b"true" | b"false") => Some(at),
        _ => None,
    }
}

/// A literal, or an invisible group of exactly one: one an earlier expansion took whole.
fn expect_literal_fragment(view: &View<'_, '_>, src: &mut Level) -> bool {
    if !src.is_empty() && view.kind(src.at) == TokenKind::Open(Delim::Invisible) {
        let mut inner = view.inside(src.at);
        if expect_literal(view, &mut inner).is_none() || !inner.is_empty() {
            return false;
        }
        src.at = view.after_tree(src.at);
        return true;
    }
    expect_literal(view, src).is_some()
}

/// `'name`: the name's index.
fn expect_lifetime(view: &View<'_, '_>, src: &mut Level) -> Option<u32> {
    let quote = expect_leaf(view, src)?;
    if view.char(quote) != Some('\'') {
        return None;
    }
    expect_ident(view, src, true)
}

/// One token tree: a lifetime, glued punctuation, or any other tree.
pub(super) fn expect_tt(view: &View<'_, '_>, src: &mut Level) -> bool {
    if src.is_empty() {
        return false;
    }
    match view.char(src.at) {
        Some('\'') => expect_lifetime(view, src).is_some(),
        Some(_) => {
            src.at += u32::from(view.glued(src.at, src.end));
            true
        }
        None => {
            src.at = view.after_tree(src.at);
            true
        }
    }
}

/// Takes the next token if it is the punctuation `c`.
fn eat_char(view: &View<'_, '_>, src: &mut Level, c: char) -> bool {
    let found = !src.is_empty() && view.char(src.at) == Some(c);
    if found {
        src.at += 1;
    }
    found
}

/// Whether `src` starts with the separator: where the input goes on after it.
pub(super) fn expect_separator(view: &View<'_, '_>, def: super::Def<'_>, src: Level, sep: Sep) -> Option<u32> {
    let mut fork = src;
    let ok = match sep {
        Sep::Ident(lhs) => {
            expect_ident(view, &mut fork, true).is_some_and(|at| view.bytes(at) == def.bytes(lhs))
        }
        // mbe compares a literal's text alone, `'a'` equal to `"a"`; this compares kinds too,
        // as rustc's token equality does.
        Sep::Literal(lhs) => expect_literal(view, &mut fork).is_some_and(|at| {
            view.bytes(at) == def.bytes(lhs)
                && match (view.kind(at), def.kind(lhs)) {
                    (TokenKind::Literal { kind: a, .. }, TokenKind::Literal { kind: b, .. }) => a == b,
                    (TokenKind::Ident { .. }, _) => true,
                    _ => false,
                }
        }),
        Sep::Puncts { at: lhs, len } => {
            let glued = match fork.is_empty() {
                true => 0,
                false => view.glued(fork.at, fork.end),
            };
            let same = glued > 0 && same_puncts(view, fork.at, glued, def, lhs, len);
            fork.at += u32::from(glued);
            same
        }
        Sep::Lifetime { name, .. } => {
            expect_lifetime(view, &mut fork).is_some_and(|at| view.bytes(at) == def.bytes(name))
        }
    };
    ok.then_some(fork.at)
}
