//! The tables resolved in a stand-in toolchain: what each modeled function is, what is not
//! modeled, that a crate is the toolchain's by its origin, what `check_resolved` names, and that
//! a lookup allocates nothing.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_alloc_count::assert_at_most;
use hir_def::{
    AdtId, FunctionId, ModuleDefId,
    expr_store::Body,
    nameres::crate_def_map,
    signatures::{EnumSignature, FunctionSignature, StructSignature, TraitSignature, UnionSignature},
    test_items,
};
use hir_expand::name::Name;
use hir_ty::{
    db::HirDatabase,
    next_solver::{Ty, TyKind},
};
use rustc_type_ir::inherent::IntoKind;
use hir_expand::fixture;

use super::fixture::{TOOLCHAIN, crate_named, db_with, lang_crate};
use crate::{
    Abs, Effects,
    models::{Applied, Leaf, Model, Transfer, apply_leaf, check_resolved, crate_models, model_of},
};

const SCRIPT: &str = "
//- /build.rs crate:build_script deps:std
pub mod env {
    pub fn var(_: &str) {}
}
fn main() {}
";

/// The function at `path` in `krate`, through re-exports.
#[track_caller]
fn free(db: &Db, krate: Crate, path: &[&str]) -> FunctionId {
    let (last, modules) = path.split_last().expect("a path");
    let mut module = crate_def_map(db, krate).root_module_id();
    let scope = |module: hir_def::ModuleId| module.to_db(db).def_map(db).module(db, module);
    for segment in modules {
        match scope(module).name_in_scope(db, &Name::new_root(segment)).take_types() {
            Some(ModuleDefId::ModuleId(it)) => module = it,
            other => panic!("`{segment}` of {path:?} is {other:?}"),
        }
    }
    match scope(module).name_in_scope(db, &Name::new_root(last)).take_values() {
        Some(ModuleDefId::FunctionId(it)) => it,
        other => panic!("{path:?} is {other:?}"),
    }
}

/// The name of `ty` (`str` for `str`, any references taken off), `None` for one with none.
fn type_name(db: &Db, mut ty: Ty<'_>) -> Option<String> {
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
    }
    let name = match ty.kind() {
        TyKind::Str => return Some("str".to_owned()),
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::StructId(id) => StructSignature::of(db, id).name.clone(),
            AdtId::EnumId(id) => EnumSignature::of(db, id).name.clone(),
            AdtId::UnionId(id) => UnionSignature::of(db, id).name.clone(),
        },
        _ => return None,
    };
    Some(name.as_str().to_owned())
}

/// The method `name` of the impl in `krate` for the type named `self_ty` (as [`type_name`]),
/// of the trait named `trait_` or inherent; `Tr<T>` names the impl of `Tr` whose first argument
/// is the type named `T`.
#[track_caller]
fn method(db: &Db, krate: Crate, self_ty: &str, trait_: Option<&str>, name: &str) -> FunctionId {
    let mut found = Vec::new();
    for imp in test_items::impls(db, krate) {
        if type_name(db, db.impl_self_ty(imp).skip_binder()).as_deref() != Some(self_ty) {
            continue;
        }
        let trait_ref = db.impl_trait(imp).map(|it| it.skip_binder());
        let matches = match (trait_ref, trait_) {
            (None, None) => true,
            (Some(trait_ref), Some(wanted)) => {
                let (wanted, arg) = match wanted.split_once('<') {
                    Some((it, arg)) => (it, arg.strip_suffix('>')),
                    None => (wanted, None),
                };
                let of = TraitSignature::of(db, trait_ref.def_id.0).name.as_str() == wanted;
                let first = || trait_ref.args.iter().nth(1).and_then(|it| it.ty());
                of && arg.is_none_or(|arg| first().and_then(|it| type_name(db, it)).as_deref() == Some(arg))
            }
            _ => false,
        };
        if !matches {
            continue;
        }
        for (item_name, item) in imp.impl_items(db).items.iter() {
            if let hir_def::AssocItemId::FunctionId(func) = *item
                && item_name.as_str() == name
            {
                found.push(func);
            }
        }
    }
    match found[..] {
        [one] => one,
        _ => panic!("{} methods `{name}` of {trait_:?} for `{self_ty}`", found.len()),
    }
}

