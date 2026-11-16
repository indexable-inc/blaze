//! Vtables (docs/v2/plan/design/codegen.md, "Trait objects: vtables and dynamic calls"): the
//! table a wide pointer to `dyn Tr` carries in its second word, one record per
//! `(concrete type, principal trait with its arguments)`.
//!
//! A table is 8-byte words: word 0 the drop glue (`drop_in_place::<T>`, or 0 when `T` needs no
//! drop), word 1 the size, word 2 the alignment, then the [`Shape`]'s words. The shape is a
//! function of the traits' text alone ([`ObjectTrait`], the world's answer), so two builders
//! agree: the supertraits depth first in declaration order, the principal last, each trait's
//! dispatchable methods in declaration order, a trait reached twice only once. The first
//! supertrait chain is a prefix of the table (upcasting along it keeps the pointer); every other
//! supertrait `S` gets one word after its methods, the address of `(T, S)`'s table (upcasting
//! to it is one load).
//!
//! Every function word and every supertrait word is a relocation ([`VtableTarget`]), never an
//! address: the bytes hold only the size and alignment, so an edit to one impl method changes no
//! table's bytes. The key ([`vtable_key`]) covers every input of the bytes and relocations
//! (rule 21): the concrete type, the principal and its arguments, and the principal's
//! [`ObjectTrait::shape`], the world's hash of the method lists and supertraits it reaches. So a
//! method appended to a trait changes the shape, hence the key, of that trait's tables and of
//! the traits below it, and nothing else; a method inserted in the middle moves the slot of every
//! method after it, in that trait and in every trait below it (the order is the text's, by
//! design: no table is renumbered by anything but its own traits' text).

use core::alloc::Allocator;
use core::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_own::Alloc;
use blaze_typeck::{ItemKey, List, PTy, PortableTys};

use crate::data::{DataAlloc, DataRecord, DataReloc, DataTarget};
use crate::{BuildError, Instance, InstanceKey, instance_key};

/// Words before the methods: the drop glue, the size, the alignment.
pub const HEADER_WORDS: u32 = 3;

/// A trait as a trait object sees it, from the world (the checker's trait facts).
#[derive(Clone, Copy, Debug)]
pub struct ObjectTrait<'a> {
    /// The supertraits, in declaration order. A supertrait with generic arguments is refused
    /// by the world by name until its arguments are carried here.
    pub supers: &'a [ItemKey],
    /// The methods, in declaration order.
    pub methods: &'a [ObjectMethod],
    /// The hash of what the table's shape reads: this trait's methods (and whether each is
    /// dispatchable), its supertraits, and theirs, transitively. Part of every table's key.
    pub shape: ContentId,
}

/// One method of a trait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjectMethod {
    pub item: ItemKey,
    /// Whether it has a slot: false for a method with `where Self: Sized` or generic
    /// parameters (the Reference, "Trait objects", dyn compatibility: such a method is not
    /// callable on the object).
    pub dispatch: bool,
}

/// A word after the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Word {
    /// Method `item` of trait `trait_`.
    Method { trait_: ItemKey, item: ItemKey },
    /// The address of the same type's table for supertrait `S`, off the first chain.
    Super(ItemKey),
}

/// How a pointer to `dyn Principal` becomes a pointer to `dyn S`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Upcast {
    /// `S` is the principal or on its first supertrait chain: the same vtable pointer.
    Prefix,
    /// `S`'s table is the word at this slot (after the header).
    Load(u32),
}

/// Why a table was not made, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VtableError {
    /// The world gave no [`ObjectTrait`] for this trait.
    NoTrait(ItemKey),
    /// A table past 2^32 words.
    TooLarge,
}

impl fmt::Display for VtableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VtableError::NoTrait(t) => write!(f, "trait {:#x}: no trait-object facts (its methods and supertraits)", t.0),
            VtableError::TooLarge => write!(f, "a vtable of more than 2^32 words"),
        }
    }
}

/// The words after the header of `principal`'s tables, and which traits are its prefix.
pub struct Shape<A: Allocator> {
    pub principal: ItemKey,
    pub words: Vec<Word, A>,
    /// The principal and its first supertrait chain: an upcast to one keeps the pointer.
    pub prefix: Vec<ItemKey, A>,
}

