//! A crate's attributes given on the command line (rustc's `-Z crate-attr`,
//! [`BuiltCrateData`](crate::BuiltCrateData)'s `crate_attrs`) as files of their own: one per
//! attribute, as rustc parses each on its own (`rustc_builtin_macros/src/cmdline_attrs.rs`),
//! shown as [`CRATE_ATTR_FILE_NAME`] (rustc's `FileName::CliCrateAttr`). A file's text is the
//! attribute with [`CRATE_ATTR_PREFIX`] and [`CRATE_ATTR_SUFFIX`] around it, so the ordinary parse
//! ([`EditionedFileId::parse`], at the crate's edition) reads it as one inner attribute, and what
//! reads an attribute's file (its span, its edition, whether it came from an expansion) reads
//! this one as any other.
//!
//! The files are base-db's. Their IDs come from where a view's new files get theirs
//! ([`Files::unused_file_id`]); setting the crate graph again with the same attributes keeps
//! every file and writes nothing; a changed attribute's file gets its new text; the file of an
//! attribute or a crate that goes is taken out (it has no text, and its ID goes to the next
//! attribute). A file is in its crate root's source root ([`Db::file_source_root`]) but has no
//! path there: no `mod`, `#[path]` or `include!` reaches it, it takes no path of the root, and the
//! setters of the VFS's files refuse it. An attribute rustc refuses has a [`CrateAttrError`]; the
//! others are [`applied_crate_attr_files`], which rustc appends after the crate root's own inner
//! attributes (`krate.attrs.push`).

use blaze_ids::ContentId;
use blaze_mem::{Frame, Tag, frame};
use blaze_query::db::Durability;
use smallvec::SmallVec;
use syntax::{LexedStr, SourceFile, SyntaxKind, T, TextRange, TextSize};
use crate::FileId;

use crate::{
    Crate, Db, EditionedFileId, FileSourceRootInput, FileText, Files, relevant_crates,
    files::{Writer, set},
};

/// The name rustc shows a crate attribute's file by (`FileName::CliCrateAttr`).
pub const CRATE_ATTR_FILE_NAME: &str = "<crate attribute>";
/// What a crate attribute's file has before the attribute as given. rustc lexes and shows the
/// attribute wrapped the same way (`#![<attr>]`), so a column in the file is rustc's column in
/// [`CRATE_ATTR_FILE_NAME`], with nothing to subtract.
pub const CRATE_ATTR_PREFIX: &str = "#![";
/// What a crate attribute's file has after the attribute as given.
pub const CRATE_ATTR_SUFFIX: &str = "]";

/// Why rustc refuses a crate attribute, ranges in its file (rustc's, see
/// [`CRATE_ATTR_PREFIX`]). rustc reports it and the crate does not build; the attribute is not
/// applied. Delimiters are matched over the whole file, the `[` and `]` around the attribute as
/// given among them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CrateAttrError {
    /// The file's syntax errors ([`EditionedFileId::parse_errors`]) are the error, reported as
    /// any file's are: a token rustc's lexer refuses, an attribute rustc's `parse_attribute`
    /// refuses, or one not closed where its meta ends ("expected `]`").
    Syntax,
    /// A closing delimiter that closes nothing: rustc's lexer's "unexpected closing delimiter".
    UnexpectedClose { close: TextRange },
    /// A closing delimiter of another kind than the one open, the `[` around the attribute
    /// included: "mismatched closing delimiter".
    MismatchedClose { open: TextRange, close: TextRange },
    /// A delimiter never closed (the innermost): "this file contains an unclosed delimiter".
    Unclosed { open: TextRange },
    /// A token after an attribute that a `]` written in it closed: rustc's `parse_in`'s
    /// `unexpected`, "unexpected token", at that token.
    UnexpectedToken { token: TextRange },
}

impl Crate {
    /// The files of its crate attributes, one each in the order given: see [`crate::crate_attrs`].
    pub fn crate_attr_files(self, db: &Db) -> &[FileId] {
        self.attr_files_field(db)
    }
}

/// The files of `krate`'s crate attributes at its edition, in the order given, refused or not.
#[blaze_query::tracked(returns(deref))]
pub fn crate_attr_files(db: &Db, krate: Crate) -> Box<[EditionedFileId]> {
    let files = krate.crate_attr_files(db);
    if files.is_empty() {
        return Box::default();
    }
    let edition = krate.data(db).edition;
    // alloc: the memo's value, once per change of the crate's attributes.
    files.iter().map(|&file| EditionedFileId::new(db, file, edition)).collect()
}

/// The files of `krate`'s crate attributes rustc applies, in the order given: those without a
/// [`crate_attr_error`]. They come after the crate root's own inner attributes.
#[blaze_query::tracked(returns(deref))]
pub fn applied_crate_attr_files(db: &Db, krate: Crate) -> Box<[EditionedFileId]> {
    let files = crate_attr_files(db, krate);
    if files.is_empty() {
        return Box::default();
    }
    // alloc: the memo's value, once per change of the crate's attributes.
    files.iter().copied().filter(|&file| crate_attr_error(db, file).is_none()).collect()
}

