//! Field heat (docs/v2/plan/design/field-order.md): how much the code a profile shows
//! running reads and writes each field of each struct and enum variant, from the bodies' built
//! MIR, so [`Heat::ranked`] can name each type's hot fields for a [`FieldOrderPlan`].
//!
//! A body's weight is what the profile counts for it (the sum of its record's counters), spread
//! over its blocks by a static estimate of how often each runs: [`LOOP_FACTOR`] to the power of
//! the loops around it. A field access in a block adds the block's share; building a value
//! (`Aggregate`) writes every field at once and is no access.
//!
//! [`FieldOrderPlan`]: super::FieldOrderPlan

use std::{alloc::Global, cmp::Reverse};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_mem::Frame;
use hir_def::{AdtId, HasModule, LocalFieldId, VariantId};
use rustc_type_ir::inherent::IntoKind as _;

use crate::{
    InferBodyId,
    db::HirDatabase,
    layout::{enum_variant_ids, field_order::AdtPlan},
    mir::{
        FieldIndex, MirBody, MirOwner, MirStage, Operand, OperandKind, PlaceTy, ProjectionElem,
        Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::{
        DbInterner, ParamEnv, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    providers::mir_at,
};

/// How many times more often a block inside one more loop is taken to run.
pub const LOOP_FACTOR: u64 = 32;

/// The deepest loop nest told apart: a block in more loops counts as in this many.
pub const MAX_DEPTH: u32 = 4;

/// A body with more blocks than this is not counted ([`Heat::too_big`]): the loop search over
/// it is bounded.
pub const BODY_FUEL: usize = 4096;

/// A field is hot when its heat is at least its variant's hottest field's over this.
pub const HOT_RATIO: u128 = 32;

/// The fixed point a body's weight is spread with.
const SCALE: u128 = 1 << 20;

/// The heat of every field seen so far.
#[derive(Debug)]
pub struct Heat {
    /// By the ADT, the variant's index in declaration order (0 for a struct), and the field.
    by_field: HashMap<(AdtId, u32, LocalFieldId), u128, Global>,
    /// Bodies (or closures in them) whose MIR did not build: their accesses are not counted.
    pub unlowered: u32,
    /// Bodies over [`BODY_FUEL`] blocks, not counted.
    pub too_big: u32,
}

impl Default for Heat {
    fn default() -> Heat {
        Heat { by_field: HashMap::new_in(Global), unlowered: 0, too_big: 0 }
    }
}

impl Heat {
    pub fn new() -> Heat {
        Heat::default()
    }

    /// Counts the field accesses of `body` and of the closures written in it, `weight` being
    /// what the profile counts for the body. A closure is weighted as the body it is in: the
    /// profile's name for it folds onto that body's (`blaze_lints::profile::names`).
    pub fn add_body(&mut self, db: &Db, body: InferBodyId<'_>, weight: u128) {
        if weight == 0 {
            return;
        }
        let _span = tracing::info_span!(target: "blaze", "field_heat", ?body).entered();
        let interner = DbInterner::new_with(db, body.krate(db));
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let env = db.trait_environment(body.generic_def(db));
        blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let mut owners = Vec::new_in(frame);
            owners.push(MirOwner::Body(body));
            // Each closure is in exactly one body's `closures`, so this ends.
            while let Some(owner) = owners.pop() {
                let Ok(mir) = mir_at(db, owner, MirStage::Built) else {
                    self.unlowered += 1;
                    continue;
                };
                owners.extend(mir.closures.iter().map(|&it| MirOwner::Closure(it)));
                if mir.basic_blocks.len() > BODY_FUEL {
                    self.too_big += 1;
                    continue;
                }
                let mut visit =
                    Visit { db, mir, infcx: &infcx, env, share: 0, by_field: &mut self.by_field };
                visit.body(weight, frame);
            }
        });
    }

    /// Each ADT seen with its hot fields, in a plan's form: per variant, the fields whose heat
    /// is at least the variant's hottest's over [`HOT_RATIO`], hottest first (declaration order
    /// between equals). Ordered by ADT; a variant with no field seen ranks none.
    pub fn ranked(&self, db: &Db) -> Vec<(AdtId, AdtPlan)> {
        // alloc: once per plan
        let mut seen: Vec<_> = self.by_field.iter().map(|(&key, &heat)| (key, heat)).collect();
        seen.sort_unstable_by_key(|&((adt, variant, field), heat)| {
            (adt, variant, Reverse(heat), u32::from(field.into_raw()))
        });
        // alloc: the plan's
        let mut out = Vec::new();
        let mut rest = &seen[..];
        while let Some(&((adt, _, _), _)) = rest.first() {
            let end = rest.iter().position(|&((it, _, _), _)| it != adt).unwrap_or(rest.len());
            let (of_adt, after) = rest.split_at(end);
            rest = after;
            if let Some(plan) = adt_plan(db, adt, of_adt) {
                out.push((adt, plan));
            }
        }
        out
    }
}

