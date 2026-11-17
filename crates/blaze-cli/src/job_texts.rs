//! The world's texts as a pool worker reads them: one home for body_types.rs's file jobs and
//! names.rs's crate scopes (rule 30). A worker shares no live world: it is handed the packs'
//! views and the corrections the command read (record bytes and a few words each), reads the
//! checkout's files itself, each once a job, into its own frame, and lists each it read or
//! looked for and did not find.

use core::cell::RefCell;
use std::alloc::Global;

use blaze_fetch::pack::PackView;
use blaze_scope::crate_env::CrateEnv;
use blaze_scope::resolve::SourceTexts;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::work::held_str;

pub(crate) struct JobTexts<'n> {
    pub(crate) cx: &'n Cx,
    pub(crate) root: &'n Dir<ReadWrite>,
    pub(crate) frame: &'n Frame,
    pub(crate) packs: &'n [PackView<'n>],
    pub(crate) corrected: &'n [(u32, &'n str, &'n str)],
    /// The environment of the one crate the job is about, by its world index: what `env!`
    /// reads. `None` (a body job's): every `env!` asked through these texts is refused by name.
    pub(crate) env: Option<(usize, &'n CrateEnv<'n>)>,
    /// Each checkout path asked (tree 0, after `/0/`), in the order first asked, with its text
    /// or `None` when there is no such file.
    pub(crate) read: RefCell<std::vec::Vec<(&'n str, Option<&'n str>), Global>>,
}

impl<'n> JobTexts<'n> {
    fn checkout(&self, inner: &str) -> Option<&'n str> {
        if inner.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        let at = Path::new(inner).ok()?;
        let mut bytes = Bytes::new_in(self.frame, "a pool job's checkout files");
        self.root.read_all(self.cx, &at, &mut bytes).ok()?;
        let held: &'n Bytes<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(bytes, self.frame));
        core::str::from_utf8(held.as_slice()).ok()
    }
}

impl<'n> SourceTexts<'n> for JobTexts<'n> {
    fn read(&self, _krate: usize, path: &str) -> Option<&'n str> {
        let rest = path.strip_prefix('/')?;
        let (tree, inner) = rest.split_once('/')?;
        let tree: usize = tree.parse().ok()?;
        if let Some((_, _, text)) = self.corrected.iter().find(|(t, p, _)| *t as usize == tree && *p == inner) {
            return Some(text);
        }
        if tree != 0 {
            return core::str::from_utf8(self.packs.get(tree - 1)?.find(inner)?.bytes).ok();
        }
        if let Some((_, text)) = self.read.borrow().iter().find(|(p, _)| *p == inner) {
            return *text;
        }
        let text = self.checkout(inner);
        self.read.borrow_mut().push((held_str(inner, self.frame), text));
        text
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'n>> {
        self.env.filter(|(at, _)| *at == krate).map(|(_, env)| env)
    }
}
