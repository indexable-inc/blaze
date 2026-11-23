//! Digests: of a question's arguments (its key in a read list) and of an answer (what the read
//! list records of it). Content only, as blaze-typeck's read digest is (world.rs `Hashes`): a
//! type is hashed as the term it reaches, its node's shape then its children's digests, never as
//! its index, so two tables that hold one term in different orders give one digest. Every
//! integer is written little-endian whatever the host (rule 25).

use core::hash::{Hash, Hasher};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_ids::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_typeck::{Adt, FnSig, ItemKey, List, Name, PTy, PTys, StableHasher, node_shape};

use crate::answer::{Answer, Literal, Resolved, Vocab};
use crate::fact::{ConstOf, Fact, Ground, Instance, LitAt, Term};
use crate::reads::FactError;

/// A value a read list may cover: an answer, or a part of one. The back end's own answer types
/// ([`Vocab`]) implement it where they are defined.
pub trait Digest {
    /// Feeds this value's content, and nothing of where it lives, to `d`.
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError>;
}

/// The hasher of one run's questions and answers, with the memo of the terms it has hashed.
/// The memo lives for one run (one `Reads`), so nothing in it outlives the tables it read.
pub struct Digester<'a> {
    h: StableHasher,
    /// A table's identity and a node's index to the digest of the term under it.
    memo: Map<((usize, usize), u32), u64, FixedState, Alloc<'a>>,
    work: Vec<(u32, bool), Alloc<'a>>,
    kids: Vec<PTy, Alloc<'a>>,
}

/// `core::hash::Hasher` over a `StableHasher`, every integer little-endian: what a derived
/// `Hash` of a small typed value (a `PNode`'s shape, an `IntTy`) is fed through.
struct Le(StableHasher);

impl Hasher for Le {
    fn finish(&self) -> u64 {
        self.0.finish()
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0 = self.0.bytes(bytes);
    }
    fn write_u8(&mut self, i: u8) {
        self.write(&[i]);
    }
    fn write_u16(&mut self, i: u16) {
        self.write(&i.to_le_bytes());
    }
    fn write_u32(&mut self, i: u32) {
        self.write(&i.to_le_bytes());
    }
    fn write_u64(&mut self, i: u64) {
        self.write(&i.to_le_bytes());
    }
    fn write_u128(&mut self, i: u128) {
        self.write(&i.to_le_bytes());
    }
    fn write_usize(&mut self, i: usize) {
        self.write(&(i as u64).to_le_bytes());
    }
    fn write_i8(&mut self, i: i8) {
        self.write(&i.to_le_bytes());
    }
    fn write_i16(&mut self, i: i16) {
        self.write(&i.to_le_bytes());
    }
    fn write_i32(&mut self, i: i32) {
        self.write(&i.to_le_bytes());
    }
    fn write_i64(&mut self, i: i64) {
        self.write(&i.to_le_bytes());
    }
    fn write_i128(&mut self, i: i128) {
        self.write(&i.to_le_bytes());
    }
    fn write_isize(&mut self, i: isize) {
        self.write(&(i as i64).to_le_bytes());
    }
}

const ORDER: FactError = FactError::Malformed { what: "a term whose child is not before it in its table" };
const PAST: FactError = FactError::Malformed { what: "a term or a list past its table" };

