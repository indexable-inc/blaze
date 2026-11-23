//! Constants AT AN INSTANCE'S GROUND ARGUMENTS: the value of a constant item read by path (a
//! trait's constant as declared, an inherent impl's constant over the impl's generics) and of
//! a `const { .. }` block, at the generic arguments of the instance whose body reads it.
//!
//! The Reference, "Associated Items", "Associated constants": "An associated constant
//! definition defines a constant associated with a type", and an impl that writes no definition
//! takes the trait's default (its example "Using default values"); so `<T as P>::S` has one
//! value per `T`, the selected impl's. The Reference, "Constant evaluation": "Constant
//! evaluation is the process of computing the result of expressions during compilation", and
//! "Block expressions", "Const blocks": "A const block is a variant of a block expression whose
//! body evaluates at compile-time instead of at runtime" and "Const blocks have the ability to
//! reference generic parameters in scope, unlike free constant items. They are desugared to
//! constant items with generic parameters in scope": so each instance has its own value.
//!
//! The design (the item layer's pull at item grain):
//! 1. NEEDS ARE KNOWN BEFORE THE WALK. An instance (fn, ground args) needs exactly each row of
//!    its body's `Checked::const_args` (a constant path and the arguments it is read at) and
//!    each const block and each promoted borrow (`Checked::promoted`) of the body, every
//!    argument grounded at the instance's ([`needs_of`]).
//! 2. THE LOOP PULLS THEM. The build's instance loop (run.rs `build_from`) asks the front end
//!    for each need not held yet ([`pull`], the seam's `FrontEnd::const_at`), with the
//!    program mutable: front2's evaluator, kept by the program after it is made, may check a
//!    body or make an impl row it has not made yet. Each answer is filed in the build's
//!    [`ConstTable`], keyed by the need and its ground arguments; then the instance is walked
//!    and the builder's `Adts::const_value` / `Adts::const_block_value` only READ the table.
//!    No walk is done twice, nothing is parked or retried, and a value depends only on its
//!    need and arguments, never on the order instances are built in.
//! 3. A need the evaluator refuses is the instance's refusal ([`ConstAtRefusal`]), carrying
//!    the evaluator's own reason.
//! 4. A constant whose value needs another constant at arguments is met by the evaluator's own
//!    work list (blaze-consteval `Settle`, consts.rs `Consts::value_at`): no recursion here; a
//!    cycle is the evaluator's typed refusal naming the constants.
//!
//! The table's keys are item keys, expression ids and types of the build's own ground table
//! (interned: one id per type), never a machine fact; nothing in it reaches a record, a digest
//! or an output in its map order (it is only looked up).

use core::alloc::Allocator;
use core::fmt;
// The builder's data record's own vector and allocator (blaze-build data.rs): its form.
use std::{alloc::Global, vec::Vec as DataVec};

use blaze_build::BuildError;
use blaze_build::data::{DataAlloc, DataRecord};
use blaze_build::layout::LayoutFacts;
use blaze_consteval::ConstRecord;
use blaze_ids::Hasher;
use blaze_own::{Alloc, SortedMap, Vec};
use blaze_typeck::{BodyView, Checked, Def, Expr, ExprId, ItemKey, List, PNode, PTy, PTys, PortableTys};

use super::consts::{BodyRoot, ConstFail};
use crate::seam::FrontEnd;

/// One constant an instance's body reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstNeed {
    /// A constant item read by path (`Def::Const` in `expr_defs`): a trait's constant as the
    /// trait declares it (the impl is selected at the arguments' `Self`), or an inherent
    /// impl's constant over the impl's generics.
    Item(ItemKey),
    /// `const { .. }` block `block` of `owner`'s body, at the owner's generic arguments.
    Block { owner: ItemKey, block: ExprId },
    /// Promoted borrow `borrow` (`&operand` in `Checked::promoted`) of `owner`'s body, at the
    /// owner's generic arguments: the operand's value, whose row the builder borrows
    /// (blaze-build walk/promote.rs). Per instance as a const block is: the operand may name
    /// the fn's generics (`&RawWakerVTable::new(clone_arc_raw::<W>, ..)`).
    Promoted { owner: ItemKey, borrow: ExprId },
}

