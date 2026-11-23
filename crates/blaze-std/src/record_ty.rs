//! Zero-cost, `#![forbid(unsafe_code)]` Structural Records & Named Arguments with Defaults
//! (`bstd::record_ty`, `docs/v2/plan/design/records.md`, edition change 4 / objective E5).
//!
//! # Two-Tier Design
//! 1. **Compiler-native records (`#![blaze::records]`)**: backed by `blaze_ty::record_ty` and
//!    `blaze_lower` (`TypeRef::Record`, `Expr::Record`, `Pat::Record`), lowering
//!    `fn obj_row(ui: &uniq UiWorld, p: { name: &str, depth: u32 = 0, open: bool = false })`
//!    directly to `core::record::Record<"depth,name,open", (u32, &str, bool)>`.
//! 2. **Portable zero-cost Rust surface (this module)**: works today under both Blaze and `rustc`
//!    with **zero `unsafe`**, **zero proc macros**, and **zero heap allocation** via
//!    [`crate::record_fn!`] and [`crate::call!`]:
//!
//! ```ignore
//! bstd::record_fn! {
//!     pub fn obj_row<'a>(
//!         ui: &mut UiWorld,
//!         p: { name: &'a str, depth: u32 = 0, open: bool = false }
//!     ) -> RowSlots {
//!         // `p.name`, `p.depth`, `p.open` are ordinary typed fields.
//!         ...
//!     }
//! }
//!
//! // Named arguments with `..` defaults (order-independent; missing required field = compile error):
//! let slots = bstd::call!(obj_row(&mut ui, { open: true, name: "agent-1", .. }));
//!
//! // Exhaustive named arguments without `..` (omitting any field = compile error):
//! let slots = bstd::call!(obj_row(&mut ui, { name: "agent-1", depth: 2, open: false }));
//! ```

#![forbid(unsafe_code)]

/// Compile-time type-state marker: a required record field has **not** yet been supplied.
///
/// Attempting to finish a record builder while any required field is still `Missing` fails at
/// compile time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Missing;

/// Compile-time type-state marker: a required record field has been supplied with value `T`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Provided<T>(pub T);

/// Type-level field list schema for a structural [`Record`].
pub trait FieldSchema {
    /// Canonical comma-separated sorted field names (e.g. `"depth,name,open"`).
    const FIELDS: &'static str;
}

/// Structural anonymous record wrapper mirroring `core::record::Record<FIELDS, T>` for crates
/// compiled without unstable `#![feature(blaze_records)]` or `unsized_const_params`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
#[repr(transparent)]
pub struct Record<S: FieldSchema, T> {
    pub tuple: T,
    _schema: core::marker::PhantomData<S>,
}

impl<S: FieldSchema, T> Record<S, T> {
    #[inline(always)]
    pub const fn new(tuple: T) -> Self {
        Self { tuple, _schema: core::marker::PhantomData }
    }

    #[inline(always)]
    pub const fn fields_str() -> &'static str {
        S::FIELDS
    }

    #[inline(always)]
    pub const fn as_tuple(&self) -> &T {
        &self.tuple
    }

    #[inline(always)]
    pub fn into_tuple(self) -> T {
        self.tuple
    }
}

