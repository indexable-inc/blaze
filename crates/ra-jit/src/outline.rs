//! Telling an edit to the bodies of functions from one the rest of the program can see.
//!
//! Most edits change what a function does and nothing else. What was compiled from other
//! bodies is then as it was, and only what comes from the edited file needs another look. That
//! holds for a body nothing else sees into: not that of a `const fn`, which other items are
//! evaluated with, nor one whose type is part of its signature (`async fn`, `-> impl Trait`),
//! nor one a macro is handed, nor one that declares items other than statics and constants.
//! Such bodies count as part of what the rest of the program sees.

use std::hash::{Hash, Hasher};

use hir_def::{FunctionId, Lookup, ModuleDefId, expr_store::Body};
use ide_db::{
    RootDatabase,
    base_db::{EditionedFileId, relevant_crates},
};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use syntax::{
    AstNode, Edition, SyntaxKind, SyntaxNode, TextRange, WalkEvent,
    ast::{self, HasAttrs, HasGenericParams, HasName},
};
use vfs::FileId;

use crate::Listed;

/// Attributes that leave the item they are on as it is written.
const INERT: &[&str] = &[
    "allow",
    "cfg",
    "cold",
    "deny",
    "deprecated",
    "doc",
    "expect",
    "forbid",
    "inline",
    "must_use",
    "track_caller",
    "warn",
];

/// Each file as it was when last looked at.
#[derive(Default)]
pub(crate) struct Outlines {
    files: FxHashMap<FileId, Option<Outline>>,
    /// The types each file defines, as [`types`] has them, and those it defined and no longer
    /// does: one that comes back is the same type, and a running program may still have
    /// values of it.
    types: FxHashMap<FileId, FxHashMap<String, u64>>,
    gone: FxHashMap<FileId, FxHashMap<String, u64>>,
    /// What [`copied`] found in each file before its first edit: as rustc compiled it.
    copied: FxHashMap<FileId, Copied>,
    /// The files of the edit about to be applied that it adds functions to, as
    /// [`Outlines::adding`] found them.
    adding: FxHashMap<FileId, Adding>,
}

/// An edit about to be applied that adds functions to a file, or takes them away, and changes
/// nothing else of it but bodies that nothing else sees into.
struct Adding {
    /// Where the new functions are in the new text, or those taken away in the text before.
    functions: FxHashSet<TextRange>,
    /// Of an edit that takes them away: the [`outline`] of the file before it without them,
    /// which the file is to have after it.
    away: Option<u64>,
    /// The crates the file is a module of.
    crates: Vec<ide_db::base_db::Crate>,
    /// The files of those crates.
    files: Vec<FileId>,
}

/// A file's [`outline`], and its [`Signatures`] when it has them.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Outline {
    hash: u64,
    signatures: Option<Signatures>,
}

/// The signatures of the functions of a file that nothing else sees into, and the rest of
/// its [`outline`]: an edit that leaves the rest as it was changed only those signatures and
/// bodies. They are the functions of its modules and of `impl`s of no trait, none of them
/// generic, with a body that declares no items.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Signatures {
    rest: u64,
    /// By the modules, the type of the `impl` and the name: the name and a hash of the
    /// parameters, the return type and the `where` clause.
    of: FxHashMap<String, (String, u64)>,
}

impl Signatures {
    /// The names of the functions whose signatures changed, when nothing else did but those
    /// and bodies.
    fn changed(was: &Signatures, now: &Signatures) -> Option<Vec<String>> {
        if was.rest != now.rest || was.of.len() != now.of.len() {
            return None;
        }
        let mut changed = Vec::new();
        for (key, (name, hash)) in &now.of {
            match was.of.get(key) {
                Some((_, was)) if was == hash => {}
                Some(_) => changed.push(name.clone()),
                None => return None,
            }
        }
        changed.sort_unstable();
        changed.dedup();
        Some(changed)
    }
}

/// What an edit just applied changed of a file.
pub(crate) struct Changed {
    pub(crate) file: FileId,
    /// Whether nothing changed but bodies that nothing else sees into.
    pub(crate) bodies_only: bool,
    /// Whether a function that rustc copies into each crate or instance that calls it is
    /// not as rustc compiled it, or not where: a patch of rustc's executable redirects the
    /// one copy its symbol names.
    pub(crate) copied: bool,
    /// The names of the generic functions of the file's modules that are not as rustc
    /// compiled them, or not where: a patch replaces their copies when it knows each of them.
    pub(crate) generics: Vec<String>,
    /// The names of the `#[inline]` functions that are not generic and are not as rustc
    /// compiled them, or not where: a patch is right when the executable has no copy of them.
    pub(crate) inline: Vec<String>,
    /// When the edit added functions or took them away, changed nothing else but bodies that
    /// nothing else sees into, and is one [`Outlines::adding`] took: the crates the file is a
    /// module of, and their files, whose code is all that can see the functions.
    pub(crate) added: Option<(Vec<ide_db::base_db::Crate>, Vec<FileId>)>,
    /// When the edit changed nothing but bodies and the signatures of functions that nothing
    /// else sees into ([`Signatures`]): the names of those functions.
    pub(crate) signatures: Vec<String>,
}

impl Outlines {
    /// Takes note of the files that are about to change, unless they are known already.
    pub(crate) fn before_edit(&mut self, db: &RootDatabase, listed: &Listed, files: &[FileId]) {
        for &file in files {
            self.types.entry(file).or_insert_with(|| types(db, file));
            self.copied.entry(file).or_insert_with(|| copied(db, file));
        }
        let new: Vec<_> = files.iter().copied().filter(|it| !self.files.contains_key(it)).collect();
        let outlines = outlines(db, listed, &new);
        self.files.extend(new.into_iter().zip(outlines));
    }

    /// The files of the crates of each file the edit about to give them `texts` adds functions
    /// to, when it changes nothing else of it but bodies that nothing else sees into, no other
    /// crate depends on those, and no file of them names any of the new functions yet. Only
    /// their code sees the new functions, and only what names one can mean another thing now:
    /// what rustc compiled of every file of them is to be taken in before the edit. So too of
    /// an edit that takes such functions away, which no file of them names after it.
    pub(crate) fn adding(
        &mut self,
        db: &RootDatabase,
        listed: &Listed,
        texts: &[(FileId, String)],
    ) -> Vec<FileId> {
        self.adding.clear();
        let mut homed = Vec::new();
        for (file, text) in texts {
            if let Some(adding) = adding(db, listed, *file, text) {
                homed.extend(&adding.files);
                self.adding.insert(*file, adding);
            }
        }
        homed
    }

    /// The types that the files of the edit just applied define otherwise than they did, or
    /// than before they took them away: a running program has values laid out as they were,
    /// which code of the new definition misreads. A type added under a name of its own, or
    /// taken away, is no change: no value the program has is of it, or code that reads one
    /// is gone with it.
    fn types_changed(&mut self, db: &RootDatabase, files: &[FileId]) -> Vec<String> {
        let mut changed = Vec::new();
        for &file in files {
            let now = types(db, file);
            let Some(was) = self.types.insert(file, now) else { continue };
            let now = &self.types[&file];
            let gone = self.gone.entry(file).or_default();
            for (name, hash) in &was {
                match now.get(name) {
                    Some(now) if now != hash => changed.push(name.clone()),
                    Some(_) => {}
                    None => _ = gone.insert(name.clone(), *hash),
                }
            }
            for (name, hash) in now.iter().filter(|it| !was.contains_key(it.0)) {
                if gone.remove(name).is_some_and(|it| it != *hash) {
                    changed.push(name.clone());
                }
            }
        }
        changed.sort_unstable();
        changed
    }

    /// What the edit that was just applied changed of each of its files. Asked after
    /// [`Outlines::before_edit`].
    pub(crate) fn after_edit(
        &mut self,
        db: &RootDatabase,
        listed: &Listed,
        files: Vec<FileId>,
    ) -> Vec<Changed> {
        let off = std::env::var_os("RA_JIT_NO_BODIES").is_some();
        let start = std::time::Instant::now();
        let changed = self.types_changed(db, &files);
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!("types-us: {}", start.elapsed().as_micros());
        }
        if !changed.is_empty() {
            crate::live::types_changed(&changed);
        }
        let outlines = outlines(db, listed, &files);
        let files: Vec<_> = files
            .into_iter()
            .zip(outlines)
            .map(|(file, now)| {
                let was = self.files.insert(file, now.clone());
                let (was, now) = (was.flatten(), now);
                let hash = |it: &Option<Outline>| it.as_ref().map(|it| it.hash);
                let bodies_only = !off && now.is_some() && hash(&was) == hash(&now);
                let added = self.adding.remove(&file).filter(|adding| {
                    !off && !bodies_only
                        && match adding.away {
                            Some(away) => hash(&now) == Some(away),
                            None => {
                                was.is_some()
                                    && outline_without(db, listed, file, &adding.functions)
                                        == hash(&was)
                            }
                        }
                });
                let signatures = match (&was, &now) {
                    _ if off || bodies_only || added.is_some() => None,
                    (Some(was), Some(now)) => was.signatures.as_ref().zip(now.signatures.as_ref()),
                    _ => None,
                };
                let signatures = signatures
                    .and_then(|(was, now)| Signatures::changed(was, now))
                    .unwrap_or_default();
                let (copied, generics, inline) = match self.copied.get(&file) {
                    Some(was) => {
                        let now = copied(db, file);
                        let changed = |was: &FxHashMap<String, u64>, now: &FxHashMap<_, _>| {
                            let mut changed: Vec<String> = (was.keys().chain(now.keys()))
                                .filter(|it| was.get(*it) != now.get(*it))
                                .cloned()
                                .collect();
                            changed.sort_unstable();
                            changed.dedup();
                            changed
                        };
                        let generics = changed(&was.generics, &now.generics);
                        (was.others != now.others, generics, changed(&was.inline, &now.inline))
                    }
                    None => (true, Vec::new(), Vec::new()),
                };
                Changed {
                    file,
                    bodies_only,
                    copied,
                    generics,
                    inline,
                    added: added.map(|it| (it.crates, it.files)),
                    signatures,
                }
            })
            .collect();
        self.adding.clear();
        // An edit to more than bodies can change which functions there are, in any file, and
        // what is kept here was found by asking the ones listed before it.
        if files.iter().any(|it| !it.bodies_only) {
            self.files.clear();
        }
        files
    }
}

