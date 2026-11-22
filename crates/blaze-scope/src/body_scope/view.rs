//! A `BodyScope` record read in place (D189 step 5's reader): the bytes [`BodyScope::write`]
//! wrote, checked once when opened (layout, version, order, every block's scope record through
//! blaze-surface's `ScopeView::read`), then answered without a copy. A reader of a body's block
//! names (blaze-ladder front2/scope_table.rs `BlockNames::Bodies`) and the store's loader read
//! this, never the builder's `BodyScope`.
//!
//! The record must be placed at a multiple of `blaze_format::ALIGN` (as the store places every
//! record): each block's scope record is written at such an offset from the record's start.

#![blaze::sans_io]

use core::fmt;

use blaze_format::FormatError;
use blaze_id::ItemKey;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_surface::{Binding, Namespace, ScopeView};

use super::{BODY_SCOPE_VERSION, BodyScope, MAGIC};

/// Why bytes are not a `BodyScope` record, naming where (rule 19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyScopeReadError {
    /// The bytes do not start with the record's magic.
    Magic,
    /// A record of another version: it is rebuilt, never read (rule 26).
    Version { found: u32 },
    /// The record ends inside a field that starts at byte `at`.
    Short { at: usize },
    /// A name at byte `at` is not UTF-8.
    Text { at: usize },
    /// The blocks are not in strictly increasing order of module key (block `at`).
    Order { at: usize },
    /// Block `module`'s scope record does not read.
    Scope { module: ItemKey, error: FormatError },
    /// Bytes are left after the record's last field, from `at`.
    Trailing { at: usize },
}

impl fmt::Display for BodyScopeReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyScopeReadError::Magic => write!(f, "a body scope record (D189): the bytes are no body scope record"),
            BodyScopeReadError::Version { found } => write!(f, "a body scope record (D189): version {found}, this blaze reads {BODY_SCOPE_VERSION}"),
            BodyScopeReadError::Short { at } => write!(f, "a body scope record (D189): it ends inside the field at byte {at}"),
            BodyScopeReadError::Text { at } => write!(f, "a body scope record (D189): the name at byte {at} is not UTF-8"),
            BodyScopeReadError::Order { at } => write!(f, "a body scope record (D189): block {at} is out of order"),
            BodyScopeReadError::Scope { module, error } => write!(f, "a body scope record (D189): block {module:?}'s scope does not read: {error:?}"),
            BodyScopeReadError::Trailing { at } => write!(f, "a body scope record (D189): bytes are left from byte {at}"),
        }
    }
}

/// One impl the body's blocks write, as the record holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImplAt<'a> {
    pub key: ItemKey,
    pub module: ItemKey,
    pub header: &'a str,
}

/// A `BodyScope` record, checked once.
#[derive(Debug)]
pub struct BodyScopeView<'a> {
    pub key: ContentId,
    pub reads: ContentId,
    /// The impl rows' digest (version 3): what the crate's merged impl table is keyed by
    /// (super::merged).
    pub impl_rows: ContentId,
    /// Each block's module and scope, by module key.
    blocks: AVec<'a, (ItemKey, ScopeView<'a>)>,
    /// Each written block's ordinal ([`super::BlockRow::ordinal`]) and its index in `blocks`,
    /// sorted by ordinal: a block entered by the body's own numbering of its blocks
    /// ([`BodyScopeView::binding_at`]), never by a module key another strategy made.
    ordinals: AVec<'a, (u32, u32)>,
    pub impls: AVec<'a, ImplAt<'a>>,
    pub outermost: AVec<'a, ItemKey>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BodyScopeReadError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(BodyScopeReadError::Short { at: self.at })?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], BodyScopeReadError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn key(&mut self) -> Result<ItemKey, BodyScopeReadError> {
        Ok(ItemKey(ContentId(self.array::<32>()?)))
    }

    fn u32(&mut self) -> Result<u32, BodyScopeReadError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    /// A count or a length: never more than the bytes left, so no reader allocates by it.
    fn count(&mut self) -> Result<usize, BodyScopeReadError> {
        let at = self.at;
        let n = u64::from_le_bytes(self.array::<8>()?);
        usize::try_from(n).ok().filter(|n| *n <= self.bytes.len() - self.at).ok_or(BodyScopeReadError::Short { at })
    }

    fn str(&mut self) -> Result<&'a str, BodyScopeReadError> {
        let len = self.count()?;
        let at = self.at;
        core::str::from_utf8(self.take(len)?).map_err(|_| BodyScopeReadError::Text { at })
    }
}

/// The view is covariant in its lifetime (slices, `&str`, `AVec` over `Alloc`, `ScopeView`):
/// front2 hands `&'n BodyScopeView<'n>` out of a lifetime-free trait (locals.rs `BodyScopes`)
/// and keeps `Locals<'n>` covariant. A field that breaks it fails to compile here.
#[expect(dead_code, reason = "a compile-time proof of covariance, never called")]
fn covariant<'s, 'l: 's>(view: BodyScopeView<'l>) -> BodyScopeView<'s> {
    view
}