const SAME: Option<Model> = Some(Model::Transfer(Transfer::Same { arg: 0 }));

fn leaf(leaf: Leaf) -> Option<Model> {
    Some(Model::Leaf(leaf))
}

#[test]
fn std_functions_have_their_models() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let std = lang_crate(&db, LangCrateOrigin::Std);
        let free = |path: &[&str]| model_of(&db, free(&db, std, path));
        let method = |ty, trait_, name| model_of(&db, method(&db, std, ty, trait_, name));
        assert_eq!(free(&["env", "var"]), leaf(Leaf::EnvVar));
        assert_eq!(free(&["env", "var_os"]), leaf(Leaf::EnvVar));
        assert_eq!(free(&["io", "_print"]), leaf(Leaf::Print));
        assert_eq!(free(&["io", "_eprint"]), leaf(Leaf::Print));
        assert_eq!(free(&["io", "stdout"]), leaf(Leaf::Print));
        assert_eq!(method("Stdout", Some("Write"), "write"), leaf(Leaf::Print));
        assert_eq!(method("Stderr", Some("Write"), "write"), leaf(Leaf::Print));
        assert_eq!(free(&["fs", "read"]), leaf(Leaf::ReadFile));
        assert_eq!(free(&["fs", "read_to_string"]), leaf(Leaf::ReadFile));
        assert_eq!(method("File", None, "open"), leaf(Leaf::ReadFile));
        assert_eq!(free(&["fs", "write"]), leaf(Leaf::WriteFile));
        assert_eq!(free(&["fs", "create_dir_all"]), leaf(Leaf::WriteFile));
        assert_eq!(method("File", None, "create"), leaf(Leaf::WriteFile));
        assert_eq!(method("Path", None, "exists"), leaf(Leaf::Probe));
        assert_eq!(method("File", Some("Read"), "read"), leaf(Leaf::Handle));
        assert_eq!(method("File", Some("Write"), "write"), leaf(Leaf::Handle));
        assert_eq!(method("OwnedFd", Some("Drop"), "drop"), leaf(Leaf::Handle));
        assert_eq!(method("OwnedFd", Some("FromRawFd"), "from_raw_fd"), leaf(Leaf::RawHandle));
        assert_eq!(method("File", Some("FromRawFd"), "from_raw_fd"), leaf(Leaf::RawHandle));
        assert_eq!(free(&["process", "exit"]), leaf(Leaf::Exit));
        assert_eq!(method("Path", None, "new"), SAME);
        assert_eq!(method("Path", None, "join"), Some(Model::Transfer(Transfer::Join { base: 0, rel: 1 })));
        assert_eq!(method("Path", None, "to_path_buf"), SAME);
        assert_eq!(method("PathBuf", None, "push"), Some(Model::Transfer(Transfer::Push { target: 0, rel: 1 })));
        assert_eq!(method("PathBuf", None, "as_path"), SAME);
        assert_eq!(method("PathBuf", Some("Deref"), "deref"), SAME);
        assert_eq!(method("PathBuf", Some("From"), "from"), SAME);
        assert_eq!(method("PathBuf", Some("AsRef"), "as_ref"), SAME);
        assert_eq!(method("String", Some("AsRef<Path>"), "as_ref"), SAME);
        assert_eq!(method("OsStr", None, "to_str"), SAME);
        // Walked as any code: what they do is what their bodies reach.
        assert_eq!(method("Command", None, "output"), None);
        assert_eq!(method("Command", None, "new"), None);
        assert_eq!(method("OsString", None, "new"), None);
        assert_eq!(free(&["fs", "open"]), None);
        assert_eq!(free(&["sys", "posix_spawnp"]), None);
        assert_eq!(free(&["sys", "getenv"]), None);
    });
}

