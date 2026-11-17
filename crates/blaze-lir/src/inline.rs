//! The trivial inliner (docs/v2/plan/design/backend-next.md, D98's first point): blaze's only
//! inliner, LIR to LIR, the first pass of the linear simplify stage in front of the fast tier.
//! It splices small loop-free bodies and `#[inline(always)]` ones into their callers; it reads no
//! profile. What it spliced comes back sorted and unique ([`Inlined::read`]): the fast code
//! record's read list gains exactly those bodies, so a change to any of them reruns the caller.
//!
//! # Which sites
//!
//! A site is an `Op::Call` (never `invoke`, `tail_call` or an indirect call) whose callee
//! [`Bodies::callee`] answers, and the callee
//!
//! - carries `#[inline(always)]` ([`FnAttrs::inline`]), or has at most [`Bound::insts`]
//!   instructions and an acyclic block graph; never `#[inline(never)]`;
//! - holds no `tail_call`, `suspend`, `cancelled`, `asm`, `asm_br`, `frame_address` or
//!   `return_address`, and at least one `return`;
//! - is not on the inlining stack (no cycle), and is at most [`Bound::depth`] calls deep;
//! - fits the growth left: the total of spliced instructions is at most [`Bound::growth`],
//!   first come first served in the output's layout order, so the choice is deterministic;
//! - has the caller's pointer width (another is refused by name: a malformed input), the
//!   caller's target features, a signature of at most one return, no `byval` parameter and no
//!   C variadic, and parameter and return types equal to the call's arguments and result;
//! - has no spans, or the call has a location (an inlined row needs its call site);
//! - is CLOSED: every reference an instruction of it names that stays in the output is the
//!   callee's own data without relocations. A reference to a function, static, thread-local or
//!   vtable is by PLACE in the callee instance's references (func.rs, `RefRec`), which the
//!   caller's references cannot name, and [`Inlined`] carries no table of new places; so a
//!   call in a callee is kept only by splicing it too, and a callee whose call is not spliced
//!   is not spliced either (its whole subtree is given back to the growth budget).
//!
//! # How
//!
//! Two phases, each a work list (no recursion): the plan walks the sites in the output's
//! layout order, depth first, and keeps a tree of spliced instances; the emission copies the
//! root's columns in layout order and, at a planned site, ends the block with a `jump` to a
//! copy of the callee's entry block (its parameters are the arguments), copies the callee's
//! blocks after it, turns each `return` into a `jump` to a continuation block whose one
//! parameter (if the call has a result) is the call's value, and goes on with the rest of the
//! caller's block there. The columns are written directly, as `coroutine::emit` does (it does
//! not go through `Builder` either): values and blocks are renumbered by fixups resolved at the
//! end, since a use may come before its value in layout order. A callee's tables come along:
//! slots, ranges, memory attributes, scopes, variables and spans appended; compound types,
//! signatures, parameter facts, intrinsics and data found by content first (deduplicated) and
//! appended only when new; a spliced body's spans name a new row of `inlined` whose parent is
//! the call's own inlined row, as `Builder::inlined(parent, call_site)` makes one. The output
//! is verified before it is returned.

use std::alloc::Allocator;
use std::convert::Infallible;
use std::fmt;

use crate::{
    builder::BuildError,
    body::{Bodies, BodyId},
    func::{
        Block, BlockKind, CompoundRec, DataId, DataRec, FnAttrs, Func, Inline, InlinedRec, IntrinsicId, IntrinsicRec,
        NONE, ParamKind, ParamRec, RefId, RefKind, RefRec, SCOPE_IN, SCOPE_OUTSIDE, SigId, SigRec, View,
    },
    op::{Field, Layout, Op, Word, walk},
    ty::Ty,
    verify::{VerifyError, verify},
};

/// How much the inliner may splice.
#[derive(Clone, Copy, Debug)]
pub struct Bound {
    /// A callee without `#[inline(always)]` has at most this many instructions.
    pub insts: u32,
    /// Sites are nested at most this deep.
    pub depth: u8,
    /// The spliced instructions of one function, in total, are at most this many.
    pub growth: u32,
}

/// The fast tier's bound: estimates until the measurement named in backend-next.md's section.
pub const TRIVIAL: Bound = Bound { insts: 16, depth: 4, growth: 256 };

/// A function with its trivial sites spliced.
pub struct Inlined<A: Allocator> {
    pub func: Func<A>,
    /// The spliced bodies, sorted, unique: the fast record's read list gains exactly these.
    pub read: Vec<BodyId, A>,
}

/// Why the inliner made no function.
#[derive(Debug)]
pub enum InlineError {
    /// A table past what its index can name.
    Build(BuildError),
    /// The output breaks a rule of the verifier: an inliner bug.
    Verify(VerifyError),
    /// Instruction `inst` of the body holding it is a form the inliner does not take.
    Refused { inst: usize, what: &'static str },
}

impl fmt::Display for InlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InlineError::Build(error) => write!(f, "the inlined function: {error}"),
            InlineError::Verify(error) => write!(f, "the inlined function is not LIR: {error}"),
            InlineError::Refused { inst, what } => {
                write!(f, "v{inst}: {what} (refused by the trivial inliner, blaze_lir::inline)")
            }
        }
    }
}

