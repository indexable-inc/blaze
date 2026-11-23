//! Symbols: the stable names code records give what they reference (docs/v2/plan/design/run.md,
//! "End state", "The code record"; pieces 3 and 7).
//!
//! A FUNCTION INSTANCE's symbol is the hash of its INSTANCE NAME: the canonical bytes of its
//! identity, the item's key and its ground arguments written as ONE SHARED FOREST
//! ([`encode_instance`]): each distinct type node once, its children by reference to nodes
//! written before it. Never an address, an index of this run (a slot, a ground-table id, a fn's
//! place in the program) or a path: two builds of one program give equal symbols, whatever else
//! their ground tables hold and whatever order they met the instances in. The name decodes back
//! into any ground table ([`decode_instance`]), so whoever is asked for a symbol's code (the
//! run's parent, on a stub's first call) can make the instance from the name a record declared
//! it by.
//!
//! WHY A FOREST, NOT TREES (form 2; form 1 wrote each argument as a tree, pre-order, with no
//! sharing): a type is a DAG, and its tree can be exponentially larger. `fn f<T>(t: T) {
//! g((t, t)) }` chains double the tree at each level; a closure's type holds its owner's
//! arguments, so a closure inside an iterator chain inside a generic fn repeats the caller's
//! arguments once per closure, at every level. The ground table the maker holds is interned
//! (blaze-build instance.rs `map_tree` maps by index: linear in DISTINCT nodes), so only the name
//! blew up: `blaze test -p tempo-hardfork` (2026-11-21 06:27) made one name of over 134 million
//! item keys and stopped in the allocator. Here a name is linear in its distinct nodes, and
//! [`NAME_MOST_NODES`] counts distinct nodes, as rustc's type-length walker does.
//!
//! The instance name's form, little-endian throughout:
//!
//! - the item's key, 16 bytes; the count `A` of arguments, 4 bytes; with `A == 0` nothing
//!   follows (a name at no argument is 20 bytes);
//! - else the count `N` of distinct nodes, 4 bytes; the `N` nodes, each a tag byte, the tag's
//!   fixed payload, its count of children `c` (4 bytes) and `c` child references (4 bytes each:
//!   the index of a node written BEFORE it, in [`blaze_build::instance::each_child`]'s order);
//! - the `A` arguments, each a reference (4 bytes) to one of the `N` nodes.
//!
//! CANONICAL by construction: the nodes are written in the post-order of their first visit by a
//! walk of the arguments left to right, children in order, and a node equal IN CONTENT to one
//! already written (its tag, payload and child references) is that node, whatever its ground
//! table id. So the bytes are a function of the arguments' structure alone (rule 25).
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
//! | 23 | another body's future (`PNode::Coroutine`) | the owner's key (16), its ordinal (4) |
//! | 24 | extern type (`PNode::Foreign`) | the item's key (16) |
//!
//! A type with no ground form (a parameter, an alias, an opaque type, a hole, an error) is
//! refused by name ([`SymbolError::NotGround`]).
//!
//! STATICS, THREAD-LOCALS and VTABLES the same way: a static's and a thread-local's symbol is
//! its item's key under its own domain; a vtable's is its principal trait's key and two forests,
//! of its concrete type and of the trait's arguments. The thread-local descriptor and getter are one
//! each a program: fixed symbols. C names stay text (`Named::C`): the record keeps the name and
//! `blaze_code::record::Symbol::of_name` is its one symbol.

use core::fmt;

use blaze_build::instance::each_child;
use blaze_build::vtable::VtableRef;
use blaze_code::record::Symbol;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::{FloatTy, FnPtrHeader, IntTy, ItemKey, List, Mutbl, Name, PNode, PTy, PTys, PortableTys, UintTy};

// Form 2 (the shared forest): a symbol of form 1 (trees) is never equal to one of form 2, so a
// record of an old home is never read as a new one's (rule 26: reported and rebuilt).
const FN_DOMAIN: &str = "blaze-ladder fn instance symbol 2";
const STATIC_DOMAIN: &str = "blaze-ladder static symbol 1";
const TLS_DOMAIN: &str = "blaze-ladder thread-local symbol 1";
const VTABLE_DOMAIN: &str = "blaze-ladder vtable symbol 2";
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

/// The bytes of one reference (a child's, an argument's): a node's index in its name.
const REF: usize = 4;