/// Hashes of the functions in a file whose code rustc copies into what calls them, and where
/// each starts. A panic in a copy says the line and column rustc compiled it at.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Copied {
    /// Generic functions of the file's modules and of their `impl`s that are not generic, not
    /// `#[inline]`, by name: rustc's symbols name each copy by the types it is of.
    generics: FxHashMap<String, u64>,
    /// `#[inline]` functions that are not generic, of the file's modules and their `impl`s,
    /// by name: rustc copies one into each crate that calls it.
    inline: FxHashMap<String, u64>,
    /// The rest: generic ones through their `impl` or trait, the default bodies of trait
    /// methods, those declared in bodies, and generic or `#[inline(always)]` `#[inline]` ones.
    others: u64,
}

fn copied(db: &RootDatabase, file: FileId) -> Copied {
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    let tree = EditionedFileId::new(db, file, edition).parse(db).syntax_node();
    copied_in(&tree)
}

fn copied_in(tree: &SyntaxNode) -> Copied {
    let generic = |params: Option<ast::GenericParamList>| {
        params.is_some_and(|it| {
            it.generic_params().any(|it| !matches!(it, ast::GenericParam::LifetimeParam(_)))
        })
    };
    let is_copied = |func: &ast::Fn| {
        // A trait's method declared without a body has no code to copy.
        if func.body().is_none() {
            return false;
        }
        let impl_trait_parameter = func.param_list().is_some_and(|it| {
            it.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
        });
        let inline = func.attrs().any(|attr| {
            let text = attr.syntax().text().to_string();
            text.contains("inline") && !text.contains("never")
        });
        let around = func.syntax().ancestors().skip(1).any(|it| {
            ast::Trait::cast(it.clone()).is_some()
                || ast::Impl::cast(it).is_some_and(|it| generic(it.generic_param_list()))
        });
        generic(func.generic_param_list()) || impl_trait_parameter || inline || around
    };
    let inline = |func: &ast::Fn| {
        func.attrs().any(|attr| attr.syntax().text().to_string().contains("inline"))
    };
    // Of the file's modules or an `impl` of them that is not generic, where the function's
    // path names it alone.
    let of_modules = |func: &ast::Fn| {
        let in_modules = func.syntax().ancestors().skip(1).all(|it| match it.kind() {
            SyntaxKind::SOURCE_FILE
            | SyntaxKind::MODULE
            | SyntaxKind::ITEM_LIST
            | SyntaxKind::ASSOC_ITEM_LIST => true,
            SyntaxKind::IMPL => {
                !generic(ast::Impl::cast(it).and_then(|it| it.generic_param_list()))
            }
            _ => false,
        });
        in_modules && !inline(func)
    };
    // Not generic, and not `#[inline(always)]`, which LLVM puts into each caller at every
    // level.
    let inline_alone = |func: &ast::Fn| {
        let generic = generic(func.generic_param_list())
            || func.syntax().ancestors().skip(1).any(|it| {
                ast::Trait::cast(it.clone()).is_some()
                    || ast::Impl::cast(it).is_some_and(|it| generic(it.generic_param_list()))
            })
            || func.param_list().is_some_and(|it| {
                it.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
            });
        let always = func.attrs().any(|attr| attr.syntax().text().to_string().contains("always"));
        let of_items = func.syntax().ancestors().skip(1).all(|it| {
            matches!(
                it.kind(),
                SyntaxKind::SOURCE_FILE
                    | SyntaxKind::MODULE
                    | SyntaxKind::ITEM_LIST
                    | SyntaxKind::IMPL
                    | SyntaxKind::ASSOC_ITEM_LIST
            )
        });
        inline(func) && !generic && !always && of_items
    };
    let text = tree.text().to_string();
    let line_starts: Vec<usize> =
        std::iter::once(0).chain(text.match_indices('\n').map(|(at, _)| at + 1)).collect();
    let mut generics: FxHashMap<String, FxHasher> = FxHashMap::default();
    let mut inlined: FxHashMap<String, FxHasher> = FxHashMap::default();
    let mut others = FxHasher::default();
    for func in tree.descendants().filter_map(ast::Fn::cast).filter(is_copied) {
        let name = func.name().map(|it| it.text().to_string());
        let hasher = match name {
            Some(name) if of_modules(&func) => generics.entry(name).or_default(),
            Some(name) if inline_alone(&func) => inlined.entry(name).or_default(),
            _ => &mut others,
        };
        let start = usize::from(func.syntax().text_range().start());
        let line = line_starts.partition_point(|&it| it <= start) - 1;
        (line, text[line_starts[line]..start].chars().count()).hash(hasher);
        func.syntax().text().to_string().hash(hasher);
    }
    let generics = generics.into_iter().map(|(name, it)| (name, it.finish())).collect();
    let inline = inlined.into_iter().map(|(name, it)| (name, it.finish())).collect();
    Copied { generics, inline, others: others.finish() }
}

/// The [`outline`] of each file. A wide edit is of thousands of files: their bodies are
/// lowered together and the files are hashed on all threads.
fn outlines(db: &RootDatabase, listed: &Listed, files: &[FileId]) -> Vec<Option<Outline>> {
    let stats = std::env::var_os("RA_JIT_STATS").is_some();
    if files.len() > 1 {
        let mut crates: Vec<_> =
            files.iter().flat_map(|&file| relevant_crates(db, file).iter().copied()).collect();
        crates.sort_unstable();
        crates.dedup();
        crate::resolve_names(db, &crates);
    }
    let start = std::time::Instant::now();
    let functions = listed.in_files(db, files);
    let found = start.elapsed();
    let bodies: Vec<_> = functions.iter().flatten().flatten().map(|&it| it.into()).collect();
    let count = bodies.len();
    crate::lower(db, bodies);
    let lowered = start.elapsed();
    let one = |file: FileId, functions: &Option<Vec<FunctionId>>| {
        let Some(functions) = functions else {
            if stats {
                eprintln!("outline: {file:?} has functions that are not listed");
            }
            return None;
        };
        outline(db, listed, file, functions)
    };
    if let ([file], [functions]) = (files, &functions[..]) {
        // One file, the usual edit: hashed here rather than on a thread of its own.
        let outline = hir::attach_db(db, || one(*file, functions));
        if stats {
            let us = |it: std::time::Duration| it.as_micros();
            eprintln!(
                "outlines-us: list={} lower={} bodies={count} hash={}",
                us(found),
                us(lowered - found),
                us(start.elapsed() - lowered)
            );
        }
        return vec![outline];
    }
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut outlines = vec![None; files.len()];
    std::thread::scope(|scope| {
        let parts: Vec<_> = (0..threads.min(files.len()))
            .map(|part| {
                let (db, functions) = (db.clone(), &functions);
                scope.spawn(move || {
                    hir::attach_db(&db, || {
                        let files = files.iter().zip(functions).enumerate();
                        files
                            .skip(part)
                            .step_by(threads)
                            .map(|(at, (&file, functions))| {
                                let Some(functions) = functions else {
                                    if stats {
                                        eprintln!(
                                            "outline: {file:?} has functions that are not listed"
                                        );
                                    }
                                    return (at, None);
                                };
                                (at, outline(&db, listed, file, functions))
                            })
                            .collect::<Vec<_>>()
                    })
                })
            })
            .collect();
        for (at, outline) in parts.into_iter().flat_map(|it| it.join().unwrap_or_default()) {
            outlines[at] = outline;
        }
    });
    outlines
}

