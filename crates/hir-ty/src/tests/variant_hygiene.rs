//! Macros 2.0 hygiene of enum variants, as rustc's `tests/ui/hygiene/cross-crate-variants.rs`
//! checks it: an enum a `macro` writes binds the variant it names under that `macro`'s context
//! and the one it is handed under the caller's (`def_site::variant_name`), so `Variant` and
//! `$variant_name` given `Variant` are two variants. A path's segment after the enum (`E::V`,
//! `<E>::V`, `Self::V`, a pattern, `use E::*`) finds the one written with its own context,
//! adjusted to the enum's expansion (`EnumVariants::variant_written`).

use std::fmt::Write as _;

use expect_test::{Expect, expect};
use hir_def::{
    DefWithBodyId, EnumVariantId, FunctionId, ModuleDefId, VariantId,
    expr_store::{Body, path::Path},
    hir::{Expr, Pat},
    nameres::{DefMap, crate_def_map},
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult};

/// rustc's `tests/ui/hygiene/cross-crate-variants.rs` with its `auxiliary/variants.rs`, its
/// `assert_eq!`s written as `let`s of the paths they compare.
const CROSS_CRATE: &str = r#"
//- minicore: sized
//- /main.rs crate:main deps:variants
extern crate variants;

use variants::*;

fn main() {
    check_variants();

    test_variants!();
    test_variants2!();

    let _ = MyEnum::Variant;
}
//- /variants.rs crate:variants
#![feature(decl_macro)]

macro x($macro_name:ident, $macro2_name:ident, $type_name:ident, $variant_name:ident) {
    #[repr(u8)]
    pub enum $type_name {
        Variant = 0,
        $variant_name = 1,
    }

    #[macro_export]
    macro_rules! $macro_name {
        () => {{
            let _ = $type_name::Variant;
            let _ = $type_name::$variant_name;
            let _ = <$type_name>::Variant;
            let _ = <$type_name>::$variant_name;
        }};
    }

    pub macro $macro2_name {
        () => {{
            let _ = $type_name::Variant;
            let _ = $type_name::$variant_name;
            let _ = <$type_name>::Variant;
            let _ = <$type_name>::$variant_name;
        }},
    }
}

x!(test_variants, test_variants2, MyEnum, Variant);

pub fn check_variants() {
    test_variants!();
    test_variants2!();
}
"#;

/// The same in one crate, with the other places a variant is named: `Self::V` in an impl the
/// `macro` writes, patterns, a glob of the enum the `macro` writes and one its caller does, a
/// glob a `macro` writes of an enum of a real file, which imports `W` for the `macro`'s own
/// names (`reverse_glob_adjust`), and record, tuple and unit variants of enums the `macro`
/// writes and names itself, after `Self`, a type (`<Rec>::V`), the enum and a type alias: the
/// struct paths of inference, and record literal and pattern paths, which take the path's
/// hygiene as a path expression does. Checked against the pinned rustc, each `let` an
/// `assert_eq!` of the discriminant: `in_x` gives 0, 1, 0, 0, 1 (the patterns 0 and 1),
/// `caller` 1 each, `own` 0, `handed` 1, `in_globbed` 0, `rec_anchor` 0, `rec_plain` and
/// `rec_alias` 1, `rec_alias_own` 0, and each `match` of `rec_pat`, `tup` and `unit` its arm.
const SAME_CRATE: &str = r#"
//- minicore: sized
#![feature(decl_macro, more_qualified_paths)]

