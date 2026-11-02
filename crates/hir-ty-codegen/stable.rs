//! Names for instances that are the same in every run of the daemon on the same sources, so
//! that code compiled by one run can be found by the next. A crate of the checkout is named
//! by what is written in it, so an edit makes other names of everything in it.

#![forbid(unsafe_code)]

use std::{
    cell::RefCell,
    fmt::Write,
    sync::{Arc, OnceLock},
};

use blaze_hash::HashMap;
use blaze_query::db::FromId;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup, ModuleDefId,
    StaticId, TraitId, expr_store::Body, item_scope::ItemScope, nameres::crate_def_map,
};
use hir_expand::{HirFileId, MacroCallKind};
use parking_lot::Mutex;
use rustc_ast_ir::Mutability;
use rustc_type_ir::{
    AliasTy, AliasTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, RegionKind,
    inherent::{IntoKind, Safety as _, Term as _},
};
use span::ErasedFileAstId;

use base_db::{Crate, Db};

use super::{Instance, InstanceDef, SymbolResolver};
use hir_ty::{
    ImplTraitId, InferBodyId,
    consteval::try_const_usize,
    db::{ClosureKey, HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{GenericArgs, Region, TermId, Ty, TyKind},
};

/// Before the content of a crate of the checkout, where a crate of rustc's alone has its id.
const OF_CHECKOUT: char = '~';

/// Before the hash that stands for a type written longer than [`SPELLED`]: a closure that
/// wraps closures has a type that holds each of theirs, and written out in full its name
/// doubles with each level.
const HASHED: char = '$';

enum Declared {
    Function(FunctionId),
    Static(StaticId),
}

/// Lowers what [`StableNames`] lists of `crates`, so that a build that lists them later
/// finds it done. `lower` is given the bodies of each round to lower at once.
pub fn lower_declared(db: &Db, crates: &[Crate], lower: &impl Fn(&[DefWithBodyId])) {
    declared(db, crates, lower, &mut |_| ());
}

/// The functions and statics `crates` declare, in their modules and in the blocks of their
/// bodies: a `static` is often written by a macro.
fn declared(
    db: &Db,
    crates: &[Crate],
    lower: &impl Fn(&[DefWithBodyId]),
    found: &mut impl FnMut(Declared),
) {
    // alloc: once per listing, the bodies of a round, kept for every round
    let mut bodies: Vec<DefWithBodyId> = Vec::new();
    let mut scope = |scope: &ItemScope, bodies: &mut Vec<DefWithBodyId>| {
        for def in scope.declarations() {
            match def {
                ModuleDefId::StaticId(id) => {
                    found(Declared::Static(id));
                    bodies.push(id.into());
                }
                ModuleDefId::FunctionId(id) => {
                    found(Declared::Function(id));
                    bodies.push(id.into());
                }
                ModuleDefId::ConstId(id) => bodies.push(id.into()),
                // A method a trait provides has blocks of its own.
                ModuleDefId::TraitId(id) => {
                    for &(_, item) in id.trait_items(db).items.iter() {
                        match item {
                            AssocItemId::FunctionId(id) => bodies.push(id.into()),
                            AssocItemId::ConstId(id) => bodies.push(id.into()),
                            AssocItemId::TypeAliasId(_) => {}
                        }
                    }
                }
                _ => {}
            }
        }
        bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => {
                        found(Declared::Function(id));
                        bodies.push(id.into());
                    }
                    AssocItemId::ConstId(id) => bodies.push(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    };
    for &krate in crates {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            scope(module.scope(db), &mut bodies);
        }
    }
    // alloc: once per listing, the round taken from `bodies`, kept for every round
    let mut round = Vec::new();
    while !bodies.is_empty() {
        std::mem::swap(&mut round, &mut bodies);
        lower(&round);
        for body in round.drain(..) {
            for (_, def_map) in Body::of(db, body).blocks(db) {
                for (_, module) in def_map.modules(db) {
                    scope(module.scope(db), &mut bodies);
                }
            }
        }
    }
}

/// The statics and the functions without type parameters that one build of a crate declares.
#[derive(Default)]
struct Listed {
    statics: HashMap<String, StaticId>,
    functions: HashMap<String, Vec<FunctionId>>,
}

/// A file by its path as `named` has it, or for what a macro wrote by the call that wrote it.
fn file_name(
    db: &Db,
    // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
    symbols: &dyn SymbolResolver,
    out: &mut String,
    file: HirFileId,
    named: &impl Fn(Option<String>) -> Option<String>,
) -> Option<()> {
    let written_at = |out: &mut String, file: HirFileId, id: ErasedFileAstId| {
        file_name(db, symbols, out, file, named)?;
        write!(out, "#{}", id.into_raw()).ok()
    };
    match file {
        HirFileId::FileId(file) => {
            // alloc: named gives an owned name
            let path = symbols.source_path(file.file_id(db)).map(|it| it.as_str().to_owned());
            out.push_str(&named(path)?);
        }
        HirFileId::MacroFile(call) => match &call.loc(db).kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                out.push('!');
            }
            MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                write!(out, "!derive {derive_attr_index:?} {derive_index}").ok()?;
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                write!(out, "!attr {censored_attr_ids:?}").ok()?;
            }
        },
    }
    Some(())
}

fn hashed(spelled: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(66);
    out.push(HASHED);
    if spelled.contains(OF_CHECKOUT) {
        out.push(OF_CHECKOUT);
    }
    for byte in blaze_ids::b3::hash(spelled.as_bytes()) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 15)] as char);
    }
    out
}

