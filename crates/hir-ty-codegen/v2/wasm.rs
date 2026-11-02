//! What wasm32 code says to the linker beside its body ([`blaze_codegen::wasm::Code`]): what
//! each function outside the build it names is imported as and the name the module exports it
//! under ([`Header`]), and the kinds of its relocations. The rest of the header (its type, the
//! types of its indirect calls, the places of the stack pointer) is `wasm::lower`'s.

use base_db::Db;
use blaze_codegen::{
    RelocKind,
    wasm::{Code, Import},
};
use cranelift_codegen::binemit::Reloc;
use hir_def::{ItemContainerId, Lookup, attrs::AttrFlags};

use super::{HirInstance, Kind, fold::BODY};
use crate::{
    Instance, InstanceDef, exported_symbol,
    fragment::{FuncTarget, RelocName, Requests},
    link_name,
    wasm::{CALL, WasmSignature},
};

/// The module a function of an `extern` block is imported from when the block names none
/// (`#[link(wasm_import_module)]`), as rustc's wasm targets import it.
pub(crate) const ENV: &str = "env";

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
pub(crate) fn header(db: &Db, instance: &HirInstance, requests: &Requests) -> Result<Header, String> {
    let imports = imports(db, requests, || format!("{instance:?}"))?; // alloc: an error's name of the function
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
/// item from [`ENV`]; the field is the symbol, and the type the one a call of it is lowered to
/// ([`WasmSignature::of`]). Empty but on a wasm target. `owner` names what asks, in an error:
/// a function whose `Code::imports` these are, or a static (`Static::imports`).
pub(crate) fn imports(
    db: &Db,
    requests: &Requests,
    owner: impl Fn() -> String,
) -> Result<Vec<(String, Import)>, String> {
    // alloc: empty, allocates only for what names an outside function
    let mut imports: Vec<(String, Import)> = Vec::new();
    if !crate::Target::selected()?.is_wasm() {
        return Ok(imports);
    }
    for request in &requests.funcs {
        let (name, module) = match &request.target {
            // clone: the import's to keep, keyed by it
            FuncTarget::Symbol(symbol) => (symbol.clone(), ENV.to_owned()),
            FuncTarget::Instance(instance, _) => {
                let InstanceDef::Fn(func) = instance.def else { continue };
                let ItemContainerId::ExternBlockId(block) = func.lookup(db).container else { continue };
                let Some(name) = link_name(db, func) else {
                    // alloc: an error
                    return Err(format!("{} names a function of an `extern` block with no link name", owner()));
                };
                let module = match AttrFlags::wasm_import_module(db, block) {
                    // alloc: the import's module, which it keeps
                    Some(module) => module.as_str().to_owned(),
                    // alloc: as above
                    None => ENV.to_owned(),
                };
                (name, module)
            }
            FuncTarget::Path(_) | FuncTarget::Asm(_) => continue,
        };
        let ty = WasmSignature::of(&request.sig).map_err(|what| {
            // alloc: an error
            format!("{} names `{name}` of the wasm module `{module}`, whose signature has no wasm type: {what}", owner())
        })?;
        // clone: the field is the symbol, and both are the import's to keep
        let import = Import { module, field: name.clone(), ty: ty.format() };
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
/// (`Abs4`), each a padded LEB of five bytes. A function is a request's or the body a fold's
/// thunk calls.
pub(crate) fn reloc_kind(kind: Reloc, name: RelocName) -> Result<RelocKind, String> {
    let function = matches!(name, RelocName::Func(_) | RelocName::User { namespace: BODY, index: 0 });
    match name {
        _ if function && kind == CALL => Ok(RelocKind::WasmFunction),
        _ if function && kind == Reloc::Abs4 => Ok(RelocKind::WasmTable),
        RelocName::Data(_) if kind == Reloc::Abs4 => Ok(RelocKind::WasmMemory),
        // alloc: an error
        _ => Err(format!("a {kind:?} relocation at {name:?} in wasm code, which `wasm::lower` makes none of")),
    }
}
