//! TYPED QUANTITIES (D184): a model cannot add metres to degrees, a profile cannot add wall time
//! to CPU time, and a value has ONE key however its author computed it.
//!
//! # The closed table
//!
//! | name | holds | base unit | why it is here |
//! |---|---|---|---|
//! | [`Length`] | `i64` | nanometre | every authored dimension, offset, radius and tolerance of the CAD kernel (D183) |
//! | [`Angle`] | `i64` | micro-arcsecond (1/1,296,000,000,000 turn) | rotations, twists, arcs, revolves, draft |
//! | [`Area`] | `f64` | square metre | sections and the checks' answers (`Length * Length`) |
//! | [`Volume`] | `f64` | cubic metre | mass properties (`Area * Length`) |
//! | [`Mass`] | `f64` | kilogram | a part's mass, from the Python model's `Mass` |
//! | [`Density`] | `f64` | kilogram per cubic metre | a material: `Mass / Volume`, `Density * Volume = Mass` |
//! | [`Nanos<K>`] | `u64` | nanosecond | telemetry: kinds [`Wall`], [`Cpu`] |
//! | [`Bytes<K>`] | `u64` | byte | telemetry: kinds [`Allocated`], [`Resident`], [`Read`], [`Written`], [`Hashed`] |
//! | [`Count<K>`] | `u64` | one | telemetry: the kind ([`Counted`]) names what is counted |
//!
//! Left out because nothing needs them yet: time as a physical quantity, speed, force, energy,
//! temperature. Adding one is a row here.
//!
//! # Authored quantities are exact (D183, "the same number computed two ways")
//!
//! A [`Length`] is a whole number of nanometres and an [`Angle`] a whole number of
//! micro-arcseconds. `Length::mm(0.1) + Length::mm(0.2) == Length::mm(0.3)`, because each
//! constructor rounds its argument once to the grid; a value computed two ways that lands on
//! one grid point is one key. The interpreters' f64 is derived by one fixed conversion
//! (`as_m`: the integer as f64 divided by 10^9, one rounding). Range: +-9.2 * 10^9 m, a
//! kilometre ship with nanometre steps with nine orders of magnitude to spare; an operation
//! leaving it panics (a bug at the caller, never a silent wrap). An angle unit divides a turn
//! by 4, 8, 12, 360 and 3600: quarter, eighth and twelfth turns and every whole degree,
//! arc-minute and arc-second are exact, and blaze-math's `sin_cos_turns` makes the sines and
//! cosines of the first three exact or correctly rounded.
//!
//! Derived quantities ([`Area`], [`Volume`], [`Mass`], [`Density`]) are f64 in base units:
//! they are answers, not keys. A product of two lengths would need 128 bits as an integer (a
//! square kilometre in square nanometres is 10^24) for no key that needs it.
//!
//! # Units in and out by name only
//!
//! No `From<f64>`, `Into`, `Deref` or `as` path: `Length::mm(2.0)` in, `.as_mm()` out;
//! `Angle::degrees(30.0)`, `.as_radians()`. A dimensionless result is a plain `f64`
//! (`Length / Length`).
//!
//! # Kinds
//!
//! Each quantity takes a KIND, a zero-sized type parameter defaulting to [`Plain`]: a
//! `Length<WallThickness>` cannot be added to a `Length<Plain>`. Sums, differences and
//! comparisons need one kind; products and quotients give plain kinds. The telemetry kinds
//! have no default: a `Nanos` is always `Nanos<Wall>` or `Nanos<Cpu>`.
//!
//! # The form this table waits for
//!
//! The end state is one type `Quantity<const D: Dim, K>`, `Dim` a struct of small integer
//! exponents (length, angle, mass, time, bytes), with `Mul` giving `Quantity<{ D1.add(D2) }, _>`.
//! That needs `generic_const_exprs` (and `adt_const_params` for a struct-valued parameter),
//! incomplete on the pinned nightly, so it is not used. Every public name here is a struct a
//! future `type Length<K = Plain> = Quantity<LENGTH, K>` can stand behind with the same
//! constructors and accessors; the integer-held ones keep their representation as a
//! parameter of that form.
//!
//! # Printing
//!
//! [`blaze_text::Show`] for every quantity picks the unit for people (`2 mm`, `1.4 m`, `45°`,
//! `12.5ms`, `1.40 GiB`), in `show.rs` only. Durations and byte counts are blaze-text's own
//! `duration` and `byte_count` (rule 30: one home for each), which print binary byte units.
//!
//! # Not duplicated: `blaze_int::time`
//!
//! `blaze_int::time::Nanos(pub u64)` is an instant or a length of time with a `Clock` and a
//! `Deadline`. [`Nanos<K>`] here is only a length of time of a kind. The end state is one home:
//! see `telemetry.rs` for the plan given to blaze-int's owner.

#![no_std]
#![feature(register_tool, custom_inner_attributes, core_float_math)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod exact;
mod float;
mod kind;
mod show;
mod telemetry;

#[cfg(test)]
mod tests;

pub use exact::{Angle, Length, UnitsPlant};
pub use float::{Area, Density, Mass, Volume};
pub use kind::{Allocated, ByteKind, Counted, Cpu, Hashed, Kind, Plain, Read, Resident, TimeKind, Wall, Written};
pub use telemetry::{Bytes, Count, Nanos};