impl<A: Allocator + Clone> Shape<A> {
    /// The shape of `principal`'s tables. `facts` is the world's [`ObjectTrait`] of a trait.
    /// Depth first without recursion: a stack of (trait, next supertrait, on the first chain).
    pub fn of<'f>(principal: ItemKey, facts: impl Fn(ItemKey) -> Option<ObjectTrait<'f>>, alloc: A) -> Result<Shape<A>, VtableError> {
        let mut words = Vec::new_in(alloc.clone());
        let mut prefix = Vec::new_in(alloc.clone());
        let mut seen: Vec<ItemKey, A> = Vec::new_in(alloc.clone());
        let mut stack: Vec<(ItemKey, usize, bool), A> = Vec::new_in(alloc);
        seen.push(principal);
        stack.push((principal, 0, true));
        while let Some(&(t, next, first)) = stack.last() {
            let o = facts(t).ok_or(VtableError::NoTrait(t))?;
            if let Some(&s) = o.supers.get(next) {
                let top = stack.len() - 1;
                stack[top].1 = next + 1;
                let is_first = next == 0;
                if !seen.contains(&s) {
                    seen.push(s);
                    stack.push((s, 0, first && is_first));
                }
                continue;
            }
            stack.pop();
            words.extend(o.methods.iter().filter(|m| m.dispatch).map(|m| Word::Method { trait_: t, item: m.item }));
            match first {
                true => prefix.push(t),
                false => words.push(Word::Super(t)),
            }
        }
        u32::try_from(words.len()).ok().and_then(|n| n.checked_add(HEADER_WORDS)).ok_or(VtableError::TooLarge)?;
        Ok(Shape { principal, words, prefix })
    }

    /// The slot (after the header) of method `item`, `None` when it has none.
    pub fn slot(&self, item: ItemKey) -> Option<u32> {
        self.words.iter().position(|w| matches!(*w, Word::Method { item: i, .. } if i == item)).map(|at| at as u32)
    }

    /// How to upcast to supertrait `to`; `None` when `to` is not the principal or above it.
    pub fn upcast(&self, to: ItemKey) -> Option<Upcast> {
        if self.prefix.contains(&to) {
            return Some(Upcast::Prefix);
        }
        self.words.iter().position(|w| *w == Word::Super(to)).map(|at| Upcast::Load(at as u32))
    }
}

/// A table a built function takes the address of (`RefKind::Vtable`, by place in
/// `Built::vtables`): the concrete type and the principal trait with its arguments after `Self`,
/// types of the ground table. Placement makes the record ([`Shape::of`], [`Vtable::build`],
/// [`Vtable::record`]) once per program and fills the word with its address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VtableRef {
    pub concrete: PTy,
    pub principal: ItemKey,
    pub args: List,
}

/// A key naming one table: [`vtable_key`]'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VtableKey(pub ContentId);

const VTABLE_DOMAIN: &str = "blaze-build vtable key 1";

/// The key of `(concrete, principal<args>)`'s table, `shape` the principal's
/// [`ObjectTrait::shape`]: the types written as trees ([`instance_key`]'s form), never as
/// table indices.
pub fn vtable_key(concrete: PTy, principal: ItemKey, args: &[PTy], shape: ContentId, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<VtableKey, BuildError> {
    let mut code = Hasher::new(VTABLE_DOMAIN);
    code.id(&shape).update(&principal.0.to_le_bytes());
    let mut all: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(1 + args.len(), scratch);
    all.push(concrete);
    all.extend_from_slice(args);
    let list = ground.intern_list(&all).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
    Ok(VtableKey(instance_key(code.finish(), list, ground.view(), scratch)?.0))
}

/// What a body's code reads of a trait's shape (codegen.md, "Trait objects: vtables and dynamic
/// calls", "What a function's key holds of a trait"): a word number, which is the only fact of
/// the shape the code holds. An unsizing to `dyn Tr` reads none (its table is a relocation by
/// `(concrete, principal, args)`, resolved to today's [`VtableKey`] at link), nor does a read of
/// header words 0 to 2 (drop, size, alignment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectRead {
    /// A dynamic call of `method` through an object of `principal`: the slot it loads.
    Call { principal: ItemKey, method: ItemKey, slot: u32 },
    /// An upcast from `dyn from` to `dyn to`: `None` the same table pointer (`to` on `from`'s
    /// first supertrait chain), else the slot whose word is `to`'s table.
    Upcast { from: ItemKey, to: ItemKey, slot: Option<u32> },
}

/// The key of a function's record: its [`InstanceKey`] (code identity and arguments, by which
/// references name it) and the [`ObjectRead`]s of its body, in expression order. Computed before
/// the build, from the checked body and the world's [`ObjectTrait`]s ([`crate::object_reads`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordKey(pub ContentId);

const RECORD_DOMAIN: &str = "blaze-build fn record key 1";

