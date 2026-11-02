use std::{convert::Infallible, ops::ControlFlow};

use base_db::Db;
use either::Either;
use hir_expand::{
    HirFileId, Lookup,
    attrs::{AstKeyValueMetaExt, AstPathExt, expand_cfg_attr_view},
};
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use span::Edition;
use syntax::{
    AstNode, AstPtr, Parse, SourceFile, ast,
    view::{self, AstNode as _, HasAttrs as _, HasName as _, Typed},
};
use hir_expand::fixture;

use super::{
    AttrFlags, Completions, CrateAttrFlags, DocAtom, DocWord, LinkKey, LinkKind, LinkModifier,
    MacroStyle, NativeLib, NativeLibKind, ReprWord, SkipDuringMethodDispatch, Tree, Word,
    attrs_source, collect_attrs_in, column_flags, extract_ra_completions, extract_ra_macro_style,
    extract_rustc_skip_during_method_dispatch, file_attr_flags, for_each_field, link_cfg,
    match_attr_flags, parse_link_tt, parse_repr_tt, symbol_attr, walked_flags, word,
};
use crate::{
    AdtId, AssocItemId, AttrDefId, FieldId, FunctionId, ModuleDefId, VariantId,
    nameres::{assoc::TraitItems, crate_def_map},
    src::HasSource,
    test_db::{TestDB, TestDBExt},
};

#[test]
fn crate_attrs() {
    let fixture = r#"
//- /lib.rs crate:foo crate-attr:no_std crate-attr:cfg(target_arch="x86")
        "#;
    let fx = fixture::db(fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    let module = db.module_for_file(file_id.file_id(&db));
    let attrs = AttrFlags::query(&db, AttrDefId::ModuleId(module));
    assert!(attrs.contains(AttrFlags::IS_NO_STD | AttrFlags::HAS_CFG));
}

/// Each member of every name set found at its tag, and the near misses of each (a byte short or
/// over, another case, `r#`) found as a linear search of the texts finds them.
macro_rules! check_name_set {
    ($Enum:ident) => {{
        assert_eq!($Enum::ALL.len(), $Enum::TEXTS.len());
        for (at, (&text, &member)) in $Enum::TEXTS.iter().zip($Enum::ALL).enumerate() {
            assert_eq!(member as usize, at, "{text}");
            assert_eq!($Enum::of(text), Some(member), "{text}");
            assert_eq!($Enum::of(&text.to_owned()), Some(member), "{text}");
        }
        let model = |name: &str| {
            $Enum::TEXTS.iter().position(|it| *it == name).map(|at| $Enum::ALL[at])
        };
        for &text in $Enum::TEXTS {
            let near = [
                text[..text.len() - 1].to_owned(),
                text[1..].to_owned(),
                format!("{text}x"),
                format!("x{text}"),
                format!("r#{text}"),
                text.to_uppercase(),
                text.to_lowercase(),
                text.replace('_', "-"),
                text.replace('-', "_"),
            ];
            for near in &near {
                assert_eq!($Enum::of(near), model(near), "{near}");
            }
        }
        for other in ["", "a", "_", "-", "ünused", "allow", "test", "cfg_attr"] {
            assert_eq!($Enum::of(other), model(other), "{other}");
        }
    }};
}

#[test]
fn every_name_set_finds_its_members_and_only_them() {
    check_name_set!(Word);
    check_name_set!(DocWord);
    check_name_set!(Completions);
    check_name_set!(MacroStyle);
    check_name_set!(SkipDuringMethodDispatch);
    check_name_set!(ReprWord);
    check_name_set!(LinkKey);
    check_name_set!(LinkKind);
    check_name_set!(LinkModifier);
}

#[test]
fn every_word_is_found_at_its_tag() {
    for (at, (&text, &found)) in Word::TEXTS.iter().zip(Word::ALL).enumerate() {
        assert_eq!(found as usize, at, "{text}");
        assert_eq!(word(text), Some(found), "{text}");
    }
}

#[test]
fn other_names_are_no_word() {
    for name in [
        "",
        "a",
        "r#inline",
        "inlines",
        "inlin",
        "Inline",
        "rustc",
        "rustc_",
        "rustc_allocator_zero",
        "rustc_allocator_zeroed_",
        "allow",
        "cfg_attr",
        "test",
        "derive",
        "rustc_const_stable",
        "rustc_on_unimplemented",
        "ünused",
    ] {
        assert_eq!(word(name), None, "{name}");
    }
    let model = |name: &str| Word::TEXTS.iter().position(|it| *it == name).map(|at| Word::ALL[at]);
    for &text in Word::TEXTS {
        let (after, before) = (format!("{text}x"), format!("x{text}"));
        for near in [&text[..text.len() - 1], &text[1..], after.as_str(), before.as_str()] {
            assert_eq!(word(near), model(near), "{near}");
        }
    }
}

/// The flags of one attribute, as the walk before the word index computed them: by string
/// `match`, through `AstPathExt`'s `SmolStr`s, `#[inline]`'s text and [`DocAtom`].
fn old_match_attr_flags(
    attr_flags: &mut AttrFlags,
    root: view::Node<'_>,
    attr: ast::Meta,
) -> ControlFlow<Infallible> {
    fn old_extract_doc_tt_attr(
        attr_flags: &mut AttrFlags,
        root: view::Node<'_>,
        tt: ast::TokenTree,
    ) {
        for atom in DocAtom::parse(view_of(root, &tt)) {
            match atom {
                DocAtom::Flag(flag) => match &*flag {
                    "notable_trait" => attr_flags.insert(AttrFlags::IS_DOC_NOTABLE_TRAIT),
                    "hidden" => attr_flags.insert(AttrFlags::IS_DOC_HIDDEN),
                    _ => {}
                },
                DocAtom::KeyValue { key, value: _ } => match &*key {
                    "alias" => attr_flags.insert(AttrFlags::HAS_DOC_ALIASES),
                    "keyword" => attr_flags.insert(AttrFlags::HAS_DOC_KEYWORD),
                    _ => {}
                },
                DocAtom::Alias(_) => attr_flags.insert(AttrFlags::HAS_DOC_ALIASES),
            }
        }
    }

    match attr {
        ast::Meta::CfgMeta(_) => attr_flags.insert(AttrFlags::HAS_CFG),
        ast::Meta::KeyValueMeta(attr) => {
            let path = attr.path().map(|it| view_of(root, &it));
            let Some(key) = path.as_one_segment() else { return ControlFlow::Continue(()) };
            match &*key {
                "deprecated" => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                "ignore" => attr_flags.insert(AttrFlags::IS_IGNORE),
                "lang" => attr_flags.insert(AttrFlags::LANG_ITEM),
                "must_use" => attr_flags.insert(AttrFlags::IS_MUST_USE),
                "path" => attr_flags.insert(AttrFlags::HAS_PATH),
                "unstable" => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                "rustc_reservation_impl" => attr_flags.insert(AttrFlags::RUSTC_RESERVATION_IMPL),
                "export_name" => {
                    if let Some(value) = view_of(root, &attr).value_string()
                        && *value == *"main"
                    {
                        attr_flags.insert(AttrFlags::IS_EXPORT_NAME_MAIN);
                    }
                }
                _ => {}
            }
        }
        ast::Meta::TokenTreeMeta(attr) => {
            let path = attr.path().map(|it| view_of(root, &it));
            let (Some((first_segment, second_segment)), Some(tt)) =
                (path.as_up_to_two_segment(), attr.token_tree())
            else {
                return ControlFlow::Continue(());
            };
            match second_segment {
                None => match &*first_segment {
                    "deprecated" => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                    "used" => attr_flags.insert(AttrFlags::USED),
                    "inline" if !tt.syntax().text().contains_char('n') => {
                        attr_flags.insert(AttrFlags::IS_INLINE);
                        if tt.syntax().text().to_string().contains("always") {
                            attr_flags.insert(AttrFlags::INLINE_ALWAYS)
                        }
                    }
                    "inline" if tt.syntax().text().to_string().contains("never") => {
                        attr_flags.insert(AttrFlags::INLINE_NEVER)
                    }
                    "doc" => old_extract_doc_tt_attr(attr_flags, root, tt),
                    "repr" | "rustc_scalable_vector" => attr_flags.insert(AttrFlags::HAS_REPR),
                    "target_feature" => attr_flags.insert(AttrFlags::HAS_TARGET_FEATURE),
                    "link_ordinal" => attr_flags.insert(AttrFlags::HAS_LINK_ORDINAL),
                    "proc_macro_derive" | "rustc_builtin_macro" => {
                        attr_flags.insert(AttrFlags::IS_DERIVE_OR_BUILTIN_MACRO)
                    }
                    "unstable" => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                    "stable" => attr_flags.insert(AttrFlags::IS_STABLE),
                    "rustc_layout_scalar_valid_range_start"
                    | "rustc_layout_scalar_valid_range_end" => {
                        attr_flags.insert(AttrFlags::RUSTC_LAYOUT_SCALAR_VALID_RANGE)
                    }
                    "rustc_legacy_const_generics" => {
                        attr_flags.insert(AttrFlags::HAS_LEGACY_CONST_GENERICS)
                    }
                    "rustc_skip_during_method_dispatch" => {
                        extract_rustc_skip_during_method_dispatch(attr_flags, view_of(root, &tt))
                    }
                    "rustc_deprecated_safe_2024" => {
                        attr_flags.insert(AttrFlags::RUSTC_DEPRECATED_SAFE_2024)
                    }
                    "rustc_must_implement_one_of" => {
                        attr_flags.insert(AttrFlags::HAS_RUSTC_MUST_IMPLEMENT_ONE_OF)
                    }
                    _ => {}
                },
                Some(second_segment) => match &*first_segment {
                    "rust_analyzer" => match &*second_segment {
                        "completions" => extract_ra_completions(attr_flags, view_of(root, &tt)),
                        "macro_style" => extract_ra_macro_style(attr_flags, view_of(root, &tt)),
                        "define_opaque" => attr_flags.insert(AttrFlags::HAS_DEFINE_OPAQUE),
                        _ => {}
                    },
                    _ => {}
                },
            }
        }
        ast::Meta::PathMeta(attr) => {
            let is_test = attr.path().is_some_and(|path| {
                let Some(segment1) = (|| path.segment()?.name_ref())() else { return false };
                let segment2 = path.qualifier();
                let segment3 = segment2.as_ref().and_then(|it| it.qualifier());
                let segment4 = segment3.as_ref().and_then(|it| it.qualifier());
                let segment3 = segment3.and_then(|it| it.segment()?.name_ref());
                let segment4 = segment4.and_then(|it| it.segment()?.name_ref());
                segment1.text() == "test"
                    && segment3.is_none_or(|it| it.text() == "prelude")
                    && segment4.is_none_or(|it| matches!(it.text(), "core" | "std"))
            });
            if is_test {
                attr_flags.insert(AttrFlags::IS_TEST);
            }

            let path = attr.path().map(|it| view_of(root, &it));
            let Some((first_segment, second_segment)) = path.as_up_to_two_segment() else {
                return ControlFlow::Continue(());
            };
            match second_segment {
                None => match &*first_segment {
                    "rustc_has_incoherent_inherent_impls" => {
                        attr_flags.insert(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS)
                    }
                    "rustc_allow_incoherent_impl" => {
                        attr_flags.insert(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL)
                    }
                    "rustc_scalable_vector" => attr_flags.insert(AttrFlags::HAS_REPR),
                    "fundamental" => attr_flags.insert(AttrFlags::FUNDAMENTAL),
                    "no_std" => attr_flags.insert(AttrFlags::IS_NO_STD),
                    "may_dangle" => attr_flags.insert(AttrFlags::MAY_DANGLE),
                    "rustc_paren_sugar" => attr_flags.insert(AttrFlags::RUSTC_PAREN_SUGAR),
                    "rustc_coinductive" => attr_flags.insert(AttrFlags::RUSTC_COINDUCTIVE),
                    "rustc_force_inline" => attr_flags.insert(AttrFlags::RUSTC_FORCE_INLINE),
                    "rustc_promotable" => attr_flags.insert(AttrFlags::RUSTC_PROMOTABLE),
                    "rustc_pass_indirectly_in_non_rustic_abis" => {
                        attr_flags.insert(AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS)
                    }
                    "unstable" => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                    "deprecated" => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                    "macro_export" => attr_flags.insert(AttrFlags::IS_MACRO_EXPORT),
                    "must_use" => attr_flags.insert(AttrFlags::IS_MUST_USE),
                    "no_mangle" => attr_flags.insert(AttrFlags::NO_MANGLE),
                    "rustc_std_internal_symbol" | "panic_handler" => {
                        attr_flags.insert(AttrFlags::LINKED_BY_NAME)
                    }
                    "used" => attr_flags.insert(AttrFlags::USED),
                    "track_caller" => attr_flags.insert(AttrFlags::TRACK_CALLER),
                    "inline" => attr_flags.insert(AttrFlags::IS_INLINE),
                    "rustc_inherit_overflow_checks" => {
                        attr_flags.insert(AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS)
                    }
                    "thread_local" => attr_flags.insert(AttrFlags::THREAD_LOCAL),
                    "pointee" => attr_flags.insert(AttrFlags::IS_POINTEE),
                    "non_exhaustive" => attr_flags.insert(AttrFlags::NON_EXHAUSTIVE),
                    "ignore" => attr_flags.insert(AttrFlags::IS_IGNORE),
                    "bench" => attr_flags.insert(AttrFlags::IS_BENCH),
                    "rustc_const_panic_str" => attr_flags.insert(AttrFlags::RUSTC_CONST_PANIC_STR),
                    "rustc_intrinsic" => attr_flags.insert(AttrFlags::RUSTC_INTRINSIC),
                    "rustc_safe_intrinsic" => attr_flags.insert(AttrFlags::RUSTC_SAFE_INTRINSIC),
                    "rustc_intrinsic_must_be_overridden" => {
                        attr_flags.insert(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN)
                    }
                    "rustc_allocator" => attr_flags.insert(AttrFlags::RUSTC_ALLOCATOR),
                    "rustc_deallocator" => attr_flags.insert(AttrFlags::RUSTC_DEALLOCATOR),
                    "rustc_reallocator" => attr_flags.insert(AttrFlags::RUSTC_REALLOCATOR),
                    "rustc_allocator_zeroed" => {
                        attr_flags.insert(AttrFlags::RUSTC_ALLOCATOR_ZEROED)
                    }
                    "rustc_reservation_impl" => {
                        attr_flags.insert(AttrFlags::RUSTC_RESERVATION_IMPL)
                    }
                    "rustc_deprecated_safe_2024" => {
                        attr_flags.insert(AttrFlags::RUSTC_DEPRECATED_SAFE_2024)
                    }
                    "rustc_skip_array_during_method_dispatch" => {
                        attr_flags.insert(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH)
                    }
                    _ => {}
                },
                Some(second_segment) => match &*first_segment {
                    "rust_analyzer" => match &*second_segment {
                        "skip" => attr_flags.insert(AttrFlags::RUST_ANALYZER_SKIP),
                        "prefer_underscore_import" => {
                            attr_flags.insert(AttrFlags::PREFER_UNDERSCORE_IMPORT)
                        }
                        _ => {}
                    },
                    "diagnostic" => match &*second_segment {
                        "do_not_recommend" => {
                            attr_flags.insert(AttrFlags::DIAGNOSTIC_DO_NOT_RECOMMEND)
                        }
                        _ => {}
                    },
                    "blaze" => match &*second_segment {
                        "may_suspend" => attr_flags.insert(AttrFlags::MAY_SUSPEND),
                        _ => {}
                    },
                    _ => {}
                },
            }
        }
        _ => {}
    };
    ControlFlow::Continue(())
}

/// The flags that gate a value query: set exactly where that query, reading the source, finds
/// its attribute (a key-value or a word attribute whose path `is1` the name).
fn gating_flags(root: view::Node<'_>, meta: &ast::Meta) -> AttrFlags {
    const KEY_VALUE: &[(&str, AttrFlags)] = &[
        ("export_name", AttrFlags::HAS_EXPORT_NAME),
        ("link_name", AttrFlags::HAS_LINK_NAME),
        ("link_section", AttrFlags::HAS_LINK_SECTION),
        ("linkage", AttrFlags::HAS_LINKAGE),
        ("rustc_diagnostic_item", AttrFlags::HAS_DIAGNOSTIC_ITEM),
        ("must_not_suspend", AttrFlags::MUST_NOT_SUSPEND),
    ];
    // Every word flag set for one segment only, the old walk had none of them.
    const WORD: &[(&str, AttrFlags)] = &[
        ("rustc_nounwind", AttrFlags::RUSTC_NOUNWIND),
        ("rustc_comptime", AttrFlags::RUSTC_COMPTIME),
        ("panic_handler", AttrFlags::PANIC_HANDLER),
        ("rustc_insignificant_dtor", AttrFlags::RUSTC_INSIGNIFICANT_DTOR),
        ("rustc_as_ptr", AttrFlags::RUSTC_AS_PTR),
        ("rustc_no_implicit_autorefs", AttrFlags::RUSTC_NO_IMPLICIT_AUTOREFS),
        ("rustc_never_returns_null_ptr", AttrFlags::RUSTC_NEVER_RETURNS_NULL_PTR),
        ("rustc_canonical_symbol", AttrFlags::RUSTC_CANONICAL_SYMBOL),
        ("marker", AttrFlags::MARKER),
        ("automatically_derived", AttrFlags::AUTOMATICALLY_DERIVED),
        ("rustc_main", AttrFlags::RUSTC_MAIN),
        ("default", AttrFlags::DEFAULT),
        ("naked", AttrFlags::NAKED),
        ("rustc_deny_explicit_impl", AttrFlags::RUSTC_DENY_EXPLICIT_IMPL),
        ("rustc_no_mir_inline", AttrFlags::RUSTC_NO_MIR_INLINE),
        ("cold", AttrFlags::COLD),
        ("must_not_suspend", AttrFlags::MUST_NOT_SUSPEND),
    ];
    const TOKEN_TREE: &[(&str, AttrFlags)] = &[
        ("sanitize", AttrFlags::HAS_SANITIZE),
        ("instruction_set", AttrFlags::HAS_INSTRUCTION_SET),
    ];
    let (path, names) = match meta {
        ast::Meta::KeyValueMeta(meta) => (meta.path(), KEY_VALUE),
        ast::Meta::PathMeta(meta) => (meta.path(), WORD),
        ast::Meta::TokenTreeMeta(meta) if meta.token_tree().is_some() => (meta.path(), TOKEN_TREE),
        _ => return AttrFlags::empty(),
    };
    let path = path.map(|it| view_of(root, &it));
    let optimize_none = match meta {
        ast::Meta::TokenTreeMeta(meta) if path.is1("optimize") => {
            meta.token_tree().is_some_and(|tt| {
                tt.syntax().children_with_tokens().any(|it| it.as_token().is_some_and(|it| it.text() == "none"))
            })
        }
        _ => false,
    };
    let flags = if optimize_none { AttrFlags::OPTIMIZE_NONE } else { AttrFlags::empty() };
    names.iter().filter(|(name, _)| path.is1(name)).fold(flags, |flags, &(_, flag)| flags | flag)
}

const GATING: AttrFlags = AttrFlags::HAS_EXPORT_NAME
    .union(AttrFlags::HAS_LINK_NAME)
    .union(AttrFlags::HAS_LINK_SECTION)
    .union(AttrFlags::HAS_LINKAGE)
    .union(AttrFlags::HAS_DIAGNOSTIC_ITEM)
    .union(AttrFlags::RUSTC_NOUNWIND)
    .union(AttrFlags::RUSTC_COMPTIME)
    .union(AttrFlags::PANIC_HANDLER)
    .union(AttrFlags::HAS_SANITIZE)
    .union(AttrFlags::RUSTC_INSIGNIFICANT_DTOR)
    .union(AttrFlags::RUSTC_AS_PTR)
    .union(AttrFlags::RUSTC_NO_IMPLICIT_AUTOREFS)
    .union(AttrFlags::RUSTC_NEVER_RETURNS_NULL_PTR)
    .union(AttrFlags::RUSTC_CANONICAL_SYMBOL)
    .union(AttrFlags::MARKER)
    .union(AttrFlags::AUTOMATICALLY_DERIVED)
    .union(AttrFlags::RUSTC_MAIN)
    .union(AttrFlags::DEFAULT)
    .union(AttrFlags::NAKED)
    .union(AttrFlags::RUSTC_DENY_EXPLICIT_IMPL)
    .union(AttrFlags::RUSTC_NO_MIR_INLINE)
    .union(AttrFlags::COLD)
    .union(AttrFlags::OPTIMIZE_NONE)
    .union(AttrFlags::HAS_INSTRUCTION_SET)
    .union(AttrFlags::MUST_NOT_SUSPEND);

/// `attr` on a function, parsed.
fn parse_of(attr: &str) -> Parse<SourceFile> {
    SourceFile::parse(&format!("{attr} fn f() {{}}"), Edition::CURRENT)
}

/// The view of `handle` in the tree `root` views: the same node, for the tests that feed one
/// parse to a walk on handles and to a walk on views.
fn view_of<'t, N: AstNode + Typed>(root: view::Node<'t>, handle: &N) -> N::Of<'t> {
    AstPtr::new(handle).to_view(root)
}

