//! The keys of an expansion: [`call_key`] before the work (rule 65), the ONE key of a call's
//! expansion in every layer (the names library's kept expansions, the lowering's expansion
//! scopes, a stored expansion record), and [`expansion_key`], the record's, which adds the
//! outside reads the work made.

use blaze_id::{ContentId, Portable};
use blaze_ids::Hasher;
use blaze_ids::producer::{Expander, ProducerId};
use blaze_surface::DefSite;

use crate::{
    call::Call,
    def::{MacroContent, MacroDefinition},
    expansion::Reads,
    tokens::{Buffered, Sink},
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

/// The key an expansion is KEPT under (decision D105, docs/v2/plan/design/producer-id.md): the
/// call's key (what is asked) and the expander's code identity (what answers, rule 52), which
/// the program's entry hands down ([`blaze_ids::producer::Producers`]). A set of kept
/// expansions is filed and searched by it alone, so an expansion made by other expander code
/// is never found. A proc macro's code is its runner's key (the `ProcMacro { krate }` of its
/// [`MacroContent`]), in the call key.
pub fn kept_key(expander: ProducerId<Expander>, call: CallKey) -> CallKey {
    let mut hasher = Hasher::new("blaze v2 expand kept 1");
    hasher.id(expander.id());
    hasher.id(&call.0);
    CallKey(hasher.finish())
}

/// The key of `call` of `def`, computed before the work: [`call_key_of`] of the definition's
/// content and site.
pub fn call_key(def: &MacroDefinition<'_>, call: &Call<'_>) -> CallKey {
    call_key_of(def.content(), def.site(), call)
}

/// The one key of a call's expansion, from the definition's identity (`content`, `site`: what a
/// macro definition record's head holds, so a caller need not read its streams) and the call.
/// Its inputs, each once (rule 21), and nothing else:
/// - (not the expander's code: a call's key says what is asked; [`kept_key`] adds who answers);
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
    let mut hasher = Hasher::new("blaze v2 expand call 3");
    hasher.id(&content.0);
    hasher.id(&site.krate.0);
    match site.module {
        None => hasher.put(&[0]),
        Some(module) => {
            hasher.put(&[1]);
            hasher.id(&module.0);
        }
    }
    // Through a buffer: the call's tokens are a few tiny puts each (tokens.rs `Buffered`); the
    // temporary is dropped, so its held bytes fed, at the end of this statement.
    call.encode(&mut Buffered::new(&mut hasher));
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
    reads.encode(&mut Buffered::new(&mut hasher));
    ExpansionKey(hasher.finish())
}
