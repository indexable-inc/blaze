//! This module resolves `mod foo;` declaration to file.
use base_db::{Db, AnchoredPath, Crate};
use hir_expand::{EditionedFileId, name::Name};
use intern::Symbol;

use crate::HirFileId;

const MOD_DEPTH_LIMIT: usize = 32;

/// 16 bytes and `Copy`: the directory is an interned symbol, so the collector keeps one per module
/// and hands it to every expansion collected there without copying the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ModDir {
    /// `` for `mod.rs`, `lib.rs`
    /// `foo/` for `foo.rs`
    /// `foo/bar/` for `mod bar { mod x; }` nested in `foo.rs`
    /// Invariant: path.is_empty() || path.ends_with('/')
    dir_path: DirPath,
    /// inside `./foo.rs`, mods with `#[path]` should *not* be relative to `./foo/`
    root_non_dir_owner: bool,
    depth: u32,
}

const _: () = assert!(size_of::<ModDir>() == 12);

impl ModDir {
    pub(super) fn root() -> ModDir {
        ModDir { dir_path: DirPath::empty(), root_non_dir_owner: false, depth: 0 }
    }

    /// `buf` is scratch the caller keeps across declarations.
    pub(super) fn descend_into_definition(
        &self,
        name: &Name,
        attr_path: Option<&str>,
        buf: &mut String,
    ) -> Option<ModDir> {
        let depth = self.child_depth()?;
        buf.clear();
        match attr_path {
            None => {
                buf.push_str(self.dir_path.as_str());
                buf.push_str(name.as_str());
                buf.push('/');
            }
            Some(attr_path) => {
                self.dir_path.join_attr(attr_path, self.root_non_dir_owner, buf);
                if !(buf.is_empty() || buf.ends_with('/')) {
                    buf.push('/')
                }
            }
        }
        Some(ModDir { dir_path: DirPath::new(buf), root_non_dir_owner: false, depth })
    }

    fn child_depth(&self) -> Option<u32> {
        let depth = self.depth + 1;
        if depth as usize > MOD_DEPTH_LIMIT {
            tracing::error!("MOD_DEPTH_LIMIT exceeded");
            cov_mark::hit!(circular_mods);
            return None;
        }
        Some(depth)
    }

    /// The candidate files are built one after the other in `buf`, scratch the caller keeps
    /// across declarations.
    pub(super) fn resolve_declaration(
        &self,
        db: &Db,
        krate: Crate,
        file_id: HirFileId,
        name: &Name,
        attr_path: Option<&str>,
        buf: &mut String,
    ) -> Result<(EditionedFileId, bool, ModDir), Box<[String]>> {
        let name = name.as_str();
        let orig_file_id = file_id.original_file_respecting_includes(db);
        let anchor = orig_file_id.file_id(db);

        buf.clear();
        let suffixes: &[&str] = match attr_path {
            Some(attr_path) => {
                self.dir_path.join_attr(attr_path, self.root_non_dir_owner, buf);
                &[""]
            }
            None => {
                buf.push_str(self.dir_path.as_str());
                buf.push_str(name);
                &[".rs", "/mod.rs"]
            }
        };
        let stem = buf.len();
        for suffix in suffixes {
            buf.truncate(stem);
            buf.push_str(suffix);
            if let Some(file_id) = db.resolve_path(AnchoredPath { anchor, path: buf.as_str() }) {
                let is_mod_rs = buf.ends_with("/mod.rs");

                let root_dir_owner = is_mod_rs || attr_path.is_some();
                if let Some(depth) = self.child_depth() {
                    let dir_path = if root_dir_owner {
                        DirPath::empty()
                    } else {
                        buf.clear();
                        buf.push_str(name);
                        buf.push('/');
                        DirPath::new(buf)
                    };
                    return Ok((
                        // FIXME: Edition, is this rightr?
                        EditionedFileId::new(db, file_id, orig_file_id.edition(db)),
                        is_mod_rs,
                        ModDir { dir_path, root_non_dir_owner: !root_dir_owner, depth },
                    ));
                }
            }
        }

        let candidate_files: Box<[String]> = match attr_path {
            Some(attr_path) => {
                // alloc: once per out-of-line `mod` while collecting; the not-found diagnostic keeps them
                let mut path = String::new();
                self.dir_path.join_attr(attr_path, self.root_non_dir_owner, &mut path);
                // alloc: once per out-of-line `mod` while collecting; the not-found diagnostic keeps them
                Box::new([path])
            }
            None => {
                let dir = self.dir_path.as_str();
                // alloc: once per out-of-line `mod` while collecting; the not-found diagnostic keeps them
                Box::new([format!("{dir}{name}.rs"), format!("{dir}{name}/mod.rs")])
            }
        };
        // A `#[path]` can name a file outside the package, which its source root does not have.
        for candidate in &candidate_files {
            db.include_source(AnchoredPath { anchor, path: candidate.as_str() }, krate);
        }
        Err(candidate_files)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct DirPath(Symbol);

impl DirPath {
    fn new(repr: &str) -> DirPath {
        assert!(repr.is_empty() || repr.ends_with('/'));
        DirPath(Symbol::intern(repr))
    }
    fn empty() -> DirPath {
        DirPath(Symbol::empty())
    }
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
    fn parent(&self) -> Option<&str> {
        let path = self.as_str();
        if path.is_empty() {
            return None;
        };
        let idx = path[..path.len() - '/'.len_utf8()].rfind('/').map_or(0, |it| it + '/'.len_utf8());
        Some(&path[..idx])
    }
    /// So this is the case which doesn't really work I think if we try to be
    /// 100% platform agnostic:
    ///
    /// ```ignore
    /// mod a {
    ///     #[path="C://sad/face"]
    ///     mod b { mod c; }
    /// }
    /// ```
    ///
    /// Here, we need to join logical dir path to a string path from an
    /// attribute. Ideally, we should somehow losslessly communicate the whole
    /// construction to `FileLoader`.
    fn join_attr(&self, attr: &str, relative_to_parent: bool, out: &mut String) {
        let base = if relative_to_parent { self.parent().unwrap() } else { self.as_str() };
        let attr = attr.strip_prefix("./").unwrap_or(attr);
        out.push_str(base);
        if attr.contains('\\') {
            out.extend(attr.chars().map(|c| if c == '\\' { '/' } else { c }));
        } else {
            out.push_str(attr);
        }
    }
}
