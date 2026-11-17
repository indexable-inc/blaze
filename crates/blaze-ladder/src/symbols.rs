//! Symbols: the stable names code records give what they reference (docs/v2/plan/design/run.md,
//! "End state", "The code record"; pieces 3 and 7).
//!
//! A FUNCTION INSTANCE's symbol is the hash of its INSTANCE NAME: the canonical bytes of its
//! identity, the item's key and its ground arguments written as trees ([`encode_instance`]).
//! Never an address, an index of this run (a slot, a ground-table id, a fn's place in the
//! program) or a path: two builds of one program give equal symbols, whatever else their ground
//! tables hold and whatever order they met the instances in. The name decodes back into any
//! ground table ([`decode_instance`]), so whoever is asked for a symbol's code (the run's parent,
//! on a stub's first call) can make the instance from the name a record declared it by.
//!
//! The instance name's form, little-endian throughout:
//!
//! - the item's key, 16 bytes; the count of arguments, 4 bytes;
//! - each argument's tree in pre-order, children in [`blaze_build::instance::each_child`]'s
//!   order: per node a tag byte, the tag's fixed payload, then its count of children (4 bytes).
//!
//! | tag | node | payload |
//! |---|---|---|
//! | 0..=6 | bool, char, `iN`, `uN`, `fN`, str, `!` | the scalar's index byte for 2, 3, 4 |
//! | 7, 10, 11, 15, 16 | tuple, array, slice, pattern type, `dyn` | none |
//! | 8, 9 | `&`, `*` | the mutability byte |
//! | 12, 13, 17, 19 | ADT, fn item, a `dyn`'s principal, auto trait | the item's key (16) |
//! | 14 | const value | its bits (16) |
//! | 18 | a `dyn`'s projection | the trait's key (16), the name (8) |
//! | 20 | closure | the owner's key (16), its ordinal (4) |
//! | 21 | fn pointer | the header byte (`FnPtrHeader::bits`) |
//! | 22 | async block's future | its ordinal (4) |
//!
//! A type with no ground form (a parameter, an alias, an opaque type, a hole, an error) is
//! refused by name ([`SymbolError::NotGround`]).
//!
//! STATICS, THREAD-LOCALS and VTABLES the same way: a static's and a thread-local's symbol is
//! its item's key under its own domain; a vtable's is its principal trait's key and the trees of
//! its concrete type and the trait's arguments. The thread-local descriptor and getter are one
//! each a program: fixed symbols. C names stay text (`Named::C`): the record keeps the name and
//! `blaze_code::record::Symbol::of_name` is its one symbol.

use core::fmt;

use blaze_build::instance::each_child;
use blaze_build::vtable::VtableRef;
use blaze_code::record::Symbol;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::{FloatTy, FnPtrHeader, IntTy, ItemKey, List, Mutbl, Name, PNode, PTy, PTys, PortableTys, UintTy};

const FN_DOMAIN: &str = "blaze-ladder fn instance symbol 1";
const STATIC_DOMAIN: &str = "blaze-ladder static symbol 1";
const TLS_DOMAIN: &str = "blaze-ladder thread-local symbol 1";
const VTABLE_DOMAIN: &str = "blaze-ladder vtable symbol 1";
const TLS_DESC_DOMAIN: &str = "blaze-ladder thread-local descriptor symbol 1";
const TLS_GETTER_DOMAIN: &str = "blaze-ladder thread-local getter symbol 1";
const ITEM_DOMAIN: &str = "blaze-ladder item identity 1";

/// The scalar types by the index byte the name writes (their declaration order).
const INTS: [IntTy; 6] = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
const UINTS: [UintTy; 6] = [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
const FLOATS: [FloatTy; 4] = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];
const MUTBLS: [Mutbl; 2] = [Mutbl::Not, Mutbl::Mut];

/// The fewest bytes a node of a name takes (a tag and a child count): bounds every count read.
const LEAST_NODE: usize = 5;

