//! Rewrites of the user's types: elided lifetimes to the database lifetime, the database
//! lifetime to `'static` (what a row stores), and `Self` to the type a tracked `impl` is for.

// alloc: expansion time, once per annotated item; no query runs this

use blaze_hash::HashSet;
use proc_macro2::Span;
use quote::quote;
use syn::visit_mut::{self, VisitMut};
use syn::{Expr, Lifetime, Type};

/// Turns every lifetime named `from` (outside a `for<..>` binder) into `to`.
pub(crate) struct ChangeLt {
    from: String,
    to: Lifetime,
}

impl ChangeLt {
    /// `'_` to the database lifetime.
    pub(crate) fn elided_to(db_lt: &Lifetime) -> ChangeLt {
        // alloc: expansion time; clone: expansion time, a lifetime
        ChangeLt { from: "_".to_owned(), to: db_lt.clone() }
    }

    /// The database lifetime to `'static`: the type a row holds.
    pub(crate) fn erase(db_lt: &Lifetime) -> ChangeLt {
        // alloc: expansion time
        ChangeLt { from: db_lt.ident.to_string(), to: Lifetime::new("'static", db_lt.span()) }
    }

    pub(crate) fn in_type(mut self, ty: &Type) -> Type {
        // clone: expansion time, the user's type rewritten
        let mut ty = ty.clone();
        self.visit_type_mut(&mut ty);
        ty
    }
}

impl VisitMut for ChangeLt {
    fn visit_lifetime_mut(&mut self, lifetime: &mut Lifetime) {
        if lifetime.ident == self.from {
            // clone: expansion time, a lifetime
            *lifetime = self.to.clone();
        }
    }

    fn visit_type_bare_fn_mut(&mut self, _: &mut syn::TypeBareFn) {}

    fn visit_trait_bound_mut(&mut self, bound: &mut syn::TraitBound) {
        if bound.lifetimes.is_none() {
            visit_mut::visit_trait_bound_mut(self, bound);
        }
    }
}

/// The type with `'_` made the database lifetime, and its stored form.
pub(crate) struct Both {
    pub(crate) db: Type,
    pub(crate) stored: Type,
    /// Whether the type names the database lifetime: then a value of it crosses between the two
    /// forms by a transmute.
    pub(crate) names_db: bool,
}

impl Both {
    pub(crate) fn of(ty: &Type, db_lt: &Lifetime) -> Both {
        let db = ChangeLt::elided_to(db_lt).in_type(ty);
        let stored = ChangeLt::erase(db_lt).in_type(&db);
        let names_db = names(&db, db_lt);
        Both { db, stored, names_db }
    }

