//! ECS / Flecs-style archetypal enum storage: each variant of an enum lives in its own dense,
//! tightly typed [`Vec`](crate::Vec) column addressed by a 4-byte [`ArchetypeHandle`].
//!
//! In a standard Rust `Vec<E>`, every element pays for the largest variant's size and alignment,
//! and sweeping one variant branches on every element's tag. Archetypal storage splits `E` by
//! discriminant:
//!
//! - A 4-byte payload variant lives in `Vec<u32, A>` — never paying a single byte of padding for a
//!   64-byte sibling variant.
//! - A unit variant stores only a `u32` instance counter — zero bytes per element!
//! - Sweeping all instances of one variant is a branch-free contiguous slice loop
//!   (`store.lit_slice()` / `store.lit_slice_mut()`).
//! - Any heterogeneous reference is a compact 4-byte [`ArchetypeHandle`] packing `(tag: u8, row: u24)`.
//!
//! # Example
//!
//! ```
//! use blaze_own::{ArchetypeHandle, Inline, archetypal};
//!
//! archetypal! {
//!     pub enum ExprStore, #[derive(Debug, PartialEq, Eq)] ExprRef {
//!         unit {
//!             Empty => push_empty / get_empty / empty_count,
//!         }
//!         tuple {
//!             Lit(u64) => push_lit / get_lit / lit_slice / lit_slice_mut,
//!         }
//!         struct {
//!             #[derive(Clone, Copy, Debug, PartialEq, Eq)]
//!             Call { callee: u32, args: u32 } => push_call / get_call / call_slice / call_slice_mut,
//!         }
//!     }
//! }
//!
//! let arena = Inline::<1024>::new();
//! let mut store = ExprStore::new_in(&arena);
//! let h_empty: ArchetypeHandle = store.push_empty();
//! let h_lit = store.push_lit(42);
//! let h_call = store.push_call(7, 3);
//!
//! assert_eq!(store.len(), 3);
//! assert_eq!(store.get_lit(h_lit), Some(&42));
//! assert_eq!(store.get_lit(h_call), None);
//! assert_eq!(store.get(h_empty), Some(ExprRef::Empty));
//! assert_eq!(store.get(h_call), Some(ExprRef::Call(&Call { callee: 7, args: 3 })));
//! assert_eq!(store.lit_slice(), &[42]);
//! ```
//!
//! # Future replacement via comptime reflection
//!
//! Once `edition = "blaze-2026.11"` comptime reflection (`type_info`) lands broadly across the
//! workspace, a generic `Archetypal<E, A>` synthesized directly from `E`'s reflected variants will
//! replace the [`archetypal!`] macro with zero caller change.

pub use core::alloc::Allocator;
pub use core::marker::PhantomData;
use core::{fmt, mem::size_of};

/// Maximum row index representable inside an [`ArchetypeHandle`] (`(1 << 24) - 1 = 16,777,215`).
pub const MAX_ROWS_PER_VARIANT: u32 = (1 << 24) - 1;

/// Maximum number of variants addressable by an [`ArchetypeHandle`] (`u8::MAX as usize + 1 = 256`).
pub const MAX_VARIANTS: usize = 256;

/// Compact 4-byte handle addressing one element in an archetypal store.
///
/// Bit layout:
/// - Bits `31..24`: `tag: u8` (variant / archetype discriminant, up to 256 variants).
/// - Bits `23..0`: `row: u24` (dense row index within that variant's column, `0..=16_777_215`).
///
/// Ordering handles by `Ord` groups them by `tag` first and then ascending `row`, matching
/// cache-friendly archetype-by-archetype sweeps.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArchetypeHandle(u32);

const _: () = assert!(size_of::<ArchetypeHandle>() == 4);

impl ArchetypeHandle {
    /// Packs `(tag, row)` into a 4-byte handle, returning `None` if `row > MAX_ROWS_PER_VARIANT`.
    #[inline]
    pub const fn new(tag: u8, row: u32) -> Option<Self> {
        if row > MAX_ROWS_PER_VARIANT {
            return None;
        }
        Some(Self(((tag as u32) << 24) | row))
    }

