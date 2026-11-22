//! The derived quantities, f64 in base units: [`Area`] (m^2), [`Volume`] (m^3), [`Mass`] (kg),
//! [`Density`] (kg/m^3). Answers, not keys (see the crate's doc). Each constructor refuses a
//! value that is not finite, so comparison is total on every value that exists.

use core::{
    marker::PhantomData,
    ops::{Add, Div, Mul, Neg, Sub},
};

use crate::{Length, kind::Kind, kind::Plain};

macro_rules! float_quantity {
    (
        $(#[$doc:meta])*
        $name:ident, $field:ident, base $base:ident / $as_base:ident,
        units [$($(#[$udoc:meta])* $unit:ident / $as_unit:ident = $per:expr;)*]
    ) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, PartialOrd, Debug, Default)]
        pub struct $name<K: Kind = Plain> {
            $field: f64,
            kind: PhantomData<K>,
        }

        impl<K: Kind> $name<K> {
            /// None of it.
            pub const ZERO: $name<K> = $name { $field: 0.0, kind: PhantomData };

            #[doc = concat!("`x` in the base unit, ", stringify!($base), ".")]
            ///
            /// # Panics
            ///
            /// When `x` is NaN or infinite.
            pub fn $base(x: f64) -> $name<K> {
                assert!(x.is_finite(), concat!("blaze-units: ", stringify!($name), "::", stringify!($base), " of a value that is not finite"));
                $name { $field: x, kind: PhantomData }
            }

            #[doc = concat!("In the base unit, ", stringify!($base), ".")]
            pub fn $as_base(self) -> f64 {
                self.$field
            }

            $(
                $(#[$udoc])*
                ///
                /// # Panics
                ///
                /// When `x` is NaN or infinite.
                pub fn $unit(x: f64) -> $name<K> {
                    $name::$base(x * $per)
                }

                #[doc = concat!("In ", stringify!($unit), " (one rounding).")]
                pub fn $as_unit(self) -> f64 {
                    self.$field / $per
                }
            )*

            /// The magnitude.
            pub fn abs(self) -> $name<K> {
                $name { $field: self.$field.abs(), kind: PhantomData }
            }

            /// The same quantity as a plain one.
            pub fn plain(self) -> $name<Plain> {
                $name { $field: self.$field, kind: PhantomData }
            }
        }

        impl<K: Kind> Add for $name<K> {
            type Output = $name<K>;
            fn add(self, o: $name<K>) -> $name<K> {
                $name::$base(self.$field + o.$field)
            }
        }

        impl<K: Kind> Sub for $name<K> {
            type Output = $name<K>;
            fn sub(self, o: $name<K>) -> $name<K> {
                $name::$base(self.$field - o.$field)
            }
        }

        impl<K: Kind> Neg for $name<K> {
            type Output = $name<K>;
            fn neg(self) -> $name<K> {
                $name { $field: -self.$field, kind: PhantomData }
            }
        }

        impl<K: Kind> Mul<f64> for $name<K> {
            type Output = $name<K>;
            fn mul(self, k: f64) -> $name<K> {
                $name::$base(self.$field * k)
            }
        }

        impl<K: Kind> Div<f64> for $name<K> {
            type Output = $name<K>;
            fn div(self, k: f64) -> $name<K> {
                $name::$base(self.$field / k)
            }
        }

        impl<K: Kind> Div for $name<K> {
            type Output = f64;
            fn div(self, o: $name<K>) -> f64 {
                self.$field / o.$field
            }
        }
    };
}

float_quantity! {
    /// An area, in square metres.
    Area, m2, base m2 / as_m2,
    units [
        /// `x` square millimetres.
        mm2 / as_mm2 = 1e-6;
    ]
}

float_quantity! {
    /// A volume, in cubic metres.
    Volume, m3, base m3 / as_m3,
    units [
        /// `x` cubic millimetres.
        mm3 / as_mm3 = 1e-9;
        /// `x` litres.
        litres / as_litres = 1e-3;
    ]
}

float_quantity! {
    /// A mass, in kilograms.
    Mass, kg, base kg / as_kg,
    units [
        /// `x` grams.
        g / as_g = 1e-3;
        /// `x` tonnes.
        tonnes / as_tonnes = 1e3;
    ]
}

float_quantity! {
    /// A density, in kilograms per cubic metre.
    Density, kg_per_m3, base kg_per_m3 / as_kg_per_m3,
    units [
        /// `x` grams per cubic centimetre.
        g_per_cm3 / as_g_per_cm3 = 1e3;
    ]
}

impl<K1: Kind, K2: Kind> Mul<Length<K2>> for Area<K1> {
    type Output = Volume;
    /// The volume of the prism of this base and that height.
    fn mul(self, h: Length<K2>) -> Volume {
        Volume::m3(self.as_m2() * h.as_m())
    }
}

impl<K1: Kind, K2: Kind> Div<Length<K2>> for Area<K1> {
    type Output = f64;
    /// Not a length: an area over a length is a length only as f64 metres, and a length is
    /// authored, not derived, so the one way back to [`Length`] is `Length::m(..)` by name.
    fn div(self, l: Length<K2>) -> f64 {
        self.as_m2() / l.as_m()
    }
}

impl<K1: Kind, K2: Kind> Div<Volume<K2>> for Mass<K1> {
    type Output = Density;
    /// Mass per volume.
    fn div(self, v: Volume<K2>) -> Density {
        Density::kg_per_m3(self.as_kg() / v.as_m3())
    }
}

impl<K1: Kind, K2: Kind> Mul<Volume<K2>> for Density<K1> {
    type Output = Mass;
    /// The mass of that volume of this material.
    fn mul(self, v: Volume<K2>) -> Mass {
        Mass::kg(self.as_kg_per_m3() * v.as_m3())
    }
}

impl<K1: Kind, K2: Kind> Div<Density<K2>> for Mass<K1> {
    type Output = Volume;
    /// The volume this mass of that material fills.
    fn div(self, d: Density<K2>) -> Volume {
        Volume::m3(self.as_kg() / d.as_kg_per_m3())
    }
}