/// Each type the file defines, by its kind and name, with a hash of how it is written: the
/// fields a value of it is laid out by and, of a trait, what its objects' tables hold. A
/// default body of a trait method, the layout and comments are left out, and attributes
/// but `repr`.
fn types(db: &RootDatabase, file: FileId) -> FxHashMap<String, u64> {
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    let file = EditionedFileId::new(db, file, edition);
    let tree = file.parse(db).syntax_node();
    let mut found = FxHashMap::<String, u64>::default();
    // Every item has an id, also one in a body: the types are found without walking the file.
    let kinds = [
        SyntaxKind::STRUCT,
        SyntaxKind::ENUM,
        SyntaxKind::UNION,
        SyntaxKind::TYPE_ALIAS,
        SyntaxKind::TRAIT,
    ];
    let items =
        hir::HirFileId::from(file).ast_id_map(db).ptrs().filter(|it| kinds.contains(&it.kind()));
    for node in items.map(|it| it.to_node(&tree)) {
        let (kind, name) = if let Some(it) = ast::Struct::cast(node.clone()) {
            ("struct", it.name())
        } else if let Some(it) = ast::Enum::cast(node.clone()) {
            ("enum", it.name())
        } else if let Some(it) = ast::Union::cast(node.clone()) {
            ("union", it.name())
        } else if let Some(it) = ast::TypeAlias::cast(node.clone()) {
            ("type", it.name())
        } else if let Some(it) = ast::Trait::cast(node.clone()) {
            ("trait", it.name())
        } else {
            continue;
        };
        let Some(name) = name else { continue };
        let bodies: Vec<TextRange> = (node.descendants())
            .filter_map(ast::Fn::cast)
            .filter_map(|it| it.body())
            .map(|it| it.syntax().text_range())
            .collect();
        let mut hasher = FxHasher::default();
        for token in node.descendants_with_tokens().filter_map(|it| it.into_token()) {
            // A doc comment is an attribute, which this parser gives a kind of its own.
            let doc = [SyntaxKind::OUTER_DOC_COMMENT, SyntaxKind::INNER_DOC_COMMENT];
            if token.kind().is_trivia()
                || doc.contains(&token.kind())
                || bodies.iter().any(|it| it.contains_range(token.text_range()))
            {
                continue;
            }
            let attribute = token.parent_ancestors().find_map(ast::Attr::cast);
            if attribute.is_some_and(|it| it.simple_name().is_none_or(|it| it != "repr")) {
                continue;
            }
            token.text().hash(&mut hasher);
        }
        *found.entry(format!("{kind} {name}")).or_default() ^= hasher.finish();
    }
    found
}

/// A hash of everything in the file but the bodies nothing else sees into, or `None` when
/// the file is not read as a file of its own. `functions` are those written in it.
fn outline(
    db: &RootDatabase,
    listed: &Listed,
    file: FileId,
    functions: &[FunctionId],
) -> Option<Outline> {
    let Some(seen) = Written::of(db, functions) else {
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!("outline: {file:?} is included in another file");
        }
        return None;
    };
    // As the crate reads it, so that the file is parsed once for both.
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    let tree = EditionedFileId::new(db, file, edition).parse(db).syntax_node();
    let off = Off::of(db, listed, file);
    let (hash, found, signatures) =
        hash_outside(&tree, &seen.declaring, &FxHashSet::default(), &off, true);
    // Each of them has to be one of the bodies that were kept in.
    (found == seen.declaring.len()).then_some(Outline { hash, signatures })
}

/// The [`outline`] of the file as if the functions at `added` were not written in it, when
/// none of them declares items.
fn outline_without(
    db: &RootDatabase,
    listed: &Listed,
    file: FileId,
    added: &FxHashSet<TextRange>,
) -> Option<u64> {
    let functions = listed.in_files(db, &[file]).pop().flatten()?;
    let seen = hir::attach_db(db, || Written::of(db, &functions))?;
    if seen.declaring.iter().any(|it| added.contains(it)) {
        return None;
    }
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    let tree = EditionedFileId::new(db, file, edition).parse(db).syntax_node();
    let (hash, found) =
        hash_outside_bodies(&tree, &seen.declaring, added, &Off::of(db, listed, file));
    (found == seen.declaring.len()).then_some(hash)
}

/// What [`Outlines::adding`] finds of an edit that gives `file` the text `text`.
fn adding(db: &RootDatabase, listed: &Listed, file: FileId, text: &str) -> Option<Adding> {
    let edition = relevant_crates(db, file).first()?.data(db).edition;
    let was = EditionedFileId::new(db, file, edition).parse(db).syntax_node();
    let now = ast::SourceFile::parse(text, edition).tree().syntax().clone();
    // Functions taken away are those the text before has that the edit added to it, and what
    // names them is looked for in the file as the edit leaves it.
    let (added, away) = match added_functions(&was, &now) {
        Some(added) => (added, false),
        None => (added_functions(&now, &was)?, true),
    };
    let crates = listed.crates_of(db, file);
    if crates.is_empty() || crates.iter().any(|it| it.transitive_rev_deps(db).len() != 1) {
        return None;
    }
    let names: FxHashSet<String> =
        added.iter().filter_map(|it| it.name()).map(|it| it.text().to_string()).collect();
    let mut files: Vec<(FileId, Edition)> = hir::attach_db(db, || {
        let modules = crates.iter().flat_map(|&krate| {
            let edition = krate.data(db).edition;
            hir::Crate::from(krate).modules(db).into_iter().map(move |it| (it, edition))
        });
        modules
            .map(|(module, edition)| {
                (module.definition_source_file_id(db).original_file(db).file_id(db), edition)
            })
            .collect()
    });
    files.sort_unstable();
    files.dedup();
    for &(other, edition) in &files {
        let tree = match other == file && away {
            true => now.clone(),
            false => EditionedFileId::new(db, other, edition).parse(db).syntax_node(),
        };
        // A file another one includes is written as part of it, and the names of macros are
        // matched by name.
        let includes = tree
            .descendants()
            .filter_map(ast::MacroCall::cast)
            .any(|it| it.path().is_some_and(|path| path.syntax().text() == "include"));
        let names_one = tree.descendants_with_tokens().filter_map(|it| it.into_token()).any(|it| {
            it.kind() == SyntaxKind::IDENT && names.contains(it.text().trim_start_matches("r#"))
        });
        if includes || names_one {
            return None;
        }
    }
    let functions = added.iter().map(|it| it.syntax().text_range()).collect();
    let away = match away {
        true => Some(outline_without(db, listed, file, &functions)?),
        false => None,
    };
    Some(Adding { functions, away, crates, files: files.into_iter().map(|it| it.0).collect() })
}

/// The functions written in `now` that `was` has none of, when nothing else changed but bodies
/// that nothing else sees into: functions of the file's modules, each of a name no function of
/// its module had, that rustc compiles once under a symbol of their own.
fn added_functions(was: &SyntaxNode, now: &SyntaxNode) -> Option<Vec<ast::Fn>> {
    let (was_written, now_written) = (functions_of_modules(was)?, functions_of_modules(now)?);
    if was_written.keys().any(|it| !now_written.contains_key(it)) {
        return None;
    }
    let added: Vec<ast::Fn> = (now_written.into_iter())
        .filter(|(key, _)| !was_written.contains_key(key))
        .map(|it| it.1)
        .collect();
    if added.is_empty() || !added.iter().all(added_alone) {
        return None;
    }
    let skipped: FxHashSet<TextRange> = added.iter().map(|it| it.syntax().text_range()).collect();
    let (none, off) = (FxHashSet::default(), Off::default());
    let same = hash_outside_bodies(was, &none, &none, &off).0
        == hash_outside_bodies(now, &none, &skipped, &off).0;
    same.then_some(added)
}

/// The functions of a file's modules by the names of the modules and their own, or `None` when
/// two have one name: a `#[cfg]` tells which is compiled.
fn functions_of_modules(tree: &SyntaxNode) -> Option<FxHashMap<(Vec<String>, String), ast::Fn>> {
    let mut found = FxHashMap::default();
    for func in tree.descendants().filter_map(ast::Fn::cast) {
        let Some(name) = func.name() else { continue };
        let mut modules = Vec::new();
        let of_modules = func.syntax().ancestors().skip(1).all(|it| match it.kind() {
            SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST => true,
            SyntaxKind::MODULE => {
                let name = ast::Module::cast(it).and_then(|it| it.name());
                modules.push(name.map(|it| it.text().to_string()).unwrap_or_default());
                true
            }
            _ => false,
        });
        if !of_modules {
            continue;
        }
        modules.reverse();
        if found.insert((modules, name.text().to_string()), func).is_some() {
            return None;
        }
    }
    Some(found)
}

/// Whether a function added to a file is compiled once, under its own symbol, and seen by
/// nothing but what calls it: not generic, not `#[inline]`, and with a body that nothing else
/// sees into and that declares no items.
fn added_alone(func: &ast::Fn) -> bool {
    let impl_trait_parameter = func.param_list().is_some_and(|it| {
        it.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
    });
    let inline = func.attrs().any(|attr| {
        let text = attr.syntax().text().to_string();
        text.contains("inline") && !text.contains("never")
    });
    let Some(body) = func.body() else { return false };
    let declares = body.syntax().descendants().skip(1).any(|it| {
        ast::Item::can_cast(it.kind())
            && !matches!(
                it.kind(),
                SyntaxKind::MACRO_CALL | SyntaxKind::USE | SyntaxKind::EXTERN_CRATE
            )
    });
    func.generic_param_list().is_none()
        && !impl_trait_parameter
        && !inline
        && !declares
        && is_unseen_body(body.syntax())
}

/// What looking at the functions written in a file tells that how they are written does not.
struct Written {
    /// Where the functions are whose bodies declare items.
    declaring: FxHashSet<TextRange>,
}

impl Written {
    /// `None` when the functions are in a file that `include!` reads: those are written by a
    /// macro as far as we can tell.
    fn of(db: &RootDatabase, functions: &[FunctionId]) -> Option<Written> {
        let mut declaring = FxHashSet::default();
        for &func in functions {
            let at = func.lookup(db).id;
            if at.file_id.is_macro() {
                let included = at.file_id.original_file_respecting_includes(db)
                    != at.file_id.original_file(db);
                if included {
                    return None;
                }
                // What a macro wrote comes from what it was handed, which is no body that
                // is left out.
                continue;
            }
            if declares_items(db, func) {
                declaring.insert(at.to_range(db));
            }
        }
        Some(Written { declaring })
    }
}