/// The most DISTINCT nodes one forest of a name holds (rule 18): 2^24, rustc's default
/// `type_length_limit` (rustc_interface `limits.rs`: `2usize.pow(24)`), which rustc counts with
/// its type walker, visiting each distinct argument once. A name holds each distinct node once,
/// so no instance under rustc's default is refused here for size; past it, the maker's ground
/// table alone would hold 16.7 million types for one instance. (Form 1 wrote trees: there the
/// same number refused types whose DAG is small; tempo-hardfork's name passed it eightfold.)
pub const NAME_MOST_NODES: u32 = 1 << 24;

/// The most bytes one forest of a name holds (rule 18): 256 MiB. A node is at least 5 bytes, so
/// [`NAME_MOST_NODES`] nodes of few children fit; a name is held about five times while it is
/// declared and walked (the made entry, the declared list, the walk's arena, the step's copy),
/// so one name past this would cost gigabytes before anything is built.
pub const NAME_MOST_BYTES: usize = 1 << 28;

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
    /// A forest past its cap (rule 18): `nodes` distinct nodes and `bytes` bytes reached when
    /// it stopped, against [`NAME_MOST_NODES`] and [`NAME_MOST_BYTES`].
    TooLarge { nodes: u32, bytes: usize },
    /// A reference, at byte `at`, to a node not written before it.
    Ref { at: usize },
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
            SymbolError::TooLarge { nodes, bytes } => write!(
                f,
                "an instance name reached {nodes} distinct type nodes and {bytes} bytes, past its cap of {NAME_MOST_NODES} nodes (rustc's default `type_length_limit`) or {NAME_MOST_BYTES} bytes (rule 18): {rule}"
            ),
            SymbolError::Ref { at } => write!(f, "an instance name refers, at byte {at}, to a node not written before it: {rule}"),
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
    encode_forest(top, ground, scratch, out)
}

/// Appends the forest of `top` (the module doc's form): its count, and when it is not empty,
/// the count of distinct nodes, the nodes, and a reference per type of `top`. Linear in the
/// distinct nodes `top` reaches; at most [`NAME_MOST_NODES`] of them and [`NAME_MOST_BYTES`].
fn encode_forest(top: &[PTy], ground: PTys<'_>, scratch: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), SymbolError> {
    out.extend_from_slice(&(top.len() as u32).to_le_bytes());
    if top.is_empty() {
        return Ok(());
    }
    let count_at = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let base = out.len();
    let mut forest = Forest::new_in(scratch);
    let mut refs: AVec<'_, u32> = Vec::with_capacity_in(top.len(), scratch);
    for &t in top {
        refs.push(forest.node_of(t, ground, out, base)?);
    }
    let count = forest.spans.len() as u32;
    out[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
    for r in refs.iter() {
        out.extend_from_slice(&r.to_le_bytes());
    }
    Ok(())
}

/// One open node of [`Forest::node_of`]'s walk: its type, its node, where its children are in
/// `kids` and where their references are in `done`.
#[derive(Clone, Copy)]
struct Frame {
    ty: PTy,
    node: PNode,
    kids_at: u32,
    kids_len: u32,
    done_at: u32,
}

/// The writer of one forest: the nodes written, found by content and by ground type, and the
/// walk's stacks. Every table is in the caller's scratch allocator, and none outlives the call.
struct Forest<'s> {
    /// Each node's (start, length), relative to the forest's first node.
    spans: AVec<'s, (u32, u32)>,
    /// Open addressing over `spans` by the hash of a node's bytes: 0 is empty, else the node + 1.
    by_bytes: AVec<'s, u32>,
    /// (ground type, node) rows: a type met again is its node with no walk.
    memo: AVec<'s, (PTy, u32)>,
    /// Open addressing over `memo` by the type: 0 is empty, else the row + 1.
    by_ty: AVec<'s, u32>,
    /// The bytes of the node being written, before it is known new.
    node: AVec<'s, u8>,
    frames: AVec<'s, Frame>,
    kids: AVec<'s, PTy>,
    done: AVec<'s, u32>,
}

