//! A `Signature`: one head the checker asks for, as a drive task (D114, drive.md "Check times:
//! the end state in numbers" rule 4, and "D114 rulings / Signatures").
//!
//! THE UNIT is one `heads::Want` of front2 (blaze-ladder front2/project/head_digest.rs): a fn,
//! `const` or `static`; a struct, enum or union with its variants and constructors; a trait with
//! its members' fn signatures; an impl's header with its member list; one fn of an impl; a type
//! alias or trait alias. A trait's member has no `Signature` of its own (its trait's covers it);
//! an impl's fn has.
//!
//! ITS NAME ([`signature_name`]): the file's subject (the file's real path in the checkout, as
//! the items records list it after `/0/`, so the body that reads it and the file job that makes
//! it name it alike) and the entry's chain up to the file, as work.rs `body_name` walks it.
//!
//! ITS SOURCE ([`signature_source`]), its key's text: the entry's head bytes (a fn's, `const`'s or
//! `static`'s signature; a struct's, enum's, union's or alias's whole text, whose fields are its
//! head), for a trait or impl each member's (kind, name, disambiguator) and, for a trait's fn and
//! any non-fn member, that member's bytes; and each enclosing impl's or trait's signature.
//! Nothing of its siblings: what its paths resolve through (the file's `use`s and declarations)
//! is read by its task, never keyed. Two entries of one name are told apart by their names
//! ([`signature_name`]: each step's disambiguator counts the earlier siblings of the same kind
//! and name, and an unnamed step (an impl, an `extern` block, a macro call) holds its head, as
//! the chain does), never by their keys. A body edit leaves it equal; any edit to the head moves it.
//!
//! ITS OUT ([`signature_out`]): the digest of the head records the want makes
//! (`ProjectWorld::want_digest`) with [`unencoded`], the head's tokens without whitespace or
//! comments, for what the head record form leaves out by name (a fn's and an ADT's lifetimes, an
//! impl's const parameters, a trait's or impl's non-fn members). A reformat or a comment leaves
//! the out equal; a type edit moves it, and with it each body that read it (body_types.rs).
//!
//! A HEAD A MACRO WROTE has no file entry of its own, but its call has one: the fn-like call at
//! module level (`write_two!();`, an outline `MacroCall` entry), or the item an attribute or
//! derive takes (`#[derive(Clone)] struct S`, a blaze edition's root `fn main`, which
//! `bstd::main` takes unasked, D104). A call written by another expansion goes up its origins
//! (blaze-scope expansion_origins.rs) to the file entry the outermost call is at. So ONE
//! `Signature` per such file entry covers its own head and every head the expansions of calls
//! at it write: its name is the entry's ([`signature_name`], from text, listed by the file's
//! outline job before any expansion is made, as every other part), and its out is
//! [`entry_out`]: the entry's own out, then, per call in call-key order, [`expansion_out`] (each
//! head's want digest, and the expansion's heads' tokens). Two call sites of one macro are two
//! entries, so two names; one call in two crates is two files' subjects; a rerun is the same
//! name. A head is read by the entry its call is at (body_types/heads.rs `expansion_head`).
//!
//! Why the entry and not (call, entry in the expansion): a part is named before the work, from
//! text (rule 65), and the job that lists a file's parts runs before the crate's expansions are
//! made; the job that makes them, the crate's `CrateScope`, is no home a `Signature` can have
//! (the drive keys a `Signature` by its home's `Owners` and `Surface`, request.rs `locate`, and
//! the command runs one in its home FILE's body job, work.rs, `Kind::BodyCheck |
//! Kind::Signature`). The grain is the call's entry: an edit of one head a call wrote reruns the
//! readers of every head that call wrote (all of them, for one call writing one head).
//! Only a WORKSPACE crate's such head is read so: another crate's heads are under its
//! `CrateScope` identity.
//!
//! WHICH ENTRY COVERS A HEAD ([`covering_entry`]), for every head a workspace body can read:
//! - an item in modules, or a fn of an impl in modules: its own entry;
//! - a trait's member (fn, const, type, macro call): its trait's (the trait's want holds them);
//! - an impl's const or type: its impl's; a member a macro call inside the impl block writes
//!   (`impl T { some_macro!(..); }`): its impl's, whose out holds that call's expansion as a
//!   module-level call's entry holds its own ([`entry_out`]; the call's text is the impl's
//!   source, never its out);
//! - a foreign fn or static: its `extern` block's, which is an entry of its own whose whole text
//!   is its heads and whose out holds each foreign item's want digest;
//! - a head an expansion wrote: the entry its (outermost) call is at, as above, so a call in an
//!   impl block is covered by its impl;
//! - a head inside a body: none. A body's items are named only inside it and checked in its
//!   task (body_types/heads.rs `body_local`), covered by its text; the one way out of a body
//!   (an impl written in it) is a finding by name ([`uncovered_why`]).

