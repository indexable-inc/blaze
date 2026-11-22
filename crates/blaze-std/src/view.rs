//! Zero-cost, `#![forbid(unsafe_code)]` **View Types** for Rust & `bstd` (`docs/v2/plan/design/edition.md` §3).
//!
//! # Two-Tier Design
//! 1. **Compiler-native view types** ([`blaze_ty::view_ty`]): `&self.{config, mut cache}` with
//!    single-pointer unchanged ABI, automatic private-method view inference, and `pub` contract
//!    checking.
//! 2. **Portable zero-cost Rust view types (this module)**: works in ANY Rust crate today with
//!    **zero `unsafe`**, **zero proc macros**, and **zero runtime allocation** via [`views!`],
//!    [`ProjectView`], [`ProjectViewMut`], and [`DisjointSplit`].
//!
//! # Example
//! ```ignore
//! bstd::views! {
//!     pub struct Engine {
//!         pub config: Config,
//!         pub cache: Cache,
//!         pub metrics: Metrics,
//!     }
//!     views {
//!         pub view ConfigView = { config };
//!         pub view CacheView = { config, mut cache };
//!         pub view MetricsView = { config, mut metrics };
//!     }
//!     splits {
//!         pub fn split_cache_metrics(&mut self) -> (CacheView<'_>, MetricsView<'_>);
//!     }
//! }
//! ```
//!
//! Rust's own borrow checker statically verifies that `split_cache_metrics` borrows `config`
//! shared in both views while borrowing `cache` and `metrics` uniquely and disjointly — zero
//! `unsafe`, zero runtime cost (LLVM/Sota scalarizes view structs into direct field pointers in
//! registers).

#![forbid(unsafe_code)]

/// Shared projection from `Self` (a root struct or a wider view) into a narrower view `V`.
pub trait ProjectView<'a, V> {
    fn project_view(&'a self) -> V;
}

/// Unique / mixed-mutability projection from `Self` into a view `V`.
pub trait ProjectViewMut<'a, V> {
    fn project_view_mut(&'a mut self) -> V;
}

/// Zero-cost simultaneous split of `Self` into two disjoint views `(A, B)`.
pub trait DisjointSplit<'a, A, B> {
    fn split_views(&'a mut self) -> (A, B);
}

/// Zero-cost simultaneous split of `Self` into three disjoint views `(A, B, C)`.
pub trait DisjointSplit3<'a, A, B, C> {
    fn split_views3(&'a mut self) -> (A, B, C);
}

