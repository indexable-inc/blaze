//! What the records held answer one read of the machine (bottom.md 6, "Names"): a finished
//! scope's binding, glob edges or textual scope, an enum's variants, or the module to wait on.

#![blaze::sans_io]

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{
    BindingState, Def, DefKind, GlobEdge, Globs, Namespace, Origin, TextualDef, Visibility,
    globs_digest, textual_digest,
};

use crate::facts::Name;
use crate::read::{Answer, NameAnswer, Read};
use crate::scope_record::ScopeOf;

const HELPERS_DOMAIN: &str = "blaze-scope derive helpers 1";

/// One variant of an enum: what a path through the enum reads. A record variant (`V { .. }`)
/// has no value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Variant<N> {
    pub name: N,
    pub key: ItemKey,
    pub record: bool,
}

/// What the records hold of one key.
pub enum Held<'h, N> {
    /// A module's finished scope; `chain` is the module and each parent up to its crate's
    /// root (`None`: the module alone). A working form's scope or a dependency's exports
    /// record's, read in place (crate::scope_record).
    Scope { scope: ScopeOf<'h, N>, krate: CrateItemKey, chain: Option<&'h [ItemKey]> },
    /// An enum's variants.
    Variants { variants: AVec<'h, Variant<N>>, krate: CrateItemKey },
    /// A module whose resolution failed: it is read as empty, and the read is noted.
    Failed { krate: CrateItemKey, chain: Option<&'h [ItemKey]> },
    /// A module of this fill not finished yet: a read of it waits.
    Pending,
    /// A key of no crate the records know: read as empty, and noted.
    Unknown,
}

/// The records a task reads: finished scopes and enums of every crate, and the modules this
/// fill computes. Each method is a lookup by key, nothing more.
pub trait Records<N> {
    fn held(&self, key: ItemKey) -> Held<'_, N>;

    /// `Some(parent)` for a module this fill computes (walked, or written by a macro); `None`
    /// for any other key. A module of another crate has no entry scope here: its textual
    /// entry is read as empty.
    fn parent(&self, module: ItemKey) -> Option<Option<ItemKey>>;

    /// `read` was answered `own` ([`answer`]): the hook a solve's read list is noted by
    /// ([`crate::asked`]); nothing by default.
    fn asked(&self, read: Read<N>, own: &Own<'_, N>) {
        let _ = (read, own);
    }
}

/// An answer, owned: the replies of one step borrow from these.
#[derive(Debug)]
pub enum Own<'a, N> {
    Name(NameAnswer),
    Globs { edges: AVec<'a, GlobEdge>, krate: CrateItemKey, chain: AVec<'a, ItemKey>, digest: ContentId },
    Textual(AVec<'a, TextualDef<N>>, ContentId),
    Helpers(ContentId),
    /// In progress, waiting on the module named (the one `answer`'s `Lack::Wait` named).
    InProgress(ItemKey),
}

impl<N: Copy> Own<'_, N> {
    pub fn answer(&self) -> Answer<'_, N> {
        match self {
            Own::Name(a) => Answer::Name(*a),
            Own::Globs { edges, krate, chain, digest } => Answer::Globs { edges, krate: *krate, chain, digest: *digest },
            Own::Textual(defs, digest) => Answer::Textual { defs, digest: *digest },
            Own::Helpers(digest) => Answer::Helpers { names: &[], digest: *digest },
            Own::InProgress(module) => Answer::InProgress { module: *module },
        }
    }
}

/// A read answered from what is not a finished scope: counted by the caller, never an error of
/// the user's code by itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Note {
    /// A read of a module whose resolution failed, answered empty.
    FailedModule,
    /// A read of a module of another crate with no finished scope, answered empty.
    NoScope,
    /// A read of a key of no crate the records know, answered empty.
    Unknown,
}

/// Why a read has no answer now.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lack {
    /// The module the read waits on: its scope is not finished.
    Wait(ItemKey),
    /// The chain of parents above `module` is longer than [`MAX_MODULE_DEPTH`].
    TooDeep(ItemKey),
}