/// The first attribute's meta in `parse`.
fn meta_in(parse: &Parse<SourceFile>) -> view::Meta<'_> {
    parse
        .root()
        .descendants()
        .find_map(view::Attr::cast)
        .and_then(|it| it.meta())
        .unwrap_or_else(|| panic!("no attribute in {:?}", parse.root().text()))
}

/// [`meta_in`] as a handle, for the walks as they were.
fn old_meta_in(parse: &Parse<SourceFile>) -> ast::Meta {
    parse
        .tree()
        .syntax()
        .descendants()
        .find_map(ast::Attr::cast)
        .and_then(|it| it.meta())
        .unwrap_or_else(|| panic!("no attribute in {:?}", parse.root().text()))
}

/// Every word in every form an attribute takes, unknown names, and the `#[doc(..)]`, `#[test]`
/// and `#[inline(..)]` shapes the flags look into.
fn corpus() -> Vec<String> {
    let mut corpus = Vec::new();
    let names = Word::TEXTS.iter().copied().chain(["allow", "test", "x"]);
    for name in names {
        for form in [
            format!("#[{name}]"),
            format!("#[r#{name}]"),
            format!("#[{name} = \"main\"]"),
            format!("#[{name} = \"x\"]"),
            format!("#[{name} = 1]"),
            format!("#[{name}(always)]"),
            format!("#[{name}(never)]"),
            format!("#[{name}()]"),
            format!("#[{name}(x, y)]"),
            format!("#[a::{name}]"),
            format!("#[a::{name}(x)]"),
            format!("#[a::{name} = \"x\"]"),
            format!("#[a::b::{name}]"),
            format!("#[a::b::{name}(x)]"),
            format!("#[{name}::x]"),
            format!("#[rust_analyzer::{name}]"),
            format!("#[rust_analyzer::{name}(braces)]"),
            format!("#[diagnostic::{name}]"),
            format!("#[blaze::{name}]"),
            format!("#[unsafe({name})]"),
        ] {
            corpus.push(form);
        }
    }
    for extra in [
        "#[cfg(x)]",
        "#[test]",
        "#[core::prelude::v1::test]",
        "#[std::prelude::test]",
        "#[a::prelude::v1::test]",
        "#[core::x::v1::test]",
        "#[inline(alw)]",
        "#[doc(hidden)]",
        "#[doc(notable_trait)]",
        "#[doc(alias = \"a\")]",
        "#[doc(alias(\"a\", \"b\"))]",
        "#[doc(alias())]",
        "#[doc(keyword = \"k\")]",
        "#[doc(alias = 1)]",
        "#[doc(keyword = \"\\q\")]",
        "#[doc(foo(bar), hidden)]",
        "#[doc(alias(x), keyword = \"y\", hidden)]",
        "#[doc(inline, notable_trait)]",
        "#[doc(test(attr(x)), hidden)]",
        "#[doc(hidden = \"x\")]",
        "#[doc = \"text\"]",
        "#[rustc_skip_during_method_dispatch(array, boxed_slice)]",
        "#[rust_analyzer::completions(ignore_flyimport)]",
        "#[rust_analyzer::macro_style(parentheses)]",
        "#[export_name = \"ma\\u{69}n\"]",
        "#[optimize(none)]",
        "#[optimize(size)]",
        "#[optimize(speed)]",
        "#[r#optimize(none)]",
        "#[a::optimize(none)]",
        "#[instruction_set(arm::t32)]",
    ] {
        corpus.push(extra.to_owned());
    }
    corpus
}