impl std::error::Error for InlineError {}

/// `view` (the body `root`) with its trivial sites spliced, the callees found through
/// `bodies`, within `bound`, in `alloc`.
pub fn inline_trivial<A: Allocator + Clone>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    alloc: A,
) -> Result<Inlined<A>, InlineError> {
    inline_as(view, root, bodies, bound, false, alloc)
}

/// [`inline_trivial`] with a planted bug (test builds only): the first spliced call's first two
/// arguments are swapped.
#[cfg(test)]
pub(crate) fn inline_swapping_first_args<A: Allocator + Clone>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    alloc: A,
) -> Result<Inlined<A>, InlineError> {
    inline_as(view, root, bodies, bound, true, alloc)
}

fn inline_as<A: Allocator + Clone>(
    view: View<'_>,
    root: BodyId,
    bodies: &dyn Bodies,
    bound: Bound,
    swap: bool,
    alloc: A,
) -> Result<Inlined<A>, InlineError> {
    let nodes = plan(view, root, bodies, bound, &alloc)?;
    let func = emit(&nodes, swap, &alloc)?;
    let mut read = Vec::with_capacity_in(nodes.len().saturating_sub(1), alloc.clone());
    read.extend(nodes.iter().skip(1).map(|node| node.id));
    read.sort_unstable();
    read.dedup();
    Ok(Inlined { func, read })
}

/// One body in the inlining tree: the root (node 0) or an instance spliced at `site` of
/// `parent`.
#[derive(Clone, Copy)]
struct Node<'v> {
    id: BodyId,
    view: View<'v>,
    parent: u32,
    site: usize,
    depth: u8,
}

fn too_large(table: &'static str) -> InlineError {
    InlineError::Build(BuildError::Full { table })
}

fn row(len: usize, table: &'static str) -> Result<u32, InlineError> {
    u32::try_from(len).map_err(|_| too_large(table))
}

fn filled<T: Clone, A: Allocator + Clone>(len: usize, item: T, alloc: &A) -> Vec<T, A> {
    let mut column = Vec::with_capacity_in(len, alloc.clone());
    column.resize(len, item);
    column
}