/// The digest of a derive's helper attributes, as a read list records it.
pub fn helpers_digest<N: AsRef<str>>(names: &[N], alloc: Alloc<'_>) -> ContentId {
    let mut bytes = AVec::new_in(alloc);
    for name in names {
        let name = name.as_ref();
        bytes.extend_from_slice(&u64::try_from(name.len()).expect("a name fits a u64").to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
    }
    ContentId::of(HELPERS_DOMAIN, &bytes)
}

/// What `records` answer `read`, owned in `alloc`, with a note when it was answered empty;
/// `here` is the crate the fill computes (a key of no crate is answered as one of it).
pub fn answer<'a, N: Name, R: Records<N>>(
    read: Read<N>,
    records: &R,
    here: CrateItemKey,
    alloc: Alloc<'a>,
) -> Result<(Own<'a, N>, Option<Note>), Lack> {
    let owner = read.module();
    let empty_textual = || Own::Textual(AVec::new_in(alloc), textual_digest::<N>(&[], alloc));
    match read {
        Read::DeriveHelpers { .. } => return Ok((Own::Helpers(helpers_digest::<N>(&[], alloc)), None)),
        Read::TextualEntry { module } => {
            let defs = entry(module, records, alloc)?;
            let digest = textual_digest(&defs, alloc);
            return Ok((Own::Textual(defs, digest), None));
        }
        Read::Name { .. } | Read::Globs { .. } | Read::TextualOut { .. } => {}
    }
    let (own, note) = match records.held(owner) {
        Held::Scope { scope, krate, chain } => {
            let own = match read {
                Read::Name { ns, name, .. } => Own::Name(NameAnswer::from(scope.get(ns, name))),
                Read::Globs { module } => globs(scope.globs(), krate, chain, module, alloc),
                _ => {
                    let defs = AVec::from_slice_in(scope.textual(), alloc);
                    let digest = textual_digest(&defs, alloc);
                    Own::Textual(defs, digest)
                }
            };
            (own, None)
        }
        Held::Variants { variants, krate } => {
            let own = match read {
                Read::Name { ns, name, .. } => Own::Name(variant_answer(&variants, ns, name)),
                // An enum has no glob edges: its variants are its own names.
                Read::Globs { module } => globs(Globs::of(&[]), krate, None, module, alloc),
                _ => empty_textual(),
            };
            (own, None)
        }
        Held::Pending => return Err(Lack::Wait(owner)),
        Held::Failed { krate, chain } => {
            let note = if krate == here { Note::FailedModule } else { Note::NoScope };
            (empty(read, krate, chain, alloc), Some(note))
        }
        Held::Unknown => (empty(read, here, None, alloc), Some(Note::Unknown)),
    };
    Ok((own, note))
}

fn empty<'a, N: Name>(read: Read<N>, krate: CrateItemKey, chain: Option<&[ItemKey]>, alloc: Alloc<'a>) -> Own<'a, N> {
    match read {
        Read::Name { .. } => Own::Name(NameAnswer::Absent),
        Read::Globs { module } => globs(Globs::of(&[]), krate, chain, module, alloc),
        _ => Own::Textual(AVec::new_in(alloc), textual_digest::<N>(&[], alloc)),
    }
}

/// A `Globs` answer: the module's edges, its crate, its chain up to its crate's root.
fn globs<'a, N>(edges: Globs<'_>, krate: CrateItemKey, chain: Option<&[ItemKey]>, module: ItemKey, alloc: Alloc<'a>) -> Own<'a, N> {
    let edges = AVec::from_iter_in(edges.iter(), alloc);
    let chain = match chain {
        Some(chain) => AVec::from_slice_in(chain, alloc),
        None => AVec::from_slice_in(&[module], alloc),
    };
    let digest = globs_digest(&edges, krate, &chain, alloc);
    Own::Globs { edges, krate, chain, digest }
}

/// A `Name` read of an enum: its variant of the name (a record variant has no value).
fn variant_answer<N: Name>(variants: &[Variant<N>], ns: Namespace, name: N) -> NameAnswer {
    let hit = variants.iter().find(|v| v.name == name && (ns == Namespace::Types || (ns == Namespace::Values && !v.record)));
    match hit {
        Some(v) => NameAnswer::Found {
            def: Def { key: v.key, kind: DefKind::Other },
            vis: Visibility::Public,
            state: BindingState::Determined,
            origin: Origin::Declared,
        },
        None => NameAnswer::Absent,
    }
}

/// The macros visible at `module`'s `mod` line: each ancestor's textual scope up to the place
/// it marks for the child on the way down, outermost first. A failed ancestor contributes
/// nothing above it; an unfinished one is waited on.
fn entry<'a, N: Name, R: Records<N>>(module: ItemKey, records: &R, alloc: Alloc<'a>) -> Result<AVec<'a, TextualDef<N>>, Lack> {
    // ONE level (bottom.md section 6, scope lead 19): the parent's own definitions before the
    // `mod module` line. The resolver walks the chain one read per level, so this read waits on
    // the parent only, the one module it names.
    let mut out = AVec::new_in(alloc);
    let Some(Some(parent)) = records.parent(module) else { return Ok(out) };
    match records.held(parent) {
        Held::Scope { scope, .. } => {
            let len = scope.mark(module).unwrap_or(0) as usize;
            let textual = scope.textual();
            out.extend_from_slice(&textual[..len.min(textual.len())]);
            Ok(out)
        }
        Held::Failed { .. } => Ok(out),
        Held::Variants { .. } | Held::Pending | Held::Unknown => Err(Lack::Wait(parent)),
    }
}