fn declares_items(db: &RootDatabase, func: FunctionId) -> bool {
    Body::of(db, func.into()).blocks(db).any(|(_, def_map)| {
        def_map.modules(db).any(|(_, module)| {
            let scope = module.scope(db);
            // A static or constant is the body's own: only its code names it, and neither is
            // a function to list. Macros that log declare them.
            scope
                .declarations()
                .any(|it| !matches!(it, ModuleDefId::StaticId(_) | ModuleDefId::ConstId(_)))
                || scope.impls().len() != 0
                || scope.unnamed_consts().next().is_some()
        })
    })
}

/// A hash of the file but the bodies nothing else sees into, when the functions written in it
/// are what they were whenever it hashes the same: when no such body has an item in it, and
/// the macros that write items are called at the top of the file. The functions are known
/// by ids handed out a level of nesting at a time, kind by kind; an item in a body, or a call
/// of a macro, takes one of the ids of its level that a function or a macro call further on
/// would have had otherwise.
pub(crate) fn skeleton(db: &RootDatabase, file: FileId) -> Option<u64> {
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    skeleton_of(&EditionedFileId::new(db, file, edition).parse(db).syntax_node())
}

fn skeleton_of(tree: &SyntaxNode) -> Option<u64> {
    let written_by_macro = |node: &SyntaxNode| {
        node.kind() == SyntaxKind::MACRO_CALL
            && node.parent().is_some_and(|it| {
                !matches!(
                    it.kind(),
                    SyntaxKind::SOURCE_FILE
                        | SyntaxKind::MACRO_EXPR
                        | SyntaxKind::MACRO_PAT
                        | SyntaxKind::MACRO_TYPE
                )
            })
    };
    // An import's id is of a kind of its own, which no function's is found by.
    let has_items = |body: &SyntaxNode| {
        body.descendants().skip(1).any(|it| {
            ast::Item::can_cast(it.kind())
                && !matches!(
                    it.kind(),
                    SyntaxKind::MACRO_CALL | SyntaxKind::USE | SyntaxKind::EXTERN_CRATE
                )
        })
    };
    for node in tree.descendants() {
        if written_by_macro(&node) || is_unseen_body(&node) && has_items(&node) {
            return None;
        }
    }
    let none = FxHashSet::default();
    Some(hash_outside_bodies(tree, &none, &none, &Off::default()).0)
}

/// What a `#[cfg]` takes out of every crate that is built with the file as a module of it. None
/// of it is compiled, and only the attributes of what it is written on can change that. The
/// crates of the file's source root are more: a package's tests are crates of its own, that
/// have `cfg(test)` and no module of the library's.
#[derive(Default)]
struct Off(Vec<cfg::CfgOptions>);

impl Off {
    fn of(db: &RootDatabase, listed: &Listed, file: FileId) -> Off {
        Off(listed.crates_of(db, file).iter().map(|it| it.cfg_options(db).clone()).collect())
    }

    fn takes_out(&self, node: &SyntaxNode) -> bool {
        if self.0.is_empty() {
            return false;
        }
        let Some(item) = ast::AnyHasAttrs::cast(node.clone()) else { return false };
        item.attrs().any(|attr| match attr.meta() {
            Some(ast::Meta::CfgMeta(meta)) => meta.cfg_predicate().is_some_and(|predicate| {
                let cfg = cfg::CfgExpr::parse_from_ast(predicate);
                self.0.iter().all(|options| options.check(&cfg) == Some(false))
            }),
            _ => false,
        })
    }
}

/// The hash, and how many of the `declaring` functions were come by. The functions at
/// `skipped` are left out whole.
fn hash_outside_bodies(
    file: &SyntaxNode,
    declaring: &FxHashSet<TextRange>,
    skipped: &FxHashSet<TextRange>,
    off: &Off,
) -> (u64, usize) {
    let (hash, found, _) = hash_outside(file, declaring, skipped, off, false);
    (hash, found)
}

/// [`hash_outside_bodies`], and with `signatures` the file's [`Signatures`], or `None` when two
/// of those functions have one key.
fn hash_outside(
    file: &SyntaxNode,
    declaring: &FxHashSet<TextRange>,
    skipped: &FxHashSet<TextRange>,
    off: &Off,
    signatures: bool,
) -> (u64, usize, Option<Signatures>) {
    let mut hasher = FxHasher::default();
    let mut rest = FxHasher::default();
    let mut of: FxHashMap<String, (String, u64)> = FxHashMap::default();
    let mut owners: FxHashMap<String, Option<TextRange>> = FxHashMap::default();
    let mut twice = false;
    // The part of a signature the walk is in, the key of its function and its hash so far.
    let mut signature: Option<(SyntaxNode, String, FxHasher)> = None;
    let mut found = 0;
    let mut walk = file.preorder_with_tokens();
    while let Some(event) = walk.next() {
        match event {
            WalkEvent::Enter(syntax::NodeOrToken::Node(node))
                if node.kind() == SyntaxKind::FN && skipped.contains(&node.text_range()) =>
            {
                walk.skip_subtree();
            }
            WalkEvent::Enter(syntax::NodeOrToken::Node(node)) if off.takes_out(&node) => {
                let into = match &mut signature {
                    Some((_, _, it)) => it,
                    None => &mut rest,
                };
                node.kind().hash(&mut hasher);
                node.kind().hash(into);
                let attrs =
                    ast::AnyHasAttrs::cast(node.clone()).into_iter().flat_map(|it| it.attrs());
                let tokens = attrs.flat_map(|attr| {
                    attr.syntax().descendants_with_tokens().filter_map(|it| it.into_token())
                });
                for token in tokens.filter(|it| !it.kind().is_trivia()) {
                    token.kind().hash(&mut hasher);
                    token.text().hash(&mut hasher);
                    token.kind().hash(into);
                    token.text().hash(into);
                }
                walk.skip_subtree();
            }
            WalkEvent::Enter(syntax::NodeOrToken::Node(node)) => {
                node.kind().hash(&mut hasher);
                let part = matches!(
                    node.kind(),
                    SyntaxKind::PARAM_LIST | SyntaxKind::RET_TYPE | SyntaxKind::WHERE_CLAUSE
                );
                if signatures && signature.is_none() && part {
                    let func = node.parent().and_then(ast::Fn::cast);
                    if let Some(key) = func.and_then(|it| plain_key(&it, declaring)) {
                        signature = Some((node.clone(), key, FxHasher::default()));
                    }
                }
                match &mut signature {
                    Some((_, _, it)) => node.kind().hash(it),
                    None => node.kind().hash(&mut rest),
                }
                if node.kind() == SyntaxKind::FN && declaring.contains(&node.text_range()) {
                    found += 1;
                } else if is_unseen_body(&node)
                    && node.parent().is_none_or(|it| !declaring.contains(&it.text_range()))
                {
                    walk.skip_subtree();
                }
            }
            WalkEvent::Enter(syntax::NodeOrToken::Token(token)) => {
                // Blank space and comments say nothing, except to a macro that is handed them.
                // A doc comment is a token of another kind.
                let blank = matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
                    && token.parent().is_none_or(|it| it.kind() != SyntaxKind::TOKEN_TREE);
                if !blank {
                    token.kind().hash(&mut hasher);
                    token.text().hash(&mut hasher);
                    let into = match &mut signature {
                        Some((_, _, it)) => it,
                        None => &mut rest,
                    };
                    token.kind().hash(into);
                    token.text().hash(into);
                }
            }
            WalkEvent::Leave(syntax::NodeOrToken::Node(node)) => {
                if signature.as_ref().is_some_and(|(part, ..)| *part == node) {
                    let (part, key, hash) = signature.take().unwrap_or_else(|| unreachable!());
                    let func = part.parent().and_then(ast::Fn::cast);
                    let at = func.as_ref().map(|it| it.syntax().text_range());
                    let name = func.and_then(|it| it.name()).map(|it| it.text().to_string());
                    let name = name.unwrap_or_default().trim_start_matches("r#").to_owned();
                    // Two functions of one key are told apart by a `#[cfg]`.
                    twice |= *owners.entry(key.clone()).or_insert(at) != at;
                    // The parts of one function are hashed one after another, in order.
                    let entry = of.entry(key).or_insert_with(|| (name, 0));
                    entry.1 = entry.1.rotate_left(5) ^ hash.finish();
                }
            }
            WalkEvent::Leave(_) => {}
        }
    }
    let signatures = (signatures && !twice).then(|| Signatures { rest: rest.finish(), of });
    (hasher.finish(), found, signatures)
}

/// The key a function has in [`Signatures`], when it is one of them.
fn plain_key(func: &ast::Fn, declaring: &FxHashSet<TextRange>) -> Option<String> {
    let body = func.body()?;
    if !is_unseen_body(body.syntax()) || declaring.contains(&func.syntax().text_range()) {
        return None;
    }
    let impl_trait_parameter = func.param_list().is_some_and(|it| {
        it.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
    });
    if func.generic_param_list().is_some() || impl_trait_parameter {
        return None;
    }
    let mut key = Vec::new();
    for it in func.syntax().ancestors().skip(1) {
        match it.kind() {
            SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::ASSOC_ITEM_LIST => {}
            SyntaxKind::MODULE => key.push(ast::Module::cast(it)?.name()?.text().to_string()),
            SyntaxKind::IMPL => {
                let of = ast::Impl::cast(it)?;
                if of.trait_().is_some() || of.generic_param_list().is_some() {
                    return None;
                }
                let ty = of.self_ty()?;
                let tokens = ty.syntax().descendants_with_tokens().filter_map(|it| it.into_token());
                key.push(format!(
                    "impl {}",
                    tokens
                        .filter(|it| !it.kind().is_trivia())
                        .map(|it| it.text().to_string())
                        .collect::<String>()
                ));
            }
            _ => return None,
        }
    }
    key.reverse();
    key.push(func.name()?.text().to_string());
    Some(key.join("::"))
}

