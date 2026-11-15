//! `(include "path")`: the forms of another rules file, read where the fact source keeps its
//! files. The engine owns the path rules, the cycle check, the once-only load and the caps; a
//! source only says what paths are relative to and reads the text at a key.

use std::alloc::Global;
use std::sync::Arc;

use blaze_hash::HashSet;
use snafu::ResultExt as _;

use crate::error::{
    DslSnafu, Error, IncludeCycleSnafu, IncludeMissingSnafu, IncludeNowhereSnafu,
    IncludePathSnafu, IncludeReadSnafu, TooManyIncludesSnafu, TooMuchRulesSnafu,
};
use crate::line::Line;
use crate::sexpr::{self, Sexpr};

/// The most files one load reads through `(include ...)`, each counted once however many
/// routes reach it.
pub const MAX_INCLUDED_FILES: usize = 64;

/// The most bytes of rules text one load reads: the request's own text and every included
/// file's. Every form read is held until the program is checked, so this bounds what a request
/// naming one library that names others makes the loader hold.
pub const MAX_RULES_BYTES: usize = 1 << 20;

/// What an include's path is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base<'a> {
    /// The root of the source's files, whichever file includes it: blaze's view, where the
    /// request's own text has no path and a rules file is part of the tree.
    Root,
    /// The directory of the file that includes it: the tree-sitter source, whose rules are
    /// files on disk. The request's own text is the file at this path, or `None` when it has
    /// none, and then it may include nothing.
    Beside(Option<&'a str>),
}

/// Where a load reads the files `(include ...)` names.
pub trait Includes {
    fn base(&self) -> Base<'_>;

    /// `with` called on the text of the file at `key` (a path [`Base`] resolved, `/`-separated,
    /// with no `.` or `..` segment); `Ok(None)` when there is no file there.
    ///
    /// # Errors
    ///
    /// A file that is there and cannot be read as UTF-8 text.
    fn read<R>(&self, key: &str, with: impl FnOnce(&str) -> R) -> std::io::Result<Option<R>>;
}

/// A rules text that includes nothing: every `(include ...)` in it is refused.
pub(crate) struct Nowhere;

impl Includes for Nowhere {
    fn base(&self) -> Base<'_> {
        Base::Beside(None)
    }

    fn read<R>(&self, _: &str, _: impl FnOnce(&str) -> R) -> std::io::Result<Option<R>> {
        Ok(None)
    }
}

/// Every top-level form of `src` and of the files it includes, each included file's forms in
/// place of its `(include ...)`.
///
/// # Errors
///
/// A reader error of any file; an include whose path is malformed, names no file, closes a
/// cycle (naming the files on it in order), or takes the load past [`MAX_INCLUDED_FILES`] or
/// [`MAX_RULES_BYTES`].
pub(crate) fn forms<I: Includes, S: std::error::Error + 'static>(
    src: &str,
    includes: &I,
) -> Result<Vec<Sexpr>, Error<S>> {
    let base = includes.base();
    let root = match base {
        Base::Beside(Some(path)) => Some(Arc::from(path)),
        Base::Root | Base::Beside(None) => None,
    };
    let mut loader = Loader {
        includes,
        base,
        stack: root.into_iter().collect(),
        loaded: HashSet::new_in(Global),
        bytes: src.len(),
    };
    let mut out = Vec::new();
    loader.file(src, None, &mut out)?;
    Ok(out)
}

struct Loader<'i, I> {
    includes: &'i I,
    base: Base<'i>,
    /// The files being read, outermost first: an include of one of them closes a cycle.
    stack: Vec<Arc<str>>,
    /// Every file read, so a second route to one reads nothing.
    loaded: HashSet<Arc<str>, Global>,
    bytes: usize,
}

impl<I: Includes> Loader<'_, I> {
    /// The forms of `src`, the text of `file` (`None`: the request's own), into `out`.
    fn file<S: std::error::Error + 'static>(
        &mut self,
        src: &str,
        file: Option<&Arc<str>>,
        out: &mut Vec<Sexpr>,
    ) -> Result<(), Error<S>> {
        for form in sexpr::parse(src, file.cloned())? {
            match included(&form)? {
                Some((path, line)) => self.include(path, line, out)?,
                None => out.push(form),
            }
        }
        Ok(())
    }

    fn include<S: std::error::Error + 'static>(
        &mut self,
        path: &str,
        line: &Line,
        out: &mut Vec<Sexpr>,
    ) -> Result<(), Error<S>> {
        if let Err(why) = check_path(path) {
            return IncludePathSnafu { line, path, why }.fail();
        }
        let key: Arc<str> = match self.base {
            Base::Root => Arc::from(path),
            Base::Beside(root) => {
                let Some(from) = line.file().or(root) else {
                    return IncludeNowhereSnafu { line, path }.fail();
                };
                match from.rsplit_once('/') {
                    Some((dir, _)) => Arc::from(format!("{dir}/{path}")),
                    None => Arc::from(path),
                }
            }
        };
        if let Some(at) = self.stack.iter().position(|it| *it == key) {
            let mut cycle = String::new();
            for file in &self.stack[at..] {
                cycle.push_str(&format!("`{file}` -> "));
            }
            cycle.push_str(&format!("`{key}`"));
            return IncludeCycleSnafu { line, path, cycle }.fail();
        }
        if self.loaded.contains(&key) {
            return Ok(());
        }
        if self.loaded.len() == MAX_INCLUDED_FILES {
            return TooManyIncludesSnafu { line, path, max: MAX_INCLUDED_FILES }.fail();
        }
        self.loaded.insert(Arc::clone(&key));
        self.stack.push(Arc::clone(&key));
        let includes = self.includes;
        let read = includes
            .read(&key, |text| {
                self.bytes += text.len();
                if self.bytes > MAX_RULES_BYTES {
                    return TooMuchRulesSnafu { line, path, max: MAX_RULES_BYTES }.fail();
                }
                self.file(text, Some(&key), out)
            })
            .context(IncludeReadSnafu { line, path, key: &*key })?;
        let Some(read) = read else {
            return IncludeMissingSnafu { line, path, key: &*key }.fail();
        };
        read?;
        self.stack.pop();
        Ok(())
    }
}

/// `(include "path")` as its path and line; `None` for any other form.
fn included<S: std::error::Error + 'static>(form: &Sexpr) -> Result<Option<(&str, &Line)>, Error<S>> {
    let Sexpr::List { items, line } = form else { return Ok(None) };
    let Some((Sexpr::Atom { text: head, .. }, rest)) = items.split_first() else { return Ok(None) };
    if head != "include" {
        return Ok(None);
    }
    match rest {
        [Sexpr::Str { text, .. }] => Ok(Some((text, line))),
        _ => DslSnafu { line, message: "include form is (include \"path\")".to_owned() }.fail(),
    }
}

/// Why `path` is no include path: it is relative, `/`-separated, and every segment names a
/// file or directory below, so one file has one key and no include leaves its base.
fn check_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("the path is empty");
    }
    if path.starts_with('/') {
        return Err("the path is absolute; write it relative");
    }
    if path.contains('\\') {
        return Err("the path has a `\\`; separate with `/`");
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err("the path has an empty segment"),
            "." | ".." => return Err("the path has a `.` or `..` segment"),
            _ => {}
        }
    }
    Ok(())
}
