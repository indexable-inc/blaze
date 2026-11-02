//! A file's bytes as text, read in place (docs/v2/plan/design/frontend.md item 6,
//! docs/v2/plan/lanes/frontend/text.md): a [`Text`] is the file's [`Bytes`](crate::Bytes)
//! (the tree's own allocation, or a range of the mapped image), checked to be UTF-8 once and
//! lent as `&str`, never copied. Holding it keeps the bytes alive as holding the bytes does.
//! `Text`, `Bytes` and the check are `blaze-bytes`'s, below the parse tree, so the tree's
//! source is the same text. This module has no unsafe code of its own.

use std::str::Utf8Error;

pub use blaze_bytes::{Text, text_of};
use blaze_ids::ContentId;
use blaze_image::Corrupt;

use crate::{Mode, Reader, ViewId, Vfs};

/// A file's content ID and mode, and its text or why it has none.
pub type TextFile = (ContentId, Mode, Result<Text, Utf8Error>);

impl Reader {
    /// The file at `path` under `root` as text ([`text_of`]). `Ok(None)` and `Err` as for
    /// [`Reader::file`].
    pub fn text(&self, root: ContentId, path: &str) -> Result<Option<TextFile>, Corrupt> {
        Ok(self.file(root, path)?.map(|(id, mode, bytes)| (id, mode, text_of(bytes))))
    }
}

impl Vfs {
    /// The file at `path` in `view` as text ([`text_of`]): `Ok(None)` for none or a directory,
    /// `Err` for a file of the image whose page group fails its check.
    pub fn text(&self, view: ViewId, path: &str) -> Result<Option<TextFile>, Corrupt> {
        let Some((node, mode)) = self.tree.entry(self.open_root(view), path) else {
            return Ok(None);
        };
        Ok(self.tree.owned(node)?.map(|bytes| (self.tree.id(node), mode, text_of(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use blaze_core::FileName;
    use blaze_ids::ContentId;

    use super::text_of;
    use crate::{Bytes, FILE_DOMAIN, Mode, Op, View, Vfs};

    /// A file's text, read from bytes, a reader or a view, allocates nothing: it is the tree's
    /// bytes. So is a heap file's `Arc<str>`.
    #[test]
    fn reading_text_allocates_nothing() {
        let bytes = Bytes::from(Arc::<[u8]>::from("fn main() {}\n".repeat(100).as_bytes()));
        let text = blaze_alloc_count::assert_at_most(0, "text_of", || text_of(bytes.clone()));
        let text = text.expect("UTF-8");
        let shared = blaze_alloc_count::assert_at_most(0, "Text::shared", || text.shared());
        assert!(shared.is_some());

        let mut vfs = Vfs::recover(FileName::new("state/journal").unwrap(), 0, &[]).0;
        let ops = [Op::Write { mode: Mode::Regular, path: "src/lib.rs".into(), bytes: bytes.to_vec() }];
        let empty = vfs.empty();
        let root = vfs.commit("main", empty, &ops).unwrap().root;
        let view = vfs.open(View { root }).unwrap();
        let reader = vfs.reader();
        // The first read registers this thread with the reclaimer; reads after it are measured.
        let _ = reader.text(root, "src/lib.rs");
        let read = blaze_alloc_count::assert_at_most(0, "Reader::text", || reader.text(root, "src/lib.rs"));
        assert!(read.unwrap().is_some_and(|(_, _, text)| text.is_ok()));
        let read = blaze_alloc_count::assert_at_most(0, "Vfs::text", || vfs.text(view, "src/lib.rs"));
        assert!(read.unwrap().is_some_and(|(_, _, text)| text.is_ok()));
    }

    #[test]
    fn a_tree_file_is_read_as_its_own_bytes() {
        let mut vfs = Vfs::recover(FileName::new("state/journal").unwrap(), 0, &[]).0;
        let ops = [
            Op::Write { mode: Mode::Regular, path: "src/lib.rs".into(), bytes: b"mod a;\n".to_vec() },
            Op::Write { mode: Mode::Regular, path: "data.bin".into(), bytes: vec![0xff] },
        ];
        let empty = vfs.empty();
        let root = vfs.commit("main", empty, &ops).unwrap().root;

        let reader = vfs.reader();
        let (id, mode, text) = reader.text(root, "src/lib.rs").unwrap().expect("a file");
        let text = text.expect("UTF-8");
        assert_eq!((id, mode, &*text), (ContentId::of(FILE_DOMAIN, b"mod a;\n"), Mode::Regular, "mod a;\n"));
        let (_, _, bytes) = reader.file(root, "src/lib.rs").unwrap().expect("a file");
        assert_eq!(text.as_ptr(), bytes.as_ptr(), "the tree's bytes, not a copy");
        assert!(reader.text(root, "data.bin").unwrap().expect("a file").2.is_err());
        assert!(reader.text(root, "src").unwrap().is_none());

        let view = vfs.open(View { root }).unwrap();
        let (id, mode, text) = vfs.text(view, "src/lib.rs").unwrap().expect("a file");
        assert_eq!((id, mode), (ContentId::of(FILE_DOMAIN, b"mod a;\n"), Mode::Regular));
        assert_eq!(text.unwrap().as_ptr(), bytes.as_ptr());
        assert!(vfs.text(view, "missing.rs").unwrap().is_none());
    }
}
