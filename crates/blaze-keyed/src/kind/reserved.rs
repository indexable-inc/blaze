//! Every RESERVED record tag, once: the tags at the top of the u16 range that no kind of
//! [`super::TAGS`] takes, which the store (blaze-cas) and the command (blaze-cli) file their
//! own records under. One home (rule 30): a tag is named here or nowhere, as a variant of
//! [`ReservedTag`]; nothing writes `Tag(u16::MAX - n)`.
//!
//! What holds by construction (rule 73):
//!
//! - two variants cannot share a value: an enum's discriminants never repeat (the compiler
//!   refuses it, E0081); [`ReservedTag::ALL`] is made by the same macro as the enum, so it lists
//!   every variant, and a `const` assertion over it fails the BUILD on a repeat as well;
//! - every reserved tag is at or above [`RESERVED_FROM`], and every kind's tag below it (a
//!   `const` assertion over [`super::TAGS`]): the two ranges never meet;
//! - what the store's reach walk does with a record is the registry's answer ([`reach_of`]),
//!   never a bare comparison: only [`ReservedTag::Index`] is followed where an index belongs,
//!   and a record found there of any other tag is the store's `NotAnIndex` refusal.
//!
//! Changing a value changes what a segment's entries mean (an entry carries its tag, bits
//! 40..56 of its place, beside its key, not in it): it bumps blaze-cas `SEGMENT_FORMAT`, so an
//! old home's store is another form's folder, reported and rebuilt, never read (rule 26).
//! Format 3 is the one that gave [`ReservedTag::Expansion`] its own value (it had shared
//! [`ReservedTag::Index`]'s, `u16::MAX - 6`).

#![blaze::sans_io]

use super::{TAGS, Tag};

/// The lowest tag a reserved tag may take: the top 256 of the u16 range are the reserved
/// block, the rest the kinds' ([`super::TAGS`], 1 to its length).
pub const RESERVED_FROM: u16 = u16::MAX - 255;

/// What the store's reach walk (blaze-cas core.rs `root_places`, `follow`, `reused`) does with
/// a record's payload. The walk asks [`reach_of`]; it compares no tag of its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagReach {
    /// The payload is the caller's: the store reads none of it, the record is kept by its key
    /// alone. Every kind's tag, and every reserved tag the store does not interpret.
    Opaque,
    /// A root's record, named by the manifest: sorted 32-byte keys reach keeps live.
    RootKeys,
    /// An index: sorted 32-byte keys reach keeps live when an indexed root names it. The ONE
    /// record the walk follows where an index belongs; any other is refused there.
    IndexKeys,
    /// An indexed root's record: sorted keys of indexes, each followed one level.
    Indexes,
    /// A pointer: one 32-byte target key, the newest commit's value of its name wins (the one
    /// replaceable record); reach never follows the target.
    Target,
}