impl<'a> BodyScopeView<'a> {
    /// The record in `bytes` (placed at a multiple of `blaze_format::ALIGN`), checked once.
    pub fn read(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<BodyScopeView<'a>, BodyScopeReadError> {
        let mut c = Cursor { bytes, at: 0 };
        if c.array::<4>().ok() != Some(MAGIC) {
            return Err(BodyScopeReadError::Magic);
        }
        let found = c.u32()?;
        if found != BODY_SCOPE_VERSION {
            return Err(BodyScopeReadError::Version { found });
        }
        let key = ContentId(c.array::<32>()?);
        let reads = ContentId(c.array::<32>()?);
        let impl_rows = ContentId(c.array::<32>()?);
        let n = c.count()?;
        let mut blocks: AVec<'a, (ItemKey, ScopeView<'a>)> = AVec::new_in(alloc);
        let mut ordinals: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
        for at in 0..n {
            let module = c.key()?;
            if blocks.last().is_some_and(|(last, _)| *last >= module) {
                return Err(BodyScopeReadError::Order { at });
            }
            let ordinal = c.u32()?;
            if ordinal != u32::MAX {
                ordinals.push((ordinal, blocks.len() as u32));
            }
            let _parent = c.key()?;
            for _ in 0..c.count()? {
                c.take(1)?;
                c.str()?;
                c.key()?;
                c.take(1)?;
            }
            let len = c.count()?;
            let pad = c.at.next_multiple_of(blaze_format::ALIGN) - c.at;
            c.take(pad)?;
            let scope = ScopeView::read(c.take(len)?).map_err(|error| BodyScopeReadError::Scope { module, error })?;
            blocks.push((module, scope));
        }
        let mut impls: AVec<'a, ImplAt<'a>> = AVec::new_in(alloc);
        for _ in 0..c.count()? {
            let key = c.key()?;
            let module = c.key()?;
            let header = c.str()?;
            impls.push(ImplAt { key, module, header });
        }
        for _ in 0..c.count()? {
            c.key()?;
            c.key()?;
            c.str()?;
        }
        let mut outermost: AVec<'a, ItemKey> = AVec::new_in(alloc);
        for _ in 0..c.count()? {
            outermost.push(c.key()?);
        }
        if c.at != bytes.len() {
            return Err(BodyScopeReadError::Trailing { at: c.at });
        }
        ordinals.sort_unstable();
        if let Some(w) = ordinals.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(BodyScopeReadError::Order { at: w[1].1 as usize });
        }
        Ok(BodyScopeView { key, reads, impl_rows, blocks, ordinals, impls, outermost })
    }

    /// The direct binding of `name` in `ns` in the body's written block `ordinal` (its `{`'s
    /// place among the body's written blocks), before its globs: `None` when the body has no
    /// such written block (a gap, never "unbound").
    pub fn binding_at(&self, ordinal: u32, ns: Namespace, name: &str) -> Option<Option<Binding>> {
        let i = self.ordinals.binary_search_by_key(&ordinal, |o| o.0).ok()?;
        Some(self.blocks[self.ordinals[i].1 as usize].1.get(ns, name))
    }

    /// The direct binding of `name` in `ns` in block `module`, before its globs: `None` when
    /// `module` is no block of this body (a gap, never "unbound").
    pub fn binding(&self, module: ItemKey, ns: Namespace, name: &str) -> Option<Option<Binding>> {
        let i = self.blocks.binary_search_by_key(&module, |(m, _)| *m).ok()?;
        Some(self.blocks[i].1.get(ns, name))
    }

    /// The blocks' modules, in key order.
    pub fn modules(&self) -> impl ExactSizeIterator<Item = ItemKey> + '_ {
        self.blocks.iter().map(|(m, _)| *m)
    }
}

impl BodyScope<'_> {
    /// Whether `view` (this record's bytes read back) answers every name each block declares as
    /// this builder does, and holds its key, read list, impls and outermost blocks: the
    /// round trip a restore must pass (rule 26). The first difference, named, else `None`.
    pub fn differs_from(&self, view: &BodyScopeView<'_>) -> Option<RoundTripDiff> {
        if self.key != view.key || self.reads != view.reads || self.impl_rows != view.impl_rows {
            return Some(RoundTripDiff::Key);
        }
        if self.blocks.len() != view.blocks.len() {
            return Some(RoundTripDiff::Blocks);
        }
        for b in self.blocks.iter() {
            for &(ns, name, _) in b.declared.iter() {
                let built = b.scope.get(ns, name).copied();
                if view.binding(b.module, ns, name) != Some(built) {
                    return Some(RoundTripDiff::Binding { module: b.module });
                }
            }
        }
        let impls_equal = self.impls.len() == view.impls.len() && self.impls.iter().zip(view.impls.iter()).all(|(a, b)| a.key == b.key && a.module == b.module && a.header == b.header);
        if !impls_equal {
            return Some(RoundTripDiff::Impls);
        }
        if self.outermost.as_slice() != view.outermost.as_slice() {
            return Some(RoundTripDiff::Outermost);
        }
        None
    }
}

/// Where a record read back first differs from its builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundTripDiff {
    Key,
    Blocks,
    Binding { module: ItemKey },
    Impls,
    Outermost,
}