use blaze_drive::{Kind, Name};
use blaze_file_items::Outline;
use blaze_ids::{ContentId, Hasher};
use blaze_surface::{Edition, Item, ItemIndex, ItemKind};

/// The domain of a `Signature`'s subject ([`signature_name`]). 2: the subject is the file and
/// the entry's chain, which holds an unnamed entry's head (lane RU3).
const NAME_DOMAIN: &str = "blaze-cli signature name 2";
/// The domain of a `Signature` part's source ([`signature_source`]).
const SOURCE_DOMAIN: &str = "blaze-cli signature source 2";
/// The domain of a head's tokens ([`unencoded`]). 2: each piece is its `token_digest`
/// (kinds, joints, doc comments and the edition in), no longer its bare lexemes.
const TOKENS_DOMAIN: &str = "blaze-cli signature tokens 2";
/// The domain of a `Signature`'s out ([`signature_out`]).
const OUT_DOMAIN: &str = "blaze-cli signature out 1";

/// Whether `kind` is a head of its own when written outside any body and any trait or impl.
const fn own_head(kind: ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Function
            | ItemKind::Const
            | ItemKind::Static
            | ItemKind::Struct
            | ItemKind::Enum
            | ItemKind::Union
            | ItemKind::Trait
            | ItemKind::TraitAlias
            | ItemKind::Impl
            | ItemKind::TypeAlias
    )
}

/// Whether every entry `item` is written in, up to the file, is a module: it is in no body, no
/// trait, no impl, no extern block.
fn in_modules_only(outline: &Outline<'_>, item: &Item) -> bool {
    let mut parent = item.parent;
    while let Some(at) = parent {
        let up = outline.item(at);
        if up.kind() != ItemKind::Module {
            return false;
        }
        parent = up.parent;
    }
    true
}

/// Whether `item` of `outline` has a `Signature` task: a head of its own in modules only, a
/// macro call in modules only (the heads its expansion writes, module docs), an `extern` block
/// in modules only (its foreign items' heads: a foreign fn has no body, so the block's whole
/// text is their heads), or a fn of an impl that is. The one predicate: the file job makes a
/// part for exactly these (work.rs `outline_file`), and a body names a head by the entry
/// [`covering_entry`] finds for it.
///
/// A fn of an impl is asked BEFORE the module-level rule: a fn is a head of its own too, and
/// the module-level rule refused every impl fn (its parent is the impl), so no impl fn had a
/// `Signature` and every body reading one, its own included, was not checked (tempo,
/// 2026-11-17 03:55: 1,767 notes "a head it reads could not be named as a `Signature`", each
/// at an impl fn or a caller of one).
pub(crate) fn is_signature_entry(outline: &Outline<'_>, item: &Item) -> bool {
    if let Some(parent) = item.parent {
        let up = outline.item(parent);
        if item.kind() == ItemKind::Function && up.kind() == ItemKind::Impl {
            return in_modules_only(outline, up);
        }
    }
    if own_head(item.kind()) || matches!(item.kind(), ItemKind::MacroCall | ItemKind::ExternBlock) {
        return in_modules_only(outline, item);
    }
    false
}