/// Internal helper macro expanding the per-field builder for [`crate::record_fn!`].
///
/// Required fields (no `= default`) use `Missing` -> `Provided<T>` type-state so omitting any
/// required field — even with `..` — is a compile-time error. Defaulted fields start initialized
/// to their declared default expression and may be overridden in any order.
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_record_builder {
    (
        @collect
        vis: [$vis:vis]
        mod_name: $mod_name:ident
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        req: [$($rname:ident : $rty:ty,)*]
        opt: [$($oname:ident : $oty:ty = $odefault:expr,)*]
        rest: []
    ) => {
        #[allow(non_snake_case, non_camel_case_types, dead_code)]
        $vis mod $mod_name {
            #[allow(unused_imports)]
            use super::*;

            /// The materialized record parameter passed into the function body.
            #[derive(Clone, Copy, Debug, PartialEq)]
            pub struct Args<$($lt,)* $($gen $(: $bound)?,)*> {
                $(pub $rname: $rty,)*
                $(pub $oname: $oty,)*
            }

            /// Type-state builder enforcing every required field is set before calling `.finish()`.
            pub struct Builder<$($lt,)* $($gen $(: $bound)?,)* $($rname,)*> {
                $($rname: $rname,)*
                $($oname: $oty,)*
                _lt: core::marker::PhantomData<($(&$lt (),)* $($gen,)*)>,
            }

            /// Creates a fresh builder with all defaulted fields initialized and required fields
            /// marked [`Missing`]($crate::record_ty::Missing).
            #[inline(always)]
            pub fn builder<$($lt,)* $($gen $(: $bound)?,)*>() -> Builder<$($lt,)* $($gen,)* $($crate::__blaze_missing_ty!($rname),)*> {
                Builder {
                    $($rname: $crate::record_ty::Missing,)*
                    $($oname: $odefault,)*
                    _lt: core::marker::PhantomData,
                }
            }

            // Generate one setter per optional (defaulted) field (preserves all required states).
            impl<$($lt,)* $($gen $(: $bound)?,)* $($rname,)*> Builder<$($lt,)* $($gen,)* $($rname,)*> {
                $(
                    #[inline(always)]
                    pub fn $oname(mut self, value: $oty) -> Self {
                        self.$oname = value;
                        self
                    }
                )*
            }

            // Finish is implemented ONLY when every required field is `Provided<T>`.
            impl<$($lt,)* $($gen $(: $bound)?,)*> Builder<$($lt,)* $($gen,)* $($crate::record_ty::Provided<$rty>,)*> {
                #[inline(always)]
                pub fn finish(self) -> Args<$($lt,)* $($gen,)*> {
                    Args {
                        $($rname: self.$rname.0,)*
                        $($oname: self.$oname,)*
                    }
                }
            }

            $crate::__blaze_record_req_setters! {
                lt: [$($lt),*]
                generics: [$($gen $(: $bound)?),*]
                all_opt: [$($oname),*]
                prefix: []
                pending: [$($rname : $rty,)*]
            }
        }
    };

    // Next field has a default `= expr` (with trailing comma or end).
    (
        @collect
        vis: [$vis:vis]
        mod_name: $mod_name:ident
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        req: [$($rname:ident : $rty:ty,)*]
        opt: [$($oname:ident : $oty:ty = $odefault:expr,)*]
        rest: [$fname:ident : $fty:ty = $fdef:expr $(, $($tail:tt)*)?]
    ) => {
        $crate::__blaze_record_builder! {
            @collect
            vis: [$vis]
            mod_name: $mod_name
            lt: [$($lt),*]
            generics: [$($gen $(: $bound)?),*]
            req: [$($rname : $rty,)*]
            opt: [$($oname : $oty = $odefault,)* $fname : $fty = $fdef,]
            rest: [$($($tail)*)?]
        }
    };

    // Next field is required (no `= default`).
    (
        @collect
        vis: [$vis:vis]
        mod_name: $mod_name:ident
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        req: [$($rname:ident : $rty:ty,)*]
        opt: [$($oname:ident : $oty:ty = $odefault:expr,)*]
        rest: [$fname:ident : $fty:ty $(, $($tail:tt)*)?]
    ) => {
        $crate::__blaze_record_builder! {
            @collect
            vis: [$vis]
            mod_name: $mod_name
            lt: [$($lt),*]
            generics: [$($gen $(: $bound)?),*]
            req: [$($rname : $rty,)* $fname : $fty,]
            opt: [$($oname : $oty = $odefault,)*]
            rest: [$($($tail)*)?]
        }
    };
}

/// Internal helper turning a field identifier into the `Missing` type token.
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_missing_ty {
    ($id:ident) => {
        $crate::record_ty::Missing
    };
}