/// The crate whose crate attribute `file` holds, if it holds one.
pub fn crate_attr_owner(db: &Db, file: FileId) -> Option<Crate> {
    let has = |krate: &Crate| krate.crate_attr_files(db).contains(&file);
    relevant_crates(db, file).iter().copied().find(has)
}

/// Why rustc refuses the crate attribute of `file`, if it does: rustc's rule
/// (`rustc_builtin_macros/src/cmdline_attrs.rs` `inject`) read off the file. rustc lexes the
/// wrapped text `#![<attr>]` into token trees (a lexer error or an unbalanced delimiter stops
/// it), parses one inner attribute off them (`parse_attribute`) and refuses any token after it
/// (`unexpected`).
#[blaze_query::tracked(returns(copy))]
pub fn crate_attr_error(db: &Db, file: EditionedFileId) -> Option<CrateAttrError> {
    let (file_id, edition) = file.unpack(db);
    let text: &str = db.file_text(file_id).text(db);
    // alloc: the attribute's tokens, once per change of a crate attribute (a few bytes each)
    let lexed = LexedStr::new(edition, text);
    if lexed.errors().next().is_some() {
        return Some(CrateAttrError::Syntax);
    }
    let mut open: SmallVec<[(SyntaxKind, TextRange); 8]> = SmallVec::new();
    let mut attr_close = None;
    for at in (0..lexed.len()).filter(|&at| !lexed.kind(at).is_trivia()) {
        let (kind, range) = (lexed.kind(at), range_of(&lexed, at));
        match kind {
            T!['('] | T!['['] | T!['{'] => open.push((kind, range)),
            T![')'] | T![']'] | T!['}'] => match open.pop() {
                None => return Some(CrateAttrError::UnexpectedClose { close: range }),
                Some((opened, _)) if closer(opened) == kind => {
                    if open.is_empty() {
                        attr_close.get_or_insert(at);
                    }
                }
                Some((_, opened)) => {
                    return Some(CrateAttrError::MismatchedClose { open: opened, close: range });
                }
            },
            _ => {}
        }
    }
    if let Some(&(_, opened)) = open.last() {
        return Some(CrateAttrError::Unclosed { open: opened });
    }
    let attr_close = attr_close.expect("a crate attribute's file closes the `[` of its prefix");
    if lexed.text_start(attr_close) < text.len() - CRATE_ATTR_SUFFIX.len() {
        let attr_end = lexed.text_range(attr_close).end;
        // alloc: a parse of the attribute up to its `]`, only for an attribute rustc refuses
        if !SourceFile::parse(&text[..attr_end], edition).errors().is_empty() {
            return Some(CrateAttrError::Syntax);
        }
        let after = (attr_close + 1..lexed.len())
            .find(|&at| !lexed.kind(at).is_trivia())
            .expect("the suffix's `]` comes after a `]` written in the attribute");
        return Some(CrateAttrError::UnexpectedToken { token: range_of(&lexed, after) });
    }
    file.parse_errors_with_bodies(db).is_some().then_some(CrateAttrError::Syntax)
}

fn closer(open: SyntaxKind) -> SyntaxKind {
    match open {
        T!['('] => T![')'],
        T!['['] => T![']'],
        _ => T!['}'],
    }
}

fn offset(at: usize) -> TextSize {
    TextSize::from(u32::try_from(at).expect("a crate attribute of 4 GiB or more"))
}

fn range_of(lexed: &LexedStr<'_>, at: usize) -> TextRange {
    let range = lexed.text_range(at);
    TextRange::new(offset(range.start), offset(range.end))
}

/// Whether `attr` is as a crate's data's `crate_attrs` holds one:
/// the attribute as given with the prefix and the suffix around it.
fn is_wrapped(attr: &str) -> bool {
    attr.len() >= CRATE_ATTR_PREFIX.len() + CRATE_ATTR_SUFFIX.len()
        && attr.starts_with(CRATE_ATTR_PREFIX)
        && attr.ends_with(CRATE_ATTR_SUFFIX)
}

