//! The published set (docs/v2/plan/design/published.md): which records CI publishes, and the job
//! that lays a set of them out as an origin's objects.
//!
//! - [`publishable`]: the one test of whether a record may be published, on its namespace and its
//!   lifetime class alone; its [`Publishable`] proof is what the layout takes (rule 76).
//! - [`lay_out`]: records in any order to the same segments and objects (rule 25): kept by the
//!   predicate, sorted by key, cut into segments by size, sealed by blaze-cas `seal_segment`, then
//!   by [`crate::read::publish`] into the objects an origin holds, the `prefix` among them.
//!
//! Pure: the caller walks its roots for the records and writes the objects (only CI writes a
//! shared origin: on any other machine every record is `Namespace::LOCAL`, so none passes).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::fmt;

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

#[cfg(feature = "sign")]
pub use lay::{Candidate, Laid, LayOutError, Layout, lay_out, lay_out_bound};

/// A published set's name, its id the head's signature is bound to, the reader's check of a
/// set's head (trusted keys and the rollback floor) and the order its objects are written in.
pub mod set;

/// The lifetime class a record is rooted under (blaze-cli store/roots.rs), as the publish job
/// is told it by the walk that found the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// The toolchain's records, by the root `tool.<id>`: std, core and alloc's names records with
    /// their expansion records, their frozen heads, their code.
    Toolchain,
    /// A registry package at a version, known by its archive's checksum.
    Registry { checksum: ContentId },
    /// A checkout's answer for one request shape.
    Answer,
    /// A program's code under one checkout and request shape.
    Program,
}

impl Class {
    /// Whether the class is settled: its inputs cannot change under a fixed key.
    pub const fn settled(self) -> bool {
        match self {
            Class::Toolchain | Class::Registry { .. } => true,
            Class::Answer | Class::Program => false,
        }
    }

    /// The class in words, for a refusal.
    pub const fn what(self) -> &'static str {
        match self {
            Class::Toolchain => "the toolchain's",
            Class::Registry { .. } => "a registry package's",
            Class::Answer => "a checkout's answer",
            Class::Program => "a program's code",
        }
    }
}

/// The proof that a record may be published: only [`publishable`] makes one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Publishable {
    _only_publishable: (),
}

/// Why a record is not published: each names what failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unpublishable {
    /// Not of the public namespace: a machine's own, or an organisation's.
    Namespace { found: Namespace },
    /// Of a class that is not settled: a checkout's or a program's.
    Unsettled { class: Class },
}

impl fmt::Display for Unpublishable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unpublishable::Namespace { found } => write!(f, "of namespace {:#x}, not the public one: it stays where it was built", found.0),
            Unpublishable::Unsettled { class } => write!(f, "of {}, an unsettled class: only the toolchain's and registry packages' records are published", class.what()),
        }
    }
}

impl core::error::Error for Unpublishable {}

/// Whether a record of `namespace` rooted in `class` may be published (published.md, "The
/// published set"): only when it is public (which blaze-keyed `namespace_of` and
/// [`crate::privacy::namespace_for`] give only on a trusted writer, for the toolchain's crates
/// and checksummed registry archives whose keys read nothing outside them) AND its class is
/// settled. Each guards the other's mistake.
pub fn publishable(namespace: Namespace, class: Class) -> Result<Publishable, Unpublishable> {
    if namespace != Namespace::PUBLIC && !crate::planted(crate::Plant::PrivateAdmitted) {
        return Err(Unpublishable::Namespace { found: namespace });
    }
    if !class.settled() {
        return Err(Unpublishable::Unsettled { class });
    }
    Ok(Publishable { _only_publishable: () })
}

#[cfg(feature = "sign")]
mod lay {
    use core::fmt;

    use blaze_cas::plain::{Namespace, seal_segment};
    use blaze_ids::ContentId;
    use blaze_keyed::exec::RecordRef;
    use blaze_keyed::kind::Tag;
    use blaze_keyed::store::StoreError;
    use blaze_own::{Alloc, Vec};

    use super::{Class, Publishable, publishable};
    use crate::read::form::PAGE_ENTRIES_MOST;
    use crate::read::{PublishError, Sealed, publish_bound};
    use crate::trust::Signer;