impl ConstNeed {
    /// The table's order key: the need's kind, its item, its expression.
    fn order(self) -> (u8, u128, u32) {
        match self {
            ConstNeed::Item(k) => (0, k.0, 0),
            ConstNeed::Block { owner, block } => (1, owner.0, block.0),
            ConstNeed::Promoted { owner, borrow } => (2, owner.0, borrow.0),
        }
    }

    /// The item a refusal of the need is about: the constant, or the fn whose body holds it.
    pub fn item(self) -> ItemKey {
        match self {
            ConstNeed::Item(k) => k,
            ConstNeed::Block { owner, .. } | ConstNeed::Promoted { owner, .. } => owner,
        }
    }

    /// A need read from inside a body (a const block, a promoted borrow): its owner, where the
    /// evaluator starts in the owner's body, and the key it keeps the constant under over the
    /// owner's text `owner_text` ([`block_key`], [`promoted_key`]). `None` for a constant item.
    pub fn in_body(self, owner_text: &str) -> Option<(ItemKey, BodyRoot, ItemKey)> {
        match self {
            ConstNeed::Item(_) => None,
            ConstNeed::Block { owner, block } => Some((owner, BodyRoot::Block(block), block_key(owner, block, owner_text))),
            ConstNeed::Promoted { owner, borrow } => Some((owner, BodyRoot::Promoted(borrow), promoted_key(owner, borrow, owner_text))),
        }
    }
}

impl fmt::Display for ConstNeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstNeed::Item(k) => write!(f, "constant {:#x}", k.0),
            ConstNeed::Block { owner, block } => write!(f, "the const block at expression {} of fn {:#x}", block.0, owner.0),
            ConstNeed::Promoted { owner, borrow } => write!(f, "the promoted borrow at expression {} of fn {:#x}", borrow.0, owner.0),
        }
    }
}

/// Why the front end has no value for a need.
#[derive(Debug)]
pub enum ConstAtWhy {
    /// The front end evaluates no constant at arguments (the seam's default).
    NoEvaluator,
    /// front2's evaluator refused, in its own words (a cycle of constants included).
    Eval(ConstFail),
}

/// A need of an instance the front end refused: the instance's refusal (run.rs
/// `RunError::ConstAt` names the instance).
#[derive(Debug)]
pub struct ConstAtRefusal {
    pub need: ConstNeed,
    pub why: ConstAtWhy,
}

impl fmt::Display for ConstAtRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            ConstAtWhy::NoEvaluator => write!(f, "{} at the instance's arguments: this front end evaluates no constant at arguments", self.need),
            ConstAtWhy::Eval(e) => write!(f, "{} at the instance's arguments: {e}", self.need),
        }
    }
}

/// The needs of one instance, each with its ground arguments (types of the build's ground
/// table), once each, in the order met.
pub struct Needs<A: Allocator + Copy> {
    /// The need and its arguments' range in `args`.
    rows: Vec<(ConstNeed, u32, u32), A>,
    args: Vec<PTy, A>,
}

impl<A: Allocator + Copy> Needs<A> {
    pub fn new_in(alloc: A) -> Needs<A> {
        Needs { rows: Vec::new_in(alloc), args: Vec::new_in(alloc) }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.args.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Each need and its arguments.
    pub fn iter(&self) -> impl Iterator<Item = (ConstNeed, &[PTy])> + '_ {
        self.rows.iter().map(|&(need, start, len)| (need, &self.args[start as usize..(start + len) as usize]))
    }

