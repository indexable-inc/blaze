//! What wasm32 code says to the linker beside its body ([`blaze_codegen::wasm::Code`]): what
//! each function outside the build it names is imported as and the name the module exports it
//! under ([`Header`]), and the kinds of its relocations. The rest of the header (its type, the
//! types of its indirect calls, the places of the stack pointer) is `wasm::lower`'s.

use base_db::{CrateOrigin, Db, LangCrateOrigin};
use blaze_codegen::{
    RelocKind,
    wasm::{Code, Import},
};
use cranelift_codegen::binemit::Reloc;
use hir_def::{FunctionId, HasModule, ItemContainerId, Lookup, signatures::FunctionSignature};

use super::{HirInstance, Kind, fold::BODY};
use crate::{
    Instance, InstanceDef, exported_symbol,
    fragment::{FuncTarget, RelocName, Requests},
    link_name_for,
    wasm::{Address, CALL, WasmSignature},
};

/// The module a function of an `extern` block is imported from when the block names none
/// (`#[link(wasm_import_module)]`), as rustc's wasm targets import it.
pub(crate) const ENV: &str = "env";

/// The module of the `proc_macro` bridge's imports, which `blaze-macro-eval`'s host gives a
/// proc macro's wasm module (its `src/wasm/abi.rs` `BRIDGE`).
pub(crate) const BRIDGE: &str = "blaze_proc_macro";

/// What the host gives for std's `hashmap_random_keys`: the evaluator's fixed keys
/// (`blaze-macro-eval` `os/random.rs`), so a macro's `RandomState` reads no WASI randomness and
/// its expansion stays one blaze keeps.
const RANDOM_KEYS: &str = "hashmap_random_keys";

/// The bridge's import that a call of `func` is in a proc macro's wasm module, in place of
/// std's body: std's `sys::random::hashmap_random_keys`, named as the evaluator names it
/// (`blaze-macro-eval` `os.rs` `Call::of_std`). Its `() -> (u64, u64)` lowers on wasm32 to
/// `(i32) -> ()`, the pair returned through the pointer, which is the host's
/// `hashmap_random_keys(into)`; [`imports`] checks it.
pub(crate) fn bridged(db: &Db, func: FunctionId) -> Option<&'static str> {
    let ItemContainerId::ModuleId(module) = func.lookup(db).container else { return None };
    if !matches!(func.krate(db).data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Std)) {
        return None;
    }
    let named = FunctionSignature::of(db, func).name.as_str() == RANDOM_KEYS;
    (named && module.name(db)?.as_str() == "random").then_some(RANDOM_KEYS)
}

/// The part of a function's [`Code`] that the front end says: empty for nearly every function,
/// and on every target but wasm32.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Header {
    /// What each outside function its code and its data name is imported as, by its symbol
    /// (the `Target::Extern` a relocation at it has), in the order its requests name them.
    pub(crate) imports: Vec<(String, Import)>,
    /// The symbol of a `#[no_mangle]` or `#[export_name]` function, which the module exports.
    pub(crate) export: Option<String>,
}

impl Header {
    pub(crate) fn is_empty(&self) -> bool {
        self.imports.is_empty() && self.export.is_none()
    }
}

/// The header of `instance`'s code, whose (placed) requests are `requests`.
pub(crate) fn header(
    db: &Db,
    target: &crate::Target,
    instance: &HirInstance,
    requests: &Requests,
) -> Result<Header, String> {
    let imports = imports(db, target, requests, || format!("{instance:?}"))?; // alloc: an error's name of the function
    let export = match &instance.0 {
        Kind::Code(Instance { def: InstanceDef::Fn(func), args, .. }, _)
            if args.as_ref().iter().all(|it| it.region().is_some()) =>
        {
            exported_symbol(db, *func)
        }
        Kind::Code(..) | Kind::Start(_) | Kind::Allocator(_) | Kind::Asm(..) => None,
    };
    Ok(Header { imports, export })
}