macro_rules! reserved {
    ($($(#[$meta:meta])* $name:ident = $value:expr, reach: $reach:ident;)*) => {
        /// A reserved record tag: one variant a tag, its value the tag (`as u16`).
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[repr(u16)]
        pub enum ReservedTag {
            $($(#[$meta])* $name = $value,)*
        }

        impl ReservedTag {
            /// Every reserved tag, in the order declared (descending values).
            pub const ALL: [ReservedTag; [$(ReservedTag::$name),*].len()] = [$(ReservedTag::$name),*];

            /// What the reach walk does with a record of this tag.
            pub const fn reach(self) -> TagReach {
                match self {
                    $(ReservedTag::$name => TagReach::$reach,)*
                }
            }

            /// Its name, for an error or a report.
            pub const fn name(self) -> &'static str {
                match self {
                    $(ReservedTag::$name => ::core::stringify!($name),)*
                }
            }
        }
    };
}

reserved! {
    /// The store's root records: a root's sorted keys, under `Namespace::LOCAL` (blaze-cas
    /// `Plain::root_for`).
    Root = u16::MAX, reach: RootKeys;
    /// A program's startup image: the code records a run placed before `main` (blaze-cas
    /// image.rs `StartupImage`).
    Image = u16::MAX - 1, reach: Opaque;
    /// Every drive record (blaze-cli store/plain_place.rs): the record's own head says which
    /// drive kind it is.
    Drive = u16::MAX - 2, reach: Opaque;
    /// A code entry of the code store (blaze-cli code_store.rs), and a code key's pointer's
    /// target.
    Code = u16::MAX - 3, reach: Opaque;
    /// A pointer: a name's record whose payload is another record's key, replaced by a newer
    /// commit (blaze-cas `Lent::point`).
    Pointer = u16::MAX - 4, reach: Target;
    /// A start list's bytes (blaze-cli start_list.rs), under their content key, a pointer of
    /// the list's name naming them.
    StartList = u16::MAX - 5, reach: Opaque;
    /// An index: sorted 32-byte keys, keyed by its bytes or filed beside its owner
    /// (blaze-cas `Plain::index`, `Plain::index_beside`).
    Index = u16::MAX - 6, reach: IndexKeys;
    /// An indexed root's record: sorted keys, each an index (blaze-cas `Plain::root_through`).
    IndexedRoot = u16::MAX - 7, reach: Indexes;
    /// A published set's floor record (blaze-cli store/published.rs).
    Floor = u16::MAX - 8, reach: Opaque;
    /// A macro expansion's record: a call's text and origin column, keyed by its bytes
    /// (blaze-scope expansion_records.rs), put beside its crate's `CrateScope` record.
    Expansion = u16::MAX - 9, reach: Opaque;
}

impl ReservedTag {
    /// The tag a record of this reservation is filed under.
    pub const fn tag(self) -> Tag {
        Tag(self as u16)
    }

    /// Whether `tag` is this reservation's.
    pub const fn is(self, tag: Tag) -> bool {
        tag.0 == self as u16
    }

    /// The reservation `tag` is, if any: `None` for a kind's tag and an unreserved value.
    pub const fn of(tag: Tag) -> Option<ReservedTag> {
        if tag.0 < RESERVED_FROM {
            return None;
        }
        let mut at = 0;
        while at < ReservedTag::ALL.len() {
            if ReservedTag::ALL[at] as u16 == tag.0 {
                return Some(ReservedTag::ALL[at]);
            }
            at += 1;
        }
        None
    }

    /// Whether the store reads this record's payload as keys to keep (a root's record, an
    /// index, an indexed root's indexes). A pointer's target is not followed.
    pub const fn follows_keys(self) -> bool {
        matches!(self.reach(), TagReach::RootKeys | TagReach::IndexKeys | TagReach::Indexes)
    }
}

/// What the reach walk does with a record of `tag`: [`TagReach::Opaque`] for every tag that is no
/// reservation (each kind's).
pub const fn reach_of(tag: Tag) -> TagReach {
    match ReservedTag::of(tag) {
        Some(it) => it.reach(),
        None => TagReach::Opaque,
    }
}

/// Every reserved value once, each in the reserved block: the build fails on a repeat or a
/// value below [`RESERVED_FROM`] (the enum already refuses a repeated discriminant; this
/// states it over the table the registry answers from).
const _: () = {
    let all = ReservedTag::ALL;
    let mut i = 0;
    while i < all.len() {
        assert!(all[i] as u16 >= RESERVED_FROM, "a reserved tag below RESERVED_FROM: in the kinds' range");
        let mut j = i + 1;
        while j < all.len() {
            assert!(all[i] as u16 != all[j] as u16, "two reserved tags of one value");
            j += 1;
        }
        i += 1;
    }
};

/// The kinds' range (1 to `TAGS.len()`, kind.rs `listed`) ends below the reserved block.
const _: () = assert!(TAGS.len() < RESERVED_FROM as usize, "the kinds' tags reach the reserved block (RESERVED_FROM)");

/// Exactly one reservation is followed where an index belongs, and it is the index: what
/// restores the store's `NotAnIndex` guard (an expansion record once shared its value).
const _: () = {
    let all = ReservedTag::ALL;
    let mut indexes = 0;
    let mut i = 0;
    while i < all.len() {
        if matches!(all[i].reach(), TagReach::IndexKeys) {
            indexes += 1;
            assert!(all[i] as u16 == ReservedTag::Index as u16, "a reservation other than Index is followed as an index");
        }
        i += 1;
    }
    assert!(indexes == 1, "exactly one reservation is followed as an index");
};

#[cfg(test)]
#[path = "reserved_tests.rs"]
mod tests;