/// `adt`'s plan from its fields' heat, `seen` ordered by variant then heat, hottest first.
fn adt_plan(db: &Db, adt: AdtId, seen: &[((AdtId, u32, LocalFieldId), u128)]) -> Option<AdtPlan> {
    let variants: Vec<VariantId> = match adt {
        AdtId::StructId(it) => vec![it.into()], // alloc: once per ADT of a plan
        AdtId::UnionId(_) => return None,
        // alloc: once per ADT of a plan
        AdtId::EnumId(it) => enum_variant_ids(db, it).iter().map(|&v| v.into()).collect(),
    };
    let mut ranked = Vec::with_capacity(variants.len()); // alloc: the plan's
    for (index, &variant) in variants.iter().enumerate() {
        let fields = variant.fields(db).fields();
        let mut of_variant =
            seen.iter().filter(|&&((_, v, _), heat)| v as usize == index && heat > 0).peekable();
        let hottest = of_variant.peek().map_or(0, |&&(_, heat)| heat);
        let names: Box<[_]> = of_variant
            .take_while(|&&(_, heat)| heat.saturating_mul(HOT_RATIO) >= hottest)
            .map(|&((_, _, field), _)| fields[field].name)
            .collect(); // alloc: the plan's
        ranked.push(names);
    }
    if ranked.iter().all(|it| it.is_empty()) {
        return None;
    }
    Some(AdtPlan { variants: ranked.into_boxed_slice() })
}

/// One body's walk: adds `share` to each field its places project.
struct Visit<'a, 'db> {
    db: &'db Db,
    mir: &'a MirBody<'db>,
    infcx: &'a InferCtxt<'db>,
    env: ParamEnv<'db>,
    /// The share of the body's weight of the block being walked.
    share: u128,
    by_field: &'a mut HashMap<(AdtId, u32, LocalFieldId), u128, Global>,
}

