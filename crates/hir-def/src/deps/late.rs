//! A crate's late reads, as a `Surface` record's shape asks them ([`super::image_parts`]): each
//! `env!`, `option_env!`, `include!`, `include_str!` and `include_bytes!` whose expansion the
//! crate's image or its def map pages depend on, with the value the expansion saw; and, from
//! the same walk of its macro calls, what of those calls refuses the crate ([`walk`]).
//!
//! **Which calls.** The walk starts from the calls an image is made from
//! ([`super::reads::image_calls`]: the calls the signatures name, and every call of the
//! modules' scopes and of their traits' and impls' assoc items) and goes up from each to the
//! call it is in the expansion of ([`super::reads::each_call`]). A call nested in an eager
//! macro's argument (the `env!` in `include!(concat!(env!("Y"), ".rs"))`) is reached because
//! whoever expands an eager call records every call its argument expands, at any depth, in the
//! same list as the eager call itself (`hir_expand::eager::expand_eager_macro_input_view`'s
//! `eager_calls`): the def map's module scopes (`DeclOp::AddMacroInvoc`,
//! nameres/facts/macros.rs and nameres/facts/collect.rs), a trait's or impl's assoc calls
//! (nameres/assoc.rs) and an expression store's calls (expr_store/lower.rs, what a signature
//! names). So the walk reads those lists whole, nested calls included; [`Nested::Skip`]
//! drops them, the planted control of a test.
//!
//! **Where it stops.** At item level: the calls of module scopes, assoc items and signatures.
//! A late read inside a body (a function's, a const's or a static's initializer) is in no
//! list here and is the body task's.
//!
//! Never from a log of what ran in the database (`base_db::Files::env_read`,
//! `Files::included_by`): each read is worked out from the call itself, so the list is a
//! function of this crate's expansions alone.

use std::alloc::Global;

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use hir_expand::{
    HirFileId, MacroCallId, MacroCallKind, MacroDefKind,
    builtin::{EagerExpander, include_input_to_file_id},
    proc_macro::ExpansionReads,
};

use crate::{
    Lookup,
    deps::{
        image::{Part, Portable},
        serve::{Refused, call_site, shown_path},
    },
};

/// `blaze_keyed::kind::Late::Env as u8`. These five are `blaze_keyed::kind::Late`'s bytes
/// (hir-def does not depend on blaze-keyed): a test in blaze-front, which depends on both,
/// holds them equal.
pub const LATE_ENV: u8 = 1;
/// `blaze_keyed::kind::Late::OptionEnv as u8`.
pub const LATE_OPTION_ENV: u8 = 2;
/// `blaze_keyed::kind::Late::Include as u8`.
pub const LATE_INCLUDE: u8 = 3;
/// `blaze_keyed::kind::Late::IncludeStr as u8`.
pub const LATE_INCLUDE_STR: u8 = 4;
/// `blaze_keyed::kind::Late::IncludeBytes as u8`.
pub const LATE_INCLUDE_BYTES: u8 = 5;

/// What [`super::image_parts`] gives: the image's parts, in the order of [`Part::ALL`], and
/// its late reads, sorted by `(which, name)`, each once.
pub struct ImageParts {
    pub parts: [Portable; Part::ALL.len()],
    pub late: Vec<LateSeen, Global>,
}

/// One late read an expansion of the crate made, and the value it saw.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LateSeen {
    /// Which macro: one of [`LATE_ENV`], [`LATE_OPTION_ENV`], [`LATE_INCLUDE`],
    /// [`LATE_INCLUDE_STR`], [`LATE_INCLUDE_BYTES`].
    pub which: u8,
    /// A variable's name; an included file's path in its source root, relative to the
    /// directory of the crate's root file, `/`-separated, with no leading `/` (two files'
    /// `include_str!("a.txt")` name two files, so the literal is not the name).
    pub name: Box<[u8]>,
    pub value: LateValue,
}

/// The value a late read saw.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LateValue {
    /// What `base_db::env_var` gave the expansion (the crate's own variable,
    /// `CARGO_PRIMARY_PACKAGE`, else the process's); `None` when unset.
    Var(Option<Box<str>>),
    /// The blaze-vfs content ID of the included file's bytes (`blaze_vfs::FILE_DOMAIN`: its
    /// text's, or for a file that is not UTF-8 the bytes `include_bytes!` reads): what the host
    /// answers a file ask with (`blaze_host::outside::file_answer`). Never
    /// `base_db::Db::file_content`, which is of the text and its appended root module in
    /// base-db's own domain (`FileText::DOMAIN`), or of the bytes in `IncludedFile::DOMAIN`.
    File(ContentId),
}

/// Whether [`walk`] reads the calls nested in eager macros' arguments: always, but for a
/// test's planted control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Nested {
    Walk,
    /// The calls in an eager macro's argument dropped: the bug a test plants.
    #[cfg_attr(not(test), expect(dead_code, reason = "the planted control of a test alone"))]
    Skip,
}

