//! The keys of an expansion: [`call_key`] before the work (rule 65), the ONE key of a call's
//! expansion in every layer (the names library's kept expansions, the lowering's expansion
//! scopes, a stored expansion record), and [`expansion_key`], the record's, which adds the
//! outside reads the work made.

use blaze_id::{ContentId, Portable};
use blaze_ids::Hasher;
use blaze_surface::DefSite;

use crate::{
    call::Call,
    def::{MacroContent, MacroDefinition},
    expansion::Reads,
    tokens::Sink,
};

/// A call's key ([`call_key`]): the expander's code, the definition's identity and the call's
/// tokens as shaped. Two calls with equal keys expand alike given equal reads.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CallKey(pub ContentId);

/// An expansion record's key: its call's key and the reads it made, with their answers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ExpansionKey(pub ContentId);

// SAFETY: a content hash.
unsafe impl Portable for CallKey {}
// SAFETY: a content hash.
unsafe impl Portable for ExpansionKey {}

/// The expander's code identity (rule 52): a kept expansion is a result of this crate's code,
/// so the name changes with any change to what a call expands to (a rule's matching, a
/// derive's output, a token's record). A proc macro's code is its runner's key (the
/// `ProcMacro { krate }` of its [`MacroContent`]), beside this.
/// 2: an attribute macro's input is without the derives written before it (P18-B, item.rs
/// `ItemAttrs::attr_call`).
/// 3: D96: `env!("CARGO_MANIFEST_DIR")` answers the root the crate sees (`/package`,
/// `/workspace/<dir>`), and an include of an absolute path under it is served.
/// 4: one key per expansion: [`call_key`] holds this name, and the names library's kept
/// expansions are keyed by it (the text key is gone).
pub const EXPANDER: &str = "blaze-expand expander 4";

/// The key of `call` of `def`, computed before the work: [`call_key_of`] of the definition's
/// content and site.
pub fn call_key(def: &MacroDefinition<'_>, call: &Call<'_>) -> CallKey {
    call_key_of(def.content(), def.site(), call)
}

/// The one key of a call's expansion, from the definition's identity (`content`, `site`: what a
/// macro definition record's head holds, so a caller need not read its streams) and the call.
/// Its inputs, each once (rule 21), and nothing else:
/// - the expander's code: [`EXPANDER`];
/// - the definition's [`MacroContent`]: its kind (a builtin's name; a proc macro's runner crate
///   and export name), its rules or body and parameters as TOKENS (comments and spaces are not
///   inputs), its derive helpers, edition, `#[macro_export]`, `local_inner_macros` and
///   transparency;
/// - the definition's [`DefSite`]: the crate `$crate` names, and the module a `macro`'s
///   def-site names resolve in;
/// - the call ([`Call::encode`]): its shape, the call site's edition, panic edition and
///   context, the input as tokens WITH their hygiene contexts (so a call written inside
///   another expansion is keyed by what its tokens are, not by text), an attribute's
///   arguments, a derive's facts, and an eager call's position and answers.
///
/// Not in it: any position (file, line, column), the crate the call is written in, and what
/// the work reads from outside (`cfg!`, `env!`, `include!`, `file!`, `line!`, `column!`, the
/// root a crate sees): each is a read in the expansion's read list, checked against today's
/// facts before a kept expansion is taken ([`expansion_key`] adds them). So the same call at
/// two places is one key, and one whose output depends on where it is says so by its reads.
pub fn call_key_of(content: MacroContent, site: DefSite, call: &Call<'_>) -> CallKey {
    let mut hasher = Hasher::new("blaze v2 expand call 2");
    hasher.part(EXPANDER.as_bytes());
    hasher.id(&content.0);
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
