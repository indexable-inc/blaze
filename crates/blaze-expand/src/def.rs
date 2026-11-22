//! What a macro is: [`MacroDefinition`], the working form expansion reads, and the hash of its
//! content, [`MacroContent`], which is what an expansion's key names it by (never its item key:
//! two definitions with equal content expand alike, and a renamed or moved one keeps its
//! expansions; rule 21).
//!
//! Its stored form is blaze-surface's `MacroDef` record, with each token stream a
//! [`TokenStream::write_record`] record named by its digest: [`MacroDefinition::write`] gives
//! the three, [`MacroDefinition::from`] reads them back and checks they are one definition.

use core::fmt;

use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, CollectIn, Vec};
use blaze_surface::{BuildError, MacroDef, MacroDefHead, MacroDefView};

use crate::{
    error::{StreamError, Table},
    record::{RecordBytes, RecordError, stream_record_id},
    tokens::{Buffered, DefSite, Edition, Mark, MarkOf, Sink, Text, TokenStream, Transparency, push_text},
};

/// The macro words, defined once in the schema: the builtin names, how a proc macro is called,
/// and what kind of macro a definition is.
pub use blaze_surface::{BuiltinAttr, BuiltinDerive, BuiltinFn, MacroKind, ProcKind};

/// `#[macro_export]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Export {
    Local,
    Exported,
}

/// `#[macro_export(local_inner_macros)]`: a single-name macro call the body writes resolves as
/// `$crate::name!`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LocalInner {
    No,
    Yes,
}

/// Where and how a macro is defined, besides its tokens.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DefAt {
    pub site: DefSite,
    pub edition: Edition,
    pub export: Export,
    pub local_inner: LocalInner,
    /// `#[rustc_macro_transparency = ".."]`; `None` takes the kind's own.
    pub transparency: Option<Transparency>,
}

/// The hash of what a definition is: its kind, tokens, edition, flags, helper attributes and
/// transparency. Comments and spaces are not tokens, so they are not in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MacroContent(pub ContentId);

// SAFETY: a content hash.
unsafe impl Portable for MacroContent {}
// SAFETY: a closed enum.
unsafe impl Portable for Export {}
// SAFETY: a closed enum.
unsafe impl Portable for LocalInner {}
// SAFETY: a def site of keys, an edition, flags and a transparency.
unsafe impl Portable for DefAt {}
// SAFETY: every field is `Portable` (streams, UTF-8 bytes, ranges of them, keys, closed
// enums); the allocator is where the bytes are.
unsafe impl Portable for MacroDefinition<'_> {}

/// A macro's definition, as expansion reads it.
#[derive(Clone, Debug)]
pub struct MacroDefinition<'a> {
    kind: MacroKind,
    body: TokenStream<'a>,
    params: Option<TokenStream<'a>>,
    /// The text of a proc macro's name and of the helper attributes.
    names: AVec<'a, u8>,
    /// A proc macro's export name, in `names`.
    proc_name: Option<Text>,
    helpers: AVec<'a, Text>,
    at: DefAt,
    transparency: Transparency,
    content: MacroContent,
}

/// A definition as records: the `MacroDef` record, and each token stream's record with the
/// digest the `MacroDef` names it by. Each is stored by itself.
pub struct DefinitionRecords<'a> {
    pub def: RecordBytes<'a>,
    /// The body's tokens; `None` for a body of no tokens and no contexts.
    pub body: Option<(ContentId, RecordBytes<'a>)>,
    pub params: Option<(ContentId, RecordBytes<'a>)>,
}

/// Why records are not one definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DefinitionError {
    /// A token stream's record does not read.
    Stream(RecordError),
    /// The records disagree: `what` says how (a stream given that the head does not name, or
    /// one whose digest is not the head's, a content hash that is not of what was read).
    Mismatch { item: ItemKey, what: &'static str },
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DefinitionError::Stream(error) => write!(f, "a macro's token stream record: {error}"),
            DefinitionError::Mismatch { item, what } => {
                write!(f, "the records of macro {:x} are not one definition: {what}", item.0)
            }
        }
    }
}

impl core::error::Error for DefinitionError {}

impl<'a> MacroDefinition<'a> {
    /// `macro_rules! m { body }`: `body` the tokens inside the braces.
    pub fn rules(body: TokenStream<'a>, at: DefAt) -> MacroDefinition<'a> {
        let alloc = body.allocator();
        MacroDefinition::make(MacroKind::Rules, body, None, at, alloc)
    }

    /// `macro m { body }` (`params` `None`), or `macro m(params) { body }`.
    pub fn macro2(
        params: Option<TokenStream<'a>>,
        body: TokenStream<'a>,
        at: DefAt,
    ) -> MacroDefinition<'a> {
        let alloc = body.allocator();
        MacroDefinition::make(MacroKind::Macro2, body, params, at, alloc)
    }