/// Why an instance has no name, or a name does not decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolError {
    /// A type with no ground form (`what`), or a type or list past its table.
    NotGround { what: &'static str },
    /// The name ends inside a node (at byte `at`), or holds more nodes than its bytes can.
    Truncated { at: usize },
    /// A tag the form does not have, at byte `at`.
    UnknownTag { tag: u8, at: usize },
    /// A node of tag `tag` with `got` children, which it cannot have.
    Children { tag: u8, got: u32 },
    /// A payload byte (`what`) the form never writes.
    BadByte { what: &'static str },
    /// Bytes after the last tree.
    Trailing { at: usize },
    /// The ground table is full.
    TableFull,
}

impl fmt::Display for SymbolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "an instance's symbol is its item and ground arguments (blaze-ladder symbols.rs)";
        match *self {
            SymbolError::NotGround { what } => write!(f, "{what} in an instance's arguments: {rule}"),
            SymbolError::Truncated { at } => write!(f, "an instance name ends inside a node at byte {at}: {rule}"),
            SymbolError::UnknownTag { tag, at } => write!(f, "an instance name holds tag {tag} at byte {at}, which the form does not have: {rule}"),
            SymbolError::Children { tag, got } => write!(f, "an instance name's node of tag {tag} has {got} children: {rule}"),
            SymbolError::BadByte { what } => write!(f, "an instance name's {what} is a byte the form never writes: {rule}"),
            SymbolError::Trailing { at } => write!(f, "an instance name has bytes after its last tree, from byte {at}: {rule}"),
            SymbolError::TableFull => write!(f, "the ground table is full decoding an instance name: {rule}"),
        }
    }
}

impl core::error::Error for SymbolError {}

/// An item's identity for keys that must not move when its text does (the symbol's half of a
/// key; a body's edit is in the body digest, code_records.rs).
pub fn item_identity(item: ItemKey) -> ContentId {
    ContentId::of(ITEM_DOMAIN, &item.0.to_le_bytes())
}

/// Appends the instance name of `item` at `args` (a list of `ground`) to `out`.
pub fn encode_instance(item: ItemKey, args: List, ground: PTys<'_>, scratch: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), SymbolError> {
    out.extend_from_slice(&item.0.to_le_bytes());
    let top = ground.list(args).map_err(|_| SymbolError::NotGround { what: "an argument list past the ground table" })?;
    encode_trees(top, ground, scratch, out)
}

