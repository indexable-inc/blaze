//! The keys of an expansion: [`call_key`] before the work, from text alone (rule 65), and
//! [`expansion_key`], the record's, which adds the outside reads the work made.

use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_ids::Hasher;
use blaze_surface::CallShape;

use crate::{
    call::Call,
    def::{Export, LocalInner, MacroDefinition, ProcKind},
    expansion::Reads,
    tokens::{Edition, Sink},
};

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

/// The expander's code identity (rule 52): a kept expansion is a result of this crate's code,
/// so the name changes with any change to what a call expands to (a rule's matching, a
/// derive's output, a token's record). A proc macro's code is its runner's key
/// ([`DefText::Proc`]), beside this.
pub const EXPANDER: &str = "blaze-expand expander 1";

/// A definition as the text a key is made from, before anything is lexed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefText<'t> {
    /// `macro_rules! name { body }`.
    Rules { body: &'t str },
    /// `macro name(params) { body }`, hygienic at its module.
    Macro2 { params: Option<&'t str>, body: &'t str, module: ItemKey },
    /// A builtin derive: its name is the definition.
    BuiltinDerive,
    /// A proc macro: the crate that runs it (its code's identity), its kind and its helper
    /// attributes as its declaration lists them.
    Proc { runner: CrateItemKey, kind: ProcKind, helpers: &'t str },
}

/// One macro call as text: every input of its expansion that is known before the work (rule
/// 21). What is NOT here is read while expanding and belongs to the record's read list: the
/// cfg atoms the item walk asks ([`crate::kept::Asked`]) and the expansion's [`Reads`].
///
/// The inputs, each a field:
/// - the expander's code: [`EXPANDER`], and a proc macro's runner ([`DefText::Proc`]);
/// - the definition: its text ([`DefText`]), its name, the crate it is defined in (what
///   `$crate` names), its edition, whether it is exported and has `local_inner_macros`;
/// - the call: its shape, the attribute or derive path as written (which attribute of the
///   item the call is), its input text;
/// - the call site's hygiene: the edition. A call's tokens are lexed from text at the root
///   context of their edition ([`crate::Ctx::root`]), so the edition is all an expansion can
///   observe of where it is called; a caller that gives a call's tokens another context has
///   another key to write.
///
/// The crate the call is written in is not here: a kept expansion is found in that crate's own
/// record, never in another's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextCall<'t> {
    pub def: DefText<'t>,
    pub def_name: &'t str,
    pub def_krate: CrateItemKey,
    pub def_edition: Edition,
    pub export: Export,
    pub local_inner: LocalInner,
    pub shape: CallShape,
    /// The path of the attribute or derive as the call wrote it; empty for a function-like call.
    pub attr: &'t str,
    /// The call site's edition (its tokens' and its panics').
    pub edition: Edition,
    pub input: &'t str,
}

/// A call's key from its text ([`TextCall`]): two calls with equal keys expand alike wherever
/// their read lists are answered alike.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TextKey(pub ContentId);

// SAFETY: a content hash.
unsafe impl Portable for TextKey {}

/// The key of `call`, from text alone: nothing is lexed (rule 65). Equal to
/// `text_key_with(def_digest(call), call)`: a caller with many calls of one definition digests
/// the definition once ([`def_digest`]) and keys each call with [`text_key_with`].
pub fn text_key(call: &TextCall<'_>) -> TextKey {
    text_key_with(def_digest(call), call)
}

/// The definition's part of a [`TextKey`]: the expander's code, the definition's text, name,
/// crate, edition, export and `local_inner_macros` ([`def_digest`]). Equal for every call of
/// one definition, so it is computed once per definition per command, not once per call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DefDigest(pub ContentId);

/// The digest of `call`'s definition. Reads only `def`, `def_name`, `def_krate`,
/// `def_edition`, `export` and `local_inner`; the call's own fields are [`text_key_with`]'s.
pub fn def_digest(call: &TextCall<'_>) -> DefDigest {
    let mut hasher = Hasher::new("blaze v2 expand text definition 1");
    hasher.part(EXPANDER.as_bytes());
    match call.def {
        DefText::Rules { body } => {
            hasher.update(&[0]).part(body.as_bytes());
        }
        DefText::Macro2 { params, body, module } => {
            hasher.update(&[1]).opt_part(params).part(body.as_bytes()).id(&module.0);
        }
        DefText::BuiltinDerive => {
            hasher.update(&[2]);
        }
        DefText::Proc { runner, kind, helpers } => {
            hasher.update(&[3, kind.byte()]).id(&runner.0).part(helpers.as_bytes());
        }
    }
    hasher.part(call.def_name.as_bytes());
    hasher.id(&call.def_krate.0);
    let export = match call.export {
        Export::Local => 0,
        Export::Exported => 1,
    };
    let local_inner = match call.local_inner {
        LocalInner::No => 0,
        LocalInner::Yes => 1,
    };
    hasher.update(&[call.def_edition.byte(), export, local_inner]);
    DefDigest(hasher.finish())
}

/// The key of `call` given its definition's digest `def` (`def_digest(call)`'s, computed once
/// for the definition): the digest, then the call's shape, edition, attribute and input. Every
/// input of [`TextCall`] is in the key, the definition's through its digest.
pub fn text_key_with(def: DefDigest, call: &TextCall<'_>) -> TextKey {
    let mut hasher = Hasher::new("blaze v2 expand text call 2");
    hasher.id(&def.0);
    hasher.update(&[call.shape.byte(), call.edition.byte()]);
    hasher.part(call.attr.as_bytes());
    hasher.part(call.input.as_bytes());
    TextKey(hasher.finish())
}

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