    /// Returns the 8-bit variant discriminant.
    #[inline]
    pub const fn tag(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// Returns the 24-bit dense row index within the variant's column.
    #[inline]
    pub const fn row(self) -> u32 {
        self.0 & MAX_ROWS_PER_VARIANT
    }

    /// Returns the raw packed `u32` representation.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Reconstructs a handle from its raw packed `u32` representation.
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

impl fmt::Debug for ArchetypeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchetypeHandle")
            .field("tag", &self.tag())
            .field("row", &self.row())
            .finish()
    }
}

/// Computes a variant's `u8` discriminant at compile time by locating `target` in `names`.
#[doc(hidden)]
#[inline]
pub const fn __tag_index(names: &[&str], target: &str) -> u8 {
    assert!(names.len() <= MAX_VARIANTS, "archetypal! supports at most 256 variants");
    let mut i = 0u8;
    let mut rest = names;
    while let Some((&name, next)) = rest.split_first() {
        let mut a = name.as_bytes();
        let mut b = target.as_bytes();
        let mut same = a.len() == b.len();
        while same {
            match (a.split_first(), b.split_first()) {
                (Some((&x, na)), Some((&y, nb))) => {
                    same = x == y;
                    a = na;
                    b = nb;
                }
                _ => break,
            }
        }
        if same {
            return i;
        }
        i = i.wrapping_add(1);
        rest = next;
    }
    0
}