/// What [`walk`] found besides its refusals.
pub(super) struct Walked {
    /// The late reads, sorted by `(which, name)`, each once.
    pub(super) late: Vec<LateSeen, Global>,
    /// The first input of the image a surface's shape cannot ask ([`Refused::Unasked`]): an
    /// expansion of a procedural macro that traced reading something, or an `include!` of a
    /// file outside the VFS. A store files both by their values ([`super::Reads`]) and so
    /// ignores this; [`super::image_parts`] refuses by it.
    pub(super) unasked: Option<Refused>,
}

/// One walk of the macro calls an image of `krate` is made from (`calls`: those its
/// signatures name; see the module doc for the rest and for how nested calls are reached):
/// its late reads, the first input no surface can ask, or the refusal of the crate that a
/// call makes, by its own variant:
///
/// - `Refused::OutsideSourceRoot`: an `include!`, `include_str!` or `include_bytes!` of a file
///   of the VFS in another source root, or of a path no source root has (as the store
///   refused it before: the key covers only the source root's files).
/// - `Refused::ProcMacro`: an expansion of a procedural macro whose reads are not known.
/// - `Refused::LateTwice`: one late read seen with two values. It cannot happen in one
///   snapshot (a variable's value is `base_db::env_var` of the crate and the name, a file's
///   is its content in the snapshot), so it says an input moved under the walk.
///
/// Floor: each call visited once. Here: that, the visited set and the list of calls to visit
/// ([`super::reads::each_call`]); for a crate with no late read, nothing allocated for the
/// list ([`Vec::new_in`] holds no memory until a push), and no path worked out. Each `env!`
/// reads one variable, each `include!` resolves its path once (the expansion's own rule,
/// `hir_expand::builtin::include_input_to_file_id`); the reads are sorted once.
pub(super) fn walk(
    db: &Db,
    krate: Crate,
    calls: &[MacroCallId],
    nested: Nested,
) -> Result<Walked, Refused> {
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    let mut todo = super::reads::image_calls(db, krate, calls);
    if nested == Nested::Skip {
        todo.retain(|&call| !in_eager_argument(db, call));
    }
    let mut late: Vec<LateSeen, Global> = Vec::new_in(Global);
    let mut unasked = None;
    // Worked out at the first include of the crate's source root: the directory a file's
    // name is relative to.
    let mut dir: Option<Option<String>> = None;
    super::reads::each_call(db, krate, todo, |call, loc| {
        let expander = match &loc.def.kind {
            MacroDefKind::ProcMacro(..) => {
                return match call.expansion_reads(db) {
                    ExpansionReads::Nothing => Ok(()),
                    ExpansionReads::Traced { .. } => {
                        if unasked.is_none() || super::serve::census_taken() {
                            let why = Refused::Unasked {
                                // alloc: why a crate is refused, once per refused crate
                                what: format!(
                                    "procedural macro {} traced reading names its call does not",
                                    super::reads::named_call(db, call, loc)
                                ),
                            };
                            if let Some(why) = super::serve::ends(why) {
                                unasked.get_or_insert(why);
                            }
                        }
                        Ok(())
                    }
                    &ExpansionReads::Unknown(why) => super::serve::end(Refused::ProcMacro {
                        call: super::reads::named_call(db, call, loc),
                        why,
                    }),
                };
            }
            MacroDefKind::BuiltInEager(_, expander) => *expander,
            _ => return Ok(()),
        };
        // The argument of an eager macro, expanded as a call of its own (`eager: None`), is
        // the parent of the calls in it; the call it is the argument of is walked too.
        let MacroCallKind::FnLike { eager: Some(eager), .. } = &loc.kind else { return Ok(()) };
        let (which, macro_name) = match expander {
            EagerExpander::Env => (LATE_ENV, "env!"),
            EagerExpander::OptionEnv => (LATE_OPTION_ENV, "option_env!"),
            EagerExpander::Include => (LATE_INCLUDE, "include!"),
            EagerExpander::IncludeStr => (LATE_INCLUDE_STR, "include_str!"),
            EagerExpander::IncludeBytes => (LATE_INCLUDE_BYTES, "include_bytes!"),
            _ => return Ok(()),
        };
        if expander.is_env_or_option_env() {
            // An argument that names no variable is an error of the call, which reads none.
            let Some(name) = expander.env_var_read(db, call, &eager.arg) else { return Ok(()) };
            let value = base_db::env_var(db, krate, name.as_str()).map(String::into_boxed_str);
            late.push(LateSeen { which, name: name.as_str().as_bytes().into(), value: LateValue::Var(value) });
            return Ok(());
        }
        let file = match include_input_to_file_id(db, call, &eager.arg) {
            Ok(file) => file.file_id(db),
            Err(why) => {
                // alloc: why a crate is refused, once per refused crate
                let what = format!(
                    "`{macro_name}` of crate {} at {} names a file no source root has ({})",
                    krate.named(db),
                    call_site(db, loc),
                    why.kind().render_to_string(db).message
                );
                // Under a census, read as no late read: the include as empty.
                return super::serve::end(Refused::OutsideSourceRoot { what });
            }
        };
        if db.file_source_root(file).source_root_id(db) != root {
            // alloc: as above
            let what = format!(
                "`{macro_name}` of crate {} at {} reads {}",
                krate.named(db),
                call_site(db, loc),
                shown_path(db, file)
            );
            if db.files().included_source_path(file).is_some() {
                // A file outside the VFS: a store files the crate by its content; no fact a
                // surface's shape asks names it.
                if unasked.is_none() || super::serve::census_taken() {
                    // alloc: as above
                    let why = Refused::Unasked { what: format!("{what}, outside the VFS") };
                    if let Some(why) = super::serve::ends(why) {
                        unasked.get_or_insert(why);
                    }
                }
                return Ok(());
            }
            return super::serve::end(Refused::OutsideSourceRoot { what });
        }
        let dir = dir.get_or_insert_with(|| crate_dir(db, krate));
        let Some(name) = file_name(db, root, file, dir.as_deref()) else {
            // alloc: as above
            let what = format!(
                "`{macro_name}` of crate {} at {} reads a file of its source root with no path",
                krate.named(db),
                call_site(db, loc)
            );
            return super::serve::end(Refused::OutsideSourceRoot { what });
        };
        let Some(content) = vfs_content(db, file) else {
            // alloc: as above
            let what = format!(
                "`{macro_name}` of crate {} at {} reads {}, a file of its source root with neither text nor bytes",
                krate.named(db),
                call_site(db, loc),
                shown_path(db, file)
            );
            return super::serve::end(Refused::OutsideSourceRoot { what });
        };
        late.push(LateSeen { which, name, value: LateValue::File(content) });
        Ok(())
    })?;
    settle(&mut late, db, krate)?;
    Ok(Walked { late, unasked })
}