#[test]
fn the_word_index_flags_every_attribute_as_the_string_match_did() {
    for attr in corpus() {
        let parse = parse_of(&attr);
        let old_meta = old_meta_in(&parse);
        let mut old = AttrFlags::empty();
        let _ = old_match_attr_flags(&mut old, parse.root(), old_meta.clone());
        let mut new = AttrFlags::empty();
        let _ = match_attr_flags(&mut new, meta_in(&parse));
        assert_eq!(new - GATING, old, "{attr}");
        assert_eq!(new & GATING, gating_flags(parse.root(), &old_meta), "{attr}");
    }
}

#[test]
fn gating_flags_of_known_shapes() {
    for (attr, flags) in [
        ("#[export_name = \"x\"]", AttrFlags::HAS_EXPORT_NAME),
        ("#[export_name = \"main\"]", AttrFlags::HAS_EXPORT_NAME | AttrFlags::IS_EXPORT_NAME_MAIN),
        ("#[link_name = \"x\"]", AttrFlags::HAS_LINK_NAME),
        ("#[link_section = \"x\"]", AttrFlags::HAS_LINK_SECTION),
        ("#[linkage = \"extern_weak\"]", AttrFlags::HAS_LINKAGE),
        ("#[rustc_diagnostic_item = \"x\"]", AttrFlags::HAS_DIAGNOSTIC_ITEM),
        ("#[rustc_nounwind]", AttrFlags::RUSTC_NOUNWIND),
        ("#[r#rustc_nounwind]", AttrFlags::RUSTC_NOUNWIND),
        ("#[a::rustc_nounwind]", AttrFlags::empty()),
        ("#[a::b::rustc_nounwind]", AttrFlags::empty()),
        ("#[rustc_comptime]", AttrFlags::RUSTC_COMPTIME),
        ("#[panic_handler]", AttrFlags::PANIC_HANDLER | AttrFlags::LINKED_BY_NAME),
        ("#[a::b::panic_handler]", AttrFlags::LINKED_BY_NAME),
        ("#[inline(always)]", AttrFlags::IS_INLINE | AttrFlags::INLINE_ALWAYS),
        ("#[inline(never)]", AttrFlags::INLINE_NEVER),
        ("#[inline]", AttrFlags::IS_INLINE),
        ("#[sanitize(address = \"off\")]", AttrFlags::HAS_SANITIZE),
        ("#[r#sanitize(thread = \"on\")]", AttrFlags::HAS_SANITIZE),
        ("#[sanitize]", AttrFlags::empty()),
        ("#[a::sanitize(address = \"off\")]", AttrFlags::empty()),
        ("#[a::b::sanitize(address = \"off\")]", AttrFlags::empty()),
        ("#[marker]", AttrFlags::MARKER),
        ("#[automatically_derived]", AttrFlags::AUTOMATICALLY_DERIVED),
        ("#[rustc_main]", AttrFlags::RUSTC_MAIN),
        ("#[default]", AttrFlags::DEFAULT),
        ("#[naked]", AttrFlags::NAKED),
        ("#[a::naked]", AttrFlags::empty()),
        ("#[rustc_deny_explicit_impl]", AttrFlags::RUSTC_DENY_EXPLICIT_IMPL),
        ("#[r#rustc_deny_explicit_impl]", AttrFlags::RUSTC_DENY_EXPLICIT_IMPL),
        ("#[a::rustc_deny_explicit_impl]", AttrFlags::empty()),
        ("#[rustc_deny_explicit_impl(x)]", AttrFlags::empty()),
        ("#[link_ordinal(1)]", AttrFlags::HAS_LINK_ORDINAL),
        ("#[a::link_ordinal(1)]", AttrFlags::empty()),
        ("#[link_ordinal]", AttrFlags::empty()),
        ("#[rustc_no_mir_inline]", AttrFlags::RUSTC_NO_MIR_INLINE),
        ("#[a::rustc_no_mir_inline]", AttrFlags::empty()),
        ("#[cold]", AttrFlags::COLD),
        ("#[r#cold]", AttrFlags::COLD),
        ("#[a::cold]", AttrFlags::empty()),
        ("#[optimize(none)]", AttrFlags::OPTIMIZE_NONE),
        ("#[optimize(size)]", AttrFlags::empty()),
        ("#[optimize(speed)]", AttrFlags::empty()),
        ("#[a::optimize(none)]", AttrFlags::empty()),
        ("#[instruction_set(arm::a32)]", AttrFlags::HAS_INSTRUCTION_SET),
        ("#[instruction_set(arm::t32)]", AttrFlags::HAS_INSTRUCTION_SET),
        ("#[a::instruction_set(arm::t32)]", AttrFlags::empty()),
        ("#[instruction_set]", AttrFlags::empty()),
    ] {
        let mut new = AttrFlags::empty();
        let _ = match_attr_flags(&mut new, meta_in(&parse_of(attr)));
        assert_eq!(new, flags, "{attr}");
    }
}

fn functions(db: &TestDB) -> Vec<(String, FunctionId)> {
    let def_map = crate_def_map(db, fixture::test_crate(db));
    let mut functions: Vec<_> = def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .filter_map(|decl| match decl {
            ModuleDefId::FunctionId(func) => {
                let name = func.lookup(db).source_view(db).value.name()?.text().to_owned();
                Some((name, func))
            }
            _ => None,
        })
        .collect();
    functions.sort_by(|a, b| a.0.cmp(&b.0));
    functions
}

const GATED: &str = r#"
#[export_name = "exported"] pub fn a() {}
#[export_name = ("paren")] pub fn b() {}
#[link_section = ".text.c"] pub fn c() {}
#[rustc_nounwind] pub fn d() {}
#[rustc_comptime] pub fn e() {}
#[panic_handler] pub fn f() {}
#[rustc_diagnostic_item = "g_item"] pub fn g() {}
#[cfg_attr(all(), export_name = "via_cfg_attr")] pub fn h() {}
#[cfg_attr(any(), export_name = "off")] pub fn i() {}
#[no_builtins] pub fn j() {}
#[linkage = "extern_weak"] pub fn m() {}
#[a::rustc_nounwind] #[export_name = 1] pub fn n() {}
#[cold] #[rustc_no_mir_inline] pub fn o() {}
#[cfg_attr(all(), cold)] pub fn p() {}
pub fn plain() {}
extern "C" {
    #[link_name = "real"] fn k();
}
"#;

#[test]
fn gated_queries_read_what_the_source_says() {
    let db = fixture::db(GATED).into_db();
    let functions = functions(&db);
    let by_name = |name: &str| {
        functions.iter().find(|(it, _)| it == name).unwrap_or_else(|| panic!("no fn {name}")).1
    };
    let sym = |it: Option<Symbol>| it.map(|it| it.as_str().to_owned());

    assert_eq!(sym(AttrFlags::export_name(&db, by_name("a").into())).as_deref(), Some("exported"));
    assert_eq!(sym(AttrFlags::export_name(&db, by_name("b").into())).as_deref(), Some("paren"));
    assert_eq!(sym(AttrFlags::link_section(&db, by_name("c").into())).as_deref(), Some(".text.c"));
    assert!(AttrFlags::is_rustc_nounwind(&db, by_name("d")));
    assert!(AttrFlags::has_word(&db, by_name("e").into(), "rustc_comptime"));
    assert!(AttrFlags::has_word(&db, by_name("f").into(), "panic_handler"));
    assert_eq!(sym(AttrFlags::diagnostic_item(&db, by_name("g").into())).as_deref(), Some("g_item"));
    assert_eq!(
        sym(AttrFlags::export_name(&db, by_name("h").into())).as_deref(),
        Some("via_cfg_attr")
    );
    assert_eq!(AttrFlags::export_name(&db, by_name("i").into()), None);
    assert!(AttrFlags::has_word(&db, by_name("j").into(), "no_builtins"));
    assert_eq!(sym(AttrFlags::linkage(&db, by_name("m").into())).as_deref(), Some("extern_weak"));
    assert!(!AttrFlags::is_rustc_nounwind(&db, by_name("n")));
    assert_eq!(AttrFlags::export_name(&db, by_name("n").into()), None);
    assert_eq!(sym(AttrFlags::link_name(&db, by_name("k").into())).as_deref(), Some("real"));
    let o = AttrFlags::query(&db, by_name("o").into());
    assert!(o.contains(AttrFlags::COLD | AttrFlags::RUSTC_NO_MIR_INLINE));
    assert!(AttrFlags::query(&db, by_name("p").into()).contains(AttrFlags::COLD));
    assert!(!AttrFlags::query(&db, by_name("plain").into()).intersects(AttrFlags::COLD | AttrFlags::RUSTC_NO_MIR_INLINE));

    for (name, func) in &functions {
        let owner = AttrDefId::from(*func);
        type Read = fn(&Db, AttrDefId) -> Option<Symbol>;
        for (query, read) in [
            ("export_name", AttrFlags::export_name as Read),
            ("link_name", AttrFlags::link_name as Read),
            ("link_section", AttrFlags::link_section as Read),
            ("linkage", AttrFlags::linkage as Read),
            ("rustc_diagnostic_item", AttrFlags::diagnostic_item as Read),
        ] {
            assert_eq!(read(&db, owner), symbol_attr(&db, owner, query), "{query} of {name}");
        }
        for word in
            ["rustc_nounwind", "rustc_comptime", "panic_handler", "no_builtins", "inline", "cold", "rustc_no_mir_inline"]
        {
            assert_eq!(
                AttrFlags::has_word(&db, owner, word),
                AttrFlags::has_word_in_source(&db, owner, word),
                "{word} of {name}"
            );
        }
        assert_eq!(
            AttrFlags::is_rustc_nounwind(&db, *func),
            AttrFlags::has_word_in_source(&db, owner, "rustc_nounwind"),
            "{name}"
        );
    }
}

#[test]
fn field_flags_are_indexed_by_field() {
    let db = fixture::db(
        r#"
pub struct S {
    #[cfg(any())] gone: u8,
    #[doc(hidden)] a: u8,
    b: u8,
    #[cfg_attr(all(), deprecated)] c: u8,
}
"#,
    )
    .into_db();
    let def_map = crate_def_map(&db, fixture::test_crate(&db));
    let strukt = def_map
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
        .find_map(|decl| match decl {
            ModuleDefId::AdtId(AdtId::StructId(it)) => Some(it),
            _ => None,
        })
        .expect("a struct");
    let flags = |at: u32| {
        let local_id = Idx::from_raw(RawIdx::from_u32(at));
        AttrFlags::query_field(&db, FieldId { parent: VariantId::StructId(strukt), local_id })
    };
    assert_eq!(flags(0), AttrFlags::IS_DOC_HIDDEN);
    assert_eq!(flags(1), AttrFlags::empty());
    assert_eq!(flags(2), AttrFlags::IS_DEPRECATED);
    assert_eq!(flags(3), AttrFlags::empty());
}