impl Files {
    /// Gives `krate` one file per crate attribute its data has, in order, in the source root of
    /// its root file, and takes out the files of the attributes it no longer has (all of them
    /// when it is `gone` from the crate graph): see the module. A file whose text is an
    /// attribute still given keeps it, so an equal set of attributes writes nothing, and one
    /// moved among them keeps its file.
    pub(crate) fn set_crate_attr_files(
        &self,
        db: &mut Db,
        krate: Crate,
        gone: bool,
        durability: Option<Durability>,
    ) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            let data = krate.data(&*db);
            let attrs: &[Box<str>] = if gone { &[] } else { &data.crate_attrs };
            let old = frame.alloc_slice_copy(krate.crate_attr_files(&*db));
            if attrs.is_empty() && old.is_empty() {
                return;
            }
            for attr in attrs {
                assert!(
                    is_wrapped(attr),
                    "the crate attribute {attr:?} of {krate:?} is not given with \
                     {CRATE_ATTR_PREFIX:?} and {CRATE_ATTR_SUFFIX:?} around it"
                );
            }
            let plan = self.plan_crate_attrs(&mut writer, &*db, attrs, old, frame);
            let root = match attrs.is_empty() {
                true => None,
                false => Some(self.file_source_root(&*db, data.root_file_id).source_root_id(&*db)),
            };

            for &(at, file) in &plan.rewrite {
                let attr = &*krate.data(&*db).crate_attrs[at];
                let id = ContentId::of(blaze_vfs::FILE_DOMAIN, attr.as_bytes());
                let content = FileText::content_of_file(id, None);
                let text = syntax::Text::from(attr);
                self.write_text(&mut writer, db, file, Some((text, id, content)), durability);
            }
            if let Some(root) = root {
                for &file in plan.files.iter() {
                    match self.file_source_roots.get(file.index()) {
                        Some(input) if input.source_root_id(&*db) == root => {}
                        Some(input) => set(input.set_source_root_id(db), durability, root),
                        None => {
                            let builder = FileSourceRootInput::builder(root)
                                .durability(durability.unwrap_or(Durability::MEDIUM));
                            self.file_source_roots.set(file.index(), Some(builder.new(&*db)));
                        }
                    }
                    writer.crate_attrs.insert(file, krate);
                }
            }
            for &file in plan.gone.iter() {
                self.write_text(&mut writer, db, file, None, durability);
                writer.crate_attrs.remove(file);
                writer.free_crate_attrs.push(file);
            }
            if *plan.files != *old {
                // alloc: the crate's list of files, once per change of its attributes (setup).
                set(krate.set_attr_files_field(db), durability, Box::from(&*plan.files));
            }
        })
    }

    /// Which file each of `attrs` gets (an old one whose text it is, else an old one left over,
    /// a file taken out before, or an unused ID), which of them get their text written, and
    /// which old files are left over: reads only, but for taking IDs.
    fn plan_crate_attrs<'f>(
        &self,
        writer: &mut Writer,
        db: &Db,
        attrs: &[Box<str>],
        old: &[FileId],
        frame: &'f Frame,
    ) -> CrateAttrPlan<'f> {
        let kept = frame.alloc_slice_fill_with(old.len(), |_| false);
        let files = frame.alloc_slice_fill_with(attrs.len(), |at| {
            let has = |file: FileId| {
                self.text_input(file).is_some_and(|input| **input.text(db) == *attrs[at])
            };
            let found = (0..old.len()).find(|&slot| !kept[slot] && has(old[slot]))?;
            kept[found] = true;
            Some(old[found])
        });
        let mut spare = (0..old.len()).filter(|&slot| !kept[slot]).map(|slot| old[slot]);
        let mut rewrite = Vec::new_in(frame);
        for (at, file) in files.iter_mut().enumerate() {
            if file.is_some() {
                continue;
            }
            let given = spare
                .next()
                .or_else(|| writer.free_crate_attrs.pop())
                .or_else(|| {
                    let mut next = writer.next_file;
                    let id = self.unused_file_id(writer, &mut next);
                    writer.next_file = next;
                    id
                })
                .unwrap_or_else(|| {
                    panic!("no file ID is left for the crate attribute {:?}", attrs[at])
                });
            *file = Some(given);
            rewrite.push((at, given));
        }
        let mut gone = Vec::new_in(frame);
        gone.extend(spare);
        let files = frame
            .alloc_slice_from_iter(files.iter().map(|it| it.expect("every attribute has a file")));
        CrateAttrPlan { files, rewrite, gone }
    }

    /// Refuses a setter of the VFS's files that names a crate attribute's file: its ID was
    /// handed out by [`Files::unused_file_id`] while the caller's came from elsewhere.
    pub(crate) fn refuse_crate_attr(&self, writer: &Writer, file: FileId) {
        if let Some(krate) = writer.crate_attrs.get(file) {
            panic!(
                "{file:?} is the file of a crate attribute of {krate:?}, not one of the VFS: \
                 file IDs of new files must come from base-db (`Db::new_file_id`)"
            );
        }
    }
}

/// What [`Files::plan_crate_attrs`] found.
struct CrateAttrPlan<'f> {
    /// Each attribute's file, in order.
    files: &'f [FileId],
    /// The attributes (by their place) whose file is to be given their text.
    rewrite: Vec<(usize, FileId), &'f Frame>,
    /// The old files no attribute has.
    gone: Vec<FileId, &'f Frame>,
}