    /// Files `need` at `args` unless it is filed already. Public for the code record's read
    /// list (run/make_one.rs `verify_instance`): a found entry's constant rows are pulled at
    /// their arguments before they are asked again, with no body checked.
    pub fn push(&mut self, need: ConstNeed, args: &[PTy]) -> Result<(), BuildError> {
        if self.iter().any(|(n, a)| n == need && a == args) {
            return Ok(());
        }
        let full = || BuildError::Malformed { what: "more constant needs' arguments than a u32 counts" };
        let start = u32::try_from(self.args.len()).map_err(|_| full())?;
        let len = u32::try_from(args.len()).map_err(|_| full())?;
        self.args.extend_from_slice(args);
        self.rows.push((need, start, len));
        Ok(())
    }
}

/// The needs of the instance of `view`'s body at ground `args` (types of `ground`; the
/// owner's generics in order), filed into `out` (cleared first): every constant path the check
/// recorded arguments for (`Checked::const_args`, each grounded at `args` exactly as the
/// builder's walk grounds it: blaze-build walk.rs `const_path_args`), every constant pattern
/// alike (`Checked::pat_const_args`), and every const block,
/// at `args`, and every promoted borrow (`Checked::promoted`), at `args`. A constant with no
/// generics has no row: its one value is the program's own (`FrontEnd::const_record`), made
/// when the program was.
#[allow(clippy::too_many_arguments)]
pub fn needs_of<A: Allocator + Copy>(
    view: &BodyView<'_>,
    done: &Checked<'_>,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    facts: &dyn LayoutFacts,
    scratch: Alloc<'_>,
    out: &mut Needs<A>,
) -> Result<(), BuildError> {
    out.clear();
    let malformed = |what| BuildError::Malformed { what };
    let types = done.types.view();
    let def_exprs = done.expr_defs.column::<0>();
    let defs = done.expr_defs.column::<1>();
    let mut grounded: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    for (&e, &list) in done.const_args.column::<0>().iter().zip(done.const_args.column::<1>().iter()) {
        let at = def_exprs.binary_search(&e).map_err(|_| malformed("a constant path's arguments with no definition for the path"))?;
        let &Def::Const(item) = &defs[at] else {
            return Err(malformed("a constant path's arguments whose path names no constant"));
        };
        grounded.clear();
        for &t in types.list(list).map_err(|_| malformed("a constant path's arguments past the record's types"))? {
            grounded.push(blaze_build::ground(types, t, args, ground, scratch, facts)?);
        }
        out.push(ConstNeed::Item(item), &grounded)?;
    }
    // A constant pattern's (`Checked::pat_const_args`: `U256::ZERO =>`), as blaze-build
    // walk/matches/forms.rs `const_record` grounds it. Its arguments name no generic parameter
    // (E0158), so grounding only resolves them.
    let pat_defs = done.pat_defs.column::<0>();
    let pdefs = done.pat_defs.column::<1>();
    for (&p, &list) in done.pat_const_args.column::<0>().iter().zip(done.pat_const_args.column::<1>().iter()) {
        let at = pat_defs.binary_search(&p).map_err(|_| malformed("a constant pattern's arguments with no definition for the pattern"))?;
        let &Def::Const(item) = &pdefs[at] else {
            return Err(malformed("a constant pattern's arguments whose path names no constant"));
        };
        grounded.clear();
        for &t in types.list(list).map_err(|_| malformed("a constant pattern's arguments past the record's types"))? {
            grounded.push(blaze_build::ground(types, t, args, ground, scratch, facts)?);
        }
        out.push(ConstNeed::Item(item), &grounded)?;
    }
    // RFC 3681: each field a base-less `S { a: 1, .. }` takes from its default
    // (`Checked::default_fields`), the default's constant at the struct type's ground
    // arguments (its generics are the ADT's), as blaze-build walk.rs `init_defaults` reads it.
    // A non-generic ADT's default is a row at no arguments: the program makes no record of it.
    let mut last = None;
    for (&e, &key) in done.default_fields.column::<0>().iter().zip(done.default_fields.column::<2>().iter()) {
        if last != Some(e) {
            let t = *done.expr_ty.get(e.0 as usize).ok_or(malformed("a struct expression with defaults past the record's types"))?;
            let g = blaze_build::ground(types, t, args, ground, scratch, facts)?;
            match ground.view().node(g).map_err(|_| malformed("a grounded type past the ground table"))? {
                PNode::Adt(_, list) => {
                    grounded.clear();
                    grounded.extend_from_slice(ground.view().list(list).map_err(|_| malformed("an ADT's arguments past the ground table"))?);
                }
                PNode::Record(..) => {
                    grounded.clear();
                }
                _ => return Err(malformed("a struct expression with defaults of a type that is not an ADT or record")),
            }
            last = Some(e);
        }
        out.push(ConstNeed::Item(key), &grounded)?;
    }
    for (i, x) in view.exprs.iter().enumerate() {
        if matches!(x, Expr::ConstBlock { .. }) {
            let block = ExprId(u32::try_from(i).map_err(|_| malformed("an expression index past a u32"))?);
            out.push(ConstNeed::Block { owner: view.owner, block }, args)?;
        }
    }
    for &borrow in done.promoted.iter() {
        out.push(ConstNeed::Promoted { owner: view.owner, borrow }, args)?;
    }
    Ok(())
}