#[test]
fn alloc_and_core_functions_have_their_models() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let alloc = lang_crate(&db, LangCrateOrigin::Alloc);
        let core = lang_crate(&db, LangCrateOrigin::Core);
        let free = |krate, path: &[&str]| model_of(&db, free(&db, krate, path));
        let method = |krate, ty, trait_, name| model_of(&db, method(&db, krate, ty, trait_, name));
        assert_eq!(free(alloc, &["alloc", "__rust_alloc"]), leaf(Leaf::Alloc));
        assert_eq!(free(alloc, &["alloc", "__rust_dealloc"]), leaf(Leaf::Alloc));
        assert_eq!(free(alloc, &["alloc", "__rust_no_alloc_shim_is_unstable_v2"]), leaf(Leaf::Alloc));
        assert_eq!(free(alloc, &["alloc", "__rust_alloc_error_handler"]), leaf(Leaf::Panic));
        assert_eq!(free(alloc, &["shim", "grow_by_attribute"]), leaf(Leaf::Alloc));
        assert_eq!(free(alloc, &["shim", "free_by_link_name"]), leaf(Leaf::Alloc));
        assert_eq!(free(alloc, &["shim", "unwind_by_link_name"]), leaf(Leaf::Panic));
        assert_eq!(free(alloc, &["shim", "not_a_shim"]), None);
        assert_eq!(free(alloc, &["alloc", "alloc"]), None);
        assert_eq!(free(alloc, &["alloc", "handle_alloc_error"]), None);
        assert_eq!(method(alloc, "String", None, "as_str"), SAME);
        assert_eq!(method(alloc, "String", Some("From"), "from"), SAME);
        assert_eq!(method(alloc, "String", Some("Clone"), "clone"), SAME);
        assert_eq!(method(alloc, "String", Some("Deref"), "deref"), SAME);
        assert_eq!(method(alloc, "str", Some("ToOwned"), "to_owned"), SAME);
        assert_eq!(method(alloc, "String", Some("Drop"), "drop"), None);
        assert_eq!(method(alloc, "Vec", Some("Drop"), "drop"), None);
        assert_eq!(free(core, &["panicking", "panic_fmt"]), leaf(Leaf::Panic));
        assert_eq!(free(core, &["panicking", "panic"]), leaf(Leaf::Panic));
        assert_eq!(free(core, &["panicking", "panic_display"]), leaf(Leaf::Panic));
        assert_eq!(method(core, "Result", None, "unwrap"), SAME);
        assert_eq!(method(core, "Option", None, "unwrap"), SAME);
    });
}

/// The foreign function `name` declared in a block of the body of `owner`.
#[track_caller]
fn foreign_in(db: &Db, owner: FunctionId, name: &str) -> FunctionId {
    for (_, block) in Body::of(db, owner.into()).blocks(db) {
        for (_, module) in block.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no `{name}` in the body of {owner:?}")
}

#[test]
fn strerror_r_is_fixed_error_text() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let std = lang_crate(&db, LangCrateOrigin::Std);
        let format_error = free(&db, std, &["sys", "io", "format_error"]);
        assert_eq!(model_of(&db, format_error), None, "its body is walked");
        let strerror_r = foreign_in(&db, format_error, "strerror_r");
        assert_eq!(model_of(&db, strerror_r), leaf(Leaf::ErrorText));
        assert!(
            !check_resolved(&db, std).contains(&"strerror_r in std::sys::io::format_error"),
            "found inside `format_error`'s body, by its Rust name, whatever it links to"
        );
    });
    assert_eq!(apply_leaf(Leaf::ErrorText, &[Abs::Unknown, Abs::Unknown]), Applied::Done {
        effects: Effects::NONE,
        env: None,
        ret: Abs::Unknown,
    });
}

#[test]
fn unwrap_and_expect_failures_are_panics() {
    let db = fixture::db(
        r#"
//- /core.rs crate:core
pub mod result {
    fn unwrap_failed() -> ! {
        loop {}
    }
}
pub mod option {
    const fn unwrap_failed() -> ! {
        loop {}
    }
    const fn expect_failed(msg: &str) -> ! {
        let _ = msg;
        loop {}
    }
}
"#,
    )
    .into_db();
    hir_ty::attach_db(&db, || {
        let core = lang_crate(&db, LangCrateOrigin::Core);
        for path in [&["result", "unwrap_failed"][..], &["option", "unwrap_failed"], &["option", "expect_failed"]] {
            assert_eq!(model_of(&db, free(&db, core, path)), leaf(Leaf::Panic), "{path:?}");
        }
    });
}