/// Internal helper generating one type-state-transitioning setter per required field.
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_record_req_setters {
    (
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        all_opt: [$($oname:ident),*]
        prefix: [$($pname:ident),*]
        pending: []
    ) => {};
    (
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        all_opt: [$($oname:ident),*]
        prefix: [$($pname:ident),*]
        pending: [$cur:ident : $cur_ty:ty, $($rest:ident : $rest_ty:ty,)*]
    ) => {
        impl<$($lt,)* $($gen $(: $bound)?,)* $($pname,)* $($rest,)*>
            Builder<$($lt,)* $($gen,)* $($pname,)* $crate::record_ty::Missing, $($rest,)*>
        {
            #[inline(always)]
            pub fn $cur(
                self,
                value: $cur_ty,
            ) -> Builder<$($lt,)* $($gen,)* $($pname,)* $crate::record_ty::Provided<$cur_ty>, $($rest,)*> {
                Builder {
                    $($pname: self.$pname,)*
                    $cur: $crate::record_ty::Provided(value),
                    $($rest: self.$rest,)*
                    $($oname: self.$oname,)*
                    _lt: core::marker::PhantomData,
                }
            }
        }
        $crate::__blaze_record_req_setters! {
            lt: [$($lt),*]
            generics: [$($gen $(: $bound)?),*]
            all_opt: [$($oname),*]
            prefix: [$($pname,)* $cur]
            pending: [$($rest : $rest_ty,)*]
        }
    };
}

