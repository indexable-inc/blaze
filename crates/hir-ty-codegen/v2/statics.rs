//! A static's value as v2 keeps it ([`Static`]): its bytes with what they point at by place,
//! its own read-only data by content.

use base_db::Db;
use blaze_codegen::{
    Arch, Init, Relocation, Static, backend::FrontRef, template::Refs, wasm::Import,
};
use blaze_ids::ContentId;
use blaze_items::{ItemId, ItemKey};
use hir_def::{HasModule, StaticId, attrs::AttrFlags, signatures::StaticSignature};

use super::{
    HashSource, Hashes, Places,
    places::{Placer, Ref, fill_type_ids, place_requests, resolve, weak_symbol},
};
use crate::{
    ConstBlob, compiled_static,
    fragment::{CompiledStatic, Requests},
    is_thread_local,
};

const VALUE_DOMAIN: &str = "blaze v2 static value 2";

/// A static's value, its references by place.
pub(crate) struct Value {
    /// The content ID of everything below: what the static's bytes are made of.
    pub(crate) value: ContentId,
    pub(crate) refs: Vec<Ref>,
    pub(crate) mutable: bool,
    pub(crate) thread_local: bool,
    pub(crate) section: Option<String>,
    /// On wasm, what the externs it points to are imported as ([`Static::imports`]).
    imports: Vec<(String, Import)>,
    blob: ConstBlob,
    requests: Requests,
    places: Places,
}

/// Where the value of a static the walk reaches ([`FrontRef::Static`]) comes from.
pub(crate) enum Source<'db> {
    /// An `extern_weak` static of an `extern` block: blaze's own static holding the address of
    /// this symbol, 0 when nothing defines it. Its value is no Rust code's, so it makes no
    /// vtable and calls nothing.
    Weak(String),
    /// Any other: its initializer, evaluated and compiled.
    Compiled(&'db CompiledStatic),
}

/// Where the value of the static `id` comes from. Every reader of a reached static's value
/// asks here, never `compiled_static` itself, which has no value for one of an `extern` block.
pub(crate) fn source(db: &Db, id: StaticId) -> Result<Source<'_>, String> {
    if let Some(symbol) = weak_symbol(db, id)? {
        return Ok(Source::Weak(symbol));
    }
    match compiled_static(db, id) {
        Ok(it) => Ok(Source::Compiled(it)),
        // alloc: the error's message
        Err(e) => {
            Err(format!("the static `{}`: {e}", blaze_items::key::path(db, ItemId::Static(id))))
        }
    }
}

/// What the static `id` holds, evaluated.
pub(crate) fn value(db: &Db, hashes: &HashSource, id: StaticId) -> Result<Value, String> {
    let target = crate::target_of_crate(db, id.krate(db)).map_err(|e| e.to_string())?;
    let thread_local = is_thread_local(db, id);
    // alloc: only a static with `#[link_section]`, whose `Static::section` owns its name
    let section = AttrFlags::link_section(db, id.into()).map(|it| it.as_str().to_owned());
    let mut placer = Placer::open();
    let (blob, mut requests, mutable) = match source(db, id)? {
        // What code reads of it is the address of the symbol.
        Source::Weak(symbol) => {
            placer.place(FrontRef::Weak(symbol))?;
            let word = target.word();
            let blob = ConstBlob {
                bytes: vec![0; word],  // alloc: only an `extern_weak` static, its one address
                relocs: Vec::new(),    // alloc: empty, allocates nothing
                fn_relocs: Vec::new(), // alloc: empty, allocates nothing
                align: word as u64,
                type_ids: Vec::new(), // alloc: empty, allocates nothing
            };
            (blob, Requests::default(), false)
        }
        Source::Compiled(CompiledStatic { requests, blob, writable }) => {
            // clone: the memo's, which `place_requests` rewrites in place
            let requests =
                Requests { funcs: requests.funcs.clone(), datas: requests.datas.clone() };
            // clone: the memo's, which `fill_type_ids` rewrites in place
            let mut blob = blob.clone();
            fill_type_ids(db, hashes, Some(id.krate(db)), &mut blob)?;
            (blob, requests, *writable)
        }
    };
    let places = place_requests(db, hashes, Some(id.krate(db)), &mut placer, &mut requests)?;
    let imports = wasm_imports(db, target, id, &requests)?;
    let mut bytes = super::shape::value(&blob, &requests, &places)?;
    Ok(Value {
        value: value_id(&mut bytes, mutable, thread_local, &imports, section.as_deref()),
        refs: placer.into_refs(),
        mutable,
        thread_local,
        section,
        imports,
        blob,
        requests,
        places,
    })
}