#[test]
fn inner_docs_of_a_fn_body() {
    let db = fixture::db(
        r#"
/// outer
pub fn documented() {
    //! inner
    let _ = 1;
}
"#,
    )
    .into_db();
    let (_, func) = functions(&db).into_iter().find(|(name, _)| name == "documented").unwrap();
    let docs = AttrFlags::docs(&db, func.into()).expect("docs");
    assert!(docs.docs().contains("outer") && docs.docs().contains("inner"), "{:?}", docs.docs());
}

/// The first attribute's token tree in `parse`.
fn tt_in(parse: &Parse<SourceFile>) -> view::TokenTree<'_> {
    match meta_in(parse) {
        view::Meta::TokenTreeMeta(meta) => meta.token_tree().expect("a token tree"),
        _ => panic!("no token tree in {:?}", parse.root().text()),
    }
}

/// [`tt_in`] as a handle, for the walks as they were.
fn old_tt_in(parse: &Parse<SourceFile>) -> ast::TokenTree {
    match old_meta_in(parse) {
        ast::Meta::TokenTreeMeta(meta) => meta.token_tree().expect("a token tree"),
        _ => panic!("no token tree in {:?}", parse.root().text()),
    }
}

/// Each option of a name set, the near misses of it, and other identifiers, as the one option
/// of `#[path(..)]`.
fn option_corpus(path: &str, texts: &[&str]) -> Vec<String> {
    let mut options: Vec<String> = Vec::new();
    for text in texts.iter().chain(&["xy", "Braces", "array_", "ignore"]) {
        options.push((*text).to_owned());
        options.push(format!("r#{text}"));
        options.push(format!("{text}s"));
        options.push(text[..text.len() - 1].to_owned());
    }
    let mut corpus: Vec<String> =
        options.iter().filter(|it| !it.is_empty()).map(|it| format!("#[{path}({it})]")).collect();
    corpus.push(format!("#[{path}()]"));
    corpus.push(format!("#[{path}({}, {})]", texts[0], texts[texts.len() - 1]));
    corpus.push(format!("#[{path}(\"{}\")]", texts[0]));
    corpus
}