    /// `value` (of the database form) as the stored form.
    pub(crate) fn store(&self, value: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        let Both { db, stored, names_db } = self;
        if *names_db {
            quote! {
                // SAFETY: the two types differ in the database lifetime only, which a stored
                // value names through `blaze_query::Value` types alone (checked where the query
                // is defined); the engine hands a stored value out only for borrows of the
                // database it is valid in.
                unsafe { ::core::mem::transmute::<#db, #stored>(#value) }
            }
        } else {
            quote!(#value)
        }
    }

    /// `value` (of the stored form) as the database form.
    pub(crate) fn load(&self, value: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        let Both { db, stored, names_db } = self;
        if *names_db {
            quote! {
                // SAFETY: as in `store`: a value read under a borrow of the database lives as
                // long as that borrow, and its types differ from the stored ones in the lifetime
                // alone.
                unsafe { ::core::mem::transmute::<#stored, #db>(#value) }
            }
        } else {
            quote!(#value)
        }
    }

    /// `value: &'db Stored` as `&'db Db`.
    pub(crate) fn load_ref(&self, value: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
        let Both { db, stored, names_db } = self;
        if *names_db {
            quote! {
                // SAFETY: the pointee's types differ in the database lifetime alone, and the
                // reference is no longer than the borrow of the database it was read under.
                unsafe { &*::core::ptr::from_ref::<#stored>(#value).cast::<#db>() }
            }
        } else {
            quote!(#value)
        }
    }
}

/// Whether `ty` names `lifetime` outside a `for<..>` binder.
pub(crate) fn names(ty: &Type, lifetime: &Lifetime) -> bool {
    struct Find<'a> {
        name: &'a Lifetime,
        found: bool,
    }
    impl VisitMut for Find<'_> {
        fn visit_lifetime_mut(&mut self, lifetime: &mut Lifetime) {
            self.found |= lifetime.ident == self.name.ident;
        }
        fn visit_type_bare_fn_mut(&mut self, _: &mut syn::TypeBareFn) {}
        fn visit_trait_bound_mut(&mut self, bound: &mut syn::TraitBound) {
            if bound.lifetimes.is_none() {
                visit_mut::visit_trait_bound_mut(self, bound);
            }
        }
    }
    let mut find = Find { name: lifetime, found: false };
    // clone: expansion time, a type searched
    find.visit_type_mut(&mut ty.clone());
    find.found
}

/// A database lifetime for a function that declares none: `'db`, or `'db2`, .. when a binder
/// in its signature already has that name.
pub(crate) fn conjure(signature: &syn::Signature, also: Option<&Type>, span: Span) -> Lifetime {
    struct Used(HashSet<String>);
    impl VisitMut for Used {
        fn visit_lifetime_mut(&mut self, lifetime: &mut Lifetime) {
            // alloc: expansion time
            self.0.insert(lifetime.ident.to_string());
        }
    }
    let mut used = Used(HashSet::default());
    // clone: expansion time, a signature searched
    used.visit_signature_mut(&mut signature.clone());
    if let Some(ty) = also {
        // clone: expansion time, a type searched
        used.visit_type_mut(&mut ty.clone());
    }
    // alloc: expansion time
    let mut name = "db".to_owned();
    let mut counter = 1;
    while used.0.contains(&name) {
        counter += 1;
        // alloc: expansion time
        name = format!("db{counter}");
    }
    // alloc: expansion time
    Lifetime::new(&format!("'{name}"), span)
}

/// `Self` (a type or the head of a path) to the type a tracked `impl` is for, for what is
/// taken out of the `impl`: argument, value and key types and the cycle options.
pub(crate) struct ReplaceSelf<'a> {
    pub(crate) self_ty: &'a Type,
}

impl ReplaceSelf<'_> {
    pub(crate) fn in_type(mut self, ty: &Type) -> Type {
        // clone: expansion time, the user's type rewritten
        let mut ty = ty.clone();
        self.visit_type_mut(&mut ty);
        ty
    }

    pub(crate) fn in_expr(mut self, expr: &Expr) -> Expr {
        // clone: expansion time, the user's option rewritten
        let mut expr = expr.clone();
        self.visit_expr_mut(&mut expr);
        expr
    }
}

fn self_headed(path: &syn::Path) -> bool {
    path.leading_colon.is_none() && path.segments.first().is_some_and(|it| it.ident == "Self")
}

impl VisitMut for ReplaceSelf<'_> {
    fn visit_type_mut(&mut self, ty: &mut Type) {
        if let Type::Path(path) = ty
            && path.qself.is_none()
            && self_headed(&path.path)
        {
            let self_ty = self.self_ty;
            let rest = path.path.segments.iter().skip(1);
            *ty = if path.path.segments.len() == 1 {
                // clone: expansion time, the impl's type
                self_ty.clone()
            } else {
                syn::parse_quote!(<#self_ty> #(:: #rest)*)
            };
            return;
        }
        visit_mut::visit_type_mut(self, ty);
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        if let Expr::Path(path) = expr
            && path.qself.is_none()
            && self_headed(&path.path)
            && path.path.segments.len() > 1
        {
            let self_ty = self.self_ty;
            let rest = path.path.segments.iter().skip(1);
            *expr = syn::parse_quote!(<#self_ty> #(:: #rest)*);
            return;
        }
        visit_mut::visit_expr_mut(self, expr);
    }
}