impl<'a> Digester<'a> {
    /// A digester whose memo and walk live in `alloc`.
    pub fn new_in(alloc: Alloc<'a>) -> Digester<'a> {
        Digester {
            h: StableHasher::new("blaze-facts"),
            memo: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            work: Vec::new_in(alloc),
            kids: Vec::new_in(alloc),
        }
    }

    /// Starts one digest in `domain`.
    fn start(&mut self, domain: &str) {
        self.h = StableHasher::new(domain);
    }

    /// The digest so far.
    fn take(&self) -> u64 {
        self.h.finish()
    }

    pub fn tag(&mut self, v: u8) {
        self.h.tag(v);
    }

    pub fn u32(&mut self, v: u32) {
        self.h.u32(v);
    }

    pub fn u64(&mut self, v: u64) {
        self.h.u64(v);
    }

    pub fn u128(&mut self, v: u128) {
        self.h.u128(v);
    }

    pub fn i128(&mut self, v: i128) {
        self.h.u128(v as u128);
    }

    pub fn bool(&mut self, v: bool) {
        self.h.tag(u8::from(v));
    }

    /// Bytes, after their length.
    pub fn bytes(&mut self, v: &[u8]) {
        self.h.u64(v.len() as u64);
        self.h = self.h.bytes(v);
    }

    pub fn item(&mut self, k: ItemKey) {
        self.h.u128(k.0);
    }

    pub fn name(&mut self, n: Name) {
        self.h.u64(n.0);
    }

    /// A small typed value by its derived `Hash` (an enum of kinds, a header): its shape only,
    /// never a handle into a table.
    pub fn hashed<T: Hash>(&mut self, v: &T) {
        let mut le = Le(self.h);
        v.hash(&mut le);
        self.h = le.0;
    }

    /// The term `ty` of `types`, by content.
    pub fn term(&mut self, types: PTys<'_>, ty: PTy) -> Result<(), FactError> {
        let d = self.term_digest(types, ty)?;
        self.h.u64(d);
        Ok(())
    }

    /// Terms of `types`, after their count.
    pub fn terms(&mut self, types: PTys<'_>, tys: &[PTy]) -> Result<(), FactError> {
        self.h.u64(tys.len() as u64);
        for &t in tys {
            self.term(types, t)?;
        }
        Ok(())
    }

    /// The terms of list `l` of `types`, after their count.
    pub fn list(&mut self, types: PTys<'_>, l: List) -> Result<(), FactError> {
        self.h.u64(u64::from(l.len));
        for i in l.range() {
            let t = types.list_slot(i).ok_or(PAST)?;
            self.term(types, t)?;
        }
        Ok(())
    }

    /// The digest of `root` of `t`: its node's shape, then its children's digests in order;
    /// each node hashed once per run, children first, by a work list. A child must come
    /// before its parent in the table, so the walk ends.
    fn term_digest(&mut self, t: PTys<'_>, root: PTy) -> Result<u64, FactError> {
        let tab = t.identity();
        if let Some(&d) = self.memo.get(&(tab, root.0)) {
            return Ok(d);
        }
        self.work.clear();
        self.work.push((root.0, false));
        while let Some((n, expanded)) = self.work.pop() {
            if self.memo.contains_key(&(tab, n)) {
                continue;
            }
            let node = t.node(PTy(n)).map_err(|_| PAST)?;
            self.kids.clear();
            t.children_of(node, &mut self.kids).map_err(|_| PAST)?;
            if !expanded {
                self.work.push((n, true));
                for &c in self.kids.iter() {
                    if c.0 >= n {
                        return Err(ORDER);
                    }
                    self.work.push((c.0, false));
                }
                continue;
            }
            let mut le = Le(StableHasher::new("blaze-facts term"));
            node_shape(node).hash(&mut le);
            let mut h = le.0;
            for &c in self.kids.iter() {
                let d = self.memo.get(&(tab, c.0)).copied().ok_or(ORDER)?;
                h.u64(d);
            }
            self.memo.insert((tab, n), h.finish());
        }
        self.memo.get(&(tab, root.0)).copied().ok_or(ORDER)
    }

    /// The digest of a question's arguments: its key in a read list, with its kind.
    pub fn fact(&mut self, f: &Fact<'_>) -> Result<u64, FactError> {
        self.start("blaze-facts question");
        self.tag(f.kind() as u8);
        match *f {
            Fact::Adt(k)
            | Fact::Repr(k)
            | Fact::ValidRange(k)
            | Fact::Sig(k)
            | Fact::Abi(k)
            | Fact::Nounwind(k)
            | Fact::CVariadic(k)
            | Fact::LegacyConstGenerics(k)
            | Fact::Intrinsic(k)
            | Fact::Static(k)
            | Fact::FnKind(k)
            | Fact::CodeId(k)
            | Fact::CtorOf(k)
            | Fact::OpaqueOf(k)
            | Fact::ObjectTrait(k)
            | Fact::ItemPath(k)
            | Fact::ConstBody(k) => self.item(k),
            Fact::Discriminant { adt, variant } | Fact::DiscriminantBody { adt, variant } => {
                self.item(adt);
                self.u32(variant);
            }
            Fact::NameText(n) | Fact::Lang(n) => self.name(n),
            Fact::LangMember { lang, member } => {
                self.name(lang);
                self.name(member);
            }
            Fact::Literal { owner, at } => {
                self.item(owner);
                match at {
                    LitAt::Expr(e) => {
                        self.tag(0);
                        self.u32(e.0);
                    }
                    LitAt::Pat(p) => {
                        self.tag(1);
                        self.u32(p.0);
                    }
                }
            }
            Fact::FormatTemplate { owner, expr } | Fact::CallerSite { owner, expr } => {
                self.item(owner);
                self.u32(expr.0);
            }
            Fact::Closure { owner, ordinal } | Fact::Coroutine { owner, ordinal } | Fact::CoroutineRecipe { owner, ordinal } => {
                self.item(owner);
                self.u32(ordinal);
            }
            Fact::Normalize { trait_, name, args } => {
                self.item(trait_);
                self.name(name);
                self.ground(args)?;
            }
            Fact::OpaqueHidden { opaque, args } => {
                self.item(opaque);
                self.ground(args)?;
            }
            Fact::DropImpl(ty) => ty.digest(self)?,
            Fact::Selects { trait_, ty } => {
                self.item(trait_);
                ty.digest(self)?;
            }
            Fact::Resolve(i) | Fact::TrackCaller(i) | Fact::InstanceKey(i) => self.instance(i)?,
            Fact::KeyedInstance(key) => key.digest(self)?,
            Fact::CoroutineState { owner, ordinal, args } => {
                self.item(owner);
                self.u32(ordinal);
                self.ground(args)?;
            }
            Fact::Value { of, args } => {
                match of {
                    ConstOf::Item(k) => {
                        self.tag(0);
                        self.item(k);
                    }
                    ConstOf::Block { owner, expr } => {
                        self.tag(1);
                        self.item(owner);
                        self.u32(expr.0);
                    }
                    ConstOf::Promoted { owner, expr } => {
                        self.tag(2);
                        self.item(owner);
                        self.u32(expr.0);
                    }
                }
                self.ground(args)?;
            }
            Fact::Profile => {}
        }
        Ok(self.take())
    }

    fn ground(&mut self, g: Ground<'_>) -> Result<(), FactError> {
        self.terms(g.types, g.args)
    }

    fn instance(&mut self, i: Instance<'_>) -> Result<(), FactError> {
        self.item(i.item);
        self.ground(i.args)
    }

    /// The digest of an answer: what a read list records for its question.
    pub fn answer<V: Vocab>(&mut self, a: &Answer<'_, V>) -> Result<u64, FactError> {
        self.start("blaze-facts answer");
        if let Some(k) = a.kind() {
            self.tag(k as u8);
        }
        match a {
            Answer::Adt(v) => v.digest(self)?,
            Answer::Repr(v) => v.digest(self)?,
            Answer::ValidRange(v) => v.digest(self)?,
            Answer::Discriminant(v) => v.digest(self)?,
            Answer::DiscriminantBody(v) => v.digest(self)?,
            Answer::Sig(v) => v.digest(self)?,
            Answer::Abi(v) => v.digest(self)?,
            Answer::Nounwind(v) => v.digest(self)?,
            Answer::CVariadic(v) => v.digest(self)?,
            Answer::LegacyConstGenerics(v) => v.digest(self)?,
            Answer::Intrinsic(v) => v.digest(self)?,
            Answer::Static(v) => v.digest(self)?,
            Answer::FnKind(v) => v.digest(self)?,
            Answer::CodeId(v) => v.digest(self)?,
            Answer::CtorOf(v) => v.digest(self)?,
            Answer::OpaqueOf(v) => v.digest(self)?,
            Answer::ObjectTrait(v) => v.digest(self)?,
            Answer::ItemPath(v) => v.digest(self)?,
            Answer::NameText(v) => v.digest(self)?,
            Answer::ConstBody(v) => v.digest(self)?,
            Answer::Lang(v) => v.digest(self)?,
            Answer::LangMember(v) => v.digest(self)?,
            Answer::Literal(v) => v.digest(self)?,
            Answer::FormatTemplate(v) => v.digest(self)?,
            Answer::CallerSite(v) => v.digest(self)?,
            Answer::Closure(v) => v.digest(self)?,
            Answer::Coroutine(v) => v.digest(self)?,
            Answer::CoroutineRecipe(v) => v.digest(self)?,
            Answer::Normalize(v) => v.digest(self)?,
            Answer::OpaqueHidden(v) => v.digest(self)?,
            Answer::DropImpl(v) => v.digest(self)?,
            Answer::Selects(v) => v.digest(self)?,
            Answer::Resolve(v) => v.digest(self)?,
            Answer::TrackCaller(v) => v.digest(self)?,
            Answer::InstanceKey(v) => v.digest(self)?,
            Answer::KeyedInstance(v) => v.digest(self)?,
            Answer::CoroutineState(v) => v.digest(self)?,
            Answer::Value(v) => v.digest(self)?,
            Answer::Profile(v) => v.digest(self)?,
            // Never recorded: `Reads::ask` turns both into errors before it digests.
            Answer::Unknown(_) | Answer::Missing => return Err(FactError::Malformed { what: "an unanswered fact digested" }),
        }
        Ok(self.take())
    }
}

impl<T: Digest + ?Sized> Digest for &T {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        (**self).digest(d)
    }
}

