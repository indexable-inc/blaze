//! The keys of an expansion: [`call_key`] before the work, from text alone (rule 65), and
//! [`expansion_key`], the record's, which adds the outside reads the work made.

use blaze_id::{ContentId, Portable};
use blaze_ids::Hasher;

use crate::{call::Call, def::MacroDefinition, expansion::Reads, tokens::Sink};

/// A call's key: the definition's content and site, and the call's tokens, kind, call-site
/// hygiene and eager answers. Two calls with equal keys expand alike given equal reads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CallKey(pub ContentId);

/// An expansion record's key: its call's key and the reads it made, with their answers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ExpansionKey(pub ContentId);

// SAFETY: a content hash.
unsafe impl Portable for CallKey {}
// SAFETY: a content hash.
unsafe impl Portable for ExpansionKey {}

/// The key of `call` of `def`, computed before the work.
pub fn call_key(def: &MacroDefinition<'_>, call: &Call<'_>) -> CallKey {
    let mut hasher = Hasher::new("blaze v2 expand call 1");
    hasher.id(&def.content().0);
    let site = def.site();
    hasher.id(&site.krate.0);
    match site.module {
        None => hasher.put(&[0]),
        Some(module) => {
            hasher.put(&[1]);
            hasher.id(&module.0);
        }
    }
    call.encode(&mut hasher);
    CallKey(hasher.finish())
}

/// The key of the record of `call` of `def` that made `reads`.
pub fn expansion_key(
    def: &MacroDefinition<'_>,
    call: &Call<'_>,
    reads: &Reads<'_>,
) -> ExpansionKey {
    let mut hasher = Hasher::new("blaze v2 expand expansion 1");
    hasher.id(&call_key(def, call).0);
    reads.encode(&mut hasher);
    ExpansionKey(hasher.finish())
}