    pub fn builtin_fn(which: BuiltinFn, at: DefAt, alloc: Alloc<'a>) -> MacroDefinition<'a> {
        let body = TokenStream::new_in(alloc);
        MacroDefinition::make(MacroKind::BuiltinFn(which), body, None, at, alloc)
    }

    pub fn builtin_attr(which: BuiltinAttr, at: DefAt, alloc: Alloc<'a>) -> MacroDefinition<'a> {
        let body = TokenStream::new_in(alloc);
        MacroDefinition::make(MacroKind::BuiltinAttr(which), body, None, at, alloc)
    }

    pub fn builtin_derive(which: BuiltinDerive, at: DefAt, alloc: Alloc<'a>) -> MacroDefinition<'a> {
        let body = TokenStream::new_in(alloc);
        MacroDefinition::make(MacroKind::BuiltinDerive(which), body, None, at, alloc)
    }

    /// The proc macro `name` of the proc-macro crate `krate`; a derive's `helpers` are the
    /// attributes it declares (`#[proc_macro_derive(Name, attributes(helper, ..))]`).
    pub fn proc_macro(
        krate: CrateItemKey,
        kind: ProcKind,
        name: &str,
        helpers: &[&str],
        at: DefAt,
        alloc: Alloc<'a>,
    ) -> Result<MacroDefinition<'a>, StreamError> {
        MacroDefinition::with_names(
            MacroKind::ProcMacro { krate, kind },
            TokenStream::new_in(alloc),
            None,
            Some(name),
            helpers.iter().copied(),
            at,
            alloc,
        )
    }

    fn make(
        kind: MacroKind,
        body: TokenStream<'a>,
        params: Option<TokenStream<'a>>,
        at: DefAt,
        alloc: Alloc<'a>,
    ) -> MacroDefinition<'a> {
        MacroDefinition::with_names(kind, body, params, None, core::iter::empty(), at, alloc)
            .expect("a definition with no names fits the name table")
    }

    /// The one constructor: every other is this one.
    fn with_names<'h>(
        kind: MacroKind,
        body: TokenStream<'a>,
        params: Option<TokenStream<'a>>,
        proc_name: Option<&str>,
        helpers: impl ExactSizeIterator<Item = &'h str>,
        at: DefAt,
        alloc: Alloc<'a>,
    ) -> Result<MacroDefinition<'a>, StreamError> {
        let mut names = Vec::new_in(alloc);
        let proc_name = match proc_name {
            Some(name) => Some(push_text(&mut names, name, Table::Text)?),
            None => None,
        };
        let mut helper_texts = Vec::with_capacity_in(helpers.len(), alloc);
        for helper in helpers {
            helper_texts.push(push_text(&mut names, helper, Table::Text)?);
        }
        let mut def = MacroDefinition {
            kind,
            body,
            params,
            names,
            proc_name,
            helpers: helper_texts,
            at,
            transparency: at.transparency.unwrap_or(kind.own_transparency()),
            content: MacroContent(ContentId([0; 32])),
        };
        def.content = def.hash();
        Ok(def)
    }

    /// The definition `view` and its token stream records hold, in `alloc`: `body` and
    /// `params` the records the head names ([`MacroDefHead::body`], [`MacroDefHead::params`]),
    /// each checked against its digest, and the whole checked against the head's content hash,
    /// so what is read is the definition that was written.
    pub fn from(
        view: &MacroDefView<'_>,
        body: Option<&[u8]>,
        params: Option<&[u8]>,
        alloc: Alloc<'a>,
    ) -> Result<MacroDefinition<'a>, DefinitionError> {
        let head = view.head();
        let mismatch = |what| DefinitionError::Mismatch { item: head.item, what };
        let stream = |id: Option<ContentId>, bytes: Option<&[u8]>| match (id, bytes) {
            (None, None) => Ok(None),
            (Some(id), Some(bytes)) if stream_record_id(bytes) == id => {
                TokenStream::read_record(bytes, alloc)
                    .map(Some)
                    .map_err(DefinitionError::Stream)
            }
            (Some(_), Some(_)) => Err(mismatch("a token stream whose digest is not the head's")),
            (Some(_), None) => Err(mismatch("a token stream the head names, not given")),
            (None, Some(_)) => Err(mismatch("a token stream given that the head does not name")),
        };
        let body = stream(head.body, body)?.unwrap_or_else(|| TokenStream::new_in(alloc));
        let params = stream(head.params, params)?;
        let at = DefAt {
            site: head.site,
            edition: head.edition,
            export: if head.exported { Export::Exported } else { Export::Local },
            local_inner: if head.local_inner { LocalInner::Yes } else { LocalInner::No },
            transparency: head.written_transparency,
        };
        let proc_name = match head.kind {
            MacroKind::ProcMacro { .. } => Some(head.name),
            _ => None,
        };
        let def = MacroDefinition::with_names(
            head.kind,
            body,
            params,
            proc_name,
            view.helpers().collect_in(alloc).into_iter(),
            at,
            alloc,
        )
        .map_err(|_| mismatch("names past the name table's u32::MAX bytes"))?;
        if def.transparency != head.transparency {
            return Err(mismatch("a transparency that is not the written one or the kind's"));
        }
        if def.content.0 != head.content {
            return Err(mismatch("a content hash that is not of what the records hold"));
        }
        Ok(def)
    }

    pub fn kind(&self) -> MacroKind {
        self.kind
    }

    /// The rules of a declarative macro, or a `macro m(..)`'s body; empty for the others.
    pub fn body(&self) -> &TokenStream<'a> {
        &self.body
    }

    pub fn params(&self) -> Option<&TokenStream<'a>> {
        self.params.as_ref()
    }

    pub fn at(&self) -> DefAt {
        self.at
    }

    pub fn edition(&self) -> Edition {
        self.at.edition
    }

    pub fn site(&self) -> DefSite {
        self.at.site
    }

    pub fn transparency(&self) -> Transparency {
        self.transparency
    }

    pub fn content(&self) -> MacroContent {
        self.content
    }

    /// A proc macro's export name.
    pub fn proc_name(&self) -> Option<&str> {
        self.proc_name.map(|name| name.of(&self.names))
    }

    /// A derive's helper attributes, in the order declared.
    pub fn helpers(&self) -> impl Iterator<Item = &str> + '_ {
        self.helpers.iter().map(|text| text.of(&self.names))
    }

    /// The definition as records, in `alloc`: the `MacroDef` of the macro `item` named `name`
    /// and written at `place` (`None` for a builtin), and its token streams. A proc macro's
    /// `name` is its export name, refused when it differs.
    pub fn write<'b>(
        &self,
        item: ItemKey,
        name: &str,
        place: Option<blaze_surface::Place>,
        alloc: Alloc<'b>,
    ) -> Result<DefinitionRecords<'b>, BuildError> {
        if self.proc_name().is_some_and(|proc_name| proc_name != name) {
            return Err(BuildError::Disagrees {
                table: "MacroDef",
                what: "a proc macro's export name and the name of its item",
            });
        }
        let stream = |tokens: &TokenStream<'a>| {
            let bytes = tokens.write_record(alloc);
            (stream_record_id(&bytes), bytes)
        };
        let body = match self.body.is_empty() && self.body.contexts().is_empty() {
            true => None,
            false => Some(stream(&self.body)),
        };
        let params = self.params.as_ref().map(stream);
        let head = MacroDefHead {
            item,
            name,
            content: self.content.0,
            place,
            kind: self.kind,
            site: self.at.site,
            edition: self.at.edition,
            exported: self.at.export == Export::Exported,
            local_inner: self.at.local_inner == LocalInner::Yes,
            written_transparency: self.at.transparency,
            transparency: self.transparency,
            body: body.as_ref().map(|(id, _)| *id),
            params: params.as_ref().map(|(id, _)| *id),
        };
        let mut def = MacroDef::new_in(head, alloc);
        for helper in self.helpers() {
            def.helper(helper)?;
        }
        Ok(DefinitionRecords { def: def.finish()?, body, params })
    }

    /// The mark this definition's expansion puts on a token, at `transparency`.
    pub(crate) fn mark(&self, transparency: Transparency) -> Mark {
        Mark { of: MarkOf::This, transparency, edition: self.at.edition, site: self.at.site }
    }

    fn hash(&self) -> MacroContent {
        let mut hasher = Hasher::new("blaze v2 expand macro content 1");
        match self.kind {
            MacroKind::Rules => hasher.put(&[0]),
            MacroKind::Macro2 => hasher.put(&[1]),
            MacroKind::BuiltinFn(which) => {
                hasher.put(&[2]);
                hasher.part(which.name().as_bytes());
            }
            MacroKind::BuiltinAttr(which) => {
                hasher.put(&[3]);
                hasher.part(which.name().as_bytes());
            }
            MacroKind::BuiltinDerive(which) => {
                hasher.put(&[4]);
                hasher.part(which.name().as_bytes());
            }
            MacroKind::ProcMacro { krate, kind } => {
                hasher.put(&[5, kind.byte()]);
                hasher.id(&krate.0);
                let name = self.proc_name().expect("every constructor of a proc macro names it");
                hasher.part(name.as_bytes());
            }
        }
        // Streams through a buffer (tokens.rs `Buffered`), each fed at its statement's end.
        self.body.encode(&mut Buffered::new(&mut hasher));
        match &self.params {
            None => hasher.put(&[0]),
            Some(params) => {
                hasher.put(&[1]);
                params.encode(&mut Buffered::new(&mut hasher));
            }
        }
        hasher.put(&crate::tokens::index(self.helpers.len()).to_be_bytes());
        for helper in self.helpers() {
            hasher.part(helper.as_bytes());
        }
        let export = u8::from(self.at.export == Export::Exported);
        let inner = u8::from(self.at.local_inner == LocalInner::Yes);
        hasher.put(&[self.at.edition.byte(), export, inner, self.transparency.byte()]);
        MacroContent(hasher.finish())
    }
}