impl<T: Digest> Digest for [T] {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.u64(self.len() as u64);
        for v in self {
            v.digest(d)?;
        }
        Ok(())
    }
}

impl<T: Digest> Digest for Option<T> {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        match self {
            None => {
                d.tag(0);
                Ok(())
            }
            Some(v) => {
                d.tag(1);
                v.digest(d)
            }
        }
    }
}

impl<A: Digest, B: Digest> Digest for (A, B) {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        self.0.digest(d)?;
        self.1.digest(d)
    }
}

impl Digest for str {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.bytes(self.as_bytes());
        Ok(())
    }
}

impl Digest for u8 {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.tag(*self);
        Ok(())
    }
}

impl Digest for u32 {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.u32(*self);
        Ok(())
    }
}

impl Digest for u64 {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.u64(*self);
        Ok(())
    }
}

impl Digest for i128 {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.i128(*self);
        Ok(())
    }
}

impl Digest for bool {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.bool(*self);
        Ok(())
    }
}

impl Digest for ItemKey {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.item(*self);
        Ok(())
    }
}

impl Digest for ContentId {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.h = d.h.bytes(&self.0);
        Ok(())
    }
}

impl Digest for Term<'_> {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.term(self.types, self.ty)
    }
}

impl Digest for Resolved<'_> {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.item(self.item);
        d.terms(self.types, self.args)
    }
}