/// The entry whose `Signature` covers a head written at entry `at` of `outline`: the entry
/// itself when it passes [`is_signature_entry`]; else its container's, when that is a trait, an
/// impl or an `extern` block in modules only: a trait's members are its trait's head; an impl's
/// const, type and macro call (whose expansion writes members) are its impl's; a foreign item is
/// its block's. `None`: the entry is inside a body (an item of a body, an impl in a body, a
/// macro call in one), or no head ([`uncovered_why`] says which).
pub(crate) fn covering_entry(outline: &Outline<'_>, at: ItemIndex) -> Option<ItemIndex> {
    let item = outline.items().get(at.0 as usize)?;
    if is_signature_entry(outline, item) {
        return Some(at);
    }
    let parent = item.parent?;
    let up = outline.item(parent);
    (matches!(up.kind(), ItemKind::Trait | ItemKind::Impl | ItemKind::ExternBlock) && in_modules_only(outline, up)).then_some(parent)
}

/// Why no entry covers a head written at entry `at` ([`covering_entry`] is `None`).
pub(crate) fn uncovered_why(outline: &Outline<'_>, at: ItemIndex) -> &'static str {
    let Some(item) = outline.items().get(at.0 as usize) else {
        return "a head's entry is not in its file as it is now (signature.rs `covering_entry`)";
    };
    let mut parent = item.parent;
    while let Some(up) = parent {
        let up = outline.item(up);
        if !matches!(up.kind(), ItemKind::Module | ItemKind::Impl | ItemKind::Trait | ItemKind::ExternBlock) {
            return "a head written inside a body is read from outside that body (an impl in a body, whose members are seen from outside it, or an item a body's macro call wrote): a body's items have no `Signature` (D114 FACTS (iii)), and its text covers them only for a body that holds them";
        }
        parent = up.parent;
    }
    "the entry at a head's place is no head of its own, and no trait, impl or `extern` block in modules holds it (signature.rs `covering_entry`)"
}

/// The physical checkout file whose outline supplies an entry's signature. A module slot
/// cannot be passed to the head planner or listing as a signature file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SignatureFile(ContentId);

impl SignatureFile {
    pub(crate) fn new(real_path: &[u8]) -> Self {
        Self(Name::file(real_path).subject)
    }

    pub(crate) fn subject(self) -> ContentId {
        self.0
    }

    pub(crate) fn entry(self, outline: &Outline<'_>, item: &Item, text: &str) -> Name {
        signature_name(self.0, outline, item, text)
    }

    pub(crate) fn entry_from_chain(self, chain: ContentId) -> Name {
        let mut h = Hasher::new(NAME_DOMAIN);
        h.id(&self.0).id(&chain);
        Name::new(Kind::Signature, h.finish())
    }
}

/// The subject of a file's `Signature`s: its real path in the checkout.
pub(crate) fn file_subject(real: &str) -> ContentId {
    SignatureFile::new(real.as_bytes()).subject()
}

/// The `Signature` of `item` (an entry passing [`is_signature_entry`]) in the file of subject
/// `file` (its [`file_subject`]), over its `outline` of `text`: the file and the entry's chain
/// (blaze-file-items chain.rs `item_chain`, the one identity of an entry by names, rule 30).
///
/// INJECTIVE OVER A FILE'S ENTRIES (lane RU3): the chain holds an UNNAMED entry's head (an
/// impl, an `extern` block, a macro call), whose disambiguator counts only its siblings of the
/// same head (blaze-file-items digest.rs `sibling_keys`). The walk this replaced hashed kind,
/// name and disambiguator alone, so two impls with different headers in one file (`impl Pair`
/// and `impl crate::Measure for Pair`, both unnamed, both disambiguator 0) were ONE entry name:
/// one head task, its source one impl's (rule 21), its rows two impl headers under one cell
/// (engine_heads.rs `heads_of`: "two heads of its entry fall under one cell"), and every body
/// reading either header refused; each fn of the two impls named alike (`fn measure` of two
/// `impl Measure for ..`) shared one name too. 2: the chain's form, so no task or record named
/// by the old form is read by its key.
pub(crate) fn signature_name(file: ContentId, outline: &Outline<'_>, item: &Item, text: &str) -> Name {
    SignatureFile(file).entry_from_chain(blaze_file_items::item_chain(outline, item, text))
}

/// The same grain chain as `blaze_file_items::item_chain`, over the serialized outline. Its
/// name column already contains the original spelling, including the raw-identifier prefix.
pub(crate) fn stored_chain(file: blaze_surface::FileItemsView<'_>, at: ItemIndex) -> ContentId {
    let mut h = Hasher::new(blaze_file_items::CHAIN_DOMAIN);
    let mut next = Some(at);
    while let Some(index) = next {
        let entry = file.item(index).expect("verified outline parent");
        h.update(&[entry.kind().byte()]).update(&(entry.name.len() as u32).to_le_bytes()).update(entry.name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        if entry.item.name.is_none() {
            h.id(&blaze_file_items::head_of(&entry.item));
        }
        next = entry.parent;
    }
    h.finish()
}

/// THE PLANT (rule 23): [`signature_name`]'s form before lane RU3 (kind, name and disambiguator
/// of each entry up the chain, no unnamed entry's head), so the tests show two unnamed siblings
/// under one name with it and none without (tests/engine_head_cells_tests.rs).
#[cfg(test)]
pub(crate) fn signature_name_before(file: ContentId, outline: &Outline<'_>, item: &Item, text: &str) -> Name {
    let mut h = Hasher::new("blaze-cli signature name 1");
    h.update(&file.0);
    let mut at = Some(item);
    while let Some(entry) = at {
        let name = entry.name.map_or("", |range| entry.text(text, range));
        h.update(&[entry.kind() as u8]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        at = entry.parent.map(|up| outline.item(up));
    }
    Name::new(Kind::Signature, h.finish())
}

/// Whether an entry's fields or variants are its head: its whole text is. A macro call's whole
/// text (its path and input) is what its expansion is made from: all of it is its head. An
/// `extern` block's foreign items have no bodies: all of it is their heads.
const fn whole_is_head(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Struct | ItemKind::Enum | ItemKind::Union | ItemKind::TypeAlias | ItemKind::TraitAlias | ItemKind::MacroCall | ItemKind::ExternBlock)
}

/// The digest of an entry's head bytes: its whole text where its fields are its head, else its
/// signature (blaze-file-items' `signature_hash`: attributes, visibility, name, generics, up to
/// its body, or up to a `const`'s or `static`'s `=`).
fn head_bytes(item: &Item) -> ContentId {
    if whole_is_head(item.kind()) { item.hash } else { item.signature_hash }
}

/// The source of the `Signature` part of entry `at` of `outline` (see the module docs). It
/// holds nothing of the entry's siblings (lane BH1): before, the file's surface was in it, so
/// an item added to the file moved every head of the file (the tiny fixture, g200823: 14 head
/// keys for one private fn added to `m3.rs`). What the head's paths resolve to is read, not
/// keyed: the names cells its task records (engine_heads.rs), and on the drive's path its
/// owners' `CrateScope` records whole, which every surface of the crate moves.
pub(crate) fn signature_source(outline: &Outline<'_>, at: ItemIndex, text: &str) -> ContentId {
    let item = outline.item(at);
    let children = outline.children(Some(at)).map(|(_, member)| (member.name.map_or("", |range| member.text(text, range)), *member));
    let parents = core::iter::successors(item.parent, |at| outline.item(*at).parent).map(|at| *outline.item(at));
    head_source_parts(item, children, parents)
}

/// The tokens a signature lookup can observe, including whether a function is defined.
/// A member's inherited impl header and an ADT's fields are part of its signature.
pub(crate) fn stored_signature(file: blaze_surface::FileItemsView<'_>, at: ItemIndex, item: &Item) -> ContentId {
    let children = file.children(Some(at)).map(|(_, member)| (member.name, member.item));
    let parents = core::iter::successors(item.parent, |at| file.item(*at).and_then(|entry| entry.parent))
        .map(|at| file.item(at).expect("verified outline parent").item);
    let mut h = Hasher::new("blaze-cli outline item signature tokens 1");
    h.id(&head_source_parts(item, children, parents));
    if matches!(item.kind(), ItemKind::Function | ItemKind::Const | ItemKind::Static) {
        h.update(&[u8::from(item.body.is_some())]);
    }
    if item.kind() == ItemKind::Trait {
        for (_, member) in file.children(Some(at)).filter(|(_, member)| member.kind() == ItemKind::Function) {
            h.update(&[u8::from(member.body.is_some())]);
        }
    }
    h.finish()
}

fn head_source_parts<'a>(item: &Item, children: impl Iterator<Item = (&'a str, Item)>, parents: impl Iterator<Item = Item>) -> ContentId {
    let mut h = Hasher::new(SOURCE_DOMAIN);
    h.update(&[item.kind() as u8]).update(&head_bytes(item).0);
    if matches!(item.kind(), ItemKind::Trait | ItemKind::Impl) {
        let trait_ = item.kind() == ItemKind::Trait;
        let mut count = 0u32;
        for (name, member) in children {
            count += 1;
            h.update(&[member.kind() as u8]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&member.disambiguator.to_le_bytes());
            // An impl's fn is its own `Signature`; a trait's fn and every other member are part
            // of this one. A trait's fn's default body is not a head: its signature is.
            if member.kind() == ItemKind::Function {
                if trait_ {
                    h.update(&member.signature_hash.0);
                }
            } else {
                h.update(&member.hash.0);
            }
        }
        h.update(&count.to_le_bytes());
    }
    // Each enclosing impl's or trait's signature: an impl's generics and self type are its fn's.
    for up in parents {
        if matches!(up.kind(), ItemKind::Impl | ItemKind::Trait) {
            h.update(&[up.kind() as u8]).update(&up.signature_hash.0);
        }
    }
    h.finish()
}