#[test]
fn only_the_toolchain_has_models() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let script = crate_named(&db, "build_script");
        assert_eq!(model_of(&db, free(&db, script, &["env", "var"])), None);
        assert_eq!(model_of(&db, free(&db, script, &["main"])), None);
    });
    // A crate named `std` that is not the toolchain's.
    let db = fixture::db(
        "
//- minicore: sized
//- /std.rs crate:r#std
pub mod env {
    pub fn var(_: &str) {}
}
pub mod io {
    pub fn _print() {}
}
",
    )
    .into_db();
    hir_ty::attach_db(&db, || {
        let std = crate_named(&db, "std");
        assert!(matches!(std.data(&db).origin, CrateOrigin::Local { .. }));
        assert_eq!(model_of(&db, free(&db, std, &["env", "var"])), None);
        assert_eq!(model_of(&db, free(&db, std, &["io", "_print"])), None);
        assert!(check_resolved(&db, std).len() >= 3, "a crate of no toolchain origin resolves nothing");
    });
}

#[test]
fn check_resolved_names_what_the_toolchain_lacks() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let std = lang_crate(&db, LangCrateOrigin::Std);
        let missing = check_resolved(&db, std);
        for lacked in [
            "std::process::abort",
            "std::panicking::begin_panic",
            "std::fs::copy",
            "<std::fs::File as Seek>",
            "alloc::alloc::__rust_realloc",
            "core::panicking::panic_bounds_check",
            "core::panicking::panic_const::panic_const_add_overflow",
        ] {
            assert!(missing.contains(&lacked), "{lacked} is not in the stand-in: {missing:?}");
        }
        for had in [
            "std::env::var",
            "std::fs::write",
            "std::io::_print",
            "<std::fs::File as Write>",
            "<std::os::fd::OwnedFd as Drop>",
            "std::path::Path::join",
            "alloc::alloc::__rust_alloc",
            "<String as From>",
            "core::panicking::panic_fmt",
            "core::result::Result::unwrap",
            // Of another platform than the fixture's.
            "<std::os::windows::io::OwnedHandle as Drop>",
        ] {
            assert!(!missing.contains(&had), "{had} resolves or is not of this platform: {missing:?}");
        }
    });
}

#[test]
fn a_platform_entry_must_resolve_on_its_platform() {
    let windows = TOOLCHAIN.replace(
        "//- /std.rs crate:std deps:alloc",
        "//- /std.rs crate:std deps:alloc cfg:target_family=windows",
    );
    let db = fixture::db(&format!("{windows}{SCRIPT}")).into_db();
    hir_ty::attach_db(&db, || {
        let std = lang_crate(&db, LangCrateOrigin::Std);
        let missing = check_resolved(&db, std);
        assert!(missing.contains(&"<std::os::windows::io::OwnedHandle as Drop>"), "{missing:?}");
        assert!(missing.contains(&"<std::fs::File as FromRawHandle>"), "{missing:?}");
        assert!(!missing.contains(&"<std::os::fd::OwnedFd as Drop>"), "{missing:?}");
    });
}

#[test]
fn a_table_is_built_once_per_crate() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let std = lang_crate(&db, LangCrateOrigin::Std);
        let first: *const _ = crate_models(&db, std);
        let second: *const _ = crate_models(&db, std);
        assert_eq!(first, second, "the memo is read, not rebuilt");
    });
}

#[test]
fn a_memoized_lookup_allocates_nothing() {
    let db = db_with(SCRIPT);
    hir_ty::attach_db(&db, || {
        let std = lang_crate(&db, LangCrateOrigin::Std);
        let alloc = lang_crate(&db, LangCrateOrigin::Alloc);
        let script = crate_named(&db, "build_script");
        let funcs = [
            free(&db, std, &["env", "var"]),
            free(&db, std, &["fs", "write"]),
            method(&db, std, "Path", None, "join"),
            method(&db, std, "File", Some("Write"), "write"),
            free(&db, std, &["fs", "open"]),
            free(&db, alloc, &["alloc", "__rust_alloc"]),
            free(&db, script, &["main"]),
        ];
        let warm: Vec<_> = funcs.iter().map(|&func| model_of(&db, func)).collect();
        let again = assert_at_most(0, "a memoized model_of", || {
            let mut same = 0;
            for _ in 0..16 {
                for (at, &func) in funcs.iter().enumerate() {
                    same += usize::from(model_of(&db, func) == warm[at]);
                }
            }
            same
        });
        assert_eq!(again, 16 * funcs.len());
    });
}