/// Whether the node is the body of a function that nothing else sees into, going by how the
/// function is written. What its body declares takes name resolution to tell.
fn is_unseen_body(node: &SyntaxNode) -> bool {
    if node.kind() != SyntaxKind::BLOCK_EXPR {
        return false;
    }
    let Some(func) = node.parent().and_then(ast::Fn::cast) else { return false };
    if func.const_token().is_some() || func.async_token().is_some() || func.gen_token().is_some() {
        return false;
    }
    let names_its_type = func.ret_type().is_some_and(|it| {
        it.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
    });
    if names_its_type || !is_inert(func.syntax()) {
        return false;
    }
    // rustc copies the body into the callers, also the ones it compiled for us.
    let always_inlined = func.attrs().any(|attr| {
        attr.simple_name().is_some_and(|name| name == "inline")
            && attr.syntax().text().to_string().contains("always")
    });
    if always_inlined {
        return false;
    }
    // An attribute written inside the body is the function's, and a macro declared in it can
    // be exported from the crate.
    let reaches_out = node.descendants().any(|it| match it.kind() {
        SyntaxKind::ATTR => it.ancestors().nth(2).is_some_and(|block| block == *node),
        SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => true,
        _ => false,
    });
    if reaches_out {
        return false;
    }
    // Only items, all the way up: a function in a body is that body's, and one in a macro's
    // input is the macro's.
    func.syntax().ancestors().skip(1).all(|it| match it.kind() {
        SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::ASSOC_ITEM_LIST => true,
        SyntaxKind::MODULE | SyntaxKind::IMPL | SyntaxKind::TRAIT => is_inert(&it),
        _ => false,
    })
}

/// Whether no attribute on the item can be a macro that rewrites it.
fn is_inert(item: &SyntaxNode) -> bool {
    let Some(item) = ast::AnyHasAttrs::cast(item.clone()) else { return false };
    item.attrs().all(|attr| attr.simple_name().is_some_and(|name| INERT.contains(&name.as_str())))
}

/// The crate the file is a module of, when that is one of `used`, which the crates in `built`
/// depend on, and none of `built`: rustc compiled it, and what it compiled is all there is of
/// it.
pub(crate) fn left_to_rustc(
    db: &RootDatabase,
    built: &[ide_db::base_db::Crate],
    used: &FxHashSet<ide_db::base_db::Crate>,
    file: FileId,
) -> Option<ide_db::base_db::Crate> {
    let crates = relevant_crates(db, file);
    if crates.iter().any(|it| built.contains(it)) {
        return None;
    }
    crates.iter().copied().find(|&krate| {
        used.contains(&krate)
            && hir_def::nameres::crate_def_map(db, krate)
                .modules_for_file(db, file)
                .next()
                .is_some()
    })
}

impl Listed {
    /// The crates that are built and those they use, once any is built.
    pub(crate) fn programs(
        &self,
        db: &RootDatabase,
    ) -> Option<std::sync::Arc<FxHashSet<ide_db::base_db::Crate>>> {
        // Copied out: a query that is cancelled unwinds, and would poison the lock.
        let built = crate::lock(&self.built).clone();
        let read: FxHashSet<_> = (built.iter())
            .flat_map(|&it| ide_db::base_db::Crate::from(it).transitive_deps(db))
            .collect();
        (!read.is_empty()).then(|| std::sync::Arc::new(read))
    }

    /// The crates the file is a module of, of those that are built and those they use: no
    /// other is part of a program. Before the first build, of all of them. The crates of a
    /// file's source root are every target of its package, and an example of bevy is one of
    /// hundreds.
    pub(crate) fn crates_of(&self, db: &RootDatabase, file: FileId) -> Vec<ide_db::base_db::Crate> {
        let programs = self.programs(db);
        (relevant_crates(db, file).iter().copied())
            .filter(|krate| programs.as_ref().is_none_or(|it| it.contains(krate)))
            .filter(|&krate| {
                hir_def::nameres::crate_def_map(db, krate)
                    .modules_for_file(db, file)
                    .next()
                    .is_some()
            })
            .collect()
    }

    /// Whether the file is a module of a crate that is built, or of one that such a crate uses.
    /// Any file may be one before a crate is listed.
    pub(crate) fn sees(&self, db: &RootDatabase, file: FileId) -> bool {
        // Before the first build there is no telling.
        self.programs(db).is_none() || !self.crates_of(db, file).is_empty()
    }

    /// For each file the functions written in it that no body declares, as the file is now,
    /// or `None` when it belongs to a crate that is not listed: one that the crates being
    /// built use and that is not one of them.
    pub(crate) fn in_files(
        &self,
        db: &RootDatabase,
        files: &[FileId],
    ) -> Vec<Option<Vec<FunctionId>>> {
        // Asked before the lock is taken: a query that is cancelled unwinds, and would poison it.
        let skeletons: Vec<_> = files.iter().map(|&file| skeleton(db, file)).collect();
        let found: Option<Vec<_>> = {
            let kept = crate::lock(&self.files);
            let mut found = files.iter().zip(&skeletons).map(|(file, &now)| {
                kept.get(file).filter(|it| Some(it.0) == now).map(|it| Some(it.1.clone()))
            });
            found.by_ref().collect()
        };
        if let Some(found) = found {
            return found;
        }
        let found = self.list_in_files(db, files);
        let mut kept = crate::lock(&self.files);
        for ((&file, skeleton), functions) in files.iter().zip(skeletons).zip(&found) {
            if let (Some(skeleton), Some(functions)) = (skeleton, functions) {
                kept.insert(file, (skeleton, functions.clone()));
            }
        }
        found
    }

    fn list_in_files(&self, db: &RootDatabase, files: &[FileId]) -> Vec<Option<Vec<FunctionId>>> {
        type Crate = ide_db::base_db::Crate;
        let listed: Vec<Crate> =
            self.functions.lock().unwrap().keys().map(|&it| it.into()).collect();
        // The functions of a listed crate by the file they are in, and `None` for a crate
        // that the listed ones use.
        let mut crates: FxHashMap<Crate, Option<FxHashMap<FileId, Vec<FunctionId>>>> =
            FxHashMap::default();
        let mut used = None;
        files
            .iter()
            .map(|&file| {
                let mut functions = Vec::new();
                for &krate in relevant_crates(db, file).iter() {
                    let of_crate = crates.entry(krate).or_insert_with(|| {
                        if !listed.contains(&krate) {
                            return None;
                        }
                        let mut by_file: FxHashMap<_, Vec<_>> = FxHashMap::default();
                        for func in crate::functions_of_modules(db, krate.into()) {
                            let file = hir_ty_codegen::body_file(db, func.into());
                            by_file.entry(file).or_default().push(func);
                        }
                        Some(by_file)
                    });
                    match of_crate {
                        Some(by_file) => {
                            functions.extend(by_file.get(&file).into_iter().flatten().copied())
                        }
                        None => {
                            let used = used.get_or_insert_with(|| {
                                let used = listed.iter().flat_map(|it| it.transitive_deps(db));
                                used.collect::<FxHashSet<Crate>>()
                            });
                            if used.contains(&krate) {
                                return None;
                            }
                        }
                    }
                }
                Some(functions)
            })
            .collect()
    }
}

/// Each file as it was when the check for errors last found none in it, or before its first
/// edit, which rustc built: the next check of it looks at the functions that changed since, and
/// at those of the file that name one whose signature changed.
#[derive(Default)]
pub(crate) struct Checked {
    files: FxHashMap<FileId, Option<Functions>>,
}

/// A file by the functions written among its items.
struct Functions {
    /// Everything but those functions, with the keys of the functions where they are.
    rest: u64,
    of: FxHashMap<String, Function>,
    /// What a `#[cfg]` takes out, which names nothing that is compiled.
    off: Vec<TextRange>,
}

struct Function {
    /// All of the function but its body, attributes too.
    signature: u64,
    body: u64,
    at: TextRange,
    name: String,
    /// Not in a trait or an impl of one, whose items are checked against each other.
    alone: bool,
}

impl Checked {
    /// Takes note of the files about to change that are not known yet. Those `edited` since the
    /// program was last built are as no check has seen them, and are checked whole.
    pub(crate) fn before_edit(
        &mut self,
        db: &RootDatabase,
        listed: &Listed,
        files: &[FileId],
        edited: &[FileId],
    ) {
        for &file in files {
            if !self.files.contains_key(&file) {
                let off = Off::of(db, listed, file);
                let functions = match edited.contains(&file) {
                    true => None,
                    false => functions_of(&tree_of(db, file), &off),
                };
                self.files.insert(file, functions);
            }
        }
    }

    /// The files were checked and found to have no error.
    pub(crate) fn passed(&mut self, db: &RootDatabase, listed: &Listed, files: &[FileId]) {
        for &file in files {
            let off = Off::of(db, listed, file);
            self.files.insert(file, functions_of(&tree_of(db, file), &off));
        }
    }