/// The blaze-vfs content ID of `file`'s bytes ([`LateValue::File`]): of its text, else of the
/// bytes of a file that is not UTF-8; `None` for a file with neither (an alias of a file
/// without text).
///
/// Floor: base-db holds this ID already (each text's `Reparse` slot keeps it); with no public
/// reader of it the bytes are hashed again here, once per included file per walk.
fn vfs_content(db: &Db, file: base_db::FileId) -> Option<ContentId> {
    if db.has_file_text(file) {
        return Some(ContentId::of(blaze_vfs::FILE_DOMAIN, db.file_text(file).text(db).as_bytes()));
    }
    let bytes = db.raw_file(file)?.bytes(db)?;
    Some(ContentId::of(blaze_vfs::FILE_DOMAIN, bytes))
}

/// `late` sorted by `(which, name)` with each read once: the same read twice is one, and one
/// name with two values is [`Refused::LateTwice`], naming it.
fn settle(late: &mut Vec<LateSeen, Global>, db: &Db, krate: Crate) -> Result<(), Refused> {
    late.sort_unstable();
    late.dedup();
    let Some(pair) = late.windows(2).find(|pair| (pair[0].which, &pair[0].name) == (pair[1].which, &pair[1].name))
    else {
        return Ok(());
    };
    // alloc: why a crate is refused, once per refused crate
    let what = format!(
        "crate {}: late read {} of `{}` saw {:?} and {:?}",
        krate.named(db),
        pair[0].which,
        String::from_utf8_lossy(&pair[0].name),
        pair[0].value,
        pair[1].value
    );
    // Under a census the two values stay: the census reports, it never builds.
    super::serve::end(Refused::LateTwice { what })
}

/// The directory of `krate`'s root file in its source root, which an included file's name is
/// relative to (as the crate's key writes its files, `super::key`).
fn crate_dir(db: &Db, krate: Crate) -> Option<String> {
    let file = krate.data(db).root_file_id;
    let root = db.file_source_root(file).source_root_id(db);
    let path = db.source_root(root).source_root_ref(db).path_for_file(&file)?.to_string();
    // alloc: once per crate with an include of its source root
    Some(path.rsplit_once('/')?.0.to_owned())
}

/// The name of `file`, a file of the source root `root`: its path relative to `dir`
/// (`crate::nameres::frozen::relative`: `/`-separated, `..` where it is not below `dir`, no
/// leading `/`), or its path with its leading `/` dropped when the crate's root file has no
/// directory. `None` for a file the source root has no path for.
fn file_name(db: &Db, root: base_db::SourceRootId, file: base_db::FileId, dir: Option<&str>) -> Option<Box<[u8]>> {
    let path = db.source_root(root).source_root_ref(db).path_for_file(&file)?.to_string();
    let name = match dir {
        Some(dir) => crate::nameres::frozen::relative(dir, &path),
        None => path.trim_start_matches('/').to_owned(),
    };
    Some(name.into_bytes().into_boxed_slice())
}

/// Whether `call` is a call in an eager macro's argument: its file is the expansion of the
/// argument's own call (an eager macro's `FnLike` call with `eager: None`).
fn in_eager_argument(db: &Db, call: MacroCallId) -> bool {
    let HirFileId::MacroFile(parent) = call.lookup(db).kind.file_id() else { return false };
    let parent = parent.lookup(db);
    matches!(parent.def.kind, MacroDefKind::BuiltInEager(..))
        && matches!(parent.kind, MacroCallKind::FnLike { eager: None, .. })
}