macro x($E:ident, $V:ident) {
    pub enum $E {
        V = 0,
        $V = 1,
    }

    impl $E {
        fn own() -> Self { Self::V }
        fn handed() -> Self { Self::$V }
    }

    fn in_x(e: $E) {
        let _ = $E::V;
        let _ = <$E>::$V;
        match e {
            $E::V => {}
            <$E>::$V => {}
        }
        use $E::*;
        let _ = V;
        let _ = $V;
    }

    pub enum Rec {
        V { a: u8 },
        $V { a: u8 },
    }

    type Alias = Rec;

    impl Rec {
        fn which(&self) -> u8 {
            match self {
                Self::V { a } => *a,
                Self::$V { a } => *a,
            }
        }
        fn rec_anchor() -> u8 { <Rec>::V { a: 0 }.which() }
        fn rec_plain() -> u8 { Rec::$V { a: 1 }.which() }
        fn rec_alias() -> u8 { Alias::$V { a: 1 }.which() }
        fn rec_alias_own() -> u8 { Alias::V { a: 0 }.which() }
    }

    fn rec_pat(r: Rec) -> u8 {
        match r {
            Rec::V { a } => a,
            Rec::$V { a } => a,
        }
    }

    pub enum Tup {
        V(u8),
        $V(u8),
    }

    fn tup(t: Tup) -> u8 {
        match t {
            Tup::V(a) => a,
            Tup::$V(a) => a,
        }
    }

    pub enum Unit {
        V,
        $V,
    }

    fn unit(u: Unit) -> u8 {
        match u {
            Unit::V => 0,
            Unit::$V => 1,
        }
    }
}

x!(E, V);

fn caller(e: E) {
    let _ = E::V;
    let _ = <E>::V;
    match e {
        E::V => {}
        _ => {}
    }
    use E::*;
    let _ = V;
}

pub enum Outer {
    W = 0,
}

macro globbed() {
    fn in_globbed() {
        use Outer::*;
        let _ = W;
    }
}

globbed!();
"#;

/// Each function of the fixture's crates, its dependencies' first (in their modules and their
/// impls), and for each
/// path expression, record literal and pattern of its body that names a variant, the variant
/// ([`variant_of`]).
fn check_variants(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let db = fixture::db(ra_fixture).into_db();
    blaze_query::db::attach(&db, || {
        let main = fixture::test_crate(&db);
        let mut crates: Vec<_> = main
            .data(&db)
            .dependencies
            .iter()
            .map(|dep| dep.crate_id)
            .filter(|it| !it.data(&db).origin.is_lang())
            .collect();
        crates.push(main);
        let mut functions = Vec::new();
        for krate in crates {
            collect_functions(&db, crate_def_map(&db, krate), &mut functions);
        }
        let mut out = String::new();
        for function in functions {
            let def = DefWithBodyId::FunctionId(function);
            let body = Body::of(&db, def);
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            let name = FunctionSignature::of(&db, function).name.as_str();
            assert!(infer.diagnostics().is_empty(), "`{name}`: {:?}", infer.diagnostics());
            assert!(!infer.has_type_mismatches(), "`{name}`: {:?}", infer.diagnostics());
            let mut lines = Vec::new();
            for (expr, it) in body.exprs() {
                let Expr::Path(path) = it else { continue };
                let Some(ValueNs::EnumVariantId(variant)) = infer.value_resolution(expr.into())
                else {
                    continue;
                };
                lines.push(format!("    {} -> {}", written(&db, path), variant_of(&db, variant)));
            }
            for (expr, it) in body.exprs() {
                let Expr::RecordLit { path, .. } = it else { continue };
                let Some(VariantId::EnumVariantId(variant)) =
                    infer.variant_resolution_for_expr(expr)
                else {
                    continue;
                };
                lines.push(format!(
                    "    record {} -> {}",
                    written(&db, path),
                    variant_of(&db, variant)
                ));
            }
            for (pat, it) in body.pats() {
                let path = match it {
                    Pat::Path(path) => {
                        let Some(ValueNs::EnumVariantId(variant)) =
                            infer.value_resolution(pat.into())
                        else {
                            panic!("the pattern {} names no variant", written(&db, path));
                        };
                        lines.push(format!(
                            "    pattern {} -> {}",
                            written(&db, path),
                            variant_of(&db, variant)
                        ));
                        continue;
                    }
                    Pat::Record { path, .. } | Pat::TupleStruct { path, .. } => path,
                    _ => continue,
                };
                let Some(VariantId::EnumVariantId(variant)) = infer.variant_resolution_for_pat(pat)
                else {
                    panic!("the pattern {} names no variant", written(&db, path));
                };
                lines.push(format!(
                    "    pattern {} -> {}",
                    written(&db, path),
                    variant_of(&db, variant)
                ));
            }
            if !lines.is_empty() {
                writeln!(out, "fn {name}:").unwrap();
                for line in lines {
                    writeln!(out, "{line}").unwrap();
                }
            }
        }
        expect.assert_eq(&out);
    });
}