/// The planted instance of `parent` at `site`, if any.
fn child_of(nodes: &[Node<'_>], parent: u32, site: usize) -> Option<u32> {
    nodes
        .iter()
        .enumerate()
        .skip(parent as usize + 1)
        .find(|(_, node)| node.parent == parent && node.site == site)
        .map(|(at, _)| at as u32)
}

/// Hands `f` every reference instruction `at` names.
fn each_ref(view: View<'_>, at: usize, f: &mut impl FnMut(RefId)) -> Result<(), InlineError> {
    match view.op(at).info().layout {
        Layout::Data(Field::Ref) => f(RefId(view.data[at])),
        Layout::Extra(fields) => {
            let words = &view.extra[view.data[at] as usize..];
            walk(words, fields, Ty(view.tys[at]), view.tys, &mut |word| -> Result<(), Infallible> {
                if let Word::Of(Field::Ref, reference) = word {
                    f(RefId(reference));
                }
                Ok(())
            })
            .map_err(|_| InlineError::Refused { inst: at, what: "operands that run past their words" })?;
        }
        Layout::None | Layout::Data(_) => {}
    }
    Ok(())
}

/// Hands `f` every block the terminator `at` leads to.
fn each_successor(view: View<'_>, at: usize, f: &mut impl FnMut(u32)) -> Result<(), InlineError> {
    if let Layout::Extra(fields) = view.op(at).info().layout {
        let words = &view.extra[view.data[at] as usize..];
        walk(words, fields, Ty(view.tys[at]), view.tys, &mut |word| -> Result<(), Infallible> {
            if let Word::Block(block) = word {
                f(block);
            }
            Ok(())
        })
        .map_err(|_| InlineError::Refused { inst: at, what: "operands that run past their words" })?;
    }
    Ok(())
}

/// Whether a reference stays meaningful in another function: the body's own data, with no
/// relocations (a relocation names another reference, by the body's place) and no source
/// positions (which the splice does not carry).
fn carried(view: View<'_>, reference: RefId) -> bool {
    let rec = view.reference(reference);
    rec.kind == RefKind::Data as u32
        && view.datas.get(rec.place as usize).is_some_and(|data| data.relocs_len == 0 && data.positions_len == 0)
}

/// Whether every reference node `n` keeps in the output is carried (see the module's
/// "CLOSED").
fn closed(nodes: &[Node<'_>], n: u32) -> Result<bool, InlineError> {
    let view = nodes[n as usize].view;
    for at in 0..view.len() {
        let mut open = false;
        each_ref(view, at, &mut |reference| open |= !carried(view, reference))?;
        if open && !(view.op(at) == Op::Call && child_of(nodes, n, at).is_some()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `view`'s block graph has no cycle: Kahn's order over every block.
fn acyclic<A: Allocator + Clone>(view: View<'_>, alloc: &A) -> Result<bool, InlineError> {
    let blocks = view.blocks();
    let mut edges: Vec<(u32, u32), A> = Vec::new_in(alloc.clone());
    let mut starts = filled(blocks + 1, 0usize, alloc);
    for b in 0..blocks {
        starts[b] = edges.len();
        let from = b as u32;
        each_successor(view, view.terminator(Block(from)), &mut |to| edges.push((from, to)))?;
    }
    starts[blocks] = edges.len();
    let mut incoming = filled(blocks, 0u32, alloc);
    for &(_, to) in edges.iter() {
        match incoming.get_mut(to as usize) {
            Some(count) => *count += 1,
            None => {
                return Err(InlineError::Refused {
                    inst: view.terminator(Block(0)),
                    what: "a callee edge to a block past its blocks",
                });
            }
        }
    }
    let mut ready: Vec<u32, A> = Vec::new_in(alloc.clone());
    ready.extend((0..blocks as u32).filter(|&b| incoming[b as usize] == 0));
    let mut done = 0usize;
    while let Some(b) = ready.pop() {
        done += 1;
        for &(_, to) in &edges[starts[b as usize]..starts[b as usize + 1]] {
            incoming[to as usize] -= 1;
            if incoming[to as usize] == 0 {
                ready.push(to);
            }
        }
    }
    Ok(done == blocks)
}

/// The callee of the call `at` of node `n`, if the site is inlined (the module's "Which
/// sites", all but closedness, which the plan decides when the callee's walk ends).
fn candidate<'v, A: Allocator + Clone>(
    nodes: &[Node<'v>],
    n: u32,
    at: usize,
    bodies: &'v dyn Bodies,
    bound: Bound,
    used: u64,
    alloc: &A,
) -> Result<Option<(BodyId, View<'v>)>, InlineError> {
    let caller = nodes[n as usize];
    let view = caller.view;
    let words = &view.extra[view.data[at] as usize..];
    let (reference, count) = (RefId(words[0]), words[2] as usize);
    let args = &words[3..3 + count];
    let Some((id, callee)) = bodies.callee(caller.id, reference) else {
        return Ok(None);
    };
    let root = nodes[0].view;
    if callee.head.ptr_bits != root.head.ptr_bits {
        return Err(InlineError::Refused { inst: at, what: "a callee of another pointer width than its caller" });
    }
    if caller.depth >= bound.depth {
        return Ok(None);
    }
    // Never a body already on the inlining stack.
    let mut up = n;
    loop {
        if nodes[up as usize].id == id {
            return Ok(None);
        }
        if up == 0 {
            break;
        }
        up = nodes[up as usize].parent;
    }
    let inline = FnAttrs(callee.head.attrs).inline();
    if inline == Inline::Never {
        return Ok(None);
    }
    if inline != Inline::Always && (callee.len() > bound.insts as usize || !acyclic(callee, alloc)?) {
        return Ok(None);
    }
    if used + callee.len() as u64 > u64::from(bound.growth) {
        return Ok(None);
    }
    if callee.features != root.features {
        return Ok(None);
    }
    let mut returns = false;
    for inst in 0..callee.len() {
        match callee.op(inst) {
            Op::TailCall
            | Op::TailCallIndirect
            | Op::Suspend
            | Op::Cancelled
            | Op::Asm
            | Op::AsmBr
            | Op::FrameAddress
            | Op::ReturnAddress => return Ok(None),
            Op::Return => returns = true,
            _ => {}
        }
    }
    if !returns {
        return Ok(None);
    }
    let sig = callee.sig(SigId(callee.head.sig));
    if sig.bits & SigRec::VARIADIC != 0 || sig.params.iter().any(|param| param.kind == ParamKind::Byval as u8) {
        return Ok(None);
    }
    let want = match sig.rets {
        [] => Ty::NONE,
        [one] => Ty(one.ty),
        _ => return Ok(None),
    };
    let has = Ty(view.tys[at]);
    if want != has || has.compound_row().is_some() {
        return Ok(None);
    }
    let params = callee.params(Block(0));
    if params.len() != count {
        return Ok(None);
    }
    for (k, param) in params.enumerate() {
        let (want, has) = (Ty(callee.tys[param]), Ty(view.tys[args[k] as usize]));
        if want != has || want.compound_row().is_some() {
            return Ok(None);
        }
    }
    if !callee.spans.is_empty() && view.locs[at] == 0 {
        return Ok(None);
    }
    Ok(Some((id, callee)))
}

/// The inlining tree, in the output's layout order (preorder): node 0 is the root.
fn plan<'v, A: Allocator + Clone>(
    view: View<'v>,
    root: BodyId,
    bodies: &'v dyn Bodies,
    bound: Bound,
    alloc: &A,
) -> Result<Vec<Node<'v>, A>, InlineError> {
    let mut nodes = Vec::new_in(alloc.clone());
    nodes.push(Node { id: root, view, parent: NONE, site: 0, depth: 0 });
    // Per node being walked: the node and its next instruction.
    let mut stack: Vec<(u32, usize), A> = Vec::new_in(alloc.clone());
    stack.push((0, 0));
    let mut used = 0u64;
    while let Some(&(n, at)) = stack.last() {
        let body = nodes[n as usize].view;
        if at == body.len() {
            stack.pop();
            // Every node after `n` was made while `n` was walked: its subtree. A callee that
            // keeps a reference the caller cannot name is not spliced, nor anything under it.
            if n != 0 && !closed(&nodes, n)? {
                for node in &nodes[n as usize..] {
                    used -= node.view.len() as u64;
                }
                nodes.truncate(n as usize);
            }
            continue;
        }
        if let Some(top) = stack.last_mut() {
            top.1 = at + 1;
        }
        if body.op(at) != Op::Call {
            continue;
        }
        if let Some((id, callee)) = candidate(&nodes, n, at, bodies, bound, used, alloc)? {
            let child = row(nodes.len(), "inlined bodies")?;
            used += callee.len() as u64;
            let depth = nodes[n as usize].depth + 1;
            nodes.push(Node { id, view: callee, parent: n, site: at, depth });
            stack.push((child, 0));
        }
    }
    Ok(nodes)
}

/// What a fixup names: a value or a block of its frame's body, or its frame's continuation.
#[derive(Clone, Copy)]
enum Fix {
    Value,
    Block,
    Cont,
}

/// A word of the output to set once every block is copied: `data[place]` if `in_data`, else
/// `extra[place]`, holding the body's own number until then.
#[derive(Clone, Copy)]
struct Fixup {
    place: u32,
    in_data: bool,
    frame: u32,
    fix: Fix,
}

/// One node as the output has it. Frame `k` is node `k`.
struct Frame<A: Allocator> {
    /// Whether this is the root, whose tables are the output's.
    root: bool,
    /// Per value of the body: its value here.
    values: Vec<u32, A>,
    /// Per block of the body: its block here.
    blocks: Vec<u32, A>,
    /// The block the body's returns jump to.
    cont: u32,
    slot_off: u32,
    span_off: u32,
    range_off: u32,
    mem_off: u32,
    /// Per row of the body's tables: its row here, [`NONE`] until first named.
    refs: Vec<u32, A>,
    sigs: Vec<u32, A>,
    intrinsics: Vec<u32, A>,
}

/// Where the emission is in one frame: its body's block and next instruction.
#[derive(Clone, Copy)]
struct Cursor {
    frame: u32,
    block: u32,
    at: usize,
}

struct Emit<'p, 'v, A: Allocator + Clone> {
    nodes: &'p [Node<'v>],
    out: Func<A>,
    frames: Vec<Frame<A>, A>,
    fixups: Vec<Fixup, A>,
    /// Whether the output's spans have scopes: as the root's do (all or none).
    scoped: bool,
    /// The plant (test builds only): swap the first spliced call's first two arguments.
    swap: bool,
    alloc: A,
}

fn remap_range(range: u32, off: u32) -> u32 {
    match range {
        0 => 0,
        r => r + off,
    }
}

impl<A: Allocator + Clone> Emit<'_, '_, A> {
    fn begin(&mut self, kind: BlockKind) -> Result<u32, InlineError> {
        let number = row(self.out.block_starts.len(), "blocks")?;
        let start = row(self.out.ops.len(), "instructions")?;
        self.out.block_starts.push(start);
        self.out.block_kinds.push(kind as u8);
        Ok(number)
    }

    fn raw(&mut self, op: Op, flags: u8, ty: Ty, data: u32, attrs: u32, loc: u32) -> Result<u32, InlineError> {
        let at = row(self.out.ops.len(), "instructions")?;
        self.out.ops.push(op as u8);
        self.out.flags.push(flags);
        self.out.tys.push(ty.0);
        self.out.data.push(data);
        self.out.attrs.push(attrs);
        self.out.locs.push(loc);
        Ok(at)
    }

    fn fix_extra(&mut self, frame: u32, fix: Fix, word: u32) -> Result<(), InlineError> {
        let place = row(self.out.extra.len(), "extra")?;
        self.fixups.push(Fixup { place, in_data: false, frame, fix });
        self.out.extra.push(word);
        Ok(())
    }

    fn loc(&self, frame: u32, loc: u32) -> u32 {
        let frame = &self.frames[frame as usize];
        match loc {
            0 => 0,
            _ if frame.root => loc,
            _ => loc + frame.span_off,
        }
    }

    fn attrs(&self, frame: u32, attrs: u32) -> u32 {
        let frame = &self.frames[frame as usize];
        match attrs {
            0 => 0,
            _ if frame.root => attrs,
            _ => attrs + frame.mem_off,
        }
    }

    fn slot(&self, frame: u32, slot: u32) -> u32 {
        slot + self.frames[frame as usize].slot_off
    }

    /// The type `ty` of frame `frame`'s body, as the output names it: a compound by its
    /// members, found or appended.
    fn ty(&mut self, frame: u32, ty: Ty) -> Result<Ty, InlineError> {
        let Some(compound) = ty.compound_row() else {
            return Ok(ty);
        };
        if self.frames[frame as usize].root {
            return Ok(ty);
        }
        let view = self.nodes[frame as usize].view;
        let rec = view.compounds[compound as usize];
        let members = &view.compound_tys[rec.first as usize..][..rec.len as usize];
        let out = &mut self.out;
        let found = out
            .compounds
            .iter()
            .position(|it| &out.compound_tys[it.first as usize..][..it.len as usize] == members);
        let at = match found {
            Some(at) => at,
            None => {
                let first = row(out.compound_tys.len(), "compound_tys")?;
                out.compounds.push(CompoundRec { first, len: rec.len });
                out.compound_tys.extend_from_slice(members);
                out.compounds.len() - 1
            }
        };
        Ty::compound(row(at, "compounds")?).ok_or(too_large("compounds"))
    }

    /// A row of `param_facts` of frame `frame`'s body (0, or 1 + a row), found or appended.
    fn facts(&mut self, frame: u32, facts: u32) -> Result<u32, InlineError> {
        if facts == 0 {
            return Ok(0);
        }
        let view = self.nodes[frame as usize].view;
        let mut rec = view.param_facts[facts as usize - 1];
        rec.range = remap_range(rec.range, self.frames[frame as usize].range_off);
        let at = match self.out.param_facts.iter().position(|it| *it == rec) {
            Some(at) => at,
            None => {
                self.out.param_facts.push(rec);
                self.out.param_facts.len() - 1
            }
        };
        Ok(row(at, "param_facts")? + 1)
    }

    /// A signature of frame `frame`'s body, as the output names it: found or appended.
    fn sig(&mut self, frame: u32, sig: u32) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(sig);
        }
        let cached = self.frames[frame as usize].sigs[sig as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let view = self.nodes[frame as usize].view;
        let rec = view.sigs[sig as usize];
        let len = rec.params as usize + rec.rets as usize;
        let mut params: Vec<ParamRec, A> = Vec::with_capacity_in(len, self.alloc.clone());
        for param in &view.sig_params[rec.first as usize..][..len] {
            let mut param = *param;
            param.ty = self.ty(frame, Ty(param.ty))?.0;
            param.facts = self.facts(frame, param.facts)?;
            params.push(param);
        }
        let out = &mut self.out;
        let found = out.sigs.iter().position(|it| {
            it.conv == rec.conv
                && it.bits == rec.bits
                && it.params == rec.params
                && it.rets == rec.rets
                && out.sig_params[it.first as usize..][..len] == params[..]
        });
        let at = match found {
            Some(at) => at,
            None => {
                let first = row(out.sig_params.len(), "sig_params")?;
                out.sigs.push(SigRec { first, ..rec });
                out.sig_params.extend_from_slice(&params);
                out.sigs.len() - 1
            }
        };
        let at = row(at, "sigs")?;
        self.frames[frame as usize].sigs[sig as usize] = at;
        Ok(at)
    }

    /// An intrinsic of frame `frame`'s body, as the output names it: found or appended.
    fn intrinsic(&mut self, frame: u32, intrinsic: u32) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(intrinsic);
        }
        let cached = self.frames[frame as usize].intrinsics[intrinsic as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let view = self.nodes[frame as usize].view;
        let rec = view.intrinsics[intrinsic as usize];
        let name = view.intrinsic_name(IntrinsicId(intrinsic));
        let sig = self.sig(frame, rec.sig)?;
        let out = &mut self.out;
        let found = out.intrinsics.iter().position(|it| {
            it.sig == sig && &out.names[it.name_first as usize..][..it.name_len as usize] == name
        });
        let at = match found {
            Some(at) => at,
            None => {
                let name_first = row(out.names.len(), "names")?;
                out.intrinsics.push(IntrinsicRec { name_first, name_len: rec.name_len, sig });
                out.names.extend_from_slice(name);
                out.intrinsics.len() - 1
            }
        };
        let at = row(at, "intrinsics")?;
        self.frames[frame as usize].intrinsics[intrinsic as usize] = at;
        Ok(at)
    }

    /// A reference of frame `frame`'s body named at its instruction `inst`, as the output
    /// names it. A spliced body's is its own data (the plan keeps only closed bodies), found by
    /// content or appended.
    fn reference(&mut self, frame: u32, reference: u32, inst: usize) -> Result<u32, InlineError> {
        if self.frames[frame as usize].root {
            return Ok(reference);
        }
        let cached = self.frames[frame as usize].refs[reference as usize];
        if cached != NONE {
            return Ok(cached);
        }
        let view = self.nodes[frame as usize].view;
        if !carried(view, RefId(reference)) {
            return Err(InlineError::Refused {
                inst,
                what: "a spliced body's reference to a function, static, thread-local, vtable or relocated data: its place is the callee's, which the caller's references cannot name",
            });
        }
        let place = view.reference(RefId(reference)).place;
        let data = view.datas[place as usize];
        let bytes = view.data_bytes_of(DataId(place));
        let out = &mut self.out;
        let found = out.refs.iter().position(|it| {
            it.kind == RefKind::Data as u32
                && out.datas.get(it.place as usize).is_some_and(|have| {
                    have.relocs_len == 0
                        && have.positions_len == 0
                        && have.align == data.align
                        && &out.data_bytes[have.bytes_first as usize..][..have.bytes_len as usize] == bytes
                })
        });
        let at = match found {
            Some(at) => at,
            None => {
                let place = row(out.datas.len(), "datas")?;
                let bytes_first = row(out.data_bytes.len(), "data_bytes")?;
                let relocs_first = row(out.data_relocs.len(), "data_relocs")?;
                let positions_first = row(out.data_positions.len(), "data_positions")?;
                out.datas.push(DataRec {
                    bytes_first,
                    bytes_len: data.bytes_len,
                    relocs_first,
                    relocs_len: 0,
                    align: data.align,
                    positions_first,
                    positions_len: 0,
                });
                out.data_bytes.extend_from_slice(bytes);
                out.refs.push(RefRec { place, kind: RefKind::Data as u32 });
                out.refs.len() - 1
            }
        };
        let at = row(at, "refs")?;
        self.frames[frame as usize].refs[reference as usize] = at;
        Ok(at)
    }

    /// Appends the target at `words[at..]` of frame `frame`; where its words end.
    fn target(&mut self, frame: u32, words: &[u32], mut at: usize) -> Result<usize, InlineError> {
        self.fix_extra(frame, Fix::Block, words[at])?;
        let n = words[at + 1] as usize;
        self.out.extra.push(n as u32);
        at += 2;
        for _ in 0..n {
            self.fix_extra(frame, Fix::Value, words[at])?;
            at += 1;
        }
        Ok(at)
    }

    /// Appends the operands `fields` at `words` of frame `frame`'s instruction `inst` (of
    /// result type `ty`), each as the output names it. The mirror of [`walk`].
    fn rewrite(&mut self, frame: u32, inst: usize, words: &[u32], fields: &[Field], ty: Ty) -> Result<(), InlineError> {
        let tys = self.nodes[frame as usize].view.tys;
        let mut at = 0;
        let mut scrutinee = None;
        for &field in fields {
            match field {
                Field::Value => {
                    scrutinee = Some(words[at]);
                    self.fix_extra(frame, Fix::Value, words[at])?;
                    at += 1;
                }
                Field::Block => {
                    self.fix_extra(frame, Fix::Block, words[at])?;
                    at += 1;
                }
                Field::Slot => {
                    let slot = self.slot(frame, words[at]);
                    self.out.extra.push(slot);
                    at += 1;
                }
                Field::Ref => {
                    let reference = self.reference(frame, words[at], inst)?;
                    self.out.extra.push(reference);
                    at += 1;
                }
                Field::Sig => {
                    let sig = self.sig(frame, words[at])?;
                    self.out.extra.push(sig);
                    at += 1;
                }
                Field::Intrinsic => {
                    let intrinsic = self.intrinsic(frame, words[at])?;
                    self.out.extra.push(intrinsic);
                    at += 1;
                }
                Field::Asm => {
                    if !self.frames[frame as usize].root {
                        return Err(InlineError::Refused { inst, what: "an `asm!` block in a spliced body" });
                    }
                    self.out.extra.push(words[at]);
                    at += 1;
                }
                Field::Imm | Field::Offset | Field::IntCond | Field::FloatCond | Field::Rmw => {
                    self.out.extra.push(words[at]);
                    at += 1;
                }
                Field::Target => at = self.target(frame, words, at)?,
                Field::Values => {
                    let n = words[at] as usize;
                    self.out.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.fix_extra(frame, Fix::Value, words[at])?;
                        at += 1;
                    }
                }
                Field::Blocks => {
                    let n = words[at] as usize;
                    self.out.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.fix_extra(frame, Fix::Block, words[at])?;
                        at += 1;
                    }
                }
                Field::Mask => {
                    let n = words[at] as usize;
                    self.out.extra.extend_from_slice(&words[at..at + 1 + n]);
                    at += 1 + n;
                }
                Field::Const => {
                    let n = ty.const_words() as usize;
                    self.out.extra.extend_from_slice(&words[at..at + n]);
                    at += n;
                }
                Field::Cases => {
                    let width = scrutinee.map_or(0, |it: u32| Ty(tys[it as usize]).const_words()) as usize;
                    let n = words[at] as usize;
                    self.out.extra.push(n as u32);
                    at += 1;
                    for _ in 0..n {
                        self.out.extra.extend_from_slice(&words[at..at + width]);
                        at += width;
                        at = self.target(frame, words, at)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Copies frame `frame`'s instruction `at`, its operands as the output names them.
    fn copy(&mut self, frame: u32, at: usize) -> Result<(), InlineError> {
        let view = self.nodes[frame as usize].view;
        let op = view.op(at);
        let original = Ty(view.tys[at]);
        let ty = self.ty(frame, original)?;
        let new = row(self.out.ops.len(), "instructions")?;
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(Field::Value) => {
                self.fixups.push(Fixup { place: new, in_data: true, frame, fix: Fix::Value });
                view.data[at]
            }
            Layout::Data(Field::Slot) => self.slot(frame, view.data[at]),
            Layout::Data(Field::Ref) => self.reference(frame, view.data[at], at)?,
            Layout::Data(_) => view.data[at],
            Layout::Extra(fields) => {
                let start = row(self.out.extra.len(), "extra")?;
                let words: &[u32] = &view.extra[view.data[at] as usize..];
                self.rewrite(frame, at, words, fields, original)?;
                start
            }
        };
        let (attrs, loc) = (self.attrs(frame, view.attrs[at]), self.loc(frame, view.locs[at]));
        let made = self.raw(op, view.flags[at], ty, data, attrs, loc)?;
        self.frames[frame as usize].values[at] = made;
        Ok(())
    }

    /// Begins the output's copy of block `block` of frame `frame`'s body with its parameters;
    /// where its other instructions start.
    fn body_block(&mut self, frame: u32, block: u32) -> Result<usize, InlineError> {
        let view = self.nodes[frame as usize].view;
        let number = self.begin(view.block_kind(Block(block)))?;
        self.frames[frame as usize].blocks[block as usize] = number;
        let params = view.params(Block(block));
        for at in params.clone() {
            let ty = self.ty(frame, Ty(view.tys[at]))?;
            let (attrs, loc) = (self.attrs(frame, view.attrs[at]), self.loc(frame, view.locs[at]));
            let made = self.raw(Op::Param, view.flags[at], ty, 0, attrs, loc)?;
            self.frames[frame as usize].values[at] = made;
        }
        Ok(params.end)
    }

    /// Frame `child`, its body's tables appended to the output's.
    fn make_frame(&mut self, child: u32) -> Result<(), InlineError> {
        let node = self.nodes[child as usize];
        let view = node.view;
        let parent = self.nodes[node.parent as usize].view;
        let call_loc = self.loc(node.parent, parent.locs[node.site]);
        let out = &mut self.out;
        let slot_off = row(out.slots.len(), "slots")?;
        out.slots.extend_from_slice(view.slots);
        let range_off = row(out.ranges.len(), "ranges")?;
        out.ranges.extend_from_slice(view.ranges);
        let mem_off = row(out.mem_attrs.len(), "mem_attrs")?;
        for rec in view.mem_attrs {
            let mut rec = *rec;
            rec.range = remap_range(rec.range, range_off);
            out.mem_attrs.push(rec);
        }
        let scope_off = row(out.scope_parents.len(), "scope_parents")?;
        for &up in view.scope_parents {
            out.scope_parents.push(if up == NONE { NONE } else { up + scope_off });
        }
        for var in view.variables {
            let mut var = *var;
            var.slot += slot_off;
            if var.scope != NONE {
                var.scope += scope_off;
            }
            out.variables.push(var);
        }
        let mut span_off = 0;
        if !view.spans.is_empty() {
            // The plan took this site only with a location (`candidate`).
            let call_row = (call_loc as usize).checked_sub(1).ok_or(InlineError::Refused {
                inst: node.site,
                what: "a call site without a location, of a callee with spans",
            })?;
            if out.span_inlined.len() < out.spans.len() {
                out.span_inlined.resize(out.spans.len(), NONE);
            }
            let (call_site, up) = (out.spans[call_row], out.span_inlined[call_row]);
            let instance = row(out.inlined.len(), "inlined")?;
            out.inlined.push(InlinedRec { parent: up, call_site });
            let inlined_off = row(out.inlined.len(), "inlined")?;
            for rec in view.inlined {
                let parent = if rec.parent == NONE { instance } else { rec.parent + inlined_off };
                out.inlined.push(InlinedRec { parent, call_site: rec.call_site });
            }
            span_off = row(out.spans.len(), "spans")?;
            for (r, &span) in view.spans.iter().enumerate() {
                out.spans.push(span);
                let inlined = match view.span_inlined.get(r) {
                    Some(&it) if it != NONE => it + inlined_off,
                    _ => instance,
                };
                out.span_inlined.push(inlined);
                if self.scoped {
                    let scope = match view.span_scopes.get(r) {
                        Some(&it) if it >= SCOPE_IN => it + scope_off,
                        Some(&it) => it,
                        None => SCOPE_OUTSIDE,
                    };
                    out.span_scopes.push(scope);
                }
            }
        }
        if self.frames.len() != child as usize {
            return Err(InlineError::Refused { inst: node.site, what: "a site the emission reached out of the plan's order" });
        }
        let alloc = &self.alloc;
        self.frames.push(Frame {
            root: false,
            values: filled(view.len(), NONE, alloc),
            blocks: filled(view.blocks(), NONE, alloc),
            cont: NONE,
            slot_off,
            span_off,
            range_off,
            mem_off,
            refs: filled(view.refs.len(), NONE, alloc),
            sigs: filled(view.sigs.len(), NONE, alloc),
            intrinsics: filled(view.intrinsics.len(), NONE, alloc),
        });
        Ok(())
    }

    /// The site `at` of frame `frame`, spliced as node `child`: a jump to the copy of the
    /// callee's entry, which is begun; where its other instructions start.
    fn enter(&mut self, frame: u32, at: usize, child: u32) -> Result<usize, InlineError> {
        self.make_frame(child)?;
        let view = self.nodes[frame as usize].view;
        let words = &view.extra[view.data[at] as usize..];
        let count = words[2] as usize;
        let args = &words[3..3 + count];
        let start = row(self.out.extra.len(), "extra")?;
        self.fix_extra(child, Fix::Block, 0)?;
        self.out.extra.push(count as u32);
        // Planted (test builds only): the first spliced call's first two arguments swapped.
        let swap = cfg!(test) && self.swap && child == 1 && count >= 2;
        for k in 0..count {
            let arg = match swap && k < 2 {
                true => args[1 - k],
                false => args[k],
            };
            self.fix_extra(frame, Fix::Value, arg)?;
        }
        let loc = self.loc(frame, view.locs[at]);
        self.raw(Op::Jump, 0, Ty::NONE, start, 0, loc)?;
        self.body_block(child, 0)
    }

    /// Frame `frame`'s `return` at `at`: a jump to its continuation.
    fn ret(&mut self, frame: u32, at: usize) -> Result<(), InlineError> {
        let view = self.nodes[frame as usize].view;
        let words = &view.extra[view.data[at] as usize..];
        let count = words[0] as usize;
        let start = row(self.out.extra.len(), "extra")?;
        self.fix_extra(frame, Fix::Cont, 0)?;
        self.out.extra.push(count as u32);
        for &value in &words[1..1 + count] {
            self.fix_extra(frame, Fix::Value, value)?;
        }
        let loc = self.loc(frame, view.locs[at]);
        self.raw(Op::Jump, 0, Ty::NONE, start, 0, loc)?;
        Ok(())
    }

    /// The continuation of node `child`, in its parent at `cursor`: a block whose parameter is
    /// the call's value, where the rest of the caller's block goes.
    fn after(&mut self, child: u32, cursor: Cursor) -> Result<(), InlineError> {
        let node = self.nodes[child as usize];
        let parent = cursor.frame;
        let view = self.nodes[parent as usize].view;
        let kind = match view.block_kind(Block(cursor.block)) {
            BlockKind::Normal => BlockKind::Normal,
            // The rest of a cold or landing block: reached by a jump, so not a landing block.
            BlockKind::Cold | BlockKind::Cleanup | BlockKind::Catch | BlockKind::Terminate => BlockKind::Cold,
        };
        let number = self.begin(kind)?;
        self.frames[child as usize].cont = number;
        let ty = self.ty(parent, Ty(view.tys[node.site]))?;
        if !ty.is_none() {
            let loc = self.loc(parent, view.locs[node.site]);
            let made = self.raw(Op::Param, 0, ty, 0, 0, loc)?;
            self.frames[parent as usize].values[node.site] = made;
        }
        Ok(())
    }
}

/// The output: the plan's tree, spliced.
fn emit<A: Allocator + Clone>(nodes: &[Node<'_>], swap: bool, alloc: &A) -> Result<Func<A>, InlineError> {
    let view = nodes[0].view;
    // The root's tables; its instructions and blocks are made below.
    let mut out = view.to_func_in(alloc.clone());
    out.ops.clear();
    out.flags.clear();
    out.tys.clear();
    out.data.clear();
    out.extra.clear();
    out.attrs.clear();
    out.locs.clear();
    out.block_starts.clear();
    out.block_kinds.clear();
    let mut e = Emit {
        nodes,
        out,
        frames: Vec::new_in(alloc.clone()),
        fixups: Vec::new_in(alloc.clone()),
        scoped: !view.span_scopes.is_empty(),
        swap,
        alloc: alloc.clone(),
    };
    e.frames.push(Frame {
        root: true,
        values: filled(view.len(), NONE, alloc),
        blocks: filled(view.blocks(), NONE, alloc),
        cont: NONE,
        slot_off: 0,
        span_off: 0,
        range_off: 0,
        mem_off: 0,
        refs: Vec::new_in(alloc.clone()),
        sigs: Vec::new_in(alloc.clone()),
        intrinsics: Vec::new_in(alloc.clone()),
    });
    let mut stack: Vec<Cursor, A> = Vec::new_in(alloc.clone());
    let at = e.body_block(0, 0)?;
    stack.push(Cursor { frame: 0, block: 0, at });
    while let Some(&cursor) = stack.last() {
        let body = nodes[cursor.frame as usize].view;
        if cursor.at == body.block(Block(cursor.block)).end {
            let next = cursor.block + 1;
            if (next as usize) < body.blocks() {
                let at = e.body_block(cursor.frame, next)?;
                stack.pop();
                stack.push(Cursor { frame: cursor.frame, block: next, at });
                continue;
            }
            stack.pop();
            if let Some(&parent) = stack.last() {
                e.after(cursor.frame, parent)?;
            }
            continue;
        }
        if let Some(top) = stack.last_mut() {
            top.at += 1;
        }
        let at = cursor.at;
        let op = body.op(at);
        if op == Op::Call
            && let Some(child) = child_of(nodes, cursor.frame, at)
        {
            let at = e.enter(cursor.frame, at, child)?;
            stack.push(Cursor { frame: child, block: 0, at });
            continue;
        }
        match op == Op::Return && cursor.frame != 0 {
            true => e.ret(cursor.frame, at)?,
            false => e.copy(cursor.frame, at)?,
        }
    }
    // Every value and block named in a copied operand becomes its number here.
    let Emit { mut out, frames, fixups, .. } = e;
    for fixup in fixups.iter() {
        let word = match fixup.in_data {
            true => &mut out.data[fixup.place as usize],
            false => &mut out.extra[fixup.place as usize],
        };
        let frame = &frames[fixup.frame as usize];
        let to = match fixup.fix {
            Fix::Value => frame.values.get(*word as usize).copied(),
            Fix::Block => frame.blocks.get(*word as usize).copied(),
            Fix::Cont => Some(frame.cont),
        };
        match to {
            Some(to) if to != NONE => *word = to,
            _ => {
                return Err(InlineError::Refused {
                    inst: fixup.place as usize,
                    what: "an operand naming a value or block of a body that the output does not make",
                });
            }
        }
    }
    row(out.extra.len(), "extra")?;
    verify(out.view()).map_err(InlineError::Verify)?;
    Ok(out)
}