/// The content ID of a static's value: `bytes`, its shape ([`super::shape::value`]), and the
/// rest of what the static is made of.
fn value_id(
    bytes: &mut Vec<u8>,
    mutable: bool,
    thread_local: bool,
    imports: &[(String, Import)],
    section: Option<&str>,
) -> ContentId {
    bytes.push(u8::from(mutable));
    bytes.push(u8::from(thread_local));
    bytes.extend_from_slice(&(imports.len() as u32).to_le_bytes());
    for (name, import) in imports {
        for text in [name, &import.module, &import.field] {
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        import.ty.encode(bytes);
    }
    if let Some(section) = section {
        bytes.extend_from_slice(section.as_bytes());
    }
    ContentId::of(VALUE_DOMAIN, bytes)
}

fn wasm_imports(
    db: &Db,
    target: &crate::Target,
    id: StaticId,
    requests: &Requests,
) -> Result<Vec<(String, Import)>, String> {
    // alloc: an error's name of the static
    super::wasm::imports(db, target, requests, || {
        format!("the static `{}`", StaticSignature::of(db, id).name.as_str())
    })
}

/// The static `id` with its references at `refs`.
pub(crate) fn compile(
    db: &Db,
    hashes: &HashSource,
    arch: Arch,
    id: StaticId,
    refs: &Refs,
) -> Result<Static, String> {
    let item: ItemKey = hashes.item(db, ItemId::Static(id))?;
    // The static's value is laid out for its crate's target, so `arch` has to be its.
    let target = crate::target_of_crate(db, id.krate(db)).map_err(|e| e.to_string())?;
    if arch.is_wasm() != target.is_wasm() {
        // alloc: an error
        return Err(format!(
            "the static `{}` compiled for {arch:?} while its crate's target is {:?}",
            StaticSignature::of(db, id).name.as_str(),
            target.arch
        ));
    }
    let value = value(db, hashes, id)?;
    if value.refs.len() != refs.len() {
        // alloc: the error's message
        return Err(format!("{} references given for a value of {}", refs.len(), value.refs.len()));
    }
    let (data, mut relocs) = resolve(&value.places, refs, |resolve| {
        super::convert::value(resolve.frame(), arch, &value.blob, &value.requests, &item.0, resolve)
    })?;
    if weak_symbol(db, id)?.is_some() {
        // The symbol is data whatever the static's type, as rustc declares it to LLVM: on
        // wasm32 its address is in linear memory.
        let kind = super::convert::word(arch).data;
        // clone: only an `extern_weak` static; `Relocation` owns its target
        relocs.push(Relocation { at: 0, kind, target: refs[0].clone(), addend: 0 });
    }
    // alloc: the error's message
    let align = u32::try_from(value.blob.align).map_err(|e| e.to_string())?;
    let init = if relocs.is_empty() && value.blob.bytes.iter().all(|&it| it == 0) {
        Init::Zeroed { size: value.blob.bytes.len() as u64 }
    } else {
        Init::Bytes { bytes: value.blob.bytes, relocs }
    };
    Ok(Static {
        item,
        instance: ContentId::default(),
        align,
        init,
        data,
        mutable: value.mutable,
        thread_local: value.thread_local,
        section: value.section,
        imports: value.imports,
    })
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::{Signature, ValType};

    use super::*;

    fn import(module: &str, field: &str, results: Vec<ValType>) -> Vec<(String, Import)> {
        let ty = Signature { params: vec![ValType::I32], results };
        vec![("f".to_owned(), Import { module: module.to_owned(), field: field.to_owned(), ty })]
    }

    fn id(imports: &[(String, Import)]) -> ContentId {
        value_id(&mut vec![1, 2, 3, 4, 0, 0, 0, 0], false, false, imports, Some(".data.x"))
    }

    #[test]
    fn statics_that_differ_only_in_an_import_are_two_values() {
        let base = import("env", "f", vec![]);
        assert_eq!(id(&base), id(&import("env", "f", vec![])));
        assert_ne!(id(&base), id(&[]));
        assert_ne!(id(&base), id(&import("other", "f", vec![])));
        assert_ne!(id(&base), id(&import("env", "g", vec![])));
        assert_ne!(id(&base), id(&import("env", "f", vec![ValType::I64])));
        // The texts are length-prefixed: moving a byte from the module to the field is another
        // value.
        assert_ne!(id(&import("en", "vf", vec![])), id(&import("env", "f", vec![])));
    }
}