impl Visit<'_, '_> {
    fn body(&mut self, weight: u128, frame: &Frame) {
        let mir = self.mir;
        let freq = block_frequencies(mir, frame);
        let total: u128 = freq.iter().map(|&it| u128::from(it)).sum();
        if total == 0 {
            return;
        }
        for (id, block) in mir.basic_blocks.iter() {
            let f = freq[id.into_raw().into_u32() as usize];
            if f == 0 {
                continue;
            }
            self.share = weight.saturating_mul(u128::from(f)).saturating_mul(SCALE) / total;
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(place, value) => {
                        self.place(place);
                        self.rvalue(value);
                    }
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::InlineAsm(_)
                    | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::SwitchInt { discr, .. } => self.operand(discr),
                TerminatorKind::Drop { place, .. } => self.place(place),
                TerminatorKind::DropAndReplace { place, value, .. } => {
                    self.place(place);
                    self.operand(value);
                }
                TerminatorKind::Call { func, args, destination, .. } => {
                    self.operand(func);
                    args.iter().for_each(|it| self.operand(it));
                    self.place(destination);
                }
                TerminatorKind::TailCall { func, args, .. } => {
                    self.operand(func);
                    args.iter().for_each(|it| self.operand(it));
                }
                TerminatorKind::Assert { cond, .. } => self.operand(cond),
                TerminatorKind::Yield { value, resume_arg, .. } => {
                    self.operand(value);
                    self.place(resume_arg);
                }
                TerminatorKind::Goto { .. }
                | TerminatorKind::UnwindResume
                | TerminatorKind::Abort
                | TerminatorKind::Return
                | TerminatorKind::Unreachable
                | TerminatorKind::CoroutineDrop
                | TerminatorKind::FalseEdge { .. }
                | TerminatorKind::FalseUnwind { .. }
                | TerminatorKind::ComptimeFor { .. }
                | TerminatorKind::ComptimeNext { .. } => {}
            }
        }
    }

    fn rvalue(&mut self, value: &Rvalue) {
        match value {
            Rvalue::Use(it)
            | Rvalue::Repeat(it, _)
            | Rvalue::Cast(_, it, _)
            | Rvalue::UnaryOp(_, it) => self.operand(it),
            Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                self.operand(a);
                self.operand(b);
            }
            Rvalue::Ref(_, place)
            | Rvalue::Len(place)
            | Rvalue::Discriminant(place)
            | Rvalue::CopyForDeref(place) => self.place(place),
            // The value built is no access; what it is built from is.
            Rvalue::Aggregate(_, operands) => operands.iter().for_each(|it| self.operand(it)),
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn operand(&mut self, operand: &Operand) {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place(place),
            OperandKind::Constant { .. }
            | OperandKind::Allocation { .. }
            | OperandKind::Static(_) => {}
        }
    }

    fn place(&mut self, place: &StoredPlace) {
        let elems = place.projection.as_slice();
        if !elems.iter().any(|it| matches!(it, ProjectionElem::Field(_))) {
            return;
        }
        let mut ty = PlaceTy::from_ty(self.mir.locals[place.local].ty.as_ref());
        for elem in elems {
            if let ProjectionElem::Field(field) = *elem {
                self.field(ty, field);
            }
            ty = ty.projection_ty(self.infcx, elem, self.env);
        }
    }

    /// Adds the block's share to `field` of the place of type `base`: of a struct, or of an
    /// enum's variant (`Downcast`). A field of a tuple, closure, coroutine or union is not one
    /// a plan orders, and a struct named through an unnormalized alias is not seen.
    fn field(&mut self, base: PlaceTy<'_>, field: FieldIndex) {
        let (adt, variant) = match base.variant_id {
            Some(VariantId::EnumVariantId(v)) => {
                let parent = v.loc(self.db).parent;
                let Some(index) = enum_variant_ids(self.db, parent).iter().position(|&it| it == v)
                else {
                    return;
                };
                (AdtId::EnumId(parent), index as u32)
            }
            Some(VariantId::StructId(_) | VariantId::UnionId(_)) => return,
            None => match base.ty.kind() {
                TyKind::Adt(def, _) => match def.def_id() {
                    adt @ AdtId::StructId(_) => (adt, 0),
                    AdtId::UnionId(_) | AdtId::EnumId(_) => return,
                },
                _ => return,
            },
        };
        let heat = self.by_field.entry((adt, variant, field.to_local_field_id())).or_insert(0);
        *heat = heat.saturating_add(self.share);
    }
}

/// Each block's estimated frequency, by its raw index: [`LOOP_FACTOR`] to the power of the
/// loops around it (at most [`MAX_DEPTH`]), 0 for a block on an unwind path or not reachable
/// from the start without one.
///
/// The loops are found level by level: a strongly connected set of blocks with a cycle is a
/// loop, its header the one a depth-first walk from the start reaches first, and the loops
/// nested in it are those of the same set without its header.
pub(crate) fn block_frequencies<'f>(body: &MirBody<'_>, frame: &'f Frame) -> Vec<u64, &'f Frame> {
    let n = body.basic_blocks.len();
    let raw = |id: crate::mir::BasicBlockId| id.into_raw().into_u32() as usize;
    // The edges between blocks off the unwind paths, as offsets into `targets`.
    let mut starts = filled(n + 1, 0usize, frame);
    let mut targets = Vec::new_in(frame);
    let mut cleanup = filled(n, false, frame);
    for (id, block) in body.basic_blocks.iter() {
        cleanup[raw(id)] = block.is_cleanup;
    }
    for (id, block) in body.basic_blocks.iter() {
        if let Some(terminator) = &block.terminator
            && !block.is_cleanup
        {
            targets.extend(terminator.kind.successors().map(raw).filter(|&to| !cleanup[to]));
        }
        starts[raw(id) + 1] = targets.len();
    }
    let edges = |b: usize| &targets[starts[b]..starts[b + 1]];
    // What the start reaches, in depth-first preorder.
    let mut preorder = filled(n, u32::MAX, frame);
    let start = raw(body.start_block);
    let mut stack = Vec::new_in(frame);
    let mut next = 0;
    if !cleanup[start] {
        stack.push(start);
    }
    while let Some(b) = stack.pop() {
        if preorder[b] != u32::MAX {
            continue;
        }
        preorder[b] = next;
        next += 1;
        stack.extend(edges(b).iter().rev().copied().filter(|&to| preorder[to] == u32::MAX));
    }
    let mut depth = filled(n, 0u32, frame);
    let mut member = filled(n, false, frame);
    for b in 0..n {
        member[b] = preorder[b] != u32::MAX;
    }
    let mut sccs = Sccs::new(n, frame);
    for _ in 0..MAX_DEPTH {
        sccs.find(&member, &starts, &targets);
        let mut inner = filled(n, false, frame);
        let mut any = false;
        for c in 0..sccs.count {
            let of = sccs.members(c);
            let cyclic = of.len() > 1 || edges(of[0]).contains(&of[0]);
            if !cyclic {
                continue;
            }
            any = true;
            let header = of.iter().copied().min_by_key(|&b| preorder[b]).unwrap_or(of[0]);
            for &b in of {
                depth[b] += 1;
                inner[b] = b != header;
            }
        }
        if !any {
            break;
        }
        member = inner;
    }
    let mut freq = filled(n, 0u64, frame);
    for b in 0..n {
        if preorder[b] != u32::MAX {
            freq[b] = LOOP_FACTOR.pow(depth[b]);
        }
    }
    freq
}