/// Appends the count of `top` and each one's tree.
fn encode_trees(top: &[PTy], ground: PTys<'_>, scratch: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), SymbolError> {
    out.extend_from_slice(&(top.len() as u32).to_le_bytes());
    // Pre-order, children in order: a stack of what is left to write.
    let mut left: AVec<'_, PTy> = Vec::new_in(scratch);
    left.extend(top.iter().rev().copied());
    let mut children: AVec<'_, PTy> = Vec::new_in(scratch);
    while let Some(t) = left.pop() {
        let node = ground.node(t).map_err(|_| SymbolError::NotGround { what: "a type past the ground table" })?;
        children.clear();
        each_child(ground, node, &mut children).map_err(|_| SymbolError::NotGround { what: "a type's list past the ground table" })?;
        let key = |out: &mut AVec<'_, u8>, k: ItemKey| out.extend_from_slice(&k.0.to_le_bytes());
        match node {
            PNode::Bool => out.push(0),
            PNode::Char => out.push(1),
            PNode::Int(i) => out.extend_from_slice(&[2, i as u8]),
            PNode::Uint(u) => out.extend_from_slice(&[3, u as u8]),
            PNode::Float(x) => out.extend_from_slice(&[4, x as u8]),
            PNode::Str => out.push(5),
            PNode::Never => out.push(6),
            PNode::Tuple(_) => out.push(7),
            PNode::Ref(m, _) => out.extend_from_slice(&[8, m as u8]),
            PNode::Ptr(m, _) => out.extend_from_slice(&[9, m as u8]),
            PNode::Array(..) => out.push(10),
            PNode::Slice(_) => out.push(11),
            PNode::Adt(k, _) => {
                out.push(12);
                key(out, k);
            }
            PNode::FnDef(k, _) => {
                out.push(13);
                key(out, k);
            }
            PNode::ConstValue { bits, .. } => {
                out.push(14);
                out.extend_from_slice(&bits.to_le_bytes());
            }
            PNode::Pat(..) => out.push(15),
            PNode::Dynamic(_) => out.push(16),
            PNode::ExTrait(k, _) => {
                out.push(17);
                key(out, k);
            }
            PNode::ExProj { trait_, name, .. } => {
                out.push(18);
                key(out, trait_);
                out.extend_from_slice(&name.0.to_le_bytes());
            }
            PNode::ExAuto(k) => {
                out.push(19);
                key(out, k);
            }
            PNode::Closure { owner, ordinal, .. } => {
                out.push(20);
                key(out, owner);
                out.extend_from_slice(&ordinal.to_le_bytes());
            }
            PNode::FnPtr(_, header) => out.extend_from_slice(&[21, header.bits()]),
            PNode::Future(..) => return Err(SymbolError::NotGround { what: "an async block's future named by its expression" }),
            // A future of another body by identity (`PNode::Coroutine`): its own tag, its
            // owner and ordinal, as a closure's; its children the owner's arguments.
            PNode::Coroutine { owner, ordinal, .. } => {
                out.push(23);
                key(out, owner);
                out.extend_from_slice(&ordinal.to_le_bytes());
            }
            PNode::Param(_) => return Err(SymbolError::NotGround { what: "a generic parameter" }),
            PNode::ConstParam(_) => return Err(SymbolError::NotGround { what: "a const generic parameter" }),
            PNode::Alias { .. } => return Err(SymbolError::NotGround { what: "a projection not normalized" }),
            PNode::Opaque(..) => return Err(SymbolError::NotGround { what: "an opaque type" }),
            PNode::Infer => return Err(SymbolError::NotGround { what: "a hole" }),
            PNode::Error => return Err(SymbolError::NotGround { what: "an error type" }),
        }
        out.extend_from_slice(&(children.len() as u32).to_le_bytes());
        left.extend(children.iter().rev().copied());
    }
    Ok(())
}

/// A reader of a name's bytes.
struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], SymbolError> {
        let end = self.at.checked_add(N).filter(|&end| end <= self.bytes.len()).ok_or(SymbolError::Truncated { at: self.at })?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.at..end]);
        self.at = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, SymbolError> {
        Ok(self.take::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, SymbolError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, SymbolError> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn u128(&mut self) -> Result<u128, SymbolError> {
        Ok(u128::from_le_bytes(self.take()?))
    }
    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }
    /// A count read here, refused when the bytes left cannot hold that many nodes.
    fn count(&mut self) -> Result<u32, SymbolError> {
        let at = self.at;
        let n = self.u32()?;
        if n as usize > self.left() / LEAST_NODE {
            return Err(SymbolError::Truncated { at });
        }
        Ok(n)
    }
}

/// A node read whose children are not all in yet.
#[derive(Clone, Copy)]
struct Open {
    tag: u8,
    /// A key, or a const value's bits, or a scalar's index byte.
    wide: u128,
    /// A projection's name.
    name: u64,
    /// A closure's or a future's ordinal.
    ordinal: u32,
    want: u32,
    /// Where its children start in the finished stack.
    base: usize,
}