/// Where the toolchain installs the standard library's sources, the directory `library/` is in.
const INSTALLED_LIBRARY: &str = "/lib/rustlib/src/rust/";

/// A file's path as a place names it ([`block_place`], [`static_place`]): the same on every
/// machine for a file of the standard library or of what cargo fetched. The standard library
/// from the store was built on another machine, and a program finds the statics of its bodies
/// by the name their place gives them: under a path of this machine's, std's `thread_local!`s
/// and `thread::min_stack`'s `MIN` were not found, and a program that spawned a thread or made
/// a `HashMap` was refused (#840).
// dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
fn place_path(symbols: &dyn SymbolResolver, path: Option<String>) -> Option<String> {
    let library = symbols.library_sources().map(|it| it.here);
    Some(path_everywhere(path?, symbols.cargo_home().as_deref(), library.as_deref()))
}

/// [`place_path`] with cargo's directory on this machine at `cargo_home`, and the standard
/// library's `library/` at `library`: `/rustc/library/...` names a file of it wherever it is,
/// in a toolchain or as blaze's own.
fn path_everywhere(path: String, cargo_home: Option<&str>, library: Option<&str>) -> String {
    if let Some(rest) = library.and_then(|it| path.strip_prefix(it))
        && let Some(rest) = rest.strip_prefix(['/', '\\'])
    {
        // alloc: the path is what this returns
        return format!("/rustc/library/{}", rest.replace('\\', "/"));
    }
    let slashed = path.replace('\\', "/");
    if let Some(at) = slashed.find(INSTALLED_LIBRARY) {
        // alloc: the path is what this returns
        return format!("/rustc/{}", &slashed[at + INSTALLED_LIBRARY.len()..]);
    }
    cargo_home.and_then(|home| super::moved(&path, home, super::CARGO_HOME)).unwrap_or(path)
}

/// Where a function inside a block is written, which its path does not say: two blocks of
/// one function, or two `const _` of one module, can each hold an item of the same name.
/// `None` for a function outside of any block.
pub fn block_place(
    db: &Db,
    // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
    symbols: &dyn SymbolResolver,
    func: FunctionId,
) -> Option<String> {
    func.module(db).block(db)?;
    let id = func.lookup(db).id;
    // alloc: the place is what this returns
    let mut out = String::new();
    file_name(db, symbols, &mut out, id.file_id, &|path| place_path(symbols, path))?;
    write!(out, "#{}", id.value.erase().into_raw()).ok()?;
    Some(out)
}

/// Where any function is written, block or not: for a function whose path blaze cannot write
/// (an `impl` for a type it has no name for, such as `!`, `dyn Trait` or a type taking a
/// constant), the place names it instead.
pub fn function_place(
    db: &Db,
    // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
    symbols: &dyn SymbolResolver,
    func: FunctionId,
) -> Option<String> {
    let id = func.lookup(db).id;
    // alloc: the place is what this returns
    let mut out = String::new();
    file_name(db, symbols, &mut out, id.file_id, &|path| place_path(symbols, path))?;
    write!(out, "#{}", id.value.erase().into_raw()).ok()?;
    Some(out)
}

/// Where a static inside a block is written, as [`block_place`] has it of a function.
pub fn static_place(
    db: &Db,
    // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
    symbols: &dyn SymbolResolver,
    id: hir_def::StaticId,
) -> Option<String> {
    id.module(db).block(db)?;
    let id = id.lookup(db).id;
    // alloc: the place is what this returns
    let mut out = String::new();
    file_name(db, symbols, &mut out, id.file_id, &|path| place_path(symbols, path))?;
    write!(out, "#{}", id.value.erase().into_raw()).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::path_everywhere;

    #[test]
    fn a_file_of_the_standard_library_or_of_cargo_is_named_the_same_on_every_machine() {
        let std = "lib/rustlib/src/rust/library/std/src/thread/mod.rs";
        let runner = format!("/Users/runner/.rustup/toolchains/nightly-aarch64-apple-darwin/{std}");
        let user = format!("/home/me/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/{std}");
        let windows = format!(r"C:\Users\me\.rustup\toolchains\nightly\{}", std.replace('/', r"\"));
        let named = path_everywhere(runner, None, None);
        assert_eq!(named, "/rustc/library/std/src/thread/mod.rs");
        assert_eq!(path_everywhere(user, Some("/home/me/.cargo"), None), named);
        assert_eq!(path_everywhere(windows, None, None), named);
        // blaze's own library, from a checkout or downloaded, is named as a toolchain's is.
        let own = "/home/me/.blaze/library/b8b8/library/std/src/thread/mod.rs";
        let at = Some("/home/me/.blaze/library/b8b8/library");
        assert_eq!(path_everywhere(own.to_owned(), None, at), named);
        let checkout = "/src/blaze/library/std/src/thread/mod.rs";
        assert_eq!(path_everywhere(checkout.to_owned(), None, Some("/src/blaze/library")), named);
        let beside = "/src/blaze/library-x/src/lib.rs";
        assert_eq!(path_everywhere(beside.to_owned(), None, Some("/src/blaze/library")), beside);
        let fetched = "/home/me/.cargo/registry/src/index.crates.io-1/x-1.0.0/src/lib.rs";
        assert_eq!(
            path_everywhere(fetched.to_owned(), Some("/home/me/.cargo"), None),
            "/cargo/registry/src/index.crates.io-1/x-1.0.0/src/lib.rs"
        );
        assert_eq!(
            path_everywhere("src/main.rs".to_owned(), Some("/home/me/.cargo"), None),
            "src/main.rs"
        );
    }
}