/// The tokens of `piece` in `edition` into `h`: their `token_digest`
/// (docs/v2/plan/design/token-keys.md), so a re-format or a plain comment moves nothing while
/// `1 >> 2` and `1 > > 2`, a doc comment's text and the edition do. Lexemes alone, as this was
/// before, gave those pairs one digest: two programs under one key (rule 21).
fn tokens(h: &mut Hasher, piece: &str, edition: Edition) {
    h.id(&piece_digest(piece, edition));
}

/// [`tokens`]' digest of one piece. A piece is a range of an outline, whose ranges are u32, so
/// it fits a token key and the refusal cannot be met; met, it fails this one job (rule 20).
fn piece_digest(piece: &str, edition: Edition) -> ContentId {
    match parser::token_key::token_digest(piece, crate::work::parser_edition(edition)) {
        Ok(digest) => digest,
        Err(error) => panic!("a head's piece of an outline has no token key: {error}"),
    }
}

/// What the head record form leaves out of `item`'s head (see the module docs), as the head's
/// tokens without whitespace or comments: its signature's (a struct's, enum's, union's or
/// alias's whole text's), and for a trait each member's signature and each non-fn member's
/// whole text, for an impl each non-fn member's whole text. It holds the head's lifetimes and
/// an impl's const parameters, which head records do not; a reformat leaves it equal.
pub(crate) fn unencoded(outline: &Outline<'_>, at: ItemIndex, text: &str, edition: Edition) -> ContentId {
    let item = outline.item(at);
    let mut h = Hasher::new(TOKENS_DOMAIN);
    // A macro call's own text is its source (`head_bytes`), not its out: what readers see of it
    // is its expansion's heads (`expansion_out`), so an edit inside a body the call writes
    // moves no reader.
    let head = if item.kind() == ItemKind::MacroCall {
        ""
    } else if whole_is_head(item.kind()) {
        item.range.of(text)
    } else {
        item.text(text, item.signature)
    };
    tokens(&mut h, head, edition);
    if matches!(item.kind(), ItemKind::Trait | ItemKind::Impl) {
        let trait_ = item.kind() == ItemKind::Trait;
        for (_, member) in outline.children(Some(at)) {
            // An impl's macro call is its source, not its out: what readers see of it is the
            // members its expansion writes (`expansion_out`, body_types/heads.rs
            // `make_expanded`), as for a module-level call. A trait's is kept whole: the
            // trait's want digest is the one that holds its members.
            if !trait_ && member.kind() == ItemKind::MacroCall {
                continue;
            }
            let piece = if member.kind() != ItemKind::Function {
                member.range.of(text)
            } else if trait_ {
                member.text(text, member.signature)
            } else {
                continue;
            };
            h.update(&[0xff, member.kind() as u8]);
            tokens(&mut h, piece, edition);
        }
    }
    h.finish()
}

