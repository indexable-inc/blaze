//! Explicit newtypes, affine spaces (`Offset` vs `Len`), and Kotlin-style selective delegation
//! (rules 76 `correct-by-construction` & 85 `newtypes-and-named-returns`).
//!
//! # Why explicit newtypes instead of blanket `Deref`
//!
//! Putting `Deref<Target = u64>` on both `ByteOffset` and `ByteLen` defeats the newtype: any
//! function taking `&u64` accepts both, and `*offset + *offset` compiles silently. Instead:
//!
//! - **Affine pairs (`Offset` + `Len` → `Offset`, `Offset - Offset` → `Len`)**: positions and
//!   displacements are distinct types, just like `Instant` vs `Duration`. `ByteOffset + ByteOffset`
//!   is a compile error; `ByteOffset + ByteLen` yields `ByteOffset`; `ByteOffset - ByteOffset`
//!   yields `ByteLen`.
//! - **Quantities (`Count`, `Capacity`, `Stride`)**: additive scalars with explicit `::new` /
//!   `.get()` and checked/saturating arithmetic, never implicit `From<usize>`.
//! - **Opaque identifiers (`Port`, `LineNo`, `ColNo`)**: equality, ordering, and hashing only —
//!   no arithmetic at all.
//! - **Selective method delegation (`wrap ... delegate { ... }`)**: Kotlin `by`-style forwarding
//!   of ONLY the methods that preserve the wrapper's invariant (plus nightly `#[feature(fn_delegation)]`
//!   `reuse Trait::{...} { self.0 }` for trait delegation). Read-only `Deref` is reserved strictly
//!   for transparent slice/view wrappers (`view`) whose borrowed target cannot break invariants.

use core::num::NonZeroUsize;