fn filled<T: Clone>(n: usize, value: T, frame: &Frame) -> Vec<T, &Frame> {
    let mut v = Vec::with_capacity_in(n, frame);
    v.resize(n, value);
    v
}

/// Tarjan's strongly connected components of the blocks in `member`, without recursion.
struct Sccs<'f> {
    /// The blocks by component: component `c` is `order[ends[c - 1]..ends[c]]`.
    order: Vec<usize, &'f Frame>,
    ends: Vec<usize, &'f Frame>,
    count: usize,
    index: Vec<u32, &'f Frame>,
    low: Vec<u32, &'f Frame>,
    on_stack: Vec<bool, &'f Frame>,
    stack: Vec<usize, &'f Frame>,
    calls: Vec<(usize, usize), &'f Frame>,
}

impl<'f> Sccs<'f> {
    fn new(n: usize, frame: &'f Frame) -> Sccs<'f> {
        Sccs {
            order: Vec::with_capacity_in(n, frame),
            ends: Vec::with_capacity_in(n, frame),
            count: 0,
            index: filled(n, u32::MAX, frame),
            low: filled(n, 0, frame),
            on_stack: filled(n, false, frame),
            stack: Vec::with_capacity_in(n, frame),
            calls: Vec::with_capacity_in(n, frame),
        }
    }

    fn members(&self, c: usize) -> &[usize] {
        let start = if c == 0 { 0 } else { self.ends[c - 1] };
        &self.order[start..self.ends[c]]
    }

    fn find(&mut self, member: &[bool], starts: &[usize], targets: &[usize]) {
        self.index.fill(u32::MAX);
        self.on_stack.fill(false);
        self.order.clear();
        self.ends.clear();
        self.count = 0;
        let mut next = 0;
        for root in 0..member.len() {
            if !member[root] || self.index[root] != u32::MAX {
                continue;
            }
            self.enter(root, &mut next, starts);
            while let Some(&(v, at)) = self.calls.last() {
                if at < starts[v + 1] {
                    if let Some(call) = self.calls.last_mut() {
                        call.1 += 1;
                    }
                    let w = targets[at];
                    if !member[w] {
                        continue;
                    }
                    if self.index[w] == u32::MAX {
                        self.enter(w, &mut next, starts);
                    } else if self.on_stack[w] {
                        self.low[v] = self.low[v].min(self.index[w]);
                    }
                    continue;
                }
                self.calls.pop();
                if let Some(&(u, _)) = self.calls.last() {
                    self.low[u] = self.low[u].min(self.low[v]);
                }
                if self.low[v] == self.index[v] {
                    while let Some(w) = self.stack.pop() {
                        self.on_stack[w] = false;
                        self.order.push(w);
                        if w == v {
                            break;
                        }
                    }
                    self.ends.push(self.order.len());
                    self.count += 1;
                }
            }
        }
    }

    fn enter(&mut self, v: usize, next: &mut u32, starts: &[usize]) {
        self.index[v] = *next;
        self.low[v] = *next;
        *next += 1;
        self.stack.push(v);
        self.on_stack[v] = true;
        self.calls.push((v, starts[v]));
    }
}