/// Builds an [`ArchetypeHandle`] for `row` or panics on 24-bit row overflow before mutating storage.
#[doc(hidden)]
#[inline]
#[track_caller]
pub fn __make_handle(tag: u8, row: usize, variant: &'static str) -> ArchetypeHandle {
    let Ok(row_u32) = u32::try_from(row) else {
        __overflow_variant(variant, row);
    };
    match ArchetypeHandle::new(tag, row_u32) {
        Some(handle) => handle,
        None => __overflow_variant(variant, row),
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn __overflow_variant(variant: &'static str, row: usize) -> ! {
    panic!(
        "blaze_own::archetypal: variant `{variant}` row {row} exceeds MAX_ROWS_PER_VARIANT ({MAX_ROWS_PER_VARIANT})"
    )
}

/// Defines an archetypal enum store with one dense [`Vec`](crate::Vec) column per payload variant
/// and a zero-byte-per-element `u32` counter per unit variant.
///
/// Mirrors [`soa!`](crate::soa!): `macro_rules!` cannot synthesize snake_case method names from
/// variant names, so each variant lists its accessor names after `=>`, grouped into optional
/// `unit { .. }`, `tuple { .. }`, and `struct { .. }` sections (single-pass, zero macro recursion):
///
/// - `unit { Empty => push_empty / get_empty / empty_count, }` (or omit `/ empty_count`)
/// - `tuple { Lit(u64) => push_lit / get_lit / lit_slice / lit_slice_mut, }`
/// - `struct { Call { callee: u32, args: u32 } => push_call / get_call / call_slice / call_slice_mut, }`
///
/// Generates:
/// - A payload struct `$vis struct $Variant { $($vis $field: $fty),* }` for each struct variant.
/// - A typed reference view enum `$vis enum $Ref<'a>` (`#[derive(Clone, Copy)]` plus any attributes
///   placed before `$Ref`).
/// - The store `$vis struct $Store<A: Allocator>` with `new_in(alloc: A)` (`A: Clone`), `len()`,
///   `is_empty()`, `clear()`, `get(&self, h: ArchetypeHandle) -> Option<$Ref<'_>>`, and the
///   per-variant push, get, and slice accessors.
///
/// All generated code is 100% safe Rust.
#[macro_export]
macro_rules! archetypal {
    (
        $(#[$store_meta:meta])*
        $vis:vis enum $Store:ident, $(#[$ref_meta:meta])* $Ref:ident {
            $(
                unit {
                    $(
                        $(#[$umeta:meta])*
                        $UVariant:ident => $upush:ident / $uget:ident $(/ $ucount:ident)?
                    ),* $(,)?
                }
            )?
            $(
                tuple {
                    $(
                        $(#[$tmeta:meta])*
                        $TVariant:ident ( $tty:ty ) => $tpush:ident / $tget:ident / $tslice:ident / $tslice_mut:ident
                    ),* $(,)?
                }
            )?
            $(
                struct {
                    $(
                        $(#[$smeta:meta])*
                        $SVariant:ident { $($sfield:ident : $sfty:ty),* $(,)? } => $spush:ident / $sget:ident / $sslice:ident / $sslice_mut:ident
                    ),* $(,)?
                }
            )?
        }
    ) => {
        $($(
            $(#[$smeta])*
            $vis struct $SVariant {
                $( pub $sfield: $sfty, )*
            }
        )*)?

        #[doc = concat!("Borrowed reference view of one element in `", stringify!($Store), "`.")]
        #[derive(Clone, Copy)]
        $(#[$ref_meta])*
        #[allow(dead_code)]
        $vis enum $Ref<'a> {
            $($(
                $(#[$umeta])*
                $UVariant,
            )*)?
            $($(
                $(#[$tmeta])*
                $TVariant(&'a $tty),
            )*)?
            $($(
                $SVariant(&'a $SVariant),
            )*)?
        }

        impl $Ref<'static> {
            #[doc(hidden)]
            #[inline(always)]
            const fn __blaze_archetypal_tag(name: &'static str) -> u8 {
                $crate::archetypal::__tag_index(
                    &[
                        $($(stringify!($UVariant),)*)?
                        $($(stringify!($TVariant),)*)?
                        $($(stringify!($SVariant),)*)?
                    ],
                    name,
                )
            }
        }

        $(#[$store_meta])*
        #[allow(non_snake_case)]
        $vis struct $Store<A: $crate::archetypal::Allocator> {
            $($( $UVariant: u32, )*)?
            $($( $TVariant: $crate::Vec<$tty, A>, )*)?
            $($( $SVariant: $crate::Vec<$SVariant, A>, )*)?
            _alloc: $crate::archetypal::PhantomData<A>,
        }

        #[allow(dead_code, non_snake_case)]
        impl<A: $crate::archetypal::Allocator + Clone> $Store<A> {
            #[inline]
            $vis fn new_in(alloc: A) -> Self {
                let _ = &alloc;
                Self {
                    $($( $UVariant: 0, )*)?
                    $($( $TVariant: $crate::Vec::new_in(alloc.clone()), )*)?
                    $($( $SVariant: $crate::Vec::new_in(alloc.clone()), )*)?
                    _alloc: $crate::archetypal::PhantomData,
                }
            }
        }

        #[allow(dead_code, non_snake_case)]
        impl<A: $crate::archetypal::Allocator> $Store<A> {
            #[inline]
            $vis fn len(&self) -> usize {
                0usize
                    $($( + (self.$UVariant as usize) )*)?
                    $($( + self.$TVariant.len() )*)?
                    $($( + self.$SVariant.len() )*)?
            }

            #[inline]
            $vis fn is_empty(&self) -> bool {
                self.len() == 0
            }

            #[inline]
            $vis fn clear(&mut self) {
                $($( self.$UVariant = 0; )*)?
                $($( self.$TVariant.clear(); )*)?
                $($( self.$SVariant.clear(); )*)?
            }

            #[inline]
            $vis fn get(&self, h: $crate::archetypal::ArchetypeHandle) -> Option<$Ref<'_>> {
                $($(
                    if h.tag() == const { $Ref::__blaze_archetypal_tag(stringify!($UVariant)) } {
                        return (h.row() < self.$UVariant).then_some($Ref::$UVariant);
                    }
                )*)?
                $($(
                    if h.tag() == const { $Ref::__blaze_archetypal_tag(stringify!($TVariant)) } {
                        return self.$TVariant.get(h.row() as usize).map($Ref::$TVariant);
                    }
                )*)?
                $($(
                    if h.tag() == const { $Ref::__blaze_archetypal_tag(stringify!($SVariant)) } {
                        return self.$SVariant.get(h.row() as usize).map($Ref::$SVariant);
                    }
                )*)?
                None
            }

            $($(
                #[inline]
                $vis fn $upush(&mut self) -> $crate::archetypal::ArchetypeHandle {
                    let row = self.$UVariant as usize;
                    let handle = $crate::archetypal::__make_handle(
                        const { $Ref::__blaze_archetypal_tag(stringify!($UVariant)) },
                        row,
                        stringify!($UVariant),
                    );
                    self.$UVariant += 1;
                    handle
                }

                #[inline]
                $vis fn $uget(&self, h: $crate::archetypal::ArchetypeHandle) -> Option<()> {
                    if h.tag() != const { $Ref::__blaze_archetypal_tag(stringify!($UVariant)) } {
                        return None;
                    }
                    (h.row() < self.$UVariant).then_some(())
                }

                $(
                    #[inline]
                    $vis fn $ucount(&self) -> u32 {
                        self.$UVariant
                    }
                )?
            )*)?

            $($(
                #[inline]
                $vis fn $tpush(&mut self, value: $tty) -> $crate::archetypal::ArchetypeHandle {
                    let row = self.$TVariant.len();
                    let handle = $crate::archetypal::__make_handle(
                        const { $Ref::__blaze_archetypal_tag(stringify!($TVariant)) },
                        row,
                        stringify!($TVariant),
                    );
                    self.$TVariant.push(value);
                    handle
                }

                #[inline]
                $vis fn $tget(&self, h: $crate::archetypal::ArchetypeHandle) -> Option<&$tty> {
                    if h.tag() != const { $Ref::__blaze_archetypal_tag(stringify!($TVariant)) } {
                        return None;
                    }
                    self.$TVariant.get(h.row() as usize)
                }

                #[inline]
                $vis fn $tslice(&self) -> &[$tty] {
                    self.$TVariant.as_slice()
                }

                #[inline]
                $vis fn $tslice_mut(&mut self) -> &mut [$tty] {
                    self.$TVariant.as_mut_slice()
                }
            )*)?

            $($(
                #[inline]
                $vis fn $spush(&mut self, $( $sfield: $sfty ),*) -> $crate::archetypal::ArchetypeHandle {
                    let row = self.$SVariant.len();
                    let handle = $crate::archetypal::__make_handle(
                        const { $Ref::__blaze_archetypal_tag(stringify!($SVariant)) },
                        row,
                        stringify!($SVariant),
                    );
                    self.$SVariant.push($SVariant { $( $sfield ),* });
                    handle
                }

                #[inline]
                $vis fn $sget(&self, h: $crate::archetypal::ArchetypeHandle) -> Option<&$SVariant> {
                    if h.tag() != const { $Ref::__blaze_archetypal_tag(stringify!($SVariant)) } {
                        return None;
                    }
                    self.$SVariant.get(h.row() as usize)
                }

                #[inline]
                $vis fn $sslice(&self) -> &[$SVariant] {
                    self.$SVariant.as_slice()
                }

                #[inline]
                $vis fn $sslice_mut(&mut self) -> &mut [$SVariant] {
                    self.$SVariant.as_mut_slice()
                }
            )*)?
        }
    };
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::*;
    use crate::{Vec, testing};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct BigPayload {
        bytes: [u8; 64],
    }

    archetypal! {
        enum NodeStore, #[derive(Debug, PartialEq, Eq)] NodeRef {
            unit {
                Empty => push_empty / get_empty / empty_count,
            }
            tuple {
                Small(u32) => push_small / get_small / small_slice / small_slice_mut,
                Large(BigPayload) => push_large / get_large / large_slice / large_slice_mut,
            }
            struct {
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                Call { callee: u32, args: u32 } => push_call / get_call / call_slice / call_slice_mut,
            }
        }
    }

    #[test]
    fn archetype_handle_size_round_trip_and_overflow() {
        assert_eq!(size_of::<ArchetypeHandle>(), 4);
        assert_eq!(MAX_ROWS_PER_VARIANT, (1 << 24) - 1);

        // Min and max valid bounds across tags.
        for &tag in &[0u8, 1, 127, 255] {
            for &row in &[0u32, 1, 42, MAX_ROWS_PER_VARIANT] {
                let h = ArchetypeHandle::new(tag, row).expect("in-range handle");
                assert_eq!(h.tag(), tag);
                assert_eq!(h.row(), row);
                assert_eq!(ArchetypeHandle::from_raw(h.raw()), h);
            }
            // Refuses any row past 2^24 - 1.
            assert_eq!(ArchetypeHandle::new(tag, MAX_ROWS_PER_VARIANT + 1), None);
            assert_eq!(ArchetypeHandle::new(tag, u32::MAX), None);
        }
    }

    #[test]
    fn archetypal_mixed_unit_small_struct_and_large_variants() {
        let alloc = testing::heap().alloc();
        let mut store = NodeStore::new_in(alloc);
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        let h_empty_0 = store.push_empty();
        let h_empty_1 = store.push_empty();
        let mut small_handles = Vec::new_in(alloc);
        for v in 0..100u32 {
            small_handles.push(store.push_small(v * 10));
        }
        let h_call = store.push_call(7, 3);
        let h_large = store.push_large(BigPayload { bytes: [0xAB; 64] });

        assert!(!store.is_empty());
        assert_eq!(store.len(), 104);
        assert_eq!(store.empty_count(), 2);

        // Dense per-variant slice layout: 4-byte Small column has zero bloat from 64-byte Large!
        assert_eq!(size_of::<BigPayload>(), 64);
        let smalls: &[u32] = store.small_slice();
        assert_eq!(smalls.len(), 100);
        assert_eq!(core::mem::size_of_val(smalls), 100 * 4);
        let sum: u32 = smalls.iter().copied().sum();
        assert_eq!(sum, (0..100u32).map(|v| v * 10).sum::<u32>());

        // Mutate via slice_mut without touching other variants.
        for x in store.small_slice_mut() {
            *x += 1;
        }
        assert_eq!(store.get_small(small_handles[5]), Some(&51));

        // Typed get() and per-variant get_<variant>() lookups.
        assert_eq!(store.get(h_empty_0), Some(NodeRef::Empty));
        assert_eq!(store.get(h_empty_1), Some(NodeRef::Empty));
        assert_eq!(store.get_empty(h_empty_0), Some(()));
        assert_eq!(store.get_empty(h_call), None);

        assert_eq!(store.get(small_handles[0]), Some(NodeRef::Small(&1)));
        assert_eq!(store.get_small(h_empty_0), None);
        assert_eq!(store.get_small(h_call), None);
        assert_eq!(store.get_small(h_large), None);

        assert_eq!(store.get_call(h_call), Some(&Call { callee: 7, args: 3 }));
        assert_eq!(store.get(h_call), Some(NodeRef::Call(&Call { callee: 7, args: 3 })));
        assert_eq!(store.get_call(small_handles[0]), None);
        assert_eq!(store.call_slice(), &[Call { callee: 7, args: 3 }]);

        assert_eq!(store.get_large(h_large), Some(&BigPayload { bytes: [0xAB; 64] }));
        assert_eq!(store.get(h_large), Some(NodeRef::Large(&BigPayload { bytes: [0xAB; 64] })));
        assert_eq!(store.get_large(h_call), None);
        assert_eq!(store.large_slice().len(), 1);

        // Out-of-bounds row on valid tag and unknown tag both return None.
        let oob_small = ArchetypeHandle::new(small_handles[0].tag(), 999).unwrap();
        assert_eq!(store.get_small(oob_small), None);
        assert_eq!(store.get(oob_small), None);

        let oob_empty = ArchetypeHandle::new(h_empty_0.tag(), 2).unwrap();
        assert_eq!(store.get_empty(oob_empty), None);
        assert_eq!(store.get(oob_empty), None);

        let unknown_tag = ArchetypeHandle::new(200, 0).unwrap();
        assert_eq!(store.get(unknown_tag), None);

        // Clear resets every column and unit counter.
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.empty_count(), 0);
        assert_eq!(store.get(h_empty_0), None);
        assert_eq!(store.get(small_handles[0]), None);
    }

    #[test]
    fn unit_variant_allocates_zero_bytes() {
        let count = testing::Counting::new(testing::heap().alloc());
        let mut store = NodeStore::new_in(&count);
        for _ in 0..10_000 {
            let _ = store.push_empty();
        }
        assert_eq!(store.empty_count(), 10_000);
        assert_eq!(store.len(), 10_000);
        // Zero allocations performed for 10,000 unit-variant elements!
        assert_eq!(count.live(), 0);
    }

    #[test]
    #[should_panic(expected = "MAX_ROWS_PER_VARIANT")]
    fn make_handle_panics_on_row_overflow() {
        let _ = __make_handle(0, (MAX_ROWS_PER_VARIANT as usize) + 1, "Overflow");
    }
}