/// FNV-1a over `bytes`: the content table's hash. Fixed, seedless: it decides only where a node
/// is looked for, never what is written.
fn bytes_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The memo's hash of a ground type's index.
fn ty_hash(t: PTy) -> u64 {
    u64::from(t.0).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// The slot `hash` starts at in a table of `len` (a power of two).
fn slot_of(hash: u64, len: usize) -> usize {
    ((hash ^ (hash >> 31)) as usize) & (len - 1)
}

/// The row of `slots` whose hash is `hash` and that `is` accepts, or the empty slot it would
/// take. `slots` is not empty and at most half full, so the probe ends.
fn probe(slots: &[u32], hash: u64, mut is: impl FnMut(usize) -> bool) -> Result<usize, usize> {
    let mask = slots.len() - 1;
    let mut at = slot_of(hash, slots.len());
    loop {
        match slots[at] {
            0 => return Err(at),
            n if is(n as usize - 1) => return Ok(n as usize - 1),
            _ => at = (at + 1) & mask,
        }
    }
}

/// `slots` made long enough (16 at first, doubled) for `rows + 1` rows at most half full, every
/// row filed again by `hash_of` when it grows.
fn room(slots: &mut AVec<'_, u32>, rows: usize, hash_of: impl Fn(usize) -> u64) {
    if (rows + 1) * 2 <= slots.len() {
        return;
    }
    let mut len = slots.len().max(16);
    while (rows + 1) * 2 > len {
        len *= 2;
    }
    slots.clear();
    slots.resize(len, 0);
    let mask = len - 1;
    for row in 0..rows {
        let mut at = slot_of(hash_of(row), len);
        while slots[at] != 0 {
            at = (at + 1) & mask;
        }
        slots[at] = row as u32 + 1;
    }
}

impl<'s> Forest<'s> {
    fn new_in(scratch: Alloc<'s>) -> Forest<'s> {
        Forest {
            spans: Vec::new_in(scratch),
            by_bytes: Vec::new_in(scratch),
            memo: Vec::new_in(scratch),
            by_ty: Vec::new_in(scratch),
            node: Vec::new_in(scratch),
            frames: Vec::new_in(scratch),
            kids: Vec::new_in(scratch),
            done: Vec::new_in(scratch),
        }
    }

    /// The node `t` was written as, if it was.
    fn memo_get(&self, t: PTy) -> Option<u32> {
        if self.by_ty.is_empty() {
            return None;
        }
        let memo = &self.memo;
        probe(&self.by_ty, ty_hash(t), |row| memo[row].0 == t).ok().map(|row| memo[row].1)
    }

    fn memo_put(&mut self, t: PTy, node: u32) {
        let Forest { memo, by_ty, .. } = self;
        room(by_ty, memo.len(), |row| ty_hash(memo[row].0));
        if let Err(at) = probe(by_ty, ty_hash(t), |row| memo[row].0 == t) {
            by_ty[at] = memo.len() as u32 + 1;
            memo.push((t, node));
        }
    }

    /// The node of `t`: written once, after its children (post-order, children in order), or
    /// the node already written for it or for a type equal to it in content. By a stack of
    /// open nodes, never by recursion.
    fn node_of(&mut self, t: PTy, ground: PTys<'_>, out: &mut AVec<'_, u8>, base: usize) -> Result<u32, SymbolError> {
        if let Some(node) = self.memo_get(t) {
            return Ok(node);
        }
        self.open(t, ground)?;
        let mut last = 0;
        while let Some(&f) = self.frames.last() {
            let have = self.done.len() - f.done_at as usize;
            if have < f.kids_len as usize {
                let child = self.kids[f.kids_at as usize + have];
                match self.memo_get(child) {
                    Some(node) => self.done.push(node),
                    None => self.open(child, ground)?,
                }
                continue;
            }
            // Every child is written: this node, by its head and its children's references.
            self.node.clear();
            head(f.node, &mut self.node)?;
            self.node.extend_from_slice(&f.kids_len.to_le_bytes());
            for i in f.done_at as usize..self.done.len() {
                let r = self.done[i];
                self.node.extend_from_slice(&r.to_le_bytes());
            }
            let node = self.write(out, base)?;
            self.memo_put(f.ty, node);
            self.kids.truncate(f.kids_at as usize);
            self.done.truncate(f.done_at as usize);
            self.frames.pop();
            if !self.frames.is_empty() {
                self.done.push(node);
            }
            last = node;
        }
        Ok(last)
    }

    /// `t` opened: its node read and checked to have a ground form, its children stacked.
    fn open(&mut self, t: PTy, ground: PTys<'_>) -> Result<(), SymbolError> {
        let node = ground.node(t).map_err(|_| SymbolError::NotGround { what: "a type past the ground table" })?;
        // Refused here, before any child is walked, when it has no ground form.
        self.node.clear();
        head(node, &mut self.node)?;
        let kids_at = self.kids.len();
        each_child(ground, node, &mut self.kids).map_err(|_| SymbolError::NotGround { what: "a type's list past the ground table" })?;
        let kids_len = self.kids.len() - kids_at;
        self.frames.push(Frame { ty: t, node, kids_at: kids_at as u32, kids_len: kids_len as u32, done_at: self.done.len() as u32 });
        Ok(())
    }

    /// The node whose bytes are `self.node`: the one already written with these bytes, or a
    /// new one appended to `out` (its nodes start at `base`), within the caps.
    fn write(&mut self, out: &mut AVec<'_, u8>, base: usize) -> Result<u32, SymbolError> {
        let Forest { spans, by_bytes, node, .. } = self;
        let found = {
            let written: &[u8] = &out[base..];
            let rows: &[(u32, u32)] = spans.as_slice();
            let at = move |row: usize| {
                let (start, len) = rows[row];
                &written[start as usize..(start + len) as usize]
            };
            room(by_bytes, rows.len(), |row| bytes_hash(at(row)));
            probe(by_bytes, bytes_hash(node.as_slice()), |row| at(row) == node.as_slice())
        };
        match found {
            Ok(row) => Ok(row as u32),
            Err(slot) => {
                let row = spans.len();
                let start = out.len() - base;
                if row as u32 >= NAME_MOST_NODES || start + node.len() > NAME_MOST_BYTES {
                    return Err(SymbolError::TooLarge { nodes: row as u32 + 1, bytes: start + node.len() });
                }
                out.extend_from_slice(node.as_slice());
                spans.push((start as u32, node.len() as u32));
                by_bytes[slot] = row as u32 + 1;
                Ok(row as u32)
            }
        }
    }
}

/// Appends `node`'s tag and payload (the module doc's table), or refuses a type with no ground
/// form by name.
fn head(node: PNode, out: &mut AVec<'_, u8>) -> Result<(), SymbolError> {
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
        PNode::Record(s, _) => {
            out.push(25);
            out.extend_from_slice(&s.0.to_le_bytes());
        }
        PNode::Union(_) => out.push(26),
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
        // A future of another body by identity (`PNode::Coroutine`): its own tag, its owner and
        // ordinal, as a closure's; its children the owner's arguments.
        PNode::Coroutine { owner, ordinal, .. } => {
            out.push(23);
            key(out, owner);
            out.extend_from_slice(&ordinal.to_le_bytes());
        }
        // An extern type: its item's key, no children (a tag no earlier name held, so no
        // symbol made before it moves).
        PNode::Foreign(k) => {
            out.push(24);
            key(out, k);
        }
        PNode::Param(_) => return Err(SymbolError::NotGround { what: "a generic parameter" }),
        PNode::ConstParam(_) => return Err(SymbolError::NotGround { what: "a const generic parameter" }),
        PNode::Alias { .. } => return Err(SymbolError::NotGround { what: "a projection not normalized" }),
        PNode::Opaque(..) => return Err(SymbolError::NotGround { what: "an opaque type" }),
        PNode::Infer => return Err(SymbolError::NotGround { what: "a hole" }),
        PNode::Error => return Err(SymbolError::NotGround { what: "an error type" }),
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
    /// A count read here, refused when the bytes left cannot hold that many things of at least
    /// `least` bytes each.
    fn count(&mut self, least: usize) -> Result<u32, SymbolError> {
        let at = self.at;
        let n = self.u32()?;
        if n as usize > self.left() / least {
            return Err(SymbolError::Truncated { at });
        }
        Ok(n)
    }
}

/// A node's head as read: its tag and its payload.
#[derive(Clone, Copy)]
struct Open {
    tag: u8,
    /// A key, or a const value's bits, or a scalar's index byte.
    wide: u128,
    /// A projection's name.
    name: u64,
    /// A closure's or a future's ordinal.
    ordinal: u32,
}

/// The instance `name` names, its arguments interned in `ground`: the inverse of
/// [`encode_instance`]. Every count is bounded by the bytes left; each node is interned once,
/// its children already interned (each reference is to a node before it), so no stack and no
/// recursion.
pub fn decode_instance(name: &[u8], ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<(ItemKey, List), SymbolError> {
    let mut r = Reader { bytes: name, at: 0 };
    let item = ItemKey(r.u128()?);
    let list = decode_forest(&mut r, ground, scratch)?;
    if r.at != name.len() {
        return Err(SymbolError::Trailing { at: r.at });
    }
    Ok((item, list))
}

/// One forest (the module doc's form) read at `r` and interned in `ground`: the list of its
/// arguments.
fn decode_forest(r: &mut Reader<'_>, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<List, SymbolError> {
    let top = r.count(REF)? as usize;
    if top == 0 {
        return ground.intern_list(&[]).map_err(|_| SymbolError::TableFull);
    }
    let count = r.count(LEAST_NODE)? as usize;
    let mut nodes: AVec<'_, PTy> = Vec::with_capacity_in(count, scratch);
    let mut kids: AVec<'_, PTy> = Vec::new_in(scratch);
    for _ in 0..count {
        let at = r.at;
        let tag = r.u8()?;
        let (mut wide, mut name_part, mut ordinal) = (0u128, 0u64, 0u32);
        match tag {
            0 | 1 | 5 | 6 | 7 | 10 | 11 | 15 | 16 | 26 => {}
            2 | 3 | 4 | 8 | 9 | 21 => wide = u128::from(r.u8()?),
            12 | 13 | 14 | 17 | 19 | 24 => wide = r.u128()?,
            18 => {
                wide = r.u128()?;
                name_part = r.u64()?;
            }
            20 | 23 => {
                wide = r.u128()?;
                ordinal = r.u32()?;
            }
            25 => name_part = r.u64()?,
            _ => return Err(SymbolError::UnknownTag { tag, at }),
        }
        let want = r.count(REF)?;
        kids.clear();
        for _ in 0..want {
            let ref_at = r.at;
            let k = r.u32()? as usize;
            kids.push(*nodes.get(k).ok_or(SymbolError::Ref { at: ref_at })?);
        }
        let t = intern_node(Open { tag, wide, name: name_part, ordinal }, &kids, ground)?;
        nodes.push(t);
    }
    let mut args: AVec<'_, PTy> = Vec::with_capacity_in(top, scratch);
    for _ in 0..top {
        let ref_at = r.at;
        let k = r.u32()? as usize;
        args.push(*nodes.get(k).ok_or(SymbolError::Ref { at: ref_at })?);
    }
    ground.intern_list(&args).map_err(|_| SymbolError::TableFull)
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
        24 => exactly(0).map(|()| PNode::Foreign(ItemKey(o.wide)))?,
        25 => PNode::Record(blaze_typeck::RecordSchema(o.name), list(ground, kids)?),
        26 => PNode::Union(list(ground, kids)?),
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

/// The forests a vtable's symbol and its declared row hold after the principal: its concrete
/// type's, then the trait's arguments' after `Self`, appended to `out` (the module doc's form,
/// each forest on its own; nobody decodes them: they are hashed and stored).
pub fn vtable_trees(table: VtableRef, ground: PTys<'_>, scratch: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), SymbolError> {
    encode_forest(&[table.concrete], ground, scratch, out)?;
    let args = ground.list(table.args).map_err(|_| SymbolError::NotGround { what: "a vtable's argument list past the ground table" })?;
    encode_forest(args, ground, scratch, out)
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

#[cfg(test)]
mod tests {
    //! The shared forest (form 2): a name is linear in its DISTINCT types, canonical across
    //! ground tables, and decodes back to equal bytes. Each claim with its control.

    use super::*;

    const F: ItemKey = ItemKey(0xF001);
    const OWNER: ItemKey = ItemKey(0xF0AA);

    /// The nodes of `t`'s TREE (form 1 wrote each), counted over the DAG with a memo: the
    /// control the forest is measured against.
    fn tree_nodes(ground: PTys<'_>, t: PTy, memo: &mut std::vec::Vec<Option<u128>>, scratch: Alloc<'_>) -> u128 {
        if let Some(n) = memo[t.0 as usize] {
            return n;
        }
        let node = ground.node(t).expect("a ground type");
        let mut kids: AVec<'_, PTy> = Vec::new_in(scratch);
        each_child(ground, node, &mut kids).expect("its children");
        let kids: std::vec::Vec<PTy> = kids.iter().copied().collect();
        let n = 1 + kids.iter().map(|&k| tree_nodes(ground, k, memo, scratch)).sum::<u128>();
        memo[t.0 as usize] = Some(n);
        n
    }

    /// The count of distinct nodes a name holds (its forest's `N`), 0 at no argument.
    fn nodes_of(name: &[u8]) -> u32 {
        if name.len() == 20 {
            return 0;
        }
        u32::from_le_bytes([name[20], name[21], name[22], name[23]])
    }

    /// `(t, t)` nested `depth` times over `u8`, interned in `ground`, after `noise` unrelated
    /// types (so two tables give it other ids).
    fn doubling(ground: &mut PortableTys<'_>, depth: u32, noise: usize) -> PTy {
        let others = [PNode::Bool, PNode::Char, PNode::Str, PNode::Never, PNode::Uint(UintTy::U16), PNode::Uint(UintTy::U32), PNode::Int(IntTy::I8)];
        for &other in others.iter().take(noise) {
            ground.intern(other).expect("an unrelated type");
        }
        let mut t = ground.intern(PNode::Uint(UintTy::U8)).expect("u8");
        for _ in 0..depth {
            let pair = ground.intern_list(&[t, t]).expect("a pair");
            t = ground.intern(PNode::Tuple(pair)).expect("a tuple");
        }
        t
    }

    /// (a) A doubling chain `f0..f40` (`fn f<T>(t: T) { f::<(T, T)>((t, t)) }`, cut at 40): the
    /// forest holds 41 nodes and grows by one node a level, its bytes linear; the symbol is
    /// equal from two ground tables that give the types other ids. CONTROL: the same type's
    /// tree (form 1) holds 2^41 - 1 nodes, past [`NAME_MOST_NODES`] 130 thousand times.
    #[test]
    fn a_doubling_chain_is_linear_and_its_tree_is_past_the_cap() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut a = PortableTys::new_in(alloc);
        let mut b = PortableTys::new_in(alloc);
        let mut lens = std::vec::Vec::new();
        let mut name_a: AVec<'_, u8> = Vec::new_in(alloc);
        for depth in [10, 20, 40] {
            let t = doubling(&mut a, depth, 0);
            let args = a.intern_list(&[t]).expect("args");
            name_a.clear();
            encode_instance(F, args, a.view(), alloc, &mut name_a).expect("a linear name");
            assert_eq!(nodes_of(&name_a), depth + 1, "one node a level and the u8");
            lens.push(name_a.len());
        }
        // Linear: each level adds one tuple node of two references (1 + 4 + 8 bytes).
        assert_eq!(lens[2] - lens[1], 20 * 13, "{lens:?}");
        assert_eq!(lens[1] - lens[0], 10 * 13, "{lens:?}");
        let t = doubling(&mut b, 40, 7);
        let args = b.intern_list(&[t]).expect("args");
        let mut name_b: AVec<'_, u8> = Vec::new_in(alloc);
        encode_instance(F, args, b.view(), alloc, &mut name_b).expect("a linear name");
        assert_eq!(name_a.as_slice(), name_b.as_slice(), "canonical: the bytes are the structure's, not the table's ids");
        assert_eq!(fn_symbol_of_name(&name_a), fn_symbol_of_name(&name_b));
        // Control: the tree form of the same type.
        let mut memo = std::vec![None; b.view().len()];
        let tree = tree_nodes(b.view(), t, &mut memo, alloc);
        assert_eq!(tree, (1u128 << 41) - 1);
        assert!(tree > u128::from(NAME_MOST_NODES), "control: the tree form is past the cap");
        std::println!("names: doubling chain at 40 levels: {} bytes, 41 nodes; its tree {tree} nodes", name_a.len());
    }

    /// (b) A closure instantiated at three types (`fn call<C: Fn()>(c: C)` at the closure of
    /// `owner<T>` for T = u8, u16, (u8, u16)): three names, three symbols, each holding the
    /// owner's argument once though the closure's type and the call's argument both reach it;
    /// a closure nested in a closure of the same owner arguments adds one node, not a copy.
    #[test]
    fn a_closure_at_three_types_names_each_once() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut g = PortableTys::new_in(alloc);
        let u8_ = g.intern(PNode::Uint(UintTy::U8)).expect("u8");
        let u16_ = g.intern(PNode::Uint(UintTy::U16)).expect("u16");
        let pair_l = g.intern_list(&[u8_, u16_]).expect("list");
        let pair = g.intern(PNode::Tuple(pair_l)).expect("tuple");
        let mut symbols = std::vec::Vec::new();
        let mut name: AVec<'_, u8> = Vec::new_in(alloc);
        for (t, distinct) in [(u8_, 2), (u16_, 2), (pair, 4)] {
            let owner_args = g.intern_list(&[t]).expect("owner args");
            let closure = g.intern(PNode::Closure { owner: OWNER, ordinal: 0, args: owner_args }).expect("closure");
            // The call's arguments: the closure, and T again (shared, not copied).
            let args = g.intern_list(&[closure, t]).expect("args");
            name.clear();
            encode_instance(F, args, g.view(), alloc, &mut name).expect("a name");
            assert_eq!(nodes_of(&name), distinct, "T once, the closure once");
            let mut items = std::vec::Vec::new();
            crate::body_ids::each_item(&name, |k| items.push(k)).expect("it lists");
            assert_eq!(items, [F, OWNER], "the fn, then the closure's owner, once");
            symbols.push(fn_symbol_of_name(&name));
        }
        assert!(symbols[0] != symbols[1] && symbols[1] != symbols[2] && symbols[0] != symbols[2], "three instances, three symbols");
        // Nested: a closure whose owner arguments hold the first closure and T.
        let owner_args = g.intern_list(&[pair]).expect("owner args");
        let inner = g.intern(PNode::Closure { owner: OWNER, ordinal: 0, args: owner_args }).expect("inner");
        let outer_args = g.intern_list(&[inner, pair]).expect("outer args");
        let outer = g.intern(PNode::Closure { owner: OWNER, ordinal: 1, args: outer_args }).expect("outer");
        let args = g.intern_list(&[outer, inner, pair]).expect("args");
        name.clear();
        encode_instance(F, args, g.view(), alloc, &mut name).expect("a name");
        assert_eq!(nodes_of(&name), 5, "u8, u16, the pair, inner, outer: each once (and nothing else)");
    }

    /// (d) A round trip: every name above decodes into a fresh table and encodes back to equal
    /// bytes, its symbol and items equal. CONTROLS: a reference to a node not before it, and a
    /// count past the bytes, are typed refusals, never a read past the end.
    #[test]
    fn a_name_round_trips_and_a_bad_reference_is_refused() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut g = PortableTys::new_in(alloc);
        let t = doubling(&mut g, 30, 0);
        let u8_ = g.intern(PNode::Uint(UintTy::U8)).expect("u8");
        let owner_args = g.intern_list(&[t, u8_]).expect("owner args");
        let closure = g.intern(PNode::Closure { owner: OWNER, ordinal: 2, args: owner_args }).expect("closure");
        let r = g.intern(PNode::Ref(Mutbl::Mut, closure)).expect("ref");
        let args = g.intern_list(&[r, t, u8_]).expect("args");
        let mut name: AVec<'_, u8> = Vec::new_in(alloc);
        encode_instance(F, args, g.view(), alloc, &mut name).expect("a name");
        let mut fresh = PortableTys::new_in(alloc);
        let _ = fresh.intern(PNode::Bool).expect("noise first, so ids differ");
        let (item, back) = decode_instance(&name, &mut fresh, alloc).expect("it decodes");
        assert_eq!(item, F);
        let mut again: AVec<'_, u8> = Vec::new_in(alloc);
        encode_instance(item, back, fresh.view(), alloc, &mut again).expect("it encodes again");
        assert_eq!(name.as_slice(), again.as_slice(), "equal bytes");
        assert_eq!(fn_symbol_of_name(&name), fn_symbol_of_name(&again));
        // At no argument: 20 bytes, and back.
        let mut bare: AVec<'_, u8> = Vec::new_in(alloc);
        encode_instance(F, List::EMPTY, g.view(), alloc, &mut bare).expect("a bare name");
        assert_eq!(bare.len(), 20);
        assert_eq!(decode_instance(&bare, &mut fresh, alloc).map(|(k, _)| k), Ok(F));
        // Controls: the last argument's reference past the nodes; a node count past the bytes.
        let last = name.len() - 4;
        let mut bad: std::vec::Vec<u8> = name.iter().copied().collect();
        bad[last..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_instance(&bad, &mut fresh, alloc).map(|(k, _)| k), Err(SymbolError::Ref { at: last }));
        let mut short: std::vec::Vec<u8> = name.iter().copied().collect();
        short[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_instance(&short, &mut fresh, alloc).map(|(k, _)| k), Err(SymbolError::Truncated { at: 20 }));
    }
}