/// Every constant `done` reads at NO arguments (a path or a pattern whose check recorded no
/// arguments for it: those [`needs_of`] leaves to the program's own record), filed into `out`
/// beside what is there. The code-record walk pulls these too (run/make_one.rs
/// `pull_constants`), so every constant its builds read is answered from this table and its
/// verifier, which pulls a read list's `Const` rows here (`verify_instance`), asks the same
/// table: one answer per constant, never the program's record at the build and this table's at
/// the check (rule 30).
pub fn plain_needs_of<A: Allocator + Copy>(done: &Checked<'_>, out: &mut Needs<A>) -> Result<(), BuildError> {
    let at_args = done.const_args.column::<0>();
    for (e, d) in done.expr_defs.column::<0>().iter().zip(done.expr_defs.column::<1>().iter()) {
        if let &Def::Const(item) = d
            && at_args.binary_search(e).is_err()
        {
            out.push(ConstNeed::Item(item), &[])?;
        }
    }
    let pat_at_args = done.pat_const_args.column::<0>();
    for (p, d) in done.pat_defs.column::<0>().iter().zip(done.pat_defs.column::<1>().iter()) {
        if let &Def::Const(item) = d
            && pat_at_args.binary_search(p).is_err()
        {
            out.push(ConstNeed::Item(item), &[])?;
        }
    }
    Ok(())
}

/// The values of one build's needs, by need and ground arguments (types of the build's ground
/// table, interned: equal types have one id): filled by [`pull`] before each instance's walk,
/// read by the builder's `Adts` during it.
pub struct ConstTable<A: Allocator + Copy> {
    rows: SortedMap<(u8, u128, u32), Vec<(Vec<PTy, A>, DataRecord<Global>), A>, A>,
    alloc: A,
}

impl<A: Allocator + Copy> ConstTable<A> {
    pub fn new_in(alloc: A) -> ConstTable<A> {
        ConstTable { rows: SortedMap::new_in(alloc), alloc }
    }

    /// The value of `need` at `args`, if pulled.
    pub fn get(&self, need: ConstNeed, args: &[PTy]) -> Option<&DataRecord<Global>> {
        self.rows.get(&need.order())?.iter().find(|(a, _)| a[..] == *args).map(|(_, record)| record)
    }

    fn insert(&mut self, need: ConstNeed, args: &[PTy], record: DataRecord<Global>) {
        let alloc = self.alloc;
        let at = self.rows.get_or_insert_with(&need.order(), |&k| (k, Vec::new_in(alloc)));
        let mut kept = Vec::new_in(alloc);
        kept.extend_from_slice(args);
        at.push((kept, record));
    }
}

