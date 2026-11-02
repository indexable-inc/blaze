//! A digest of each piece of a [`Built`](super::Built) that a dependency image holds, sorted
//! by what it is, so a dev run checks its pieces against an image by merging two sorted lists
//! (`blaze_premap`, fastcompile-sol-premap 1). Each is made where its piece is made: a
//! function's as it is instantiated, a static's on the cores that make it, a debug name's
//! where the walk describes its instance.
//!
//! [`static_definition`] is the one hash of a static's definition: `blaze_premap` keys its
//! images by it and checks them by it, so the two can't drift.

use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};

use crate::{Data, Init, InstanceKey, Static, content, wasm};

pub const FUNCTION_DIGEST_DOMAIN: &str = "blaze v2 function digest 1";
pub const STATIC_DIGEST_DOMAIN: &str = "blaze v2 static digest 2";
pub const DEBUG_NAME_DOMAIN: &str = "blaze v2 debug name 1";

/// Of each piece of a build, by what it is, each list sorted and each key once.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct Digests {
    /// Of each of `Built::functions`, by key: [`function_digest`].
    pub functions: Vec<(InstanceKey, ContentId)>,
    /// Of each of `Built::statics`, by item and instance: [`static_definition`].
    pub statics: Vec<((ItemKey, ContentId), ContentId)>,
    /// Of each of `Built::debug_names`, by key: [`debug_name_digest`].
    pub debug_names: Vec<(InstanceKey, ContentId)>,
}

/// What a function's key does not say of its code: the code hash it was made from and the
/// code hashes of what it built in, in its order. With nothing built in, as nearly every dev
/// function, it is the code hash itself and nothing is hashed.
pub fn function_digest(code: CodeHash, inlined: &[CodeHash]) -> ContentId {
    if inlined.is_empty() {
        return code.0;
    }
    let mut hasher = Hasher::new(FUNCTION_DIGEST_DOMAIN);
    hasher.id(&code.0).update(&(inlined.len() as u64).to_le_bytes());
    for it in inlined {
        hasher.id(&it.0);
    }
    hasher.finish()
}

/// The content ID of a static's definition: which static it is, its alignment, its value
/// and relocations, the data its value refers to, its mutability, thread-local storage,
/// section and wasm imports. Destructured, so a field added to [`Static`] is a compile error
/// here until it is hashed.
pub fn static_definition(value: &Static) -> ContentId {
    let Static { item, instance, align, init, data, mutable, thread_local, section, imports } = value;
    let mut hasher = Hasher::new(STATIC_DIGEST_DOMAIN);
    hasher.id(&item.0).id(instance).update(&align.to_le_bytes());
    match init {
        Init::Bytes { bytes, relocs } => {
            hasher.update(&[0]).part(bytes);
            content::relocs_into(&mut hasher, relocs, content::target_into);
        }
        Init::Zeroed { size } => {
            hasher.update(&[1]).update(&size.to_le_bytes());
        }
    }
    hasher.update(&(data.len() as u64).to_le_bytes());
    for Data { id, align, bytes, relocs, writable } in data {
        hasher.id(id).update(&align.to_le_bytes()).part(bytes);
        content::relocs_into(&mut hasher, relocs, content::target_into);
        hasher.update(&[u8::from(*writable)]);
    }
    hasher.update(&[*mutable as u8, *thread_local as u8]);
    match section {
        Some(section) => hasher.update(&[1]).part(section.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&(imports.len() as u64).to_le_bytes());
    for (name, wasm::Import { module, field, ty: wasm::Signature { params, results } }) in imports {
        hasher.part(name.as_bytes()).part(module.as_bytes()).part(field.as_bytes());
        for types in [params, results] {
            hasher.update(&(types.len() as u64).to_le_bytes());
            for ty in types {
                hasher.update(&[*ty as u8]);
            }
        }
    }
    hasher.finish()
}

/// A debug name's BLAKE3.
pub fn debug_name_digest(name: &str) -> ContentId {
    ContentId::of(DEBUG_NAME_DOMAIN, name.as_bytes())
}

/// `digests` sorted by key, keeping only those of `functions` (sorted by key, as dedupe leaves
/// them): a function dedupe merged away is no piece of the build.
pub(super) fn of_kept(
    mut digests: Vec<(InstanceKey, ContentId)>,
    functions: &[crate::CompiledFunction],
) -> Vec<(InstanceKey, ContentId)> {
    digests.sort_unstable_by_key(|it| it.0);
    let mut at = 0;
    digests.retain(|(key, _)| {
        while at < functions.len() && functions[at].key < *key {
            at += 1;
        }
        at < functions.len() && functions[at].key == *key
    });
    digests.dedup_by_key(|it| it.0);
    digests
}

#[cfg(test)]
mod tests;