/// Declare explicit, zero-cost (`#[repr(transparent)]`) newtypes with sound algebra or
/// Kotlin-style selective method delegation.
///
/// # Forms
///
/// 1. **Affine point + displacement + span** (`Point + Delta -> Point`, `Point - Point -> Delta`,
///    `Point + Point` forbidden):
///    ```ignore
///    blaze_kit::newtype! {
///        affine {
///            pub struct FileOffset(u64);
///            pub struct FileLen(u64);
///            pub struct FileSpan;
///        }
///    }
///    ```
/// 2. **Additive quantity** (`new`/`get`/`ZERO`/`ONE`, checked & saturating ops, `Add`/`Sub`):
///    ```ignore
///    blaze_kit::newtype! {
///        quantity pub struct Retries(u32);
///    }
///    ```
/// 3. **Opaque non-arithmetic wrapper** (`new`/`get`, `Eq`/`Ord`/`Hash`/`Debug`/`Display`):
///    ```ignore
///    blaze_kit::newtype! {
///        opaque pub struct Port(u16);
///    }
///    ```
/// 4. **Wrapped type with explicit Kotlin-style method delegation** (no leaky `DerefMut`):
///    ```ignore
///    blaze_kit::newtype! {
///        wrap pub struct Label(str_like) {
///            delegate {
///                pub fn len(&self) -> usize;
///                pub fn is_empty(&self) -> bool;
///            }
///        }
///    }
///    ```
/// 5. **Read-only transparent view** (implements `Deref<Target = Inner>`, never `DerefMut`):
///    ```ignore
///    blaze_kit::newtype! {
///        view pub struct SortedBytes([u8]);
///    }
///    ```
#[macro_export]
macro_rules! newtype {
    // 1. Affine space: Point + Delta + Span (Offset vs Len).
    (
        affine {
            $(#[$pmeta:meta])* $pvis:vis struct $point:ident($pty:ty);
            $(#[$dmeta:meta])* $dvis:vis struct $delta:ident($dty:ty);
            $(#[$smeta:meta])* $svis:vis struct $span:ident;
        }
    ) => {
        $(#[$pmeta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        $pvis struct $point($pty);

        $(#[$dmeta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        $dvis struct $delta($dty);

        $(#[$smeta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        $svis struct $span {
            start: $point,
            len: $delta,
        }

        impl $point {
            pub const ZERO: Self = Self(0);

            #[inline(always)]
            pub const fn new(raw: $pty) -> Self {
                Self(raw)
            }

            #[inline(always)]
            pub const fn get(self) -> $pty {
                self.0
            }

            #[inline(always)]
            pub fn as_usize(self) -> ::core::option::Option<usize> {
                usize::try_from(self.0).ok()
            }

            #[inline(always)]
            pub const fn checked_add(self, len: $delta) -> ::core::option::Option<Self> {
                match self.0.checked_add(len.0 as $pty) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn saturating_add(self, len: $delta) -> Self {
                Self(self.0.saturating_add(len.0 as $pty))
            }

            #[inline(always)]
            pub const fn checked_sub(self, len: $delta) -> ::core::option::Option<Self> {
                match self.0.checked_sub(len.0 as $pty) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn checked_diff(self, earlier: Self) -> ::core::option::Option<$delta> {
                match self.0.checked_sub(earlier.0) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some($delta(v as $dty)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn span(self, len: $delta) -> $span {
                $span { start: self, len }
            }
        }

        impl $delta {
            pub const ZERO: Self = Self(0);
            pub const ONE: Self = Self(1);

            #[inline(always)]
            pub const fn new(raw: $dty) -> Self {
                Self(raw)
            }

            #[inline(always)]
            pub const fn get(self) -> $dty {
                self.0
            }

            #[inline(always)]
            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            #[inline(always)]
            pub fn as_usize(self) -> ::core::option::Option<usize> {
                usize::try_from(self.0).ok()
            }

            #[inline(always)]
            pub const fn checked_add(self, rhs: Self) -> ::core::option::Option<Self> {
                match self.0.checked_add(rhs.0) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn saturating_add(self, rhs: Self) -> Self {
                Self(self.0.saturating_add(rhs.0))
            }

            #[inline(always)]
            pub const fn checked_sub(self, rhs: Self) -> ::core::option::Option<Self> {
                match self.0.checked_sub(rhs.0) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn saturating_sub(self, rhs: Self) -> Self {
                Self(self.0.saturating_sub(rhs.0))
            }

            #[inline(always)]
            pub const fn checked_mul(self, factor: $dty) -> ::core::option::Option<Self> {
                match self.0.checked_mul(factor) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }
        }

        impl ::core::ops::Add<$delta> for $point {
            type Output = $point;
            #[inline(always)]
            #[track_caller]
            fn add(self, rhs: $delta) -> $point {
                self.checked_add(rhs).expect(::core::concat!(::core::stringify!($point), " + ", ::core::stringify!($delta), " overflowed"))
            }
        }

        impl ::core::ops::AddAssign<$delta> for $point {
            #[inline(always)]
            #[track_caller]
            fn add_assign(&mut self, rhs: $delta) {
                *self = *self + rhs;
            }
        }

        impl ::core::ops::Sub<$delta> for $point {
            type Output = $point;
            #[inline(always)]
            #[track_caller]
            fn sub(self, rhs: $delta) -> $point {
                self.checked_sub(rhs).expect(::core::concat!(::core::stringify!($point), " - ", ::core::stringify!($delta), " underflowed"))
            }
        }

        impl ::core::ops::SubAssign<$delta> for $point {
            #[inline(always)]
            #[track_caller]
            fn sub_assign(&mut self, rhs: $delta) {
                *self = *self - rhs;
            }
        }

        impl ::core::ops::Sub<$point> for $point {
            type Output = $delta;
            #[inline(always)]
            #[track_caller]
            fn sub(self, earlier: $point) -> $delta {
                self.checked_diff(earlier).expect(::core::concat!(::core::stringify!($point), " - ", ::core::stringify!($point), " underflowed"))
            }
        }

        impl ::core::ops::Add<$delta> for $delta {
            type Output = $delta;
            #[inline(always)]
            #[track_caller]
            fn add(self, rhs: $delta) -> $delta {
                self.checked_add(rhs).expect(::core::concat!(::core::stringify!($delta), " + ", ::core::stringify!($delta), " overflowed"))
            }
        }

        impl ::core::ops::AddAssign<$delta> for $delta {
            #[inline(always)]
            #[track_caller]
            fn add_assign(&mut self, rhs: $delta) {
                *self = *self + rhs;
            }
        }

        impl ::core::ops::Sub<$delta> for $delta {
            type Output = $delta;
            #[inline(always)]
            #[track_caller]
            fn sub(self, rhs: $delta) -> $delta {
                self.checked_sub(rhs).expect(::core::concat!(::core::stringify!($delta), " - ", ::core::stringify!($delta), " underflowed"))
            }
        }

        impl ::core::ops::SubAssign<$delta> for $delta {
            #[inline(always)]
            #[track_caller]
            fn sub_assign(&mut self, rhs: $delta) {
                *self = *self - rhs;
            }
        }

        impl $span {
            pub const EMPTY: Self = Self { start: $point::ZERO, len: $delta::ZERO };

            #[inline(always)]
            pub const fn new(start: $point, len: $delta) -> Self {
                Self { start, len }
            }

            #[inline(always)]
            pub const fn between(start: $point, end: $point) -> ::core::option::Option<Self> {
                match end.checked_diff(start) {
                    ::core::option::Option::Some(len) => ::core::option::Option::Some(Self { start, len }),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn start(self) -> $point {
                self.start
            }

            #[inline(always)]
            pub const fn len(self) -> $delta {
                self.len
            }

            #[inline(always)]
            pub const fn is_empty(self) -> bool {
                self.len.is_zero()
            }

            #[inline(always)]
            pub const fn end(self) -> ::core::option::Option<$point> {
                self.start.checked_add(self.len)
            }

            #[inline(always)]
            pub const fn contains(self, pos: $point) -> bool {
                match self.end() {
                    ::core::option::Option::Some(end) => pos.0 >= self.start.0 && pos.0 < end.0,
                    ::core::option::Option::None => false,
                }
            }

            #[inline(always)]
            pub fn range(self) -> ::core::option::Option<::core::ops::Range<usize>> {
                let start = self.start.as_usize()?;
                let end = self.end()?.as_usize()?;
                ::core::option::Option::Some(start..end)
            }

            #[inline(always)]
            pub fn slice<'a, T>(self, buf: &'a [T]) -> ::core::option::Option<&'a [T]> {
                buf.get(self.range()?)
            }
        }

        impl ::core::fmt::Debug for $point {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({})", ::core::stringify!($point), self.0)
            }
        }

        impl ::core::fmt::Display for $point {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::core::fmt::Debug for $delta {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({})", ::core::stringify!($delta), self.0)
            }
        }

        impl ::core::fmt::Display for $delta {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::core::fmt::Debug for $span {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({}..+{})", ::core::stringify!($span), self.start.0, self.len.0)
            }
        }
    };

    // 2. Additive quantity newtype (Count, Capacity, Stride, etc.).
    (
        quantity $(#[$meta:meta])* $vis:vis struct $name:ident($ty:ty);
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        $vis struct $name($ty);

        impl $name {
            pub const ZERO: Self = Self(0);
            pub const ONE: Self = Self(1);

            #[inline(always)]
            pub const fn new(raw: $ty) -> Self {
                Self(raw)
            }

            #[inline(always)]
            pub const fn get(self) -> $ty {
                self.0
            }

            #[inline(always)]
            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            #[inline(always)]
            pub fn as_usize(self) -> ::core::option::Option<usize> {
                usize::try_from(self.0).ok()
            }

            #[inline(always)]
            pub const fn checked_add(self, rhs: Self) -> ::core::option::Option<Self> {
                match self.0.checked_add(rhs.0) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn saturating_add(self, rhs: Self) -> Self {
                Self(self.0.saturating_add(rhs.0))
            }

            #[inline(always)]
            pub const fn checked_sub(self, rhs: Self) -> ::core::option::Option<Self> {
                match self.0.checked_sub(rhs.0) {
                    ::core::option::Option::Some(v) => ::core::option::Option::Some(Self(v)),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            #[inline(always)]
            pub const fn saturating_sub(self, rhs: Self) -> Self {
                Self(self.0.saturating_sub(rhs.0))
            }
        }

        impl ::core::ops::Add for $name {
            type Output = Self;
            #[inline(always)]
            #[track_caller]
            fn add(self, rhs: Self) -> Self {
                self.checked_add(rhs).expect(::core::concat!(::core::stringify!($name), " addition overflowed"))
            }
        }

        impl ::core::ops::AddAssign for $name {
            #[inline(always)]
            #[track_caller]
            fn add_assign(&mut self, rhs: Self) {
                *self = *self + rhs;
            }
        }

        impl ::core::ops::Sub for $name {
            type Output = Self;
            #[inline(always)]
            #[track_caller]
            fn sub(self, rhs: Self) -> Self {
                self.checked_sub(rhs).expect(::core::concat!(::core::stringify!($name), " subtraction underflowed"))
            }
        }

        impl ::core::ops::SubAssign for $name {
            #[inline(always)]
            #[track_caller]
            fn sub_assign(&mut self, rhs: Self) {
                *self = *self - rhs;
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({})", ::core::stringify!($name), self.0)
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }
    };

    // 3. Opaque non-arithmetic newtype (Port, Epoch, LineNo, Generation, etc.).
    (
        opaque $(#[$meta:meta])* $vis:vis struct $name:ident($ty:ty);
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        $vis struct $name($ty);

        impl $name {
            #[inline(always)]
            pub const fn new(raw: $ty) -> Self {
                Self(raw)
            }

            #[inline(always)]
            pub const fn get(self) -> $ty {
                self.0
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({})", ::core::stringify!($name), self.0)
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }
    };

    // 4. Kotlin `by`-style selective method delegation on a wrapper newtype (never leaks DerefMut).
    (
        wrap $(#[$meta:meta])* $vis:vis struct $name:ident($inner:ty) {
            delegate {
                $(
                    $mvis:vis fn $mname:ident(&self $(, $arg:ident : $aty:ty)*) $(-> $ret:ty)?;
                )*
            }
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[repr(transparent)]
        $vis struct $name($inner);

        impl $name {
            #[inline(always)]
            $vis const fn new(inner: $inner) -> Self {
                Self(inner)
            }

            #[inline(always)]
            $vis const fn inner(&self) -> &$inner {
                &self.0
            }

            #[inline(always)]
            $vis fn into_inner(self) -> $inner {
                self.0
            }

            $(
                #[inline(always)]
                $mvis fn $mname(&self $(, $arg: $aty)*) $(-> $ret)? {
                    self.0.$mname($($arg),*)
                }
            )*
        }
    };

    // 5. Read-only transparent view wrapper (`Deref<Target = Inner>`, never `DerefMut`).
    (
        view $(#[$meta:meta])* $vis:vis struct $name:ident($inner:ty);
    ) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[repr(transparent)]
        $vis struct $name($inner);

        impl $name {
            #[inline(always)]
            $vis const fn new(inner: $inner) -> Self {
                Self(inner)
            }

            #[inline(always)]
            $vis fn into_inner(self) -> $inner {
                self.0
            }
        }

        impl ::core::ops::Deref for $name {
            type Target = $inner;
            #[inline(always)]
            fn deref(&self) -> &$inner {
                &self.0
            }
        }

        impl ::core::convert::AsRef<$inner> for $name {
            #[inline(always)]
            fn as_ref(&self) -> &$inner {
                &self.0
            }
        }
    };
}

// --- Built-in universal newtypes ---

newtype! {
    affine {
        /// Byte offset from the start of a file, record, or buffer (`ByteOffset + ByteOffset` is forbidden).
        pub struct ByteOffset(u64);
        /// Byte length / displacement (`ByteOffset + ByteLen -> ByteOffset`, `ByteOffset - ByteOffset -> ByteLen`).
        pub struct ByteLen(u64);
        /// Half-open byte span `[start, start + len)`.
        pub struct ByteSpan;
    }
}

newtype! {
    affine {
        /// Item-relative UTF-8 byte offset within a source item (rule 71 `item-relative-positions`).
        pub struct TextOffset(u32);
        /// Item-relative UTF-8 byte length.
        pub struct TextLen(u32);
        /// Item-relative half-open text span `[start, start + len)`.
        pub struct TextSpan;
    }
}

newtype! {
    quantity
    /// Non-negative item count.
    pub struct Count(u64);
}

newtype! {
    quantity
    /// Pre-allocated element capacity.
    pub struct Capacity(usize);
}

newtype! {
    quantity
    /// Fixed element stride in bytes.
    pub struct Stride(usize);
}

newtype! {
    opaque
    /// TCP/UDP network port number (distinct from lengths, counts, and indices).
    pub struct Port(u16);
}

newtype! {
    opaque
    /// 1-based source line number (printed only; never stored in item-relative keys).
    pub struct LineNo(u32);
}

newtype! {
    opaque
    /// 1-based source column number (printed only; never stored in item-relative keys).
    pub struct ColNo(u32);
}

/// Power-of-two byte alignment, validated once at construction (`correct-by-construction`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Alignment(NonZeroUsize);

impl Alignment {
    pub const ONE: Self = Self(NonZeroUsize::MIN);

    /// Construct an alignment if `bytes` is a power of two (`1, 2, 4, 8, ...`).
    #[inline(always)]
    pub const fn new(bytes: usize) -> Option<Self> {
        if bytes.is_power_of_two() {
            match NonZeroUsize::new(bytes) {
                Some(nz) => Some(Self(nz)),
                None => None,
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub const fn get(self) -> usize {
        self.0.get()
    }

    /// True if `offset` is a multiple of this alignment.
    #[inline(always)]
    pub const fn is_aligned(self, offset: ByteOffset) -> bool {
        (offset.get() & (self.0.get() as u64 - 1)) == 0
    }

    /// Round `offset` up to the next multiple of this alignment, or `None` on overflow.
    #[inline(always)]
    pub const fn align_up(self, offset: ByteOffset) -> Option<ByteOffset> {
        let mask = self.0.get() as u64 - 1;
        match offset.get().checked_add(mask) {
            Some(v) => Some(ByteOffset::new(v & !mask)),
            None => None,
        }
    }

    /// Padding needed to advance `offset` to the next multiple of this alignment.
    #[inline(always)]
    pub const fn padding_for(self, offset: ByteOffset) -> Option<ByteLen> {
        match self.align_up(offset) {
            Some(aligned) => aligned.checked_diff(offset),
            None => None,
        }
    }
}

impl core::fmt::Debug for Alignment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Alignment({})", self.0.get())
    }
}

impl core::fmt::Display for Alignment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0.get(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    newtype! {
        wrap struct NonEmptyLabel(&'static str) {
            delegate {
                fn len(&self) -> usize;
                fn is_empty(&self) -> bool;
                fn as_bytes(&self) -> &[u8];
            }
        }
    }

    newtype! {
        view struct ReadOnlyTag(&'static str);
    }

    #[test]
    fn affine_offset_len_span_algebra() {
        let base = ByteOffset::new(10);
        let len = ByteLen::new(6);
        let end = base + len;
        assert_eq!(end, ByteOffset::new(16));
        assert_eq!(end - base, len);
        assert_eq!(end - len, base);
        assert_eq!(len + ByteLen::new(4), ByteLen::new(10));

        let span = base.span(len);
        assert_eq!(span.end(), Some(end));
        assert!(span.contains(ByteOffset::new(10)));
        assert!(span.contains(ByteOffset::new(15)));
        assert!(!span.contains(ByteOffset::new(16)));
        let buf = b"0123456789abcdef0123";
        assert_eq!(span.slice(buf), Some(&b"abcdef"[..]));
        assert_eq!(ByteSpan::between(base, end), Some(span));
        assert_eq!(ByteSpan::between(end, base), None);
    }

    #[test]
    fn alignment_power_of_two_and_padding() {
        assert_eq!(Alignment::new(0), None);
        assert_eq!(Alignment::new(3), None);
        let a8 = Alignment::new(8).expect("8 is power of two");
        assert!(a8.is_aligned(ByteOffset::new(16)));
        assert!(!a8.is_aligned(ByteOffset::new(13)));
        assert_eq!(a8.align_up(ByteOffset::new(13)), Some(ByteOffset::new(16)));
        assert_eq!(a8.padding_for(ByteOffset::new(13)), Some(ByteLen::new(3)));
    }

    #[test]
    fn selective_delegation_and_readonly_view() {
        let lbl = NonEmptyLabel::new("blaze");
        assert_eq!(lbl.len(), 5);
        assert!(!lbl.is_empty());
        assert_eq!(lbl.as_bytes(), b"blaze");

        let tag = ReadOnlyTag::new("v2");
        assert_eq!(*tag, "v2");
        assert_eq!(*tag.as_ref(), "v2");
    }
}
