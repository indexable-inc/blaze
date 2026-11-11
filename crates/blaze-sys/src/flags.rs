//! Flags as typed sets: a set of one call's flags is its own type, so a flag of another call
//! is a type error and not a wrong bit.

/// A set of flags over `$repr`, with its named members.
macro_rules! flag_set {
    (
        $(#[$meta:meta])*
        $name:ident: $repr:ty {
            $($(#[$flag_meta:meta])* $flag:ident = $value:expr;)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        #[repr(transparent)]
        pub struct $name($repr);

        impl $name {
            /// No flag.
            pub const NONE: $name = $name(0);
            $($(#[$flag_meta])* pub const $flag: $name = $name($value);)*

            /// The bits, as the kernel takes them.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn bits(self) -> $repr {
                self.0
            }

            /// Both sets' flags.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn with(self, other: $name) -> $name {
                $name(self.0 | other.0)
            }

            /// Whether every flag of `other` is in this set.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn contains(self, other: $name) -> bool {
                self.0 & other.0 == other.0
            }

            /// The set the kernel wrote: bits it names that this type does not are kept.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn from_bits(bits: $repr) -> $name {
                $name(bits)
            }
        }

        impl core::ops::BitOr for $name {
            type Output = $name;

            fn bitor(self, other: $name) -> $name {
                self.with(other)
            }
        }
    };
}