impl Digest for Literal<'_> {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        match *self {
            Literal::Int(v) => {
                d.tag(0);
                d.u128(v);
            }
            Literal::Float { f64, f32 } => {
                d.tag(1);
                d.u64(f64);
                d.u32(f32);
            }
            Literal::Str(s) => {
                d.tag(2);
                d.bytes(s.as_bytes());
            }
            Literal::Bytes(b) => {
                d.tag(3);
                d.bytes(b);
            }
        }
        Ok(())
    }
}

/// An ADT's definition: its kind and counts, each variant's key, name, ctor and field range,
/// each field's name and type by content.
impl Digest for Adt<'_> {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.hashed(&self.kind);
        d.u32(self.generics);
        d.u64(self.variants.len() as u64);
        for v in self.variants {
            d.item(v.key);
            d.name(v.name);
            d.hashed(&v.ctor);
            d.u32(v.fields.start);
            d.u32(v.fields.len);
        }
        d.u64(self.fields.len() as u64);
        for f in self.fields {
            d.name(f.name);
            d.term(self.types, f.ty)?;
        }
        Ok(())
    }
}

/// A signature: its counts and flags, its header, its legacy positions, its inputs and output
/// by content.
impl Digest for FnSig<'_> {
    fn digest(&self, d: &mut Digester<'_>) -> Result<(), FactError> {
        d.u32(self.parent_count);
        d.u32(self.own_count);
        d.u32(self.declared_count);
        d.bool(self.self_param);
        d.hashed(&self.header);
        d.bool(self.c_variadic);
        self.legacy_const_generics.digest(d)?;
        d.terms(self.types, self.inputs)?;
        d.term(self.types, self.output)
    }
}