/// Asks the front end for each of `needs` (arguments of `from`, the build's ground table) not
/// in `table` yet, and files each answer. The first need refused is the instance's refusal.
/// `closures`: the build's closure facts, which a closure type among the arguments is laid
/// out from ([`FrontEnd::const_at`]).
pub fn pull<P: FrontEnd, A: Allocator + Copy>(program: &mut P, needs: &Needs<A>, from: PTys<'_>, table: &mut ConstTable<A>, closures: &crate::run::ClosureTable) -> Result<(), ConstAtRefusal> {
    for (need, args) in needs.iter() {
        if table.get(need, args).is_some() {
            continue;
        }
        let record = program.const_at(need, from, args, closures).map_err(|why| ConstAtRefusal { need, why })?;
        table.insert(need, args, record);
    }
    Ok(())
}

/// The key the evaluator keeps const block `block` of `owner` under: a function of the
/// owner's key, the block's expression and the owner's text (rule 21: the text is in it, so a
/// record of the block is never one of an older text).
pub fn block_key(owner: ItemKey, block: ExprId, owner_text: &str) -> ItemKey {
    in_body_key("blaze-ladder front2 const block", owner, block, owner_text)
}

/// The key the evaluator keeps promoted borrow `borrow` of `owner` under, as [`block_key`]
/// keeps a const block, in a domain of its own: a borrow and a block never share a key.
pub fn promoted_key(owner: ItemKey, borrow: ExprId, owner_text: &str) -> ItemKey {
    in_body_key("blaze-ladder front2 promoted borrow", owner, borrow, owner_text)
}

fn in_body_key(domain: &'static str, owner: ItemKey, at: ExprId, owner_text: &str) -> ItemKey {
    let mut h = Hasher::new(domain);
    h.update(&owner.0.to_le_bytes());
    h.update(&at.0.to_le_bytes());
    h.update(owner_text.as_bytes());
    let id = h.finish();
    let mut low = [0u8; 16];
    low.copy_from_slice(&id.0[..16]);
    ItemKey(u128::from_le_bytes(low))
}

/// The evaluator's record as the builder's data record (the same form, in the build's
/// allocator).
pub fn data_record(record: &ConstRecord<Alloc<'_>>) -> DataRecord<Global> {
    let mut allocs = DataVec::with_capacity_in(record.data.allocs.len(), Global);
    for a in record.data.allocs.iter() {
        let mut bytes = DataVec::with_capacity_in(a.bytes.len(), Global);
        bytes.extend_from_slice(&a.bytes);
        let mut relocs = DataVec::with_capacity_in(a.relocs.len(), Global);
        relocs.extend_from_slice(&a.relocs);
        allocs.push(DataAlloc { bytes, align: a.align, mutable: a.mutable, relocs });
    }
    DataRecord { allocs }
}

/// Why a type could not be copied into the evaluator's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFail {
    /// The type holds a node that is not ground (a parameter, a projection, an opaque type, a
    /// hole): a need's arguments are grounded before they are asked.
    NotGround { what: &'static str },
    /// A type or list index past its table.
    Malformed,
    /// The evaluator's table is full.
    Full,
}

impl fmt::Display for CopyFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyFail::NotGround { what } => write!(f, "an argument holds {what}, which is not a ground type"),
            CopyFail::Malformed => write!(f, "an argument's type index is past its table"),
            CopyFail::Full => write!(f, "the evaluator's type table is full"),
        }
    }
}