    /// Where the functions are that the check of `file` has to look at, or `None` when it has
    /// to look at all of the file.
    pub(crate) fn to_check(
        &self,
        db: &RootDatabase,
        listed: &Listed,
        file: FileId,
    ) -> Option<Vec<TextRange>> {
        let was = self.files.get(&file)?.as_ref()?;
        let tree = tree_of(db, file);
        let now = functions_of(&tree, &Off::of(db, listed, file))?;
        asked_of(was, &tree, &now)
    }
}

/// Where the functions of `tree` are that changed since `was`, and those that name one whose
/// signature changed, or `None` when anything else changed.
fn asked_of(was: &Functions, tree: &SyntaxNode, now: &Functions) -> Option<Vec<TextRange>> {
    if was.rest != now.rest || was.of.len() != now.of.len() {
        return None;
    }
    let (mut asked, mut named) = (Vec::new(), FxHashSet::default());
    for (key, it) in &now.of {
        let before = was.of.get(key)?;
        if before.signature != it.signature {
            if !it.alone {
                return None;
            }
            named.insert(it.name.as_str());
        }
        if before.signature != it.signature || before.body != it.body {
            asked.push(it.at);
        }
    }
    if named.is_empty() {
        return Some(asked);
    }
    // What names a function whose signature changed is to be checked again: a function
    // alone, or all of the file when something else does.
    let names = |token: &syntax::SyntaxToken| {
        token.kind() == SyntaxKind::IDENT && named.contains(token.text().trim_start_matches("r#"))
    };
    let tokens = tree.descendants_with_tokens().filter_map(|it| it.into_token());
    for token in tokens.filter(names) {
        let at = token.text_range();
        if now.off.iter().any(|it| it.contains_range(at)) {
            continue;
        }
        let func = now.of.values().find(|it| it.at.contains_range(at))?;
        if !asked.contains(&func.at) {
            asked.push(func.at);
        }
    }
    Some(asked)
}

fn tree_of(db: &RootDatabase, file: FileId) -> SyntaxNode {
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    EditionedFileId::new(db, file, edition).parse(db).syntax_node()
}

/// The file by its functions, or `None` when two of them have one key, or one has none.
fn functions_of(tree: &SyntaxNode, off: &Off) -> Option<Functions> {
    let (mut rest, mut of, mut taken_out) = (FxHasher::default(), FxHashMap::default(), vec![]);
    let mut walk = tree.preorder_with_tokens();
    while let Some(event) = walk.next() {
        match event {
            WalkEvent::Enter(syntax::NodeOrToken::Node(node)) => {
                node.kind().hash(&mut rest);
                if off.takes_out(&node) {
                    taken_out.push(node.text_range());
                    let attrs = ast::AnyHasAttrs::cast(node).into_iter().flat_map(|it| it.attrs());
                    for attr in attrs {
                        hash_tokens(attr.syntax(), None, &mut rest);
                    }
                    walk.skip_subtree();
                    continue;
                }
                let Some(func) = ast::Fn::cast(node).filter(among_items) else { continue };
                let (key, alone) = key_of(&func)?;
                key.hash(&mut rest);
                let body = func.body().map(|it| it.syntax().text_range());
                let (mut signature, mut hashed) = (FxHasher::default(), FxHasher::default());
                hash_tokens(func.syntax(), body, &mut signature);
                if let Some(body) = func.body() {
                    hash_tokens(body.syntax(), None, &mut hashed);
                }
                let function = Function {
                    signature: signature.finish(),
                    body: hashed.finish(),
                    at: func.syntax().text_range(),
                    name: func.name()?.text().trim_start_matches("r#").to_owned(),
                    alone,
                };
                if of.insert(key, function).is_some() {
                    return None;
                }
                walk.skip_subtree();
            }
            WalkEvent::Enter(syntax::NodeOrToken::Token(token)) if !token.kind().is_trivia() => {
                token.kind().hash(&mut rest);
                token.text().hash(&mut rest);
            }
            _ => {}
        }
    }
    Some(Functions { rest: rest.finish(), of, off: taken_out })
}

/// Hashes the tokens of `node` that say something, but those in `but`.
fn hash_tokens(node: &SyntaxNode, but: Option<TextRange>, into: &mut FxHasher) {
    let tokens = node.descendants_with_tokens().filter_map(|it| it.into_token());
    for token in tokens.filter(|it| !it.kind().is_trivia()) {
        if but.is_some_and(|it| it.contains_range(token.text_range())) {
            continue;
        }
        token.kind().hash(into);
        token.text().hash(into);
    }
}

/// Whether the function is an item of a module, a trait or an impl, not of a body.
fn among_items(func: &ast::Fn) -> bool {
    func.syntax().ancestors().skip(1).all(|it| {
        matches!(
            it.kind(),
            SyntaxKind::SOURCE_FILE
                | SyntaxKind::ITEM_LIST
                | SyntaxKind::ASSOC_ITEM_LIST
                | SyntaxKind::MODULE
                | SyntaxKind::IMPL
                | SyntaxKind::TRAIT
        )
    })
}