fn collect_functions(db: &base_db::Db, map: &DefMap, out: &mut Vec<FunctionId>) {
    for (_, module) in map.modules(db) {
        let scope = module.scope(db);
        for decl in scope.declarations() {
            if let ModuleDefId::FunctionId(it) = decl {
                out.push(it);
            }
        }
        for impl_ in scope.impls() {
            for &(_, item) in impl_.impl_items(db).items.iter() {
                if let hir_def::AssocItemId::FunctionId(it) = item {
                    out.push(it);
                }
            }
        }
    }
}

/// The path as written: `<_>::V` for a path after a type.
fn written(db: &base_db::Db, path: Path<'_>) -> String {
    let segments =
        path.mod_path().map_or_else(String::new, |it| it.display_verbatim(db).to_string());
    match path.type_anchor() {
        Some(_) => format!("<_>::{segments}"),
        None => segments,
    }
}

/// The variant's index in its enum, which is its discriminant in these fixtures (each written
/// `= index`), and that it is the variant of that index: the map the index is read from is the
/// one a path looks the variant up in.
fn variant_of(db: &base_db::Db, variant: EnumVariantId) -> String {
    let index = variant.index(db);
    let parent = hir_def::Lookup::lookup(&variant, db).parent;
    let at = parent.enum_variants(db).variants.get_index(index).map(|(_, &(it, _))| it);
    assert_eq!(at, Some(variant), "variant {index} of {parent:?}");
    format!("variant {index}")
}

#[test]
fn variants_a_macro_names_in_another_crate_are_found_by_its_expansion_alone() {
    check_variants(
        CROSS_CRATE,
        expect![[r#"
            fn check_variants:
                MyEnum::Variant -> variant 0
                MyEnum::Variant -> variant 1
                <_>::Variant -> variant 0
                <_>::Variant -> variant 1
                MyEnum::Variant -> variant 0
                MyEnum::Variant -> variant 1
                <_>::Variant -> variant 0
                <_>::Variant -> variant 1
            fn main:
                MyEnum::Variant -> variant 0
                MyEnum::Variant -> variant 1
                <_>::Variant -> variant 0
                <_>::Variant -> variant 1
                MyEnum::Variant -> variant 0
                MyEnum::Variant -> variant 1
                <_>::Variant -> variant 0
                <_>::Variant -> variant 1
                MyEnum::Variant -> variant 1
        "#]],
    );
}

#[test]
fn variants_a_macro_names_are_found_by_its_expansion_alone() {
    check_variants(
        SAME_CRATE,
        expect![[r#"
            fn in_x:
                E::V -> variant 0
                <_>::V -> variant 1
                V -> variant 0
                V -> variant 1
                pattern E::V -> variant 0
                pattern <_>::V -> variant 1
            fn rec_pat:
                pattern Rec::V -> variant 0
                pattern Rec::V -> variant 1
            fn tup:
                pattern Tup::V -> variant 0
                pattern Tup::V -> variant 1
            fn unit:
                pattern Unit::V -> variant 0
                pattern Unit::V -> variant 1
            fn caller:
                E::V -> variant 1
                <_>::V -> variant 1
                V -> variant 1
                pattern E::V -> variant 1
            fn in_globbed:
                W -> variant 0
            fn own:
                Self::V -> variant 0
            fn handed:
                Self::V -> variant 1
            fn which:
                pattern Self::V -> variant 0
                pattern Self::V -> variant 1
            fn rec_anchor:
                record <_>::V -> variant 0
            fn rec_plain:
                record Rec::V -> variant 1
            fn rec_alias:
                record Alias::V -> variant 1
            fn rec_alias_own:
                record Alias::V -> variant 0
        "#]],
    );
}