/// Defines a function whose final parameter is a named-argument record type with optional
/// field defaults (`docs/v2/plan/design/records.md`).
///
/// Supports zero or more leading positional parameters (`ui: &mut UiWorld`, `alloc: Alloc<'_>`, etc.)
/// followed by a record parameter `p: { required: T, defaulted: U = expr, ... }`.
///
/// Invoke via [`crate::call!`]:
/// - With `..` to fill omitted defaulted fields (required fields are still statically enforced):
///   `bstd::call!(obj_row(&mut ui, { name: "agent-1", open: true, .. }))`
/// - Without `..` to require every field explicitly:
///   `bstd::call!(obj_row(&mut ui, { name: "agent-1", depth: 2, open: false }))`
#[macro_export]
macro_rules! record_fn {
    (
        $(#[$meta:meta])*
        $vis:vis fn $name:ident $(<$($lt:lifetime),* $(,)? $($gen:ident $(: $bound:path)?),* $(,)?>)? (
            $($params:tt)*
        ) $(-> $ret:ty)? $body:block
    ) => {
        $crate::__blaze_record_fn_params! {
            meta: [$(#[$meta])*]
            vis: [$vis]
            name: $name
            lt: [$($($lt),*)?]
            generics: [$($($gen $(: $bound)?),*)?]
            ret: [$(-> $ret)?]
            body: $body
            pos: []
            rest: [$($params)*]
        }
    };
}

/// Internal parameter-list splitter for [`crate::record_fn!`] (avoids LL(1) ident ambiguity).
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_record_fn_params {
    // Final parameter is the record parameter `p: { ... }`.
    (
        meta: [$(#[$meta:meta])*]
        vis: [$vis:vis]
        name: $name:ident
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        ret: [$($ret:tt)*]
        body: $body:block
        pos: [$($arg:ident : $arg_ty:ty,)*]
        rest: [$rec_param:ident : { $($fields:tt)* } $(,)?]
    ) => {
        $crate::__blaze_record_builder! {
            @collect
            vis: [$vis]
            mod_name: $name
            lt: [$($lt),*]
            generics: [$($gen $(: $bound)?),*]
            req: []
            opt: []
            rest: [$($fields)*]
        }

        $(#[$meta])*
        $vis fn $name <$($lt,)* $($gen $(: $bound)?,)*> (
            $($arg : $arg_ty,)*
            $rec_param : $name::Args <$($lt,)* $($gen,)*>
        ) $($ret)* $body
    };
    // Leading positional parameter `arg: Ty, ...`.
    (
        meta: [$(#[$meta:meta])*]
        vis: [$vis:vis]
        name: $name:ident
        lt: [$($lt:lifetime),*]
        generics: [$($gen:ident $(: $bound:path)?),*]
        ret: [$($ret:tt)*]
        body: $body:block
        pos: [$($prev:ident : $prev_ty:ty,)*]
        rest: [$arg:ident : $arg_ty:ty, $($rest:tt)+]
    ) => {
        $crate::__blaze_record_fn_params! {
            meta: [$(#[$meta])*]
            vis: [$vis]
            name: $name
            lt: [$($lt),*]
            generics: [$($gen $(: $bound)?),*]
            ret: [$($ret)*]
            body: $body
            pos: [$($prev : $prev_ty,)* $arg : $arg_ty,]
            rest: [$($rest)+]
        }
    };
}

/// Calls a function declared with [`crate::record_fn!`] using named arguments and optional `..`
/// default filling (`docs/v2/plan/design/records.md`).
#[macro_export]
macro_rules! call {
    ($($fn_path:ident)::+ ( $($args:tt)* )) => {
        $crate::__blaze_call_args! {
            fn_path: [$($fn_path)::+]
            pos: []
            rest: [$($args)*]
        }
    };
}

/// Internal argument splitter for [`crate::call!`] (avoids LL(1) block-vs-record ambiguity).
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_call_args {
    // Final record argument with trailing `..` -> type-state builder.
    (
        fn_path: [$($fn_path:ident)::+]
        pos: [$($pos:expr,)*]
        rest: [{ $($fname:ident $(: $fval:expr)?),+ , .. } $(,)?]
    ) => {
        $($fn_path)::+ (
            $($pos,)*
            $($fn_path)::+::builder()
                $( .$fname($crate::__blaze_field_val!($fname $(: $fval)?)) )+
                .finish()
        )
    };
    // Final record argument without `..` -> exhaustive struct literal.
    (
        fn_path: [$($fn_path:ident)::+]
        pos: [$($pos:expr,)*]
        rest: [{ $($fname:ident $(: $fval:expr)?),* $(,)? } $(,)?]
    ) => {
        $($fn_path)::+ (
            $($pos,)*
            $($fn_path)::+::Args {
                $( $fname: $crate::__blaze_field_val!($fname $(: $fval)?), )*
            }
        )
    };
    // Leading positional argument followed by a comma.
    (
        fn_path: [$($fn_path:ident)::+]
        pos: [$($prev:expr,)*]
        rest: [$pos:expr, $($rest:tt)+]
    ) => {
        $crate::__blaze_call_args! {
            fn_path: [$($fn_path)::+]
            pos: [$($prev,)* $pos,]
            rest: [$($rest)+]
        }
    };
}

/// Internal shorthand helper: `{ name }` -> `name`, `{ name: expr }` -> `expr`.
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_field_val {
    ($fname:ident) => {
        $fname
    };
    ($fname:ident : $fval:expr) => {
        $fval
    };
}

#[cfg(test)]
mod tests {
    struct UiWorld {
        rows_built: u32,
        last_depth: u32,
        last_open: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RowSlots {
        root: u32,
        depth: u32,
        open: bool,
        name_len: usize,
    }

    // Mirrors: `fn obj_row(ui: &uniq UiWorld, p: { name: &str, depth: u32 = 0, open: bool = false })`
    crate::record_fn! {
        fn obj_row<'a>(
            ui: &mut UiWorld,
            p: { name: &'a str, depth: u32 = 0, open: bool = false }
        ) -> RowSlots {
            ui.rows_built += 1;
            ui.last_depth = p.depth;
            ui.last_open = p.open;
            RowSlots {
                root: ui.rows_built,
                depth: p.depth,
                open: p.open,
                name_len: p.name.len(),
            }
        }
    }

    #[test]
    fn named_args_with_defaults_and_shorthand() {
        let mut ui = UiWorld { rows_built: 0, last_depth: 99, last_open: true };

        // Order-independent with `..` filling `depth = 0`:
        let r1 = crate::call!(obj_row(&mut ui, { open: true, name: "agent-1", .. }));
        assert_eq!(r1, RowSlots { root: 1, depth: 0, open: true, name_len: 7 });

        // Both defaults filled (`depth = 0, open = false`), shorthand `{ name, .. }`:
        let name = "session-2";
        let r2 = crate::call!(obj_row(&mut ui, { name, .. }));
        assert_eq!(r2, RowSlots { root: 2, depth: 0, open: false, name_len: 9 });

        // Exhaustive without `..`:
        let r3 = crate::call!(obj_row(&mut ui, { name: "leaf", depth: 4, open: true }));
        assert_eq!(r3, RowSlots { root: 3, depth: 4, open: true, name_len: 4 });
        assert_eq!((ui.rows_built, ui.last_depth, ui.last_open), (3, 4, true));
    }
}