/// The instance `name` names, its arguments interned in `ground`: the inverse of
/// [`encode_instance`]. Every count is bounded by the bytes left, and no recursion: a stack of
/// open nodes, each interned when its last child is.
pub fn decode_instance(name: &[u8], ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<(ItemKey, List), SymbolError> {
    let mut r = Reader { bytes: name, at: 0 };
    let item = ItemKey(r.u128()?);
    let top = r.count()? as usize;
    let mut done: AVec<'_, PTy> = Vec::new_in(scratch);
    let mut open: AVec<'_, Open> = Vec::new_in(scratch);
    loop {
        while let Some(&o) = open.last() {
            if done.len() - o.base < o.want as usize {
                break;
            }
            open.pop();
            let t = intern_node(o, &done[o.base..], ground)?;
            done.truncate(o.base);
            done.push(t);
        }
        if open.is_empty() && done.len() == top {
            break;
        }
        let at = r.at;
        let tag = r.u8()?;
        let (mut wide, mut name_part, mut ordinal) = (0u128, 0u64, 0u32);
        match tag {
            0 | 1 | 5 | 6 | 7 | 10 | 11 | 15 | 16 => {}
            2 | 3 | 4 | 8 | 9 | 21 => wide = u128::from(r.u8()?),
            12 | 13 | 14 | 17 | 19 => wide = r.u128()?,
            18 => {
                wide = r.u128()?;
                name_part = r.u64()?;
            }
            20 | 23 => {
                wide = r.u128()?;
                ordinal = r.u32()?;
            }
            _ => return Err(SymbolError::UnknownTag { tag, at }),
        }
        let want = r.count()?;
        open.push(Open { tag, wide, name: name_part, ordinal, want, base: done.len() });
    }
    if r.at != name.len() {
        return Err(SymbolError::Trailing { at: r.at });
    }
    let list = ground.intern_list(&done).map_err(|_| SymbolError::TableFull)?;
    Ok((item, list))
}

/// The node `o` with its children `kids`, interned.
fn intern_node(o: Open, kids: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, SymbolError> {
    let got = kids.len() as u32;
    let exactly = |n: u32| if got == n { Ok(()) } else { Err(SymbolError::Children { tag: o.tag, got }) };
    let at_least = |n: u32| if got >= n { Ok(()) } else { Err(SymbolError::Children { tag: o.tag, got }) };
    let byte = |what: &'static str| SymbolError::BadByte { what };
    let index = o.wide as usize;
    let mutbl = || MUTBLS.get(index).copied().ok_or(byte("mutability"));
    let node = match o.tag {
        0 => exactly(0).map(|()| PNode::Bool)?,
        1 => exactly(0).map(|()| PNode::Char)?,
        2 => {
            exactly(0)?;
            PNode::Int(INTS.get(index).copied().ok_or(byte("signed integer type"))?)
        }
        3 => {
            exactly(0)?;
            PNode::Uint(UINTS.get(index).copied().ok_or(byte("unsigned integer type"))?)
        }
        4 => {
            exactly(0)?;
            PNode::Float(FLOATS.get(index).copied().ok_or(byte("float type"))?)
        }
        5 => exactly(0).map(|()| PNode::Str)?,
        6 => exactly(0).map(|()| PNode::Never)?,
        7 => PNode::Tuple(list(ground, kids)?),
        8 => {
            exactly(1)?;
            PNode::Ref(mutbl()?, kids[0])
        }
        9 => {
            exactly(1)?;
            PNode::Ptr(mutbl()?, kids[0])
        }
        10 => {
            exactly(2)?;
            PNode::Array(kids[0], kids[1])
        }
        11 => {
            exactly(1)?;
            PNode::Slice(kids[0])
        }
        12 => PNode::Adt(ItemKey(o.wide), list(ground, kids)?),
        13 => PNode::FnDef(ItemKey(o.wide), list(ground, kids)?),
        14 => {
            exactly(1)?;
            PNode::ConstValue { ty: kids[0], bits: o.wide }
        }
        15 => {
            at_least(1)?;
            PNode::Pat(kids[0], list(ground, &kids[1..])?)
        }
        16 => PNode::Dynamic(list(ground, kids)?),
        17 => PNode::ExTrait(ItemKey(o.wide), list(ground, kids)?),
        18 => {
            at_least(1)?;
            let (args, term) = kids.split_at(kids.len() - 1);
            PNode::ExProj { trait_: ItemKey(o.wide), name: Name(o.name), args: list(ground, args)?, term: term[0] }
        }
        19 => exactly(0).map(|()| PNode::ExAuto(ItemKey(o.wide)))?,
        20 => PNode::Closure { owner: ItemKey(o.wide), ordinal: o.ordinal, args: list(ground, kids)? },
        21 => {
            let header = u8::try_from(o.wide).ok().and_then(FnPtrHeader::from_bits).ok_or(byte("fn pointer header"))?;
            PNode::FnPtr(list(ground, kids)?, header)
        }
        23 => PNode::Coroutine { owner: ItemKey(o.wide), ordinal: o.ordinal, args: list(ground, kids)? },
        tag => return Err(SymbolError::UnknownTag { tag, at: 0 }),
    };
    ground.intern(node).map_err(|_| SymbolError::TableFull)
}

fn list(ground: &mut PortableTys<'_>, kids: &[PTy]) -> Result<List, SymbolError> {
    ground.intern_list(kids).map_err(|_| SymbolError::TableFull)
}

/// The symbol of the function instance whose name is `name` ([`encode_instance`]'s bytes).
pub fn fn_symbol_of_name(name: &[u8]) -> Symbol {
    Symbol(ContentId::of(FN_DOMAIN, name).0)
}

/// The symbol of the function instance `item` at `args` (a list of `ground`).
pub fn fn_symbol(item: ItemKey, args: List, ground: PTys<'_>, scratch: Alloc<'_>) -> Result<Symbol, SymbolError> {
    let mut name: AVec<'_, u8> = Vec::new_in(scratch);
    encode_instance(item, args, ground, scratch, &mut name)?;
    Ok(fn_symbol_of_name(&name))
}

/// The symbol of the `static` item `item`.
pub fn static_symbol(item: ItemKey) -> Symbol {
    Symbol(ContentId::of(STATIC_DOMAIN, &item.0.to_le_bytes()).0)
}

/// The symbol of the `#[thread_local]` static `item`.
pub fn tls_symbol(item: ItemKey) -> Symbol {
    Symbol(ContentId::of(TLS_DOMAIN, &item.0.to_le_bytes()).0)
}

/// The symbol of the vtable `table`: its principal's key, then the trees of its concrete type
/// and of the trait's arguments after `Self`.
pub fn vtable_symbol(table: VtableRef, ground: PTys<'_>, scratch: Alloc<'_>) -> Result<Symbol, SymbolError> {
    let mut trees: AVec<'_, u8> = Vec::new_in(scratch);
    vtable_trees(table, ground, scratch, &mut trees)?;
    Ok(vtable_symbol_of_parts(table.principal, &trees, scratch))
}

/// The trees a vtable's symbol and its declared row hold after the principal: its concrete
/// type's, then the trait's arguments' after `Self`, appended to `out`.
pub fn vtable_trees(table: VtableRef, ground: PTys<'_>, scratch: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), SymbolError> {
    encode_trees(&[table.concrete], ground, scratch, out)?;
    let args = ground.list(table.args).map_err(|_| SymbolError::NotGround { what: "a vtable's argument list past the ground table" })?;
    encode_trees(args, ground, scratch, out)
}

/// The symbol of the vtable of `principal` whose trees are `trees` ([`vtable_trees`]): what a
/// declared `Vtable` row (blaze-tier record/declared.rs) holds, hashed as [`vtable_symbol`] does.
pub fn vtable_symbol_of_parts(principal: ItemKey, trees: &[u8], scratch: Alloc<'_>) -> Symbol {
    let mut name: AVec<'_, u8> = Vec::with_capacity_in(16 + trees.len(), scratch);
    name.extend_from_slice(&principal.0.to_le_bytes());
    name.extend_from_slice(trees);
    Symbol(ContentId::of(VTABLE_DOMAIN, &name).0)
}

/// The program's thread-local descriptor static: one a program.
pub fn tls_desc_symbol() -> Symbol {
    Symbol(ContentId::of(TLS_DESC_DOMAIN, &[]).0)
}

/// The program's thread-local getter function: one a program.
pub fn tls_getter_symbol() -> Symbol {
    Symbol(ContentId::of(TLS_GETTER_DOMAIN, &[]).0)
}