/// A `Signature`'s out: the want's head records' digest (`want_digest`) and
/// [`unencoded`].
pub(crate) fn signature_out(want_digest: ContentId, unencoded: ContentId) -> ContentId {
    let mut h = Hasher::new(OUT_DOMAIN);
    h.update(&want_digest.0).update(&unencoded.0);
    h.finish()
}

/// The domain of an entry's out with the expansions at it ([`entry_out`]).
const ENTRY_DOMAIN: &str = "blaze-cli signature entry with expansions 1";
/// The domain of one expansion's heads ([`expansion_out`]).
const EXPANSION_DOMAIN: &str = "blaze-cli signature expansion heads 1";

/// The out of a file entry's `Signature`: `own` ([`signature_out`] of its own head) when no
/// call at it wrote a head, so an entry no macro takes keeps the out it had; else `own` and
/// each `expansions` out ([`expansion_out`], in call-key order).
pub(crate) fn entry_out(own: ContentId, expansions: &[ContentId]) -> ContentId {
    if expansions.is_empty() {
        return own;
    }
    let mut h = Hasher::new(ENTRY_DOMAIN);
    h.update(&own.0).update(&(expansions.len() as u32).to_le_bytes());
    for out in expansions {
        h.update(&out.0);
    }
    h.finish()
}

/// The out of the heads one expansion wrote: each head's want digest (`digests`, in the
/// order its caller fixes: by want), then, over the expansion's `text` and its `outline` (in
/// `edition`), each entry passing [`is_signature_entry`] by index with its [`unencoded`]
/// tokens, for what the head records leave out. The expansion's bodies are not in it: an
/// edit inside a body the macro wrote moves no reader of its heads.
pub(crate) fn expansion_out(digests: &[ContentId], outline: &Outline<'_>, text: &str, edition: Edition) -> ContentId {
    let mut h = Hasher::new(EXPANSION_DOMAIN);
    h.update(&(digests.len() as u32).to_le_bytes());
    for digest in digests {
        h.update(&digest.0);
    }
    for (index, item) in outline.items().iter().enumerate() {
        if is_signature_entry(outline, item) {
            let at = ItemIndex(index as u32);
            h.update(&at.0.to_le_bytes()).update(&unencoded(outline, at, text, edition).0);
        }
    }
    h.finish()
}

#[cfg(test)]
#[path = "signature_tokens_tests.rs"]
mod tokens_tests;