#[test]
fn option_sets_flag_as_the_string_match_did() {
    use syntax::{NodeOrToken, ast::TokenTreeChildren};

    fn old_ra_completions(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        let tt = TokenTreeChildren::new(&tt);
        if let Ok(NodeOrToken::Token(option)) = itertools::Itertools::exactly_one(tt)
            && option.kind().is_any_identifier()
        {
            match option.text() {
                "ignore_flyimport" => attr_flags.insert(AttrFlags::COMPLETE_IGNORE_FLYIMPORT),
                "ignore_methods" => attr_flags.insert(AttrFlags::COMPLETE_IGNORE_METHODS),
                "ignore_flyimport_methods" => {
                    attr_flags.insert(AttrFlags::COMPLETE_IGNORE_FLYIMPORT_METHODS)
                }
                _ => {}
            }
        }
    }
    fn old_ra_macro_style(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        let tt = TokenTreeChildren::new(&tt);
        if let Ok(NodeOrToken::Token(option)) = itertools::Itertools::exactly_one(tt)
            && option.kind().is_any_identifier()
        {
            match option.text() {
                "braces" => attr_flags.insert(AttrFlags::MACRO_STYLE_BRACES),
                "brackets" => attr_flags.insert(AttrFlags::MACRO_STYLE_BRACKETS),
                "parentheses" => attr_flags.insert(AttrFlags::MACRO_STYLE_PARENTHESES),
                _ => {}
            }
        }
    }
    fn old_skip_during_method_dispatch(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        for kind in TokenTreeChildren::new(&tt) {
            if let NodeOrToken::Token(kind) = kind
                && kind.kind().is_any_identifier()
            {
                match kind.text() {
                    "array" => {
                        attr_flags.insert(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH)
                    }
                    "boxed_slice" => {
                        attr_flags.insert(AttrFlags::RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH)
                    }
                    _ => {}
                }
            }
        }
    }

    type Extract = fn(&mut AttrFlags, view::TokenTree<'_>);
    type OldExtract = fn(&mut AttrFlags, ast::TokenTree);
    for (path, texts, new, old) in [
        (
            "rust_analyzer::completions",
            Completions::TEXTS,
            extract_ra_completions as Extract,
            old_ra_completions as OldExtract,
        ),
        (
            "rust_analyzer::macro_style",
            MacroStyle::TEXTS,
            extract_ra_macro_style as Extract,
            old_ra_macro_style as OldExtract,
        ),
        (
            "rustc_skip_during_method_dispatch",
            SkipDuringMethodDispatch::TEXTS,
            extract_rustc_skip_during_method_dispatch as Extract,
            old_skip_during_method_dispatch as OldExtract,
        ),
    ] {
        for attr in option_corpus(path, texts) {
            let parse = parse_of(&attr);
            let (mut got, mut want) = (AttrFlags::empty(), AttrFlags::empty());
            new(&mut got, tt_in(&parse));
            old(&mut want, old_tt_in(&parse));
            assert_eq!(got, want, "{attr}");
        }
    }
}

#[test]
fn doc_atoms_flag_as_the_string_match_did() {
    let mut corpus = Vec::new();
    for &text in DocWord::TEXTS.iter().chain(&["inline", "test", "xy"]) {
        for atom in [
            text.to_owned(),
            format!("r#{text}"),
            format!("{text}_"),
            text[..text.len() - 1].to_owned(),
        ] {
            if atom.is_empty() {
                continue;
            }
            corpus.push(format!("#[doc({atom})]"));
            corpus.push(format!("#[doc({atom} = \"v\")]"));
            corpus.push(format!("#[doc({atom} = 1)]"));
            corpus.push(format!("#[doc({atom}(\"a\"))]"));
            corpus.push(format!("#[doc({atom}, hidden)]"));
        }
    }
    for attr in corpus {
        let parse = parse_of(&attr);
        let (mut old, mut new) = (AttrFlags::empty(), AttrFlags::empty());
        let _ = old_match_attr_flags(&mut old, parse.root(), old_meta_in(&parse));
        let _ = match_attr_flags(&mut new, meta_in(&parse));
        assert_eq!(new - GATING, old, "{attr}");
    }
}

/// `parse_repr_tt` as it was, by string `match` and `BuiltinInt::from_suffix`.
fn old_parse_repr_tt(tt: &ast::TokenTree) -> Option<rustc_abi::ReprOptions> {
    use crate::builtin_type::{BuiltinInt, BuiltinUint};
    use either::Either;
    use rustc_abi::{Align, Integer, IntegerType, ReprFlags, ReprOptions};
    use syntax::{AstToken, NodeOrToken, ast::TokenTreeChildren};

    let mut tts = TokenTreeChildren::new(tt).peekable();
    let mut acc = ReprOptions::default();
    while let Some(tt) = tts.next() {
        let NodeOrToken::Token(ident) = tt else { continue };
        if !ident.kind().is_any_identifier() {
            continue;
        }
        let literal = |tts: &mut std::iter::Peekable<TokenTreeChildren>| {
            if let Some(NodeOrToken::Node(tt)) = tts.peek() {
                let tt = tt.clone();
                tts.next();
                let mut tt_iter = TokenTreeChildren::new(&tt);
                if let Some(NodeOrToken::Token(lit)) = tt_iter.next()
                    && let Some(lit) = ast::IntNumber::cast(lit)
                    && let Ok(lit) = lit.value()
                    && let Ok(lit) = u64::try_from(lit)
                {
                    return (true, Some(lit));
                }
                return (true, None);
            }
            (false, None)
        };
        let repr = match ident.text().trim_start_matches("r#") {
            "packed" => {
                let pack = literal(&mut tts).1.unwrap_or(0);
                ReprOptions {
                    pack: Some(Align::from_bytes(pack).unwrap_or(Align::ONE)),
                    ..Default::default()
                }
            }
            "align" => {
                let align = literal(&mut tts).1.and_then(|it| Align::from_bytes(it).ok());
                ReprOptions { align, ..Default::default() }
            }
            "C" => ReprOptions { flags: ReprFlags::IS_C, ..Default::default() },
            "transparent" => ReprOptions { flags: ReprFlags::IS_TRANSPARENT, ..Default::default() },
            "simd" => ReprOptions { flags: ReprFlags::IS_SIMD, ..Default::default() },
            repr => {
                let int = BuiltinInt::from_suffix(repr)
                    .map(Either::Left)
                    .or_else(|| BuiltinUint::from_suffix(repr).map(Either::Right))
                    .map(|builtin| match builtin {
                        Either::Left(bi) => match bi {
                            BuiltinInt::Isize => IntegerType::Pointer(true),
                            BuiltinInt::I8 => IntegerType::Fixed(Integer::I8, true),
                            BuiltinInt::I16 => IntegerType::Fixed(Integer::I16, true),
                            BuiltinInt::I32 => IntegerType::Fixed(Integer::I32, true),
                            BuiltinInt::I64 => IntegerType::Fixed(Integer::I64, true),
                            BuiltinInt::I128 => IntegerType::Fixed(Integer::I128, true),
                        },
                        Either::Right(bu) => match bu {
                            BuiltinUint::Usize => IntegerType::Pointer(false),
                            BuiltinUint::U8 => IntegerType::Fixed(Integer::I8, false),
                            BuiltinUint::U16 => IntegerType::Fixed(Integer::I16, false),
                            BuiltinUint::U32 => IntegerType::Fixed(Integer::I32, false),
                            BuiltinUint::U64 => IntegerType::Fixed(Integer::I64, false),
                            BuiltinUint::U128 => IntegerType::Fixed(Integer::I128, false),
                        },
                    });
                ReprOptions { int, ..Default::default() }
            }
        };
        super::merge_repr(&mut acc, repr);
    }
    Some(acc)
}

#[test]
fn repr_hints_read_as_the_string_match_did() {
    let mut hints: Vec<String> = Vec::new();
    for &text in ReprWord::TEXTS.iter().chain(&["Rust", "c", "i256", "f32", "x"]) {
        hints.push(text.to_owned());
        hints.push(format!("r#{text}"));
        hints.push(format!("{text}_"));
        hints.push(text.to_uppercase());
        hints.push(text[..text.len() - 1].to_owned());
    }
    let mut corpus: Vec<String> = hints
        .iter()
        .filter(|it| !it.is_empty() && !it.starts_with(|c: char| c.is_ascii_digit()))
        .map(|it| format!("#[repr({it})]"))
        .collect();
    corpus.extend(
        [
            "#[repr(packed(4))]",
            "#[repr(packed(3))]",
            "#[repr(packed)]",
            "#[repr(align(16))]",
            "#[repr(align(3))]",
            "#[repr(align)]",
            "#[repr(C, u8)]",
            "#[repr(u8, C, align(8), packed(2))]",
            "#[repr(transparent, simd)]",
            "#[repr(i32, u64)]",
            "#[repr(\"C\")]",
            "#[repr()]",
        ]
        .map(str::to_owned),
    );
    for attr in corpus {
        let parse = parse_of(&attr);
        assert_eq!(parse_repr_tt(tt_in(&parse)), old_parse_repr_tt(&old_tt_in(&parse)), "{attr}");
    }
}

/// `parse_link_tt` as it was, by string `match`.
fn old_parse_link_tt(
    root: view::Node<'_>,
    tt: &ast::TokenTree,
) -> Option<(NativeLib, Option<cfg::CfgExpr>)> {
    use syntax::{AstToken, NodeOrToken, SyntaxKind, T, ast::TokenTreeChildren};

    let mut lib = NativeLib {
        name: Symbol::empty(),
        kind: NativeLibKind::Unspecified,
        verbatim: false,
        whole_archive: false,
        bundle: true,
        as_needed: None,
        import_name_type: None,
    };
    let mut cfg = None;
    let mut named = false;
    let mut children = TokenTreeChildren::new(tt).peekable();
    while let Some(child) = children.next() {
        let NodeOrToken::Token(key) = child else { continue };
        if key.kind() != SyntaxKind::IDENT {
            continue;
        }
        match children.peek() {
            Some(NodeOrToken::Node(group)) if key.text() == "cfg" => {
                cfg = link_cfg(view_of(root, group));
                children.next();
            }
            Some(NodeOrToken::Token(eq)) if eq.kind() == T![=] => {
                children.next();
                let Some(NodeOrToken::Token(value)) = children.next() else { continue };
                let Some(value) =
                    ast::String::cast(value).and_then(|it| Some(it.value().ok()?.into_owned()))
                else {
                    continue;
                };
                match key.text() {
                    "name" => {
                        lib.name = Symbol::intern(&value);
                        named = true;
                    }
                    "kind" => {
                        lib.kind = match &*value {
                            "static" => NativeLibKind::Static,
                            "dylib" => NativeLibKind::Dylib,
                            "framework" => NativeLibKind::Framework,
                            "raw-dylib" => NativeLibKind::RawDylib,
                            other => NativeLibKind::Other(Symbol::intern(other)),
                        }
                    }
                    "import_name_type" => lib.import_name_type = Some(Symbol::intern(&value)),
                    "modifiers" => {
                        for modifier in value.split(',') {
                            let (on, name) = modifier.split_at(modifier.len().min(1));
                            match name {
                                "verbatim" => lib.verbatim = on == "+",
                                "whole-archive" => lib.whole_archive = on == "+",
                                "bundle" => lib.bundle = on == "+",
                                "as-needed" => lib.as_needed = Some(on == "+"),
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
            }
            _ => (),
        }
    }
    named.then_some((lib, cfg))
}

#[test]
fn link_attrs_read_as_the_string_match_did() {
    let mut corpus: Vec<String> = Vec::new();
    for &key in LinkKey::TEXTS.iter().chain(&["wasm_import_module", "names", "Kind", "r#name"]) {
        corpus.push(format!("#[link({key} = \"foo\")]"));
        corpus.push(format!("#[link(name = \"a\", {key} = \"static\")]"));
        corpus.push(format!("#[link(name = \"a\", {key}(unix))]"));
        corpus.push(format!("#[link(name = \"a\", {key} = 1)]"));
    }
    let kinds = LinkKind::TEXTS
        .iter()
        .chain(&["Static", "dylib_", "raw_dylib", "", "framewor", "link-arg"]);
    for kind in kinds {
        corpus.push(format!("#[link(name = \"a\", kind = \"{kind}\")]"));
    }
    let modifiers =
        LinkModifier::TEXTS.iter().chain(&["whole_archive", "Bundle", "verbatim_", "", "x"]);
    for modifier in modifiers {
        for sign in ["+", "-", "", "*"] {
            corpus.push(format!("#[link(name = \"a\", modifiers = \"{sign}{modifier}\")]"));
        }
    }
    corpus.extend(
        [
            "#[link(name = \"a\", modifiers = \"+verbatim,-bundle,+whole-archive\")]",
            "#[link(name = \"a\", modifiers = \"+as-needed,-as-needed\")]",
            "#[link(name = \"a\", kind = \"raw-dylib\", import_name_type = \"undecorated\")]",
            "#[link(name = \"a\", modifiers = \"\")]",
            "#[link(name = \"a\", kind = \"static\", kind = \"dylib\")]",
            "#[link(name = \"a\", cfg(target_os = \"linux\"))]",
            "#[link(name = \"a\", cfg = \"x\")]",
            "#[link(wasm_import_module = \"m\")]",
            "#[link()]",
        ]
        .map(str::to_owned),
    );
    for attr in corpus {
        let parse = parse_of(&attr);
        let old = old_parse_link_tt(parse.root(), &old_tt_in(&parse));
        assert_eq!(parse_link_tt(tt_in(&parse)), old, "{attr}");
    }
}

#[test]
fn link_keeps_what_blaze_cannot_link() {
    let lib = |attr: &str| parse_link_tt(tt_in(&parse_of(attr))).unwrap().0;
    let other = lib("#[link(name = \"a\", kind = \"link-arg\")]");
    assert_eq!(other.kind, NativeLibKind::Other(Symbol::intern("link-arg")));
    assert_eq!(lib("#[link(name = \"a\")]").as_needed, None);
    assert_eq!(lib("#[link(name = \"a\", modifiers = \"+as-needed\")]").as_needed, Some(true));
    assert_eq!(lib("#[link(name = \"a\", modifiers = \"-as-needed\")]").as_needed, Some(false));
    let raw = lib("#[link(name = \"a\", kind = \"raw-dylib\", import_name_type = \"noprefix\")]");
    assert_eq!(raw.import_name_type, Some(Symbol::intern("noprefix")));
    assert_eq!(lib("#[link(name = \"a\")]").import_name_type, None);
}

#[test]
fn rustc_words_are_flags() {
    let db = fixture::db(
        r#"
#[rustc_insignificant_dtor]
pub struct Insignificant;
#[cfg_attr(all(), rustc_insignificant_dtor)]
pub struct InsignificantByCfgAttr;
#[cfg_attr(any(), rustc_insignificant_dtor)]
pub struct NotInsignificant;
#[rustc_as_ptr]
pub fn as_ptr() {}
#[rustc_no_implicit_autorefs]
pub fn no_implicit_autorefs() {}
#[rustc_never_returns_null_ptr]
pub fn never_returns_null_ptr() {}
extern "C" {
    #[rustc_canonical_symbol]
    pub fn memcpy();
}
#[a::rustc_as_ptr]
pub fn two_segments() {}
"#,
    )
    .into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let flags = |name: &str| {
        let (_, per_ns) = scope.entries().find(|(it, _)| it.as_str() == name).unwrap();
        let def = match per_ns.take_types().or(per_ns.take_values()).unwrap() {
            ModuleDefId::AdtId(it) => AttrDefId::AdtId(it),
            ModuleDefId::FunctionId(it) => AttrDefId::FunctionId(it),
            it => panic!("{it:?}"),
        };
        AttrFlags::query(&db, def)
    };
    for (name, flag) in [
        ("Insignificant", AttrFlags::RUSTC_INSIGNIFICANT_DTOR),
        ("InsignificantByCfgAttr", AttrFlags::RUSTC_INSIGNIFICANT_DTOR),
        ("as_ptr", AttrFlags::RUSTC_AS_PTR),
        ("no_implicit_autorefs", AttrFlags::RUSTC_NO_IMPLICIT_AUTOREFS),
        ("never_returns_null_ptr", AttrFlags::RUSTC_NEVER_RETURNS_NULL_PTR),
        ("memcpy", AttrFlags::RUSTC_CANONICAL_SYMBOL),
    ] {
        assert!(flags(name).contains(flag), "{name}");
    }
    assert!(!flags("NotInsignificant").contains(AttrFlags::RUSTC_INSIGNIFICANT_DTOR));
    assert!(!flags("two_segments").contains(AttrFlags::RUSTC_AS_PTR));
}

/// `#[blaze::may_suspend]` on a function or a field, and rustc's `#[must_not_suspend]` (a word
/// or `= "reason"`) on an ADT, are the bits hir-ty-suspend tests; neither in another shape.
#[test]
fn suspend_attributes_are_flags() {
    let db = fixture::db(
        r#"
#[blaze::may_suspend]
pub fn parks() {}
#[may_suspend]
pub fn one_segment() {}
#[a::blaze::may_suspend]
pub fn three_segments() {}
#[blaze::may_suspend(x)]
pub fn with_arguments() {}
pub struct Holder {
    #[blaze::may_suspend] f: fn(),
    g: fn(),
}
#[must_not_suspend]
pub struct Guard;
#[must_not_suspend = "a lock is held"]
pub enum Reasoned { A }
#[a::must_not_suspend]
pub struct TwoSegments;
"#,
    )
    .into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let scope = def_map.module(&db, def_map.root).scope(&db);
    let def = |name: &str| {
        let (_, per_ns) = scope.entries().find(|(it, _)| it.as_str() == name).unwrap();
        match per_ns.take_types().or(per_ns.take_values()).unwrap() {
            ModuleDefId::AdtId(it) => AttrDefId::AdtId(it),
            ModuleDefId::FunctionId(it) => AttrDefId::FunctionId(it),
            it => panic!("{it:?}"),
        }
    };
    let flags = |name: &str| AttrFlags::query(&db, def(name));
    assert!(flags("parks").contains(AttrFlags::MAY_SUSPEND));
    for name in ["one_segment", "three_segments", "with_arguments"] {
        assert!(!flags(name).contains(AttrFlags::MAY_SUSPEND), "{name}");
    }
    let AttrDefId::AdtId(AdtId::StructId(holder)) = def("Holder") else { panic!("Holder") };
    let field = |at: u32| {
        let local_id = Idx::from_raw(RawIdx::from_u32(at));
        AttrFlags::query_field(&db, FieldId { parent: VariantId::StructId(holder), local_id })
    };
    assert_eq!(field(0), AttrFlags::MAY_SUSPEND);
    assert_eq!(field(1), AttrFlags::empty());
    for name in ["Guard", "Reasoned"] {
        assert!(flags(name).contains(AttrFlags::MUST_NOT_SUSPEND), "{name}");
    }
    assert!(!flags("TwoSegments").contains(AttrFlags::MUST_NOT_SUSPEND));
}

/// The items of the root module of `db`'s test crate, their enums' variants and its impls.
fn root_owners(db: &TestDB) -> Vec<AttrDefId> {
    let def_map = crate_def_map(db, db.fetch_test_crate());
    let scope = def_map.module(db, def_map.root).scope(db);
    let mut owners = vec![AttrDefId::ModuleId(def_map.root)];
    for def in scope.declarations() {
        match def {
            ModuleDefId::TraitId(it) => owners.push(it.into()),
            ModuleDefId::FunctionId(it) => owners.push(it.into()),
            ModuleDefId::ModuleId(it) => owners.push(AttrDefId::ModuleId(it)),
            ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                owners.push(AdtId::EnumId(it).into());
                let variants = it.enum_variants(db).variants.values();
                owners.extend(variants.map(|&(v, _)| AttrDefId::EnumVariantId(v)));
            }
            ModuleDefId::AdtId(it) => owners.push(it.into()),
            ModuleDefId::ConstId(it) => owners.push(it.into()),
            ModuleDefId::StaticId(it) => owners.push(it.into()),
            _ => {}
        }
    }
    owners.extend(scope.impls().map(AttrDefId::from));
    owners
}

/// Each word `has_word` answers by a flag gives what the walk of the source gives.
#[test]
fn has_word_flags_match_the_source() {
    let db = fixture::db(
        r#"
#[marker] pub trait M {}
#[automatically_derived] impl M for u8 {}
#[rustc_main] fn main() {}
pub enum E { #[default] A, B }
#[naked] pub extern "C" fn n() {}
#[unsafe(naked)] pub extern "C" fn u() {}
#[cfg_attr(all(), naked)] pub extern "C" fn c() {}
#[rustc_deny_explicit_impl] pub trait D {}
#[rustc_canonical_symbol] extern "C" { fn f(); }
#[rust_analyzer::marker] pub trait N {}
#[a::rustc_deny_explicit_impl] pub trait Q {}
pub struct Plain;
"#,
    )
    .into_db();
    let owners = root_owners(&db);
    for word in [
        "marker",
        "automatically_derived",
        "rustc_main",
        "default",
        "naked",
        "rustc_deny_explicit_impl",
        "rustc_canonical_symbol",
    ] {
        let mut found = 0;
        for &owner in &owners {
            let flag = AttrFlags::has_word(&db, owner, word);
            assert_eq!(flag, AttrFlags::has_word_in_source(&db, owner, word), "{word} on {owner:?}");
            found += usize::from(flag);
        }
        let want = match word {
            "naked" => 3,
            "rustc_canonical_symbol" => 0,
            _ => 1,
        };
        assert_eq!(found, want, "{word}");
    }
}

/// Every attribute lookup a later query or an IDE request makes, on an item asked once: a
/// memo read and a bit test, no allocation.
#[test]
fn warm_attribute_lookups_allocate_nothing() {
    let db = fixture::db(
        r#"
#![feature(lang_items)]
#[repr(C)]
#[must_use = "use it"]
#[doc(alias = "Ess")]
pub struct S {
    #[doc(hidden)]
    pub a: u8,
    #[cfg(any())]
    pub b: u8,
}
#[lang = "sized"]
#[marker]
pub trait Sized {}
#[rustc_diagnostic_item = "the_fn"]
#[export_name = "named"]
pub fn f() {}
"#,
    )
    .into_db();
    let (mut strukt, mut trait_, mut func) = (None, None, None);
    for owner in root_owners(&db) {
        match owner {
            AttrDefId::AdtId(AdtId::StructId(it)) => strukt = Some(it),
            AttrDefId::TraitId(it) => trait_ = Some(it),
            AttrDefId::FunctionId(it) => func = Some(it),
            _ => {}
        }
    }
    let (strukt, trait_, func) = (strukt.unwrap(), trait_.unwrap(), func.unwrap());
    let field =
        FieldId { parent: VariantId::StructId(strukt), local_id: Idx::from_raw(RawIdx::from_u32(0)) };
    let krate = db.fetch_test_crate();
    let the_fn = Symbol::intern("the_fn");
    let lookups = || {
        let s = AttrFlags::query(&db, strukt.into());
        assert!(s.contains(AttrFlags::HAS_REPR | AttrFlags::IS_MUST_USE));
        assert!(AttrFlags::query_field(&db, field).contains(AttrFlags::IS_DOC_HIDDEN));
        assert!(AttrFlags::repr(&db, AdtId::StructId(strukt)).is_some());
        assert_eq!(AttrFlags::must_use_message(&db, strukt.into()), Some(Some("use it")));
        assert_eq!(s.doc_aliases(&db, Either::Left(strukt.into())).len(), 1);
        assert!(AttrFlags::lang_item(&db, trait_.into()).is_some());
        assert!(AttrFlags::has_word(&db, trait_.into(), "marker"));
        assert!(AttrFlags::diagnostic_item(&db, func.into()).is_some());
        assert!(AttrFlags::export_name(&db, func.into()).is_some());
        assert!(AttrFlags::docs(&db, strukt.into()).is_none());
        assert!(crate::diagnostic_item::diagnostic_items(&db, krate).get(&the_fn).is_some());
    };
    lookups();
    blaze_alloc_count::assert_at_most(0, "attribute lookups on a warm item", lookups);
}

/// The flags of `owner` read off its file's parse, as they were before the outline.
fn live_flags(db: &TestDB, owner: AttrDefId) -> AttrFlags {
    let (source, outer_mod_decl, crate_attrs, krate) = attrs_source(db, owner, Tree::Live);
    let mut flags = AttrFlags::empty();
    let _ = collect_attrs_in(db, source, outer_mod_decl, crate_attrs, krate, |attr, _| {
        match_attr_flags(&mut flags, attr)
    });
    flags
}

/// The flags read off the outline are those of the file's parse, for items whose bodies it
/// leaves out, keeps (an inner attribute may start them) or that are bodies themselves.
#[test]
fn outline_flags_are_the_live_flags() {
    let db = fixture::db(
        r#"
#![doc(hidden)]
#[inline] pub fn a() -> u32 { 1 }
pub fn inner() { #![rustc_nounwind] let _ = 2; }
pub fn inner_after_comments() {
    // a comment
    /* another */ #![rustc_nounwind]
}
pub fn outer_on_a_statement() { #[cfg(any())] let _ = 3; }
pub fn with_item() { #[rustc_nounwind] fn nested() {} nested(); }
#[repr(C)] pub struct S { #[doc(hidden)] pub a: u8, #[cfg(any())] pub b: u8, pub c: u8 }
pub enum E { #[default] A, #[deprecated] B(#[doc(hidden)] u8) }
pub const C: u8 = { 1 };
#[used] pub static ST: u8 = 2;
pub mod m { #![doc(hidden)] #[must_use] pub fn b() -> u8 { 0 } }
#[automatically_derived] impl S { #[inline] pub fn method(&self) { let _ = 3; } }
"#,
    )
    .into_db();
    let owners = root_owners(&db);
    for &owner in &owners {
        assert_eq!(AttrFlags::query(&db, owner), live_flags(&db, owner), "{owner:?}");
        let variant = match owner {
            AttrDefId::AdtId(AdtId::StructId(it)) => VariantId::StructId(it),
            AttrDefId::EnumVariantId(it) => VariantId::EnumVariantId(it),
            _ => continue,
        };
        let mut live = Vec::new();
        for_each_field(&db, variant, Tree::Live, |_, cfg_options, field| {
            let mut flags = AttrFlags::empty();
            expand_cfg_attr_view(field.value.attrs(), || cfg_options, |attr, _| {
                match_attr_flags(&mut flags, attr)
            });
            live.push(flags);
        });
        for (at, flags) in live.into_iter().enumerate() {
            let local_id = Idx::from_raw(RawIdx::from_u32(at as u32));
            assert_eq!(AttrFlags::query_field(&db, FieldId { parent: variant, local_id }), flags);
        }
    }
    let by_name = |name: &str| {
        let (_, func) = functions(&db).into_iter().find(|(it, _)| it == name).unwrap();
        AttrFlags::query(&db, func.into())
    };
    assert!(by_name("inner").contains(AttrFlags::RUSTC_NOUNWIND));
    assert!(by_name("inner_after_comments").contains(AttrFlags::RUSTC_NOUNWIND));
    assert!(!by_name("with_item").contains(AttrFlags::RUSTC_NOUNWIND));
    assert!(by_name("a").contains(AttrFlags::IS_INLINE));
}

/// An edit of a fn's body leaves the file's outline the same, so no attribute query of the
/// file runs again, and each reads what it did.
#[test]
fn a_body_edit_runs_no_attribute_query() {
    const CODE: &str = r#"
#![doc(hidden)]
#[inline] pub fn a() -> u32 { 1 }
#[repr(C)] pub struct S { #[doc(hidden)] pub f: u8 }
"#;
    fn read(db: &TestDB) -> Vec<AttrFlags> {
        let mut flags = Vec::new();
        for owner in root_owners(db) {
            flags.push(AttrFlags::query(db, owner));
            if let AttrDefId::AdtId(AdtId::StructId(it)) = owner {
                let local_id = Idx::from_raw(RawIdx::from_u32(0));
                flags.push(AttrFlags::query_field(db, FieldId { parent: it.into(), local_id }));
            }
        }
        flags
    }
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let before = read(&db);
    let text = db.file_text(file.file_id(&db)).text(&db).as_str().replace("{ 1 }", "{ 1 + 2 }");
    db.set_file_text(file.file_id(&db), &text);
    let mut after = Vec::new();
    let log = db.executed_in_read_order(|| after = read(&db));
    let attrs = |it: &&String| it.contains("AttrFlags::query") || it.contains("field_attr_flags");
    assert_eq!(log.iter().filter(attrs).count(), 0, "{log:#?}");
    assert_eq!(before, after);
}

/// Every attribute owner of the test crate: each module, what it declares, its impls, extern
/// blocks, uses and extern crates, the items of each impl and trait, and each enum's variants.
fn all_owners(db: &TestDB) -> Vec<AttrDefId> {
    let def_map = crate_def_map(db, db.fetch_test_crate());
    let mut owners = Vec::new();
    let assoc = |owners: &mut Vec<AttrDefId>, items: &[(hir_expand::name::Name, AssocItemId)]| {
        owners.extend(items.iter().map(|&(_, it)| AttrDefId::from(it)));
    };
    for (module, data) in def_map.modules(db) {
        owners.push(AttrDefId::ModuleId(module));
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => owners.push(it.into()),
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    owners.push(AdtId::EnumId(it).into());
                    let variants = it.enum_variants(db).variants.values();
                    owners.extend(variants.map(|&(v, _)| AttrDefId::EnumVariantId(v)));
                }
                ModuleDefId::AdtId(it) => owners.push(it.into()),
                ModuleDefId::ConstId(it) => owners.push(it.into()),
                ModuleDefId::StaticId(it) => owners.push(it.into()),
                ModuleDefId::TraitId(it) => {
                    owners.push(it.into());
                    assoc(&mut owners, &TraitItems::query(db, it).items);
                }
                ModuleDefId::TypeAliasId(it) => owners.push(it.into()),
                ModuleDefId::MacroId(it) => owners.push(it.into()),
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for it in scope.impls() {
            owners.push(it.into());
            assoc(&mut owners, &it.impl_items(db).items);
        }
        owners.extend(scope.extern_blocks().map(AttrDefId::ExternBlockId));
        owners.extend(scope.use_decls().map(AttrDefId::from));
        owners.extend(scope.extern_crate_decls().map(AttrDefId::from));
    }
    owners
}

const EVERY_OWNER: &str = r#"
//- /lib.rs crate:test
#![doc(hidden)]
extern crate self as me;
#[doc(hidden)] use core::marker;
#[inline] pub fn a() -> u32 { 1 }
pub fn inner() { #![rustc_nounwind] let _ = 2; }
pub fn with_item() { #[rustc_nounwind] fn nested() {} nested(); }
#[repr(C)] pub struct S { #[doc(hidden)] pub a: u8 }
#[repr(C)] pub union U { a: u8 }
#[non_exhaustive] pub enum E { #[default] A, #[deprecated] B(#[doc(hidden)] u8) }
#[cfg_attr(all(), must_use)] pub const C: u8 = { 1 };
#[cfg_attr(any(), must_use)] pub const D: () = { #[repr(C)] struct InBody; };
#[used] pub static ST: u8 = 2;
#[doc(hidden)] pub type T = u8;
#[macro_export] macro_rules! m { () => {} }
#[doc(hidden)] pub macro m2() {}
#[marker] pub trait Tr { #[inline] fn f() {} #[doc(hidden)] type A; #[doc(hidden)] const K: u8; }
#[automatically_derived] impl S { #[inline] pub fn method(&self) { let _ = 3; } }
#[link(name = "c")] extern "C" { #[link_name = "x"] fn x(); #[used] static Y: u8; }
macro_rules! written { () => { #[must_use] pub fn written() {} } }
written!();
pub mod inline { #![doc(hidden)] #[must_use] pub fn b() -> u8 { 0 } }
mod file;
//- /file.rs
#[must_use] pub fn in_file() {}
"#;

/// The flags each owner's file's column gives are those walked off its syntax and off its
/// file's parse, the cursor `crate_lang_items` reads gives the same, and every owner outside a
/// body, a macro's expansion and the modules is in its column.
#[test]
fn column_flags_are_the_walked_flags() {
    let db = fixture::db(EVERY_OWNER).into_db();
    let owners = all_owners(&db);
    let mut cursor = CrateAttrFlags::new(&db);
    let mut in_columns = 0;
    for &owner in &owners {
        let walked = walked_flags(&db, owner);
        assert_eq!(AttrFlags::query(&db, owner), walked, "{owner:?}");
        assert_eq!(live_flags(&db, owner), walked, "{owner:?}");
        assert_eq!(cursor.of(owner), walked, "{owner:?}");
        let mut column = |file, krate| file_attr_flags(&db, file, krate);
        let in_column = column_flags(&db, owner, &mut column);
        let written = match owner {
            AttrDefId::FunctionId(it) => it.lookup(&db).id.file_id.is_macro(),
            _ => false,
        };
        match owner {
            AttrDefId::ModuleId(_) => assert_eq!(in_column, None),
            _ if written => assert_eq!(in_column, None, "{owner:?}"),
            _ => {
                assert_eq!(in_column, Some(walked), "{owner:?}");
                in_columns += 1;
            }
        }
    }
    // Every kind of owner a column holds, each at least once.
    assert!(in_columns >= 25, "{in_columns} owners in columns of {owners:#?}");
}

/// A warm column read, through the query or the cursor, allocates nothing.
#[test]
fn warm_column_reads_allocate_nothing() {
    let db = fixture::db(EVERY_OWNER).into_db();
    let owners = all_owners(&db);
    let read = || {
        let mut cursor = CrateAttrFlags::new(&db);
        let mut flags = AttrFlags::empty();
        for &owner in &owners {
            flags |= AttrFlags::query(&db, owner) | cursor.of(owner);
        }
        flags
    };
    let warm = read();
    let again = blaze_alloc_count::assert_at_most(0, "warm attribute-flag columns", read);
    assert_eq!(warm, again);
}

/// The crate root's attributes, each with its file: rustc appends the crate attributes given
/// on the command line to the root's own (`krate.attrs.push`), each read off its own file.
fn root_attrs_with_files(db: &TestDB, module: crate::ModuleId) -> Vec<(String, HirFileId)> {
    let (source, outer_mod_decl, crate_attrs, krate) =
        attrs_source(db, AttrDefId::ModuleId(module), Tree::Live);
    let mut seen = Vec::new();
    let _ = collect_attrs_in(db, source, outer_mod_decl, crate_attrs, krate, |meta, file| {
        seen.push((meta.syntax().text().to_owned(), file));
        ControlFlow::<Infallible>::Continue(())
    });
    seen
}

#[test]
fn crate_attrs_follow_the_roots_own() {
    let fx = fixture::db(
        "//- /lib.rs crate:foo crate-attr:feature(a) crate-attr:no_std\n#![doc(hidden)]\n",
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    let krate = db.fetch_test_crate();
    let files = base_db::crate_attr_files(&db, krate);
    assert_eq!(files.len(), 2);
    assert_eq!(base_db::applied_crate_attr_files(&db, krate), files);
    let module = db.module_for_file(file_id.file_id(&db));
    let root = HirFileId::from(file_id);
    assert_eq!(
        root_attrs_with_files(&db, module),
        [
            ("doc(hidden)".to_owned(), root),
            ("feature(a)".to_owned(), HirFileId::from(files[0])),
            ("no_std".to_owned(), HirFileId::from(files[1])),
        ]
    );
    for &file in files {
        assert_eq!(file.edition(&db), krate.data(&db).edition);
        assert_eq!(base_db::crate_attr_owner(&db, file.file_id(&db)), Some(krate));
    }
}

/// A crate attribute rustc refuses is not applied and has its error; the others still are.
#[test]
fn a_refused_crate_attr_is_not_applied() {
    let fx = fixture::db(
        "//- /lib.rs crate:foo crate-attr:no_std crate-attr:a,b crate-attr:feature(x)\n",
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    let krate = db.fetch_test_crate();
    let files = base_db::crate_attr_files(&db, krate);
    assert_eq!(files.len(), 3);
    assert_eq!(base_db::crate_attr_error(&db, files[0]), None);
    assert_eq!(base_db::crate_attr_error(&db, files[1]), Some(base_db::CrateAttrError::Syntax));
    assert_eq!(base_db::crate_attr_error(&db, files[2]), None);
    assert_eq!(base_db::applied_crate_attr_files(&db, krate), [files[0], files[2]]);
    let module = db.module_for_file(file_id.file_id(&db));
    let attrs: Vec<_> = root_attrs_with_files(&db, module).into_iter().map(|(it, _)| it).collect();
    assert_eq!(attrs, ["no_std", "feature(x)"]);
    let flags = AttrFlags::query(&db, AttrDefId::ModuleId(module));
    assert!(flags.contains(AttrFlags::IS_NO_STD));
}

/// `AttrFlags::is_cfg_enabled_for` as it was: the first `cfg` of the expanded attributes that
/// is off.
fn old_is_cfg_enabled_for(
    owner: view::AnyHasAttrs<'_>,
    cfg_options: &cfg::CfgOptions,
) -> Result<(), cfg::CfgExpr> {
    let result = expand_cfg_attr_view(
        view::attrs_including_inner(&owner),
        || cfg_options,
        |attr, _| {
            if let view::Meta::CfgMeta(attr) = attr
                && let Some(cfg_predicate) = attr.cfg_predicate()
                && let cfg = cfg::CfgExpr::parse_from_view(cfg_predicate)
                && cfg_options.check(&cfg) == Some(false)
            {
                ControlFlow::Break(cfg)
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    match result {
        Some(cfg) => Err(cfg),
        None => Ok(()),
    }
}

/// `is_cfg_enabled_for` gives every attribute owner the answer, and stops at the `cfg`, the
/// walk over the expanded attributes gives: through `cfg_attr` nests, inner attributes and
/// fields.
#[test]
fn is_cfg_enabled_for_as_the_expanded_walk() {
    let text = r#"
#![cfg(x)]
#[cfg(x)] fn a() {}
#[cfg(not(x))] fn b() {}
#[cfg_attr(all(), cfg(y))] fn c() {}
#[cfg_attr(any(), cfg(y))] fn d() {}
#[cfg_attr(x, cfg_attr(all(), cfg(any())))] fn e() {}
#[cfg(all(x, y))] #[cfg(not(z))] fn f() {}
#[cfg(x)] #[cfg(y)] fn g() {}
fn h() { #![cfg(any())] let _ = 1; }
fn i() { // a comment
    #![cfg_attr(x, cfg(y))]
}
mod m { #![cfg(y)] }
impl S { #![cfg(not(y))] #[cfg(y)] fn j() {} }
extern "C" { #![cfg(x)] }
struct S { #[cfg(x)] a: u8, #[cfg_attr(all(), cfg(any()))] b: u8, c: u8 }
enum E { #[cfg(not(y))] A, #[cfg(z)] B(#[cfg(x)] u8, u8) }
fn k<#[cfg(x)] 'a, #[cfg(y)] T>() { match () { #[cfg(y)] () => {} } }
"#;
    let parse = SourceFile::parse(text, Edition::CURRENT);
    let mut cfg_options = cfg::CfgOptions::default();
    cfg_options.insert_atom(Symbol::intern("x"));
    let (mut owners, mut disabled) = (0, 0);
    for owner in parse.root().descendants().filter_map(view::AnyHasAttrs::cast) {
        let want = old_is_cfg_enabled_for(owner, &cfg_options);
        let got = AttrFlags::is_cfg_enabled_for(owner, &cfg_options);
        assert_eq!(got, want, "{}", owner.syntax().text());
        owners += 1;
        disabled += usize::from(want.is_err());
    }
    assert!(owners > 30 && disabled > 10, "{owners} owners, {disabled} disabled");
}

/// Matching an attribute's flags on views reads lent text and the name tables: no allocation,
/// for every attribute of the corpus but those whose string value has an escape to undo.
#[test]
fn match_attr_flags_allocates_nothing() {
    for attr in corpus().into_iter().filter(|it| !it.contains('\\')) {
        let parse = parse_of(&attr);
        let meta = meta_in(&parse);
        let mut flags = AttrFlags::empty();
        blaze_alloc_count::assert_at_most(0, &attr, || match_attr_flags(&mut flags, meta));
    }
}

mod stability {
    use hir_expand::Lookup;
    use intern::Symbol;
    use hir_expand::fixture;

    use crate::{
        AdtId, AttrDefId, DefWithBodyId, FieldId, ModuleDefId, ModuleId, VariantId,
        attrs::stability::{
            DeprecatedSince, RustcVersion, Stability, StabilityDef, StabilityLevel, StableSince,
            deprecation_entry, own_deprecation, own_stability, stability,
        },
        expr_store::Body,
        nameres::crate_def_map,
        test_db::{TestDB, TestDBExt},
    };

    fn attr_def(def: ModuleDefId) -> AttrDefId {
        match def {
            ModuleDefId::ModuleId(it) => it.into(),
            ModuleDefId::FunctionId(it) => it.into(),
            ModuleDefId::AdtId(it) => it.into(),
            ModuleDefId::EnumVariantId(it) => it.into(),
            ModuleDefId::ConstId(it) => it.into(),
            ModuleDefId::StaticId(it) => it.into(),
            ModuleDefId::TraitId(it) => it.into(),
            ModuleDefId::TypeAliasId(it) => it.into(),
            ModuleDefId::MacroId(it) => it.into(),
            ModuleDefId::BuiltinType(_) => panic!("a builtin type has no attributes"),
        }
    }

    /// What `name` is in `module`'s scope, a type before a value.
    fn in_scope(db: &TestDB, module: ModuleId, name: &str) -> ModuleDefId {
        let scope = module.def_map(db).module(db, module).scope(db);
        let (_, per_ns) = scope
            .entries()
            .find(|(it, _)| it.as_str() == name)
            .unwrap_or_else(|| panic!("no `{name}` in the scope"));
        per_ns.take_types().or(per_ns.take_values()).unwrap()
    }

    /// The definition `path` names from the crate root, `::`-separated.
    fn def(db: &TestDB, path: &str) -> StabilityDef {
        let def_map = crate_def_map(db, db.fetch_test_crate());
        let mut module = def_map.root;
        let mut segments = path.split("::").peekable();
        loop {
            let found = in_scope(db, module, segments.next().unwrap());
            if segments.peek().is_none() {
                return attr_def(found).into();
            }
            match found {
                ModuleDefId::ModuleId(it) => module = it,
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    let name = segments.next().unwrap();
                    let mut variants = it.enum_variants(db).variants.values();
                    let (variant, _) =
                        variants.find(|(v, _)| v.lookup(db).name.as_str() == name).unwrap();
                    return AttrDefId::EnumVariantId(*variant).into();
                }
                _ => panic!("`{path}` goes through something that is not a module or an enum"),
            }
        }
    }

    fn first_field(def: StabilityDef) -> StabilityDef {
        let StabilityDef::Def(AttrDefId::AdtId(AdtId::StructId(it))) = def else {
            panic!("not a struct")
        };
        let local_id = la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(0));
        FieldId { parent: VariantId::StructId(it), local_id }.into()
    }

    /// The item `name` declared in the block of `function`'s body.
    fn in_body(db: &TestDB, function: StabilityDef, name: &str) -> StabilityDef {
        let StabilityDef::Def(AttrDefId::FunctionId(function)) = function else {
            panic!("not a function")
        };
        let body = Body::of(db, DefWithBodyId::FunctionId(function));
        let (_, def_map) = body.blocks(db).next().expect("a block with items");
        attr_def(in_scope(db, def_map.root, name)).into()
    }

    fn origin(db: &TestDB, def: StabilityDef) -> Option<StabilityDef> {
        deprecation_entry(db, def).map(|it| it.origin)
    }

    #[test]
    fn deprecation_is_inherited_from_the_definition_an_item_is_in() {
        let db = fixture::db(
            r#"
#[deprecated(note = "old")]
pub mod m {
    pub struct S { pub f: u8 }
    pub fn g() { pub struct InG; }
    #[deprecated(since = "TBD")]
    pub fn h() {}
    pub enum E { V }
}
#[deprecated = "fn"]
pub fn k() { pub struct InK; }
pub struct T { #[deprecated(since = "2.0", note = "field")] pub f: u8 }
pub struct U;
"#,
        )
        .into_db();
        let m = def(&db, "m");
        let s = def(&db, "m::S");
        let entry = deprecation_entry(&db, s).unwrap();
        assert_eq!(entry.origin, m);
        assert_eq!(entry.deprecation.note, Some(Symbol::intern("old")));
        assert_eq!(entry.deprecation.since, DeprecatedSince::Unspecified);
        assert_eq!(origin(&db, first_field(s)), Some(m));
        assert_eq!(origin(&db, def(&db, "m::E::V")), Some(m));
        assert_eq!(origin(&db, in_body(&db, def(&db, "m::g"), "InG")), Some(m));

        let h = def(&db, "m::h");
        let entry = deprecation_entry(&db, h).unwrap();
        assert_eq!((entry.origin, &entry.deprecation.since), (h, &DeprecatedSince::Future));
        assert!(!entry.deprecation.is_in_effect(RustcVersion { major: 1, minor: 101, patch: 0 }));

        let k = def(&db, "k");
        assert_eq!(origin(&db, in_body(&db, k, "InK")), Some(k));
        assert_eq!(deprecation_entry(&db, k).unwrap().deprecation.note, Some(Symbol::intern("fn")));

        let t = def(&db, "T");
        assert_eq!(origin(&db, t), None);
        let field = deprecation_entry(&db, first_field(t)).unwrap();
        assert_eq!(field.origin, first_field(t));
        assert_eq!(field.deprecation.since, DeprecatedSince::NonStandard(Symbol::intern("2.0")));
        assert_eq!(origin(&db, def(&db, "U")), None);
        assert_eq!(stability(&db, t), None, "a crate without `staged_api` has none");
    }

    #[test]
    fn a_core_without_staged_api_is_not_forced_unstable() {
        let db = fixture::db("//- /lib.rs crate:core\npub struct S;\n").into_db();
        assert_eq!(stability(&db, def(&db, "S")), None, "a fixture's `core` is not `rustc_private`");
    }

    /// The item `name` declared in any block of `function`'s body, nested or from a macro.
    fn in_some_block(db: &TestDB, function: StabilityDef, name: &str) -> StabilityDef {
        let StabilityDef::Def(AttrDefId::FunctionId(function)) = function else {
            panic!("not a function")
        };
        let body = Body::of(db, DefWithBodyId::FunctionId(function));
        let found = body.blocks(db).find_map(|(_, def_map)| {
            let scope = def_map.module(db, def_map.root).scope(db);
            let (_, per_ns) = scope.entries().find(|(it, _)| it.as_str() == name)?;
            per_ns.take_types()
        });
        attr_def(found.unwrap_or_else(|| panic!("no `{name}` in a block"))).into()
    }

    #[test]
    fn a_block_in_a_block_or_in_a_macro_call_of_a_macro_call_is_in_its_function() {
        let db = fixture::db(
            r#"
macro_rules! inner { () => { { pub struct InMacro; } } }
macro_rules! outer { () => { inner!() } }
#[deprecated = "f"]
pub fn f() {
    pub struct Outer;
    { pub struct Inner; { pub struct Innermost; } }
}
#[deprecated = "g"]
pub fn g() { outer!() }
"#,
        )
        .into_db();
        let f = def(&db, "f");
        for name in ["Outer", "Inner", "Innermost"] {
            assert_eq!(origin(&db, in_some_block(&db, f, name)), Some(f), "{name}");
        }
        let g = def(&db, "g");
        assert_eq!(origin(&db, in_some_block(&db, g, "InMacro")), Some(g));
    }

    fn feature(db: &TestDB, def: StabilityDef) -> Option<&str> {
        stability(db, def).map(|it| it.feature.as_str())
    }

    #[test]
    fn stability_is_inherited_when_unstable_or_by_a_field_or_variant() {
        let db = fixture::db(
            r#"
#![feature(staged_api)]
#![stable(feature = "root", since = "1.0.0")]
#[unstable(feature = "m", issue = "none", soft)]
pub mod m {
    pub struct S;
}
#[stable(feature = "t", since = "1.2")]
pub struct T { pub f: u8 }
pub struct U;
#[stable(feature = "e", since = "CURRENT_RUSTC_VERSION")]
pub enum E { V }
#[unstable(feature = "zero", issue = "0")]
pub struct Zero;
#[deprecated(since = "1.5.0", note = "n")]
#[stable(feature = "d", since = "1.0.0")]
#[rustc_allowed_through_unstable_modules = "through"]
pub fn d() {}
#[deprecated(since = "1.5.0")]
pub fn without_note() {}
"#,
        )
        .into_db();
        let m = stability(&db, def(&db, "m")).unwrap();
        let StabilityLevel::Unstable { issue, is_soft, .. } = &m.level else { panic!("{m:?}") };
        assert_eq!((m.feature.as_str(), *issue, *is_soft), ("m", None, true));
        assert_eq!(feature(&db, def(&db, "m::S")), Some("m"));

        let t = def(&db, "T");
        let Some(Stability { level: StabilityLevel::Stable { since, .. }, .. }) = stability(&db, t)
        else {
            panic!("`T` is stable")
        };
        assert_eq!(*since, StableSince::Version(RustcVersion { major: 1, minor: 2, patch: 0 }));
        assert_eq!(feature(&db, first_field(t)), Some("t"));
        assert_eq!(feature(&db, def(&db, "U")), None, "a stable parent is not inherited");
        assert_eq!(feature(&db, def(&db, "E::V")), Some("e"));
        assert_eq!(feature(&db, def(&db, "Zero")), None, "issue 0 is malformed");

        let d = def(&db, "d");
        let Some(StabilityLevel::Stable { allowed_through_unstable_modules, .. }) =
            stability(&db, d).map(|it| &it.level)
        else {
            panic!("`d` is stable")
        };
        assert_eq!(*allowed_through_unstable_modules, Some(Symbol::intern("through")));
        let since = &deprecation_entry(&db, d).unwrap().deprecation.since;
        let version = RustcVersion { major: 1, minor: 5, patch: 0 };
        assert_eq!(*since, DeprecatedSince::RustcVersion(version));
        assert_eq!(origin(&db, def(&db, "without_note")), None, "a staged crate's needs a note");
    }

    /// The `macro_rules!` `name` declared in `module`.
    fn macro_in(db: &TestDB, module: StabilityDef, name: &str) -> StabilityDef {
        let StabilityDef::Def(AttrDefId::ModuleId(module)) = module else { panic!("not a module") };
        let scope = module.def_map(db).module(db, module).scope(db);
        let (_, macros) = scope
            .legacy_macros()
            .find(|(it, _)| it.as_str() == name)
            .unwrap_or_else(|| panic!("no `{name}!` in the module"));
        AttrDefId::MacroId(*macros.last().unwrap()).into()
    }

    #[test]
    fn a_macros_own_stability_and_deprecation_inherit_nothing() {
        let db = fixture::db(
            r#"
#![feature(staged_api)]
#![stable(feature = "root", since = "1.0.0")]
#[unstable(feature = "m", issue = "none")]
pub mod m {
    macro_rules! plain { () => {} }
    #[deprecated = "x"]
    macro_rules! old { () => {} }
}
"#,
        )
        .into_db();
        let m = def(&db, "m");
        let plain = macro_in(&db, m, "plain");
        assert_eq!(feature(&db, plain), Some("m"), "`stability` inherits the module's");
        assert_eq!(own_stability(&db, plain), None);
        assert_eq!(own_deprecation(&db, plain), None);

        let old = macro_in(&db, m, "old");
        assert_eq!(own_deprecation(&db, old).unwrap().note, Some(Symbol::intern("x")));
        assert_eq!(own_stability(&db, old), None);
    }
}

mod sanitize {
    use hir_expand::Lookup;
    use syntax::view::HasName;
    use hir_expand::fixture;

    use crate::{
        AssocItemId, DefWithBodyId, FunctionId, ModuleDefId,
        attrs::{
            disabled_sanitizers,
            sanitizer::{ADDRESS, MEMORY, THREAD},
        },
        expr_store::Body,
        nameres::{DefMap, crate_def_map},
        src::HasSource,
        test_db::{TestDB, TestDBExt},
    };

    const FIXTURE: &str = r#"
#![sanitize(memory = "off")]
pub fn crate_level() {}

#[sanitize(address = "off")]
pub mod m {
    pub fn inherits() {}
    #[sanitize(address = "on", thread = "off")]
    pub fn turned_back_on() {}
    #[sanitize(address = "off", address = "on")]
    pub fn both_ways() {}
    #[cfg_attr(all(), sanitize(thread = "off"))]
    pub fn via_cfg_attr() {}
    #[cfg_attr(any(), sanitize(thread = "off"))]
    pub fn cfg_attr_off() {}
    #[sanitize(cfi = "off", r#memory = "on")]
    pub fn other_keys() {}

    pub struct S;
    #[sanitize(thread = "off")]
    impl S {
        pub fn method() {}
        #[sanitize(kernel_address = "on", memory = "on")]
        pub fn method_on() {}
    }

    #[sanitize(address = "on")]
    pub fn outer() {
        fn in_body() {}
        let _ = #[sanitize(thread = "off")] || {
            fn in_closure() {}
        };
    }

    pub fn inner_attr() {
        #![sanitize(memory = "on")]
        fn in_inner_attr() {}
    }

    macro_rules! make {
        () => {
            pub fn made() {
                fn made_inner() {}
            }
        };
    }
    make!();
}
"#;

    fn name(db: &TestDB, func: FunctionId) -> String {
        func.lookup(db)
            .source_view(db)
            .value
            .name()
            .map_or_else(String::new, |it| it.text().to_owned())
    }

    /// Every function of `def_map`'s modules and their impls, and of the blocks of their bodies,
    /// by name.
    fn every_function(db: &TestDB, def_map: &DefMap, out: &mut Vec<(String, FunctionId)>) {
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            let declared = scope.declarations().filter_map(|it| match it {
                ModuleDefId::FunctionId(it) => Some(it),
                _ => None,
            });
            let methods = scope.impls().flat_map(|it| it.impl_items(db).items.iter()).filter_map(
                |&(_, item)| match item {
                    AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                },
            );
            for func in declared.chain(methods) {
                out.push((name(db, func), func));
                let body = Body::of(db, DefWithBodyId::FunctionId(func));
                for (_, block) in body.blocks(db) {
                    every_function(db, block, out);
                }
            }
        }
    }

    #[test]
    fn disabled_sanitizers_take_the_nearest_sanitize_of_each_parent() {
        let db = fixture::db(FIXTURE).into_db();
        let mut functions = Vec::new();
        every_function(&db, crate_def_map(&db, fixture::test_crate(&db)), &mut functions);
        let off = |name: &str| {
            let (_, func) = functions
                .iter()
                .find(|(it, _)| it == name)
                .unwrap_or_else(|| panic!("no fn {name}"));
            disabled_sanitizers(&db, *func)
        };
        for (name, expected) in [
            ("crate_level", MEMORY),
            ("inherits", ADDRESS | MEMORY),
            ("turned_back_on", THREAD | MEMORY),
            ("both_ways", ADDRESS | MEMORY),
            ("via_cfg_attr", ADDRESS | THREAD | MEMORY),
            ("cfg_attr_off", ADDRESS | MEMORY),
            ("other_keys", ADDRESS),
            ("method", ADDRESS | THREAD | MEMORY),
            ("method_on", THREAD),
            ("outer", MEMORY),
            ("in_body", MEMORY),
            ("in_closure", THREAD | MEMORY),
            ("inner_attr", ADDRESS),
            ("in_inner_attr", ADDRESS),
            ("made", ADDRESS | MEMORY),
            ("made_inner", ADDRESS | MEMORY),
        ] {
            assert_eq!(off(name), expected, "{name}");
        }
    }
}