/// What each outside function `requests` name is imported as, by its symbol: a function of an
/// `extern` block from its block's `#[link(wasm_import_module)]` or [`ENV`], a symbol of no
/// item from [`ENV`]; the field is the link name (the symbol, but for a block that names its
/// module: [`link_name_for`]), and the type the one a call of it is lowered to
/// ([`WasmSignature::of`]). Empty but when `target` is wasm. `owner` names what asks, in an error:
/// a function whose `Code::imports` these are, or a static (`Static::imports`).
pub(crate) fn imports(
    db: &Db,
    target: &crate::Target,
    requests: &Requests,
    owner: impl Fn() -> String,
) -> Result<Vec<(String, Import)>, String> {
    // alloc: empty, allocates only for what names an outside function
    let mut imports: Vec<(String, Import)> = Vec::new();
    if !target.is_wasm() {
        return Ok(imports);
    }
    let address = Address::of(target);
    for request in &requests.funcs {
        let (name, module, field) = match &request.target {
            // A bridged call is recorded only in a proc macro's module (`Recorder::callee`).
            FuncTarget::Symbol(symbol) if symbol == RANDOM_KEYS => {
                let ty = WasmSignature::of(&request.sig, address).map_err(|what| {
                    // alloc: an error
                    format!(
                        "{} calls std's `{RANDOM_KEYS}`, whose signature has no wasm type: {what}",
                        owner()
                    )
                })?;
                if ty.params != [address.wasm()] || !ty.results.is_empty() {
                    // alloc: an error
                    return Err(format!(
                        "{} calls std's `{RANDOM_KEYS}` as {ty:?}, and the bridge's import takes the \
                         pointer its keys are written to, ({:?}) -> ()",
                        owner(),
                        address.wasm()
                    ));
                }
                // alloc: the import's to keep, keyed by it
                (RANDOM_KEYS.to_owned(), BRIDGE.to_owned(), RANDOM_KEYS.to_owned())
            }
            // clone: the import's to keep, keyed by it; a symbol of no item is `env`'s field
            FuncTarget::Symbol(symbol) => (symbol.clone(), ENV.to_owned(), symbol.clone()),
            FuncTarget::Instance(instance, _) => {
                let InstanceDef::Fn(func) = instance.def else { continue };
                let ItemContainerId::ExternBlockId(_) = func.lookup(db).container else { continue };
                let (Some(name), Some((module, field))) =
                    (link_name_for(db, target, func), crate::wasm_import(db, func))
                else {
                    // alloc: an error
                    return Err(format!(
                        "{} names a function of an `extern` block with no link name",
                        owner()
                    ));
                };
                // alloc: the import's module, which it keeps
                (name, module.unwrap_or_else(|| ENV.to_owned()), field)
            }
            FuncTarget::Path(_) | FuncTarget::Asm(_) => continue,
        };
        let ty = WasmSignature::of(&request.sig, address).map_err(|what| {
            // alloc: an error
            format!("{} names `{name}` of the wasm module `{module}`, whose signature has no wasm type: {what}", owner())
        })?;
        let import = Import { module, field, ty: ty.format() };
        match imports.iter().find(|it| it.0 == name) {
            Some((_, seen)) if *seen == import => {}
            Some((_, seen)) => {
                // alloc: an error
                return Err(format!(
                    "{} names `{name}` as two imports: of `{}` with {:?}, and of `{}` with {:?}",
                    owner(),
                    seen.module,
                    seen.ty,
                    import.module,
                    import.ty
                ));
            }
            None => imports.push((name, import)),
        }
    }
    Ok(imports)
}

/// `code`, a [`Code`] and the body as `wasm::lower`'s function encodes them, with `header`'s
/// imports and export in its header; and how many bytes further on than in `code` the body
/// starts, which every relocation of the body moves by. Unchanged, and no copy, for an empty
/// header.
pub(crate) fn with_header(code: Vec<u8>, header: &Header) -> Result<(Vec<u8>, u32), String> {
    if header.is_empty() {
        return Ok((code, 0));
    }
    let (mut told, start) = Code::decode(&code)?;
    if !told.imports.is_empty() || told.export.is_some() {
        // alloc: an error
        return Err("wasm: code whose header has imports or an export of its own".to_owned());
    }
    // clone: once a function that imports or exports, the few names its code keeps
    told.imports = header.imports.clone();
    // clone: as above
    told.export = header.export.clone();
    let (bytes, body) = told.encode(&code[start..]);
    let moved = body.checked_sub(start as u32).ok_or("wasm: a header that shrank as it grew")?;
    Ok((bytes, moved))
}

/// The kind of a relocation of wasm code that `wasm::lower` made: a `call`'s function index
/// ([`CALL`]), or an `i32.const` of a function's slot in the table or of the address of data
/// (`Abs4`), each a padded LEB of five bytes; on wasm64 an `i64.const` of either (`Abs8`), a
/// padded LEB of ten. A function is a request's or the body a fold's thunk calls.
pub(crate) fn reloc_kind(kind: Reloc, name: RelocName) -> Result<RelocKind, String> {
    let function =
        matches!(name, RelocName::Func(_) | RelocName::User { namespace: BODY, index: 0 });
    match name {
        _ if function && kind == CALL => Ok(RelocKind::WasmFunction),
        _ if function && kind == Reloc::Abs4 => Ok(RelocKind::WasmTable),
        _ if function && kind == Reloc::Abs8 => Ok(RelocKind::WasmTable64),
        RelocName::Data(_) if kind == Reloc::Abs4 => Ok(RelocKind::WasmMemory),
        RelocName::Data(_) if kind == Reloc::Abs8 => Ok(RelocKind::WasmMemory64),
        // alloc: an error
        _ => Err(format!(
            "a {kind:?} relocation at {name:?} in wasm code, which `wasm::lower` makes none of"
        )),
    }
}

#[cfg(test)]
mod tests {
    use base_db::{CrateOrigin, LangCrateOrigin, all_crates};
    use hir_def::test_items::function;
    use hir_expand::fixture;

    use super::{RANDOM_KEYS, bridged};

    fn std_crate(db: &base_db::Db) -> base_db::Crate {
        all_crates(db)
            .iter()
            .copied()
            .find(|it| matches!(it.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Std)))
            .expect("the fixture's std")
    }

    #[test]
    fn stds_random_keys_are_the_bridges_import() {
        let db = fixture::db(
            r#"
//- /main.rs crate:main deps:std
pub mod random { pub fn hashmap_random_keys() -> (u64, u64) { (0, 0) } }
//- /std.rs crate:std
pub mod random { pub fn hashmap_random_keys() -> (u64, u64) { (1, 2) } }
"#,
        )
        .into_db();
        assert_eq!(bridged(&db, function(&db, std_crate(&db), RANDOM_KEYS)), Some(RANDOM_KEYS));
        assert_eq!(
            bridged(&db, function(&db, fixture::test_crate(&db), RANDOM_KEYS)),
            None,
            "a crate of its own"
        );
    }

    #[test]
    fn a_std_function_of_that_name_elsewhere_keeps_its_body() {
        let db = fixture::db(
            r#"
//- /main.rs crate:main deps:std
//- /std.rs crate:std
pub mod hash { pub fn hashmap_random_keys() -> (u64, u64) { (1, 2) } }
"#,
        )
        .into_db();
        assert_eq!(bridged(&db, function(&db, std_crate(&db), RANDOM_KEYS)), None);
    }
}