/// [`RecordKey`] of `instance` whose body reads `reads` (in the order [`crate::object_reads`]
/// gives them: expression order, deterministic).
pub fn record_key(instance: InstanceKey, reads: &[ObjectRead]) -> RecordKey {
    let mut h = Hasher::new(RECORD_DOMAIN);
    h.id(&instance.0).update(&(reads.len() as u64).to_le_bytes());
    for r in reads {
        match *r {
            ObjectRead::Call { principal, method, slot } => {
                h.update(&[0]).update(&principal.0.to_le_bytes()).update(&method.0.to_le_bytes()).update(&slot.to_le_bytes());
            }
            ObjectRead::Upcast { from, to, slot } => {
                let word = slot.map_or(0, |s| u64::from(s) + 1);
                h.update(&[1]).update(&from.0.to_le_bytes()).update(&to.0.to_le_bytes()).update(&word.to_le_bytes());
            }
        }
    }
    RecordKey(h.finish())
}

/// What a relocation word of a table becomes the address of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VtableTarget {
    /// A fn instance's entry: the drop glue, or a trait method at `[T, trait args..]` (the
    /// world resolves it to the impl's, as for any trait method call).
    Fn(Instance),
    /// The same concrete type's table for supertrait `S` (a supertrait has no arguments here).
    Super(ItemKey),
}

/// One table: its bytes (the header's size and alignment, every other word 0) and relocations
/// (sorted by offset).
pub struct Vtable<A: Allocator> {
    pub bytes: Vec<u8, A>,
    pub relocs: Vec<(u32, VtableTarget), A>,
}

impl<A: Allocator + Clone> Vtable<A> {
    /// `concrete`'s table for `shape`'s principal at `args` (the principal's arguments, types of
    /// the ground table, which `concrete` is in too). `size` and `align` are bytes; `drop` the
    /// glue instance when `concrete` needs drop.
    pub fn build(shape: &Shape<A>, concrete: PTy, args: &[PTy], size: u64, align: u64, drop: Option<Instance>, ground: &mut PortableTys<'_>, alloc: A) -> Result<Vtable<A>, BuildError> {
        let full = || BuildError::Malformed { what: "the ground table is full" };
        let words = shape.words.len() + HEADER_WORDS as usize;
        let mut bytes = Vec::with_capacity_in(8 * words, alloc.clone());
        bytes.resize(8 * words, 0u8);
        bytes[8..16].copy_from_slice(&size.to_le_bytes());
        bytes[16..24].copy_from_slice(&align.to_le_bytes());
        let mut relocs = Vec::with_capacity_in(words, alloc.clone());
        if let Some(glue) = drop {
            relocs.push((0, VtableTarget::Fn(glue)));
        }
        let mut with_args: Vec<PTy, A> = Vec::with_capacity_in(1 + args.len(), alloc);
        with_args.push(concrete);
        with_args.extend_from_slice(args);
        let principal_args = ground.intern_list(&with_args).map_err(|_| full())?;
        let alone = ground.intern_list(&[concrete]).map_err(|_| full())?;
        for (i, w) in shape.words.iter().enumerate() {
            let offset = 8 * (HEADER_WORDS + i as u32);
            let to = match *w {
                Word::Method { trait_, item } => {
                    let args: List = if trait_ == shape.principal { principal_args } else { alone };
                    VtableTarget::Fn(Instance { item, args })
                }
                Word::Super(s) => VtableTarget::Super(s),
            };
            relocs.push((offset, to));
        }
        Ok(Vtable { bytes, relocs })
    }

    /// The table as the store's data record: each relocation named by key (`fn_key` gives an
    /// instance's, `super_key` a supertrait table's), the words holding addend 0.
    pub fn record<E>(
        &self,
        mut fn_key: impl FnMut(Instance) -> Result<InstanceKey, E>,
        mut super_key: impl FnMut(ItemKey) -> Result<VtableKey, E>,
        alloc: A,
    ) -> Result<DataRecord<A>, E> {
        let mut bytes = Vec::with_capacity_in(self.bytes.len(), alloc.clone());
        bytes.extend_from_slice(&self.bytes);
        let mut relocs = Vec::with_capacity_in(self.relocs.len(), alloc.clone());
        for &(offset, to) in self.relocs.iter() {
            let target = match to {
                VtableTarget::Fn(i) => DataTarget::Fn(fn_key(i)?),
                VtableTarget::Super(s) => DataTarget::Vtable(super_key(s)?),
            };
            relocs.push(DataReloc { offset, target });
        }
        let mut allocs = Vec::with_capacity_in(1, alloc);
        allocs.push(DataAlloc { bytes, align: 3, mutable: false, relocs });
        Ok(DataRecord { allocs })
    }
}

#[cfg(test)]
mod tests;