/// Declares a struct together with zero-cost borrowed **view types** (`{ a, mut b }`) and
/// compile-time borrow-checked simultaneous disjoint split methods — 100% safe Rust, zero
/// `unsafe`, zero proc-macro overhead.
///
/// # Syntax
/// ```ignore
/// bstd::views! {
///     #[derive(Debug)]
///     pub struct Engine {
///         pub config: Config,
///         pub cache: Cache,
///         pub metrics: Metrics,
///     }
///     views {
///         pub view ConfigView(as_config) = { config: Config };
///         pub view CacheView(as_cache) = { config: Config, mut cache: Cache };
///         pub view MetricsView(as_metrics) = { config: Config, mut metrics: Metrics };
///     }
///     splits {
///         pub fn split_cache_metrics(&mut self) -> (CacheView<'_> = { config, mut cache }, MetricsView<'_> = { config, mut metrics });
///     }
/// }
/// ```
#[macro_export]
macro_rules! views {
    // Entry point: struct + views + optional splits.
    (
        $(#[$smeta:meta])*
        $svis:vis struct $sname:ident {
            $(
                $(#[$fmeta:meta])*
                $fvis:vis $fname:ident : $fty:ty
            ),* $(,)?
        }
        views {
            $(
                $(#[$vmeta:meta])*
                $vvis:vis view $vname:ident ( $vfn:ident ) = {
                    $( $vspec:tt )*
                };
            )*
        }
        $(
            splits {
                $(
                    $(#[$spmeta:meta])*
                    $spvis:vis fn $spfn:ident ( &mut self ) -> (
                        $spa:ident < '_ > = { $( $spa_fields:tt )* },
                        $spb:ident < '_ > = { $( $spb_fields:tt )* } $(,)?
                    );
                )*
            }
        )?
    ) => {
        $(#[$smeta])*
        $svis struct $sname {
            $(
                $(#[$fmeta])*
                $fvis $fname : $fty,
            )*
        }

        $(
            $crate::__views_define_one!(
                $(#[$vmeta])*
                $vvis view $vname ( $vfn ) on $sname = [ $( $vspec )* ]
            );
        )*

        $(
            impl $sname {
                $(
                    $(#[$spmeta])*
                    #[inline]
                    $spvis fn $spfn(&mut self) -> ($spa<'_>, $spb<'_>) {
                        (
                            $crate::__views_construct!(self, $spa, [ $( $spa_fields )* ]),
                            $crate::__views_construct!(self, $spb, [ $( $spb_fields )* ]),
                        )
                    }
                )*
            }

            $(
                impl<'__view> $crate::view::DisjointSplit<'__view, $spa<'__view>, $spb<'__view>> for $sname {
                    #[inline]
                    fn split_views(&'__view mut self) -> ($spa<'__view>, $spb<'__view>) {
                        self.$spfn()
                    }
                }
            )*
        )?
    };
}

/// Internal helper: parse `{ a: A, mut b: B }` entries and emit view struct + projector + trait impl.
#[doc(hidden)]
#[macro_export]
macro_rules! __views_define_one {
    (
        $(#[$vmeta:meta])*
        $vvis:vis view $vname:ident ( $vfn:ident ) on $sname:ident = [ $( $rest:tt )* ]
    ) => {
        $crate::__views_parse_fields!(
            meta: [ $(#[$vmeta])* ],
            vis: $vvis,
            vname: $vname,
            vfn: $vfn,
            sname: $sname,
            has_mut: false,
            acc: [],
            rest: [ $( $rest )* ]
        );
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __views_parse_fields {
    // Next field is `mut field: Ty, ...`
    (
        meta: [ $(#[$vmeta:meta])* ],
        vis: $vvis:vis,
        vname: $vname:ident,
        vfn: $vfn:ident,
        sname: $sname:ident,
        has_mut: $hm:tt,
        acc: [ $( $acc:tt )* ],
        rest: [ mut $f:ident : $ty:ty $(, $( $tail:tt )* )? ]
    ) => {
        $crate::__views_parse_fields!(
            meta: [ $(#[$vmeta])* ],
            vis: $vvis,
            vname: $vname,
            vfn: $vfn,
            sname: $sname,
            has_mut: true,
            acc: [ $( $acc )* (mut $f : $ty) ],
            rest: [ $( $( $tail )* )? ]
        );
    };
    // Next field is `field: Ty, ...` (shared)
    (
        meta: [ $(#[$vmeta:meta])* ],
        vis: $vvis:vis,
        vname: $vname:ident,
        vfn: $vfn:ident,
        sname: $sname:ident,
        has_mut: $hm:tt,
        acc: [ $( $acc:tt )* ],
        rest: [ $f:ident : $ty:ty $(, $( $tail:tt )* )? ]
    ) => {
        $crate::__views_parse_fields!(
            meta: [ $(#[$vmeta])* ],
            vis: $vvis,
            vname: $vname,
            vfn: $vfn,
            sname: $sname,
            has_mut: $hm,
            acc: [ $( $acc )* (ref $f : $ty) ],
            rest: [ $( $( $tail )* )? ]
        );
    };
    // Done: shared-only view (`has_mut == false`) -> `&self` projector + `ProjectView`!
    (
        meta: [ $(#[$vmeta:meta])* ],
        vis: $vvis:vis,
        vname: $vname:ident,
        vfn: $vfn:ident,
        sname: $sname:ident,
        has_mut: false,
        acc: [ $( (ref $f:ident : $ty:ty) )* ],
        rest: []
    ) => {
        $(#[$vmeta])*
        #[derive(Clone, Copy)]
        $vvis struct $vname<'__view> {
            $( pub $f: &'__view $ty, )*
        }

        impl $sname {
            #[inline]
            $vvis fn $vfn(&self) -> $vname<'_> {
                $vname {
                    $( $f: &self.$f, )*
                }
            }
        }

        impl<'__view> $crate::view::ProjectView<'__view, $vname<'__view>> for $sname {
            #[inline]
            fn project_view(&'__view self) -> $vname<'__view> {
                self.$vfn()
            }
        }

        impl<'__view> ::core::convert::From<&'__view $sname> for $vname<'__view> {
            #[inline]
            fn from(root: &'__view $sname) -> Self {
                root.$vfn()
            }
        }
    };
    // Done: unique/mixed view (`has_mut == true`) -> `&mut self` projector + `ProjectViewMut`!
    (
        meta: [ $(#[$vmeta:meta])* ],
        vis: $vvis:vis,
        vname: $vname:ident,
        vfn: $vfn:ident,
        sname: $sname:ident,
        has_mut: true,
        acc: [ $( ($mode:ident $f:ident : $ty:ty) )* ],
        rest: []
    ) => {
        $(#[$vmeta])*
        $vvis struct $vname<'__view> {
            $( pub $f: $crate::__views_field_ty!($mode '__view $ty), )*
        }

        impl $sname {
            #[inline]
            $vvis fn $vfn(&mut self) -> $vname<'_> {
                $vname {
                    $( $f: $crate::__views_field_expr!($mode self $f), )*
                }
            }
        }

        impl<'__view> $crate::view::ProjectViewMut<'__view, $vname<'__view>> for $sname {
            #[inline]
            fn project_view_mut(&'__view mut self) -> $vname<'__view> {
                self.$vfn()
            }
        }

        impl<'__view> ::core::convert::From<&'__view mut $sname> for $vname<'__view> {
            #[inline]
            fn from(root: &'__view mut $sname) -> Self {
                root.$vfn()
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __views_field_ty {
    (ref $lt:lifetime $ty:ty) => { &$lt $ty };
    (mut $lt:lifetime $ty:ty) => { &$lt mut $ty };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __views_field_expr {
    (ref $self:ident $f:ident) => { &$self.$f };
    (mut $self:ident $f:ident) => { &mut $self.$f };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __views_construct {
    ($self:ident, $vname:ident, [ $( $rest:tt )* ]) => {
        $crate::__views_construct_acc!($self, $vname, [], [ $( $rest )* ])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __views_construct_acc {
    ($self:ident, $vname:ident, [ $( $acc:tt )* ], [ mut $f:ident $(, $( $tail:tt )* )? ]) => {
        $crate::__views_construct_acc!($self, $vname, [ $( $acc )* $f: &mut $self.$f, ], [ $( $( $tail )* )? ])
    };
    ($self:ident, $vname:ident, [ $( $acc:tt )* ], [ $f:ident $(, $( $tail:tt )* )? ]) => {
        $crate::__views_construct_acc!($self, $vname, [ $( $acc )* $f: &$self.$f, ], [ $( $( $tail )* )? ])
    };
    ($self:ident, $vname:ident, [ $( $acc:tt )* ], []) => {
        $vname { $( $acc )* }
    };
}

#[cfg(test)]
mod tests {
    use super::{DisjointSplit, ProjectView, ProjectViewMut};

    views! {
        #[derive(Debug, PartialEq, Eq)]
        struct Engine {
            config: u64,
            cache: u64,
            metrics: u64,
        }
        views {
            view ConfigView(as_config) = { config: u64 };
            view CacheView(as_cache) = { config: u64, mut cache: u64 };
            view MetricsView(as_metrics) = { config: u64, mut metrics: u64 };
        }
        splits {
            fn split_cache_metrics(&mut self) -> (
                CacheView<'_> = { config, mut cache },
                MetricsView<'_> = { config, mut metrics },
            );
        }
    }

    impl<'a> CacheView<'a> {
        fn bump_cache(&mut self) {
            *self.cache += *self.config;
        }
        fn as_config(&self) -> ConfigView<'_> {
            ConfigView { config: self.config }
        }
    }

    impl<'a> MetricsView<'a> {
        fn record_hit(&mut self) {
            *self.metrics += *self.config * 2;
        }
    }

    fn read_cfg<'a>(src: &'a impl ProjectView<'a, ConfigView<'a>>) -> u64 {
        *src.project_view().config
    }

    #[test]
    fn disjoint_views_mutate_simultaneously_with_zero_unsafe() {
        let mut engine = Engine { config: 10, cache: 1, metrics: 5 };
        assert_eq!(read_cfg(&engine), 10);

        // Single view projection via trait & method:
        {
            let mut cv: CacheView<'_> = engine.project_view_mut();
            cv.bump_cache();
            assert_eq!(*cv.as_config().config, 10);
        }
        assert_eq!(engine.cache, 11);

        // Simultaneous disjoint split: BOTH views share `&config`, while mutating `cache` and
        // `metrics` concurrently — proven sound by rustc with zero `unsafe`!
        {
            let (mut cache_v, mut metrics_v): (CacheView<'_>, MetricsView<'_>) = engine.split_views();
            cache_v.bump_cache();
            metrics_v.record_hit();
            assert_eq!(*cache_v.config, *metrics_v.config);
        }
        assert_eq!(engine, Engine { config: 10, cache: 21, metrics: 25 });
    }
}