/// The function's modules, impl or trait and name, and whether it is alone: in no trait or
/// impl of one.
fn key_of(func: &ast::Fn) -> Option<(String, bool)> {
    let mut key = vec![func.name()?.text().to_string()];
    let mut alone = true;
    for it in func.syntax().ancestors().skip(1) {
        match it.kind() {
            SyntaxKind::MODULE => key.push(ast::Module::cast(it)?.name()?.text().to_string()),
            SyntaxKind::TRAIT => {
                alone = false;
                key.push(format!("trait {}", ast::Trait::cast(it)?.name()?.text()));
            }
            SyntaxKind::IMPL => {
                let of = ast::Impl::cast(it)?;
                alone &= of.trait_().is_none();
                // The impl by what is written before its items.
                let items = of.assoc_item_list().map(|it| it.syntax().text_range());
                let mut header = String::from("impl");
                let tokens = of.syntax().descendants_with_tokens().filter_map(|it| it.into_token());
                for token in tokens.filter(|it| !it.kind().is_trivia()) {
                    if items.is_some_and(|it| it.contains_range(token.text_range())) {
                        break;
                    }
                    header.push(' ');
                    header.push_str(token.text());
                }
                key.push(header);
            }
            _ => {}
        }
    }
    key.reverse();
    Some((key.join("::"), alone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::SourceFile;

    fn hash(text: &str) -> u64 {
        let tree = SourceFile::parse(text, Edition::CURRENT).syntax_node();
        hash_outside_bodies(&tree, &FxHashSet::default(), &FxHashSet::default(), &Off::default()).0
    }

    #[track_caller]
    fn unseen(before: &str, after: &str) {
        assert_eq!(hash(before), hash(after), "{before:?} to {after:?} is seen");
    }

    #[track_caller]
    fn seen(before: &str, after: &str) {
        assert_ne!(hash(before), hash(after), "{before:?} to {after:?} is not seen");
    }

    fn copies(text: &str) -> Copied {
        copied_in(&SourceFile::parse(text, Edition::CURRENT).syntax_node())
    }

    fn signatures_changed(before: &str, after: &str) -> Option<Vec<String>> {
        let of = |text: &str| {
            let tree = SourceFile::parse(text, Edition::CURRENT).syntax_node();
            let none = FxHashSet::default();
            let (hash, _, signatures) = hash_outside(&tree, &none, &none, &Off::default(), true);
            assert_eq!(hash, hash_outside_bodies(&tree, &none, &none, &Off::default()).0);
            signatures
        };
        Signatures::changed(&of(before)?, &of(after)?).filter(|it| !it.is_empty())
    }

    #[test]
    fn what_a_cfg_takes_out_is_not_seen() {
        let hash = |text: &str, off: &Off| {
            let tree = SourceFile::parse(text, Edition::CURRENT).syntax_node();
            let none = FxHashSet::default();
            hash_outside_bodies(&tree, &none, &none, off).0
        };
        let tests = |call: &str| {
            format!(
                "pub fn f() {{}}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{ super::f({call}); }}\n}}\n"
            )
        };
        let built = Off(vec![cfg::CfgOptions::default()]);
        assert_eq!(hash(&tests(""), &built), hash(&tests("()"), &built));
        let mut tested = cfg::CfgOptions::default();
        tested.insert_atom(intern::Symbol::intern("test"));
        let both = Off(vec![cfg::CfgOptions::default(), tested]);
        assert_ne!(hash(&tests(""), &both), hash(&tests("()"), &both));
        assert_ne!(hash(&tests(""), &Off::default()), hash(&tests("()"), &Off::default()));
        // The `#[cfg]` itself is seen, and what is written before the item it takes out.
        let on = tests("").replace("cfg(test)", "cfg(not(test))");
        assert_ne!(hash(&tests(""), &built), hash(&on, &built));
        let doc = tests("").replace("#[cfg(test)]", "#[doc = \"x\"]\n#[cfg(test)]");
        assert_ne!(hash(&tests(""), &built), hash(&doc, &built));
    }

    #[test]
    fn a_check_looks_at_what_changed() {
        let built = Off(vec![cfg::CfgOptions::default()]);
        let asked = |before: &str, after: &str| {
            let parse = |text: &str| SourceFile::parse(text, Edition::CURRENT).syntax_node();
            let (was, tree) = (parse(before), parse(after));
            let (was, now) = (functions_of(&was, &built)?, functions_of(&tree, &built)?);
            let asked = asked_of(&was, &tree, &now)?;
            let mut names: Vec<String> = asked
                .iter()
                .map(|&at| {
                    let node = tree.covering_element(at).into_node().unwrap();
                    let func = node.ancestors().find_map(ast::Fn::cast).unwrap();
                    func.name().unwrap().text().to_string()
                })
                .collect();
            names.sort();
            Some(names)
        };
        let file = |sig: &str, call: &str, extra: &str| {
            format!(
                "pub struct V(u8);\nimpl V {{\n    pub fn get(&self{sig}) -> u8 {{ self.0 }}\n}}\n\
                 pub fn user(v: &V) -> u8 {{ v.get({call}) }}\n\
                 pub fn other() -> u8 {{ 1 }}\n{extra}\
                 #[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{ super::V(1).get({call}); }}\n}}\n"
            )
        };
        let base = file("", "", "");
        assert_eq!(asked(&base, &base), Some(vec![]));
        let body = base.replace("{ 1 }", "{ 2 }");
        assert_eq!(asked(&base, &body), Some(vec!["other".to_owned()]));
        // The signature, its callers, and none of what a `#[cfg]` takes out.
        let signature = file(", _: ()", "()", "");
        assert_eq!(asked(&base, &signature), Some(vec!["get".to_owned(), "user".to_owned()]));
        let named = file(", _: ()", "", "pub fn more(v: &V) -> u8 { v.get() }\n");
        let was = file("", "", "pub fn more(v: &V) -> u8 { v.get() }\n");
        assert_eq!(
            asked(&was, &named),
            Some(vec!["get".to_owned(), "more".to_owned(), "user".to_owned()])
        );
        // Named by what is no function, a trait's item, or anything else that changed.
        let constant = |sig: &str| file(sig, "", "const C: fn(&V) -> u8 = V::get;\n");
        assert_eq!(asked(&constant(""), &constant(", _: ()")), None);
        let traits = |sig: &str| {
            format!("trait T {{ fn f(&self{sig}); }}\nimpl T for u8 {{ fn f(&self{sig}) {{}} }}\n")
        };
        assert_eq!(asked(&traits(""), &traits(", _: u8")), None);
        assert_eq!(asked(&base, &base.replace("V(u8)", "V(u16)")), None);
        assert_eq!(asked(&base, &base.replace("pub fn other", "pub fn another")), None);
    }

    #[test]
    fn edits_of_signatures_alone_are_told() {
        let told = |before: &str, after: &str, names: &[&str]| {
            let names: Vec<String> = names.iter().map(|it| it.to_string()).collect();
            assert_eq!(signatures_changed(before, after), Some(names), "{before:?} to {after:?}");
        };
        let not = |before: &str, after: &str| {
            assert_eq!(signatures_changed(before, after), None, "{before:?} to {after:?}");
        };
        told("fn f(x: u8) -> u8 { x }", "fn f(x: u16) -> u16 { x }", &["f"]);
        told(
            "fn f(x: u8) {}\nfn g() { f(1); }",
            "fn f(x: u8, y: u8) {}\nfn g() { f(1, 2); }",
            &["f"],
        );
        told(
            "struct S;\nimpl S { fn f(&self) {} }",
            "struct S;\nimpl S { fn f(&mut self) {} }",
            &["f"],
        );
        told("mod m { fn f() {} }\nfn f() {}", "mod m { fn f() -> u8 { 1 } }\nfn f() {}", &["f"]);
        told("fn f() where u8: Copy {}", "fn f() {}", &["f"]);
        // Renamed, added, generic, of a trait, seen into, or with other attributes.
        not("fn f() {}", "fn g() {}");
        not("fn f() {}", "fn f() {}\nfn g() {}");
        not("fn f<T>(x: T) {}", "fn f<T>(x: T, y: u8) {}");
        not("fn f(x: u8) {}", "fn f(x: impl Copy) {}");
        not("struct S;\nimpl<T> S { fn f() {} }", "struct S;\nimpl<T> S { fn f(x: u8) {} }");
        not("trait T { fn f(&self); }", "trait T { fn f(&mut self); }");
        not(
            "struct S;\nimpl Clone for S { fn clone(&self) -> S { S } }",
            "struct S;\nimpl Clone for S { fn clone(&self) -> Self { S } }",
        );
        not("async fn f() {}", "async fn f(x: u8) {}");
        not("const fn f() {}", "const fn f(x: u8) {}");
        not("fn f() -> impl Copy { 1 }", "fn f() -> impl Clone { 1 }");
        not("fn f() {}", "#[cfg(test)] fn f() {}");
        not("fn f() {}", "pub fn f() {}");
        not(
            "#[cfg(a)] fn f() {}\n#[cfg(b)] fn f() {}",
            "#[cfg(a)] fn f(x: u8) {}\n#[cfg(b)] fn f() {}",
        );
        not("struct S(u8);\nfn f() {}", "struct S(u16);\nfn f(x: u8) {}");
    }

    #[test]
    fn what_rustc_copies_is_told() {
        let same = |before: &str, after: &str| {
            assert_eq!(copies(before), copies(after), "{before:?} to {after:?} is told");
        };
        let told = |before: &str, after: &str| {
            assert_ne!(copies(before), copies(after), "{before:?} to {after:?} is not told");
        };
        same("fn f() { 1; }", "fn f() { 2; }");
        same("fn f<'a>(x: &'a u8) { 1; }", "fn f<'a>(x: &'a u8) { 2; }");
        same("impl S { fn f(&self) { 1; } }", "impl S { fn f(&self) { 2; } }");
        told("fn f<T>() { 1; }", "fn f<T>() { 2; }");
        told("fn f<const N: usize>() { 1; }", "fn f<const N: usize>() { 2; }");
        told("fn f(x: impl Fn()) { 1; }", "fn f(x: impl Fn()) { 2; }");
        told("impl<T> S<T> { fn f(&self) { 1; } }", "impl<T> S<T> { fn f(&self) { 2; } }");
        told("trait T { fn f(&self) { 1; } }", "trait T { fn f(&self) { 2; } }");
        told("#[inline] fn f() { 1; }", "#[inline] fn f() { 2; }");
        same("#[inline(never)] fn f() { 1; }", "#[inline(never)] fn f() { 2; }");
        // Moved, by a line or along one.
        told("fn g() {}\nfn f<T>() {}", "fn g() {\n}\nfn f<T>() {}");
        told("fn g() {} fn f<T>() {}", "fn g() { } fn f<T>() {}");
        told("/* ab */ fn f<T>() {}", "/* é */ fn f<T>() {}");
        same("fn f<T>() {}\nfn g() {}", "fn f<T>() {}\nfn g() {\n}");
        // A trait's method without a body has no code, wherever it is.
        same("fn g() {}\ntrait T { fn f(&self); }", "fn g() {\n}\ntrait T { fn f(&self); }");
        told("fn g() {}\ntrait T { fn f(&self) {} }", "fn g() {\n}\ntrait T { fn f(&self) {} }");
    }

    #[test]
    fn generics_of_modules_are_told_apart() {
        let others = |before: &str, after: &str| copies(before).others != copies(after).others;
        let generics =
            |before: &str, after: &str| copies(before).generics != copies(after).generics;
        for (before, after) in [
            ("fn f<T>() { 1; }", "fn f<T>() { 2; }"),
            ("fn g() {}\nfn f<T>() {}", "fn g() {\n}\nfn f<T>() {}"),
            ("mod m { fn f(x: impl Fn()) { 1; } }", "mod m { fn f(x: impl Fn()) { 2; } }"),
            ("fn f<T>() {}\nfn g<T>() {}", "fn f<T>() { }\nfn g<T>() {}"),
            ("impl S { fn f<T>(&self) { 1; } }", "impl S { fn f<T>(&self) { 2; } }"),
            ("impl T for S { fn f<U>(&self) { 1; } }", "impl T for S { fn f<U>(&self) { 2; } }"),
        ] {
            assert!(generics(before, after) && !others(before, after), "{before:?}");
        }
        for (before, after) in [
            ("impl<T> S<T> { fn f(&self) { 1; } }", "impl<T> S<T> { fn f(&self) { 2; } }"),
            ("trait T { fn f(&self) { 1; } }", "trait T { fn f(&self) { 2; } }"),
            ("#[inline] fn f<T>() { 1; }", "#[inline] fn f<T>() { 2; }"),
            ("fn g() { fn f<T>() { 1; } }", "fn g() { fn f<T>() { 2; } }"),
        ] {
            assert!(others(before, after) && !generics(before, after), "{before:?}");
        }
        let (before, after) =
            (copies("fn f<T>() {}\nfn g<T>() {}"), copies("fn f<T>() { }\nfn g<T>() {}"));
        assert_ne!(before.generics["f"], after.generics["f"]);
        assert_eq!(before.generics["g"], after.generics["g"]);
    }

    #[test]
    fn inline_functions_alone_are_told_apart() {
        let others = |before: &str, after: &str| copies(before).others != copies(after).others;
        let inline = |before: &str, after: &str| copies(before).inline != copies(after).inline;
        for (before, after) in [
            ("#[inline] fn f() { 1; }", "#[inline] fn f() { 2; }"),
            ("impl S { #[inline] fn f(&self) { 1; } }", "impl S { #[inline] fn f(&self) { 2; } }"),
            (
                "impl T for S { #[inline] fn f(&self) { 1; } }",
                "impl T for S { #[inline] fn f(&self) { 2; } }",
            ),
            ("mod m { #[inline] pub fn f() { 1; } }", "mod m { #[inline] pub fn f() { 2; } }"),
        ] {
            assert!(inline(before, after) && !others(before, after), "{before:?}");
        }
        for (before, after) in [
            ("#[inline(always)] fn f() { 1; }", "#[inline(always)] fn f() { 2; }"),
            ("#[inline] fn f<T>() { 1; }", "#[inline] fn f<T>() { 2; }"),
            ("#[inline] fn f(x: impl Fn()) { 1; }", "#[inline] fn f(x: impl Fn()) { 2; }"),
            (
                "impl<T> S<T> { #[inline] fn f() { 1; } }",
                "impl<T> S<T> { #[inline] fn f() { 2; } }",
            ),
            (
                "trait T { #[inline] fn f(&self) { 1; } }",
                "trait T { #[inline] fn f(&self) { 2; } }",
            ),
            ("fn g() { #[inline] fn f() { 1; } }", "fn g() { #[inline] fn f() { 2; } }"),
        ] {
            assert!(others(before, after) && !inline(before, after), "{before:?}");
        }
        let (before, after) = (
            copies("#[inline] fn f() {}\n#[inline] fn g() {}"),
            copies("#[inline] fn f() { }\n#[inline] fn g() {}"),
        );
        assert_ne!(before.inline["f"], after.inline["f"]);
        assert_eq!(before.inline["g"], after.inline["g"]);
    }

    #[test]
    fn bodies_are_left_out() {
        unseen("fn f() { 1; }", "fn f() { 2; }");
        unseen("impl S { fn f(&self) -> u8 { 1 } }", "impl S { fn f(&self) -> u8 { 2 } }");
        unseen("trait T { fn f(&self) -> u8 { 1 } }", "trait T { fn f(&self) -> u8 { 2 } }");
        unseen("mod m { pub fn f() { a() } }", "mod m { pub fn f() { b() } }");
        unseen("fn f() { #[allow(unused)] let a = 1; }", "fn f() { #[allow(unused)] let a = 2; }");
        unseen("/// Doc.\n#[inline]\nfn f() { 1; }", "/// Doc.\n#[inline]\nfn f() { 2; }");
    }

    #[test]
    fn blank_space_and_comments_are_left_out() {
        unseen("fn f() {}", "\n\nfn f() {}\n");
        unseen("fn f() {}", "fn f() {}\n// edit\n");
        unseen("struct S;", "/* note */ struct S;");
    }

    #[test]
    fn what_others_see_is_kept() {
        seen("fn f() {}", "fn g() {}");
        seen("fn f() {}", "fn f() {} fn g() {}");
        seen("fn f() -> u8 { 1 }", "fn f() -> u16 { 1 }");
        seen("struct S(u8);", "struct S(u16);");
        seen("/// One.\nfn f() {}", "/// Two.\nfn f() {}");
        seen("const C: u8 = 1;", "const C: u8 = 2;");
        seen("static S: u8 = { 1 };", "static S: u8 = { 2 };");
    }

    #[test]
    fn bodies_others_see_into_are_kept() {
        seen("const fn f() -> u8 { 1 }", "const fn f() -> u8 { 2 }");
        seen("async fn f() -> u8 { 1 }", "async fn f() -> u8 { 2 }");
        seen("fn f() -> impl Sized { 1u8 }", "fn f() -> impl Sized { 1u16 }");
        seen("fn f() -> Box<impl Sized> { a() }", "fn f() -> Box<impl Sized> { b() }");
        seen("#[test]\nfn f() { 1; }", "#[test]\nfn f() { 2; }");
        seen("#[inline(always)]\nfn f() { 1; }", "#[inline(always)]\nfn f() { 2; }");
        seen("#[tokio::main]\nfn f() { 1; }", "#[tokio::main]\nfn f() { 2; }");
        seen("#[imp]\nimpl S { fn f() { 1; } }", "#[imp]\nimpl S { fn f() { 2; } }");
        seen("fn f() { #![attr] 1; }", "fn f() { #![attr] 2; }");
        seen(
            "fn f() { macro_rules! m { () => { 1 } } }",
            "fn f() { macro_rules! m { () => { 2 } } }",
        );
    }

    #[test]
    fn what_a_macro_is_handed_is_kept() {
        seen("m! { fn f() { 1; } }", "m! { fn f() { 2; } }");
        seen("m! { a b }", "m! { a  b }");
        seen("m! { a }", "m! { a /* note */ }");
        seen("fn f() { fn g() { 1; } }", "fn f() { fn g() { 1; } } fn h() {}");
    }

    fn skeleton(text: &str) -> Option<u64> {
        skeleton_of(&SourceFile::parse(text, Edition::CURRENT).syntax_node())
    }

    #[test]
    fn functions_are_found_again_by_the_skeleton_after_an_edit_to_bodies() {
        let file = |body: &str| {
            format!("use a::b;\nm! {{ x }}\nfn f() {{ {body} }}\nimpl S {{ fn g(&self) {{}} }}")
        };
        let before = skeleton(&file("1;"));
        assert!(before.is_some());
        for body in ["2;", "println!(\"{}\", 1);", "use std::fmt::Write; 1;", "let s = m!(a);"] {
            assert_eq!(skeleton(&file(body)), before, "{body}");
        }
        assert_ne!(skeleton("fn f() {}\nfn h() {}"), skeleton("fn f() {}"));
    }

    #[test]
    fn functions_are_not_found_by_the_skeleton_where_ids_can_move() {
        // An item in a body, and a macro called where it writes items but at the top.
        for text in [
            "fn f() { fn g() {} }",
            "fn f() { struct S; }",
            "fn f() { impl S {} }",
            "impl S { m!(); }",
            "mod m { n!(); }",
        ] {
            assert_eq!(skeleton(text), None, "{text}");
        }
    }

    #[test]
    fn a_body_that_declares_items_is_kept() {
        let text = |n: u8| format!("fn f() {{ fn g() -> u8 {{ {n} }} }}");
        let hash = |text: &str| {
            let tree = SourceFile::parse(text, Edition::CURRENT).syntax_node();
            let declaring = tree.descendants().find(|it| it.kind() == SyntaxKind::FN).unwrap();
            let declaring = FxHashSet::from_iter([declaring.text_range()]);
            hash_outside_bodies(&tree, &declaring, &FxHashSet::default(), &Off::default())
        };
        let (one, found) = hash(&text(1));
        assert_eq!(found, 1);
        assert_ne!(one, hash(&text(2)).0);
    }

    fn added(before: &str, after: &str) -> Option<Vec<String>> {
        let parse = |text: &str| SourceFile::parse(text, Edition::CURRENT).syntax_node();
        let added = added_functions(&parse(before), &parse(after))?;
        Some(added.iter().map(|it| it.name().unwrap().text().to_string()).collect())
    }

    #[test]
    fn functions_added_alone() {
        let before = "fn f() { 1; }\nmod m {\n    fn g() {}\n}\n";
        let after = "fn f() { h(); }\n\n#[inline(never)]\nfn h() -> u8 { 2 }\nmod m {\n    fn g() {}\n    fn f() {}\n}\n";
        let mut names = added(before, after).unwrap();
        names.sort();
        assert_eq!(names, ["f", "h"]);
    }

    #[test]
    fn more_than_functions_added() {
        let before = "fn f() { 1; }\n";
        for after in [
            // Nothing added, or something else.
            "fn f() { 2; }\n",
            "fn f() { 1; }\nfn g() {}\nstruct S;\n",
            // Taken away or renamed.
            "fn g() { 1; }\n",
            // Copied by rustc into what calls it.
            "fn f() { 1; }\nfn g<T>() {}\n",
            "fn f() { 1; }\nfn g(_: impl Copy) {}\n",
            "fn f() { 1; }\n#[inline]\nfn g() {}\n",
            // Seen into, or declaring an item.
            "fn f() { 1; }\nconst fn g() {}\n",
            "fn f() { 1; }\nasync fn g() {}\n",
            "fn f() { 1; }\nfn g() -> impl Copy {}\n",
            "fn f() { 1; }\nfn g() { fn h() {} }\n",
            "fn f() { 1; }\n#[test]\nfn g() {}\n",
            // A method, or one a macro is handed.
            "fn f() { 1; }\nstruct S;\nimpl S { fn g() {} }\n",
            "fn f() { 1; }\nm! { fn g() {} }\n",
            // Two of one name, one of which a `#[cfg]` takes out.
            "fn f() { 1; }\n#[cfg(unix)]\nfn g() {}\n#[cfg(not(unix))]\nfn g() {}\n",
        ] {
            assert_eq!(added(before, after), None, "{after}");
        }
    }

    #[test]
    fn functions_taken_away_alone() {
        let before = "fn f() { h(); }\n\nfn h() -> u8 { 2 }\nmod m {\n    fn g() {}\n}\n";
        let after = "fn f() { 1; }\nmod m {\n    fn g() {}\n}\n";
        assert_eq!(added(before, after), None);
        assert_eq!(added(after, before).unwrap(), ["h"]);
        // Taken away with something else, or one rustc copies into what calls it.
        assert_eq!(added("fn f() { 1; }\n", "fn f() { h(); }\n\nfn h() {}\nstruct S;\n"), None);
        assert_eq!(added("fn f() { 1; }\n", "fn f() { h(); }\n\n#[inline]\nfn h() {}\n"), None);
    }
}