/// The ground types `args` of `from`, copied into `into` (each node after its children: a
/// node's children have smaller indices, so marking from the top down and mapping in index
/// order needs no recursion), as one list of `into`.
pub fn copy_ground(from: PTys<'_>, args: &[PTy], into: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<List, CopyFail> {
    let mut out: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(args.len(), scratch);
    let mut reached: Vec<bool, Alloc<'_>> = Vec::new_in(scratch);
    let mut mapped: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let mut children: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let mut list: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    for &ty in args {
        let top = ty.0 as usize;
        reached.clear();
        reached.resize(top + 1, false);
        reached[top] = true;
        mapped.clear();
        mapped.resize(top + 1, PTy(u32::MAX));
        for at in (0..=top).rev() {
            if !reached[at] {
                continue;
            }
            children.clear();
            let node = from.node(PTy(at as u32)).map_err(|_| CopyFail::Malformed)?;
            blaze_build::instance::each_child(from, node, &mut children).map_err(|_| CopyFail::Malformed)?;
            for c in children.iter() {
                *reached.get_mut(c.0 as usize).ok_or(CopyFail::Malformed)? = true;
            }
        }
        for at in 0..=top {
            if !reached[at] {
                continue;
            }
            let raw_node = from.node(PTy(u32::try_from(at).map_err(|_| CopyFail::Malformed)?)).map_err(|_| CopyFail::Malformed)?;
            if let PNode::Record(s, _) = raw_node
                && let Some(names) = from.record_names(s)
            {
                into.register_record_schema(s, names).map_err(|_| CopyFail::Full)?;
            }
            let mut map_list = |l: List, list: &mut Vec<PTy, Alloc<'_>>| -> Result<List, CopyFail> {
                list.clear();
                for t in from.list(l).map_err(|_| CopyFail::Malformed)? {
                    list.push(*mapped.get(t.0 as usize).ok_or(CopyFail::Malformed)?);
                }
                into.intern_list(list).map_err(|_| CopyFail::Full)
            };
            let m = |t: PTy| mapped.get(t.0 as usize).copied().ok_or(CopyFail::Malformed);
            let node = match raw_node {
                PNode::Param(_) | PNode::ConstParam(_) => return Err(CopyFail::NotGround { what: "a generic parameter" }),
                PNode::Alias { .. } => return Err(CopyFail::NotGround { what: "a projection" }),
                PNode::Opaque(..) => return Err(CopyFail::NotGround { what: "an `impl Trait` type" }),
                PNode::Infer | PNode::Error => return Err(CopyFail::NotGround { what: "a hole or an error type" }),
                PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty)?, bits },
                n @ (PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never) => n,
                PNode::Tuple(l) => PNode::Tuple(map_list(l, &mut list)?),
                PNode::Record(s, l) => PNode::Record(s, map_list(l, &mut list)?),
                PNode::Union(l) => PNode::Union(map_list(l, &mut list)?),
                PNode::Ref(mu, t) => PNode::Ref(mu, m(t)?),
                PNode::Ptr(mu, t) => PNode::Ptr(mu, m(t)?),
                PNode::Array(t, n) => PNode::Array(m(t)?, m(n)?),
                PNode::Slice(t) => PNode::Slice(m(t)?),
                PNode::Pat(t, l) => PNode::Pat(m(t)?, map_list(l, &mut list)?),
                PNode::Adt(key, l) => PNode::Adt(key, map_list(l, &mut list)?),
                PNode::FnDef(key, l) => PNode::FnDef(key, map_list(l, &mut list)?),
                PNode::FnPtr(l, header) => PNode::FnPtr(map_list(l, &mut list)?, header),
                PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: map_list(args, &mut list)? },
                PNode::Future(e, o, i) => PNode::Future(e, m(o)?, m(i)?),
                PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: map_list(args, &mut list)? },
                PNode::Dynamic(l) => PNode::Dynamic(map_list(l, &mut list)?),
                PNode::ExTrait(key, l) => PNode::ExTrait(key, map_list(l, &mut list)?),
                PNode::ExProj { trait_, name, args, term } => PNode::ExProj { trait_, name, args: map_list(args, &mut list)?, term: m(term)? },
                PNode::ExAuto(key) => PNode::ExAuto(key),
                PNode::Foreign(key) => PNode::Foreign(key),
            };
            mapped[at] = into.intern(node).map_err(|_| CopyFail::Full)?;
        }
        out.push(mapped[top]);
    }
    into.intern_list(&out).map_err(|_| CopyFail::Full)
}
