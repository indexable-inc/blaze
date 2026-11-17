//! What a caller's code assumes of an instance beyond its address
//! ([`Described::abi`]; docs/v2/plan/design/launcher.md, "More than one changed body, and a
//! changed signature"), as one id.
//!
//! In a fixed order: the calling convention as written (`extern "C"`, Rust's), whether it is
//! variadic, whether its callers pass their location (`#[track_caller]`), and for each
//! argument, the result and a coroutine's `Poll`, the identity of the type
//! (`hir_ty_share::identity`, through [`Hashes::ty`]) with the interface of each trait a
//! `dyn` in it names.
//!
//! Why the type's identity is the passing mode and the layouts. The backend's mode for a
//! value (direct, a pair, indirect, ignored, with its extensions) is its function of the
//! target and the value's layout (`fn_abi` reads nothing else), and the target is the
//! toolchain's, one a program. The identity holds the type's layout and, spelled through
//! every reference, pointer and argument, the identity of each type it names: an ADT's is
//! its layout hash with its interface (its fields' types, by their interface hashes, at any
//! depth), a closure's is its captures' types. So equal identities are equal modes and equal
//! layouts wherever the callee may read, and a `&Foo` argument's changes when `Foo`'s layout
//! does. It holds more than a layout (the type's key, so its path): a rename gives another
//! `abi` where none was needed, which costs a slot, never soundness. A `dyn` type's identity
//! names its trait by key alone, and a caller that makes or calls through its vtable assumes
//! the vtable's shape: the trait's interface hash is added for each one a signature names.
//!
//! A shim's signature is its arguments' alone (drop glue of `T` takes a `*mut T`), so its
//! `abi` is which shim it is with its arguments' identity. The C `main`, the allocator
//! shim's functions and an `asm!` function have signatures no type names: the first two are
//! fixed, the last is its text's.
//!
//! Cost a describe: one `body_types` (a memo the callers of the instance ask too), one type
//! identity an argument (each ADT's a memo) and one walk of each for its `dyn`s.

use rustc_type_ir::{
    ExistentialPredicate,
    inherent::{BoundExistentialPredicates as _, IntoKind as _},
};

use super::*;

const ABI_DOMAIN: &str = "blaze v2 instance abi 1";

/// The tags of what an `abi` is of.
const BODY: u8 = 0;
const SHIM: u8 = 1;
const START: u8 = 2;
const ASM: u8 = 3;
const ALLOCATOR: u8 = 4;

/// [`Described::abi`] of `kind`, whose arguments' content ID is `plain` ([`args_of`]).
pub(super) fn abi_of(
    db: &Db,
    hashes: &HashSource,
    kind: &Kind,
    plain: ContentId,
) -> Result<ContentId, String> {
    let mut abi = Hasher::new(ABI_DOMAIN);
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Start(_) => return Ok(abi.update(&[START]).finish()),
        Kind::Asm(text, _) => return Ok(abi.update(&[ASM]).part(text.as_bytes()).finish()),
        Kind::Allocator(forward) => {
            let shape = [ALLOCATOR, forward.params, u8::from(forward.returns)];
            return Ok(abi.update(&shape).finish());
        }
    };
    let shim = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            let tracked = hir_ty_mir::mir::location::track_caller(db, func);
            abi.update(&[BODY, u8::from(signature.is_varargs()), u8::from(tracked)]);
            // alloc: once per describe, the convention's few words as text
            abi.part(format!("{:?}", signature.abi).as_bytes());
            None
        }
        // A closure's and a coroutine's body is called by Rust's convention, as its body
        // takes its values.
        InstanceDef::Closure(_) | InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => {
            abi.update(&[BODY, 0, 0]).part(b"Rust");
            None
        }
        InstanceDef::DropGlue => Some(0u8),
        InstanceDef::CallOnceShim => Some(1),
        InstanceDef::VtableShim(func) => {
            abi.id(&hashes.item(db, ItemId::Function(func))?.0);
            Some(2)
        }
        InstanceDef::FnPointerShim => Some(3),
        InstanceDef::ReifyShim { receiver: false } => Some(4),
        InstanceDef::ReifyShim { receiver: true } => Some(5),
    };
    if let Some(shim) = shim {
        abi.update(&[SHIM, shim]).id(&plain);
        for arg in instance.args.as_ref().iter() {
            if let Some(ty) = arg.ty() {
                vtables(db, &mut abi, ty)?;
            }
        }
        return Ok(abi.finish());
    }
    // clone: the query's key by value, a few `Copy` ids
    let body = crate::body_types(db, *instance, env.clone())
        .as_ref()
        .map_err(|why| why.to_string())?; // alloc: the error's message
    abi.update(&(body.params.len() as u64).to_le_bytes());
    abi.update(&[u8::from(body.poll.is_some())]);
    for ty in body.params.iter().chain([&body.returned]).chain(body.poll.as_ref()) {
        let ty = ty.as_ref();
        abi.id(&hashes.ty(db, ty)?);
        vtables(db, &mut abi, ty)?;
    }
    Ok(abi.finish())
}

/// The interface of each trait a `dyn` type in `ty` names, at any depth of its spelling, in
/// the spelling's order: the shape of the vtable a caller hands over or calls through.
fn vtables<'db>(db: &'db Db, abi: &mut Hasher, ty: Ty<'db>) -> Result<(), String> {
    for arg in ty.walk() {
        let Some(inner) = arg.ty() else { continue };
        let TyKind::Dynamic(predicates, _) = inner.kind() else { continue };
        for predicate in predicates.iter() {
            let ExistentialPredicate::Trait(named) = predicate.skip_binder() else { continue };
            let shape = blaze_items::interface_hash(db, ItemId::Trait(named.def_id.0))
                .map_err(|why| why.describe(db))?;
            abi.update(&[0xD1]).id(&shape.0);
        }
    }
    Ok(())
}