    /// One record a walk of the roots found: its key, tag, namespace, class and bytes.
    #[derive(Clone, Copy, Debug)]
    pub struct Candidate<'b> {
        pub key: ContentId,
        pub tag: Tag,
        pub namespace: Namespace,
        pub class: Class,
        pub bytes: &'b [u8],
    }

    /// How a set is cut: payload bytes a segment holds at most (a longer record is a segment of
    /// its own), index entries a page, and the generation the head names.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Layout {
        pub segment_bytes: u64,
        pub page_entries: u32,
        pub generation: u64,
    }

    /// An origin's objects for a set, and its counts.
    pub struct Laid<'a> {
        pub objects: Vec<Sealed<'a>, Alloc<'a>>,
        /// Distinct records published.
        pub published: u32,
        /// Records the predicate kept out.
        pub left_out: u32,
        pub segments: u32,
    }

    /// Why a set was not laid out: each names the record or the bound.
    #[derive(Debug)]
    pub enum LayOutError {
        /// Two values under one key (rule 21): the key misses an input.
        TwoValues { key: ContentId, tags: (u16, u16) },
        /// The layout asked for is not one a reader takes.
        Layout { what: &'static str, value: u64 },
        /// More records than an index counts.
        TooMany { records: u64 },
        Seal(StoreError),
        Publish(PublishError),
    }

    impl fmt::Display for LayOutError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                LayOutError::TwoValues { key, tags } => {
                    f.write_str("two values under the key ")?;
                    for byte in &key.0[..6] {
                        write!(f, "{byte:02x}")?;
                    }
                    write!(f, " (tags {} and {}): the key misses an input of its value", tags.0, tags.1)
                }
                LayOutError::Layout { what, value } => write!(f, "a layout of {what} {value}: not one a reader takes"),
                LayOutError::TooMany { records } => write!(f, "{records} records, past the {} a layout counts", u32::MAX),
                LayOutError::Seal(error) => write!(f, "{error}"),
                LayOutError::Publish(error) => write!(f, "{error}"),
            }
        }
    }

    impl core::error::Error for LayOutError {}

    /// The objects of an origin holding the publishable records among `candidates` (any order,
    /// a record given twice is one), signed by `signer`: the same bytes for the same set
    /// whatever its order (rule 25).
    pub fn lay_out<'a>(candidates: &[Candidate<'_>], layout: Layout, signer: &Signer, alloc: Alloc<'a>) -> Result<Laid<'a>, LayOutError> {
        lay_out_bound(candidates, layout, None, signer, alloc)
    }

    /// [`lay_out`], the head's signature bound to `bound` when given (a published set's id,
    /// [`super::set::SetName::id`]; `read::publish_bound`).
    pub fn lay_out_bound<'a>(candidates: &[Candidate<'_>], layout: Layout, bound: Option<&ContentId>, signer: &Signer, alloc: Alloc<'a>) -> Result<Laid<'a>, LayOutError> {
        if layout.segment_bytes == 0 {
            return Err(LayOutError::Layout { what: "segment bytes", value: 0 });
        }
        if !(1..=PAGE_ENTRIES_MOST).contains(&layout.page_entries) {
            return Err(LayOutError::Layout { what: "entries a page", value: u64::from(layout.page_entries) });
        }
        if candidates.len() > u32::MAX as usize {
            return Err(LayOutError::TooMany { records: candidates.len() as u64 });
        }
        // 1. Kept by the proof alone.
        let mut kept: Vec<(u32, Publishable), Alloc<'_>> = Vec::with_capacity_in(candidates.len(), alloc);
        let mut left_out = 0u32;
        for (i, candidate) in candidates.iter().enumerate() {
            match publishable(candidate.namespace, candidate.class) {
                Ok(proof) => kept.push((i as u32, proof)),
                Err(_) => left_out += 1,
            }
        }
        // 2. Sorted by everything a record is, so equal records sit together whatever the
        //    order given; two values under one key are refused.
        kept.sort_unstable_by(|a, b| {
            let (a, b) = (&candidates[a.0 as usize], &candidates[b.0 as usize]);
            (a.key, a.tag, a.bytes).cmp(&(b.key, b.tag, b.bytes))
        });
        let mut run: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(kept.len(), alloc);
        for &(i, _proof) in kept.iter() {
            let candidate = &candidates[i as usize];
            if let Some(&last) = run.as_slice().last() {
                let last = &candidates[last as usize];
                if last.key == candidate.key {
                    if last.tag == candidate.tag && last.bytes == candidate.bytes {
                        continue;
                    }
                    return Err(LayOutError::TwoValues { key: candidate.key, tags: (last.tag.0, candidate.tag.0) });
                }
            }
            run.push(i);
        }
        // 3. Cut by size, in key order: the cut depends on the sorted run alone.
        let mut segments: Vec<std::vec::Vec<u8, Alloc<'a>>, Alloc<'_>> = Vec::new_in(alloc);
        let mut records: Vec<(RecordRef, &[u8]), Alloc<'_>> = Vec::new_in(alloc);
        let mut held = 0u64;
        for &i in run.iter() {
            let candidate = &candidates[i as usize];
            let len = candidate.bytes.len() as u64;
            if !records.is_empty() && held + len > layout.segment_bytes {
                segments.push(seal(&records, alloc)?);
                records.clear();
                held = 0;
            }
            records.push((RecordRef { tag: candidate.tag, key: candidate.key }, candidate.bytes));
            held += len;
        }
        if !records.is_empty() {
            segments.push(seal(&records, alloc)?);
        }
        // 4. Sealed into the origin's objects.
        let mut refs: Vec<&[u8], Alloc<'_>> = Vec::with_capacity_in(segments.len(), alloc);
        for segment in segments.iter() {
            refs.push(segment.as_slice());
        }
        let objects = publish_bound(Namespace::PUBLIC, layout.generation, signer, bound, &refs, layout.page_entries, alloc).map_err(LayOutError::Publish)?;
        Ok(Laid { objects, published: run.len() as u32, left_out, segments: segments.len() as u32 })
    }

    /// One segment of the public namespace holding `records`.
    fn seal<'a>(records: &[(RecordRef, &[u8])], alloc: Alloc<'a>) -> Result<std::vec::Vec<u8, Alloc<'a>>, LayOutError> {
        let mut out = std::vec::Vec::new_in(alloc);
        seal_segment(Namespace::PUBLIC, records, &mut out).map_err(LayOutError::Seal)?;
        Ok(out)
    }
}
