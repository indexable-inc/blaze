//! One file as a request's edits leave it: its text, its tree reparsed in place after each
//! splice, and the splices made, which are the text edits the VFS commits.

use blaze_vfs::Op;
use syntax::{Edition, Parse, SourceFile, TextRange, TextSize, view};

/// `delete` replaced by `insert`, in the text as the splices before it left it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Splice {
    pub delete: TextRange,
    pub insert: String,
}

/// The text is the one `parse`'s tree shares, never a copy: a splice's reparse makes the new
/// text once, and nothing else here holds the file's bytes.
pub struct Working {
    /// The file's path in the view.
    pub path: Box<str>,
    pub edition: Edition,
    /// Where the view's bytes end in [`Working::text`].
    pub end: usize,
    pub parse: Parse<SourceFile>,
    /// The parse before any splice, sharing the tree `parse` was made from: where touched items
    /// are named and keyed.
    pub base: Parse<SourceFile>,
    pub splices: Vec<Splice>,
}

impl Working {
    /// A file whose view bytes are the first `end` of the text `parse` is of.
    pub fn new(path: &str, edition: Edition, end: usize, parse: Parse<SourceFile>) -> Working {
        assert!(parse.root().file().text().is_char_boundary(end), "the view's bytes end inside the text");
        let base = parse.clone(); // clone: two `Arc` counts, once per file opened
        Working { path: path.into(), edition, end, parse, base, splices: Vec::new() }
    }

    /// A file parsed here: for one the front end has not parsed, and for tests.
    pub fn parsed(path: &str, edition: Edition, text: &str) -> Working {
        let parse = SourceFile::parse(text, edition);
        Working::new(path, edition, text.len(), parse)
    }

    /// The view's bytes, then what the front end appends to a crate root (never edited), as the
    /// edits so far leave them.
    pub fn text(&self) -> &str {
        self.parse.root().file().text()
    }

    /// The tree as the edits so far leave it.
    pub fn root(&self) -> view::Node<'_> {
        self.parse.root()
    }

    /// The tree before any splice.
    pub fn base(&self) -> view::Node<'_> {
        self.base.root()
    }

    /// The view's bytes as the edits so far leave them.
    pub fn bytes(&self) -> &str {
        &self.text()[..self.end]
    }

    /// Replaces `delete` by `insert` in the text and reparses only what it touched: the
    /// reparse makes the new text, which the new tree shares.
    pub fn splice(&mut self, delete: TextRange, insert: String) -> Result<(), String> {
        let start = usize::from(delete.start());
        let stop = usize::from(delete.end());
        if stop > self.end {
            return Err(format!(
                "an edit of {} reaches past the file's own text, into the module the front end \
                 appends to a crate root",
                self.path
            ));
        }
        let text = self.text();
        if !text.is_char_boundary(start) || !text.is_char_boundary(stop) {
            return Err(format!("an edit of {} splits a character", self.path));
        }
        let len = text.len() - (stop - start) + insert.len();
        self.parse = self.parse.reparse(delete, &insert, self.edition);
        debug_assert_eq!(self.text().len(), len, "a reparse's text is the spliced text");
        self.end = self.end - (stop - start) + insert.len();
        self.splices.push(Splice { delete, insert });
        Ok(())
    }

    /// Where `range` of the base text is now; `None` when a splice replaced any of it.
    pub fn to_now(&self, range: TextRange) -> Option<TextRange> {
        let mut range = range;
        for splice in &self.splices {
            let delete = splice.delete;
            let inserted = TextSize::of(splice.insert.as_str());
            if range.end() <= delete.start() {
                continue;
            }
            if range.start() >= delete.end() {
                range = TextRange::at(range.start() - delete.len() + inserted, range.len());
            } else {
                return None;
            }
        }
        Some(range)
    }

    /// Where `at` in the text now was before any splice; `None` inside text a splice inserted.
    pub fn to_base(&self, at: TextSize) -> Option<TextSize> {
        let mut at = at;
        for splice in self.splices.iter().rev() {
            let start = splice.delete.start();
            let inserted = TextSize::of(splice.insert.as_str());
            if at <= start {
                continue;
            }
            if at >= start + inserted {
                at = at - inserted + splice.delete.len();
            } else {
                return None;
            }
        }
        Some(at)
    }

    /// The splices as the VFS's text edits, in order: each applies to what the one before left.
    pub fn ops(&self) -> impl Iterator<Item = Op> + '_ {
        self.splices.iter().map(|splice| Op::Edit {
            at: u64::from(u32::from(splice.delete.start())),
            len: u64::from(u32::from(splice.delete.len())),
            path: self.path.to_string(),
            bytes: splice.insert.as_bytes().to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_map_back_through_every_splice() {
        let mut file = Working::parsed("lib.rs", Edition::CURRENT, "fn a() {}\nfn b() {}\n");
        file.splice(TextRange::new(7.into(), 9.into()), "{ 1 }".to_owned()).unwrap();
        assert_eq!(file.bytes(), "fn a() { 1 }\nfn b() {}\n");
        assert_eq!(file.to_base(13.into()), Some(10.into()));
        assert_eq!(file.to_base(7.into()), Some(7.into()));
        assert_eq!(file.to_base(9.into()), None);
        assert_eq!(file.to_base(12.into()), Some(9.into()));
        let ops: Vec<Op> = file.ops().collect();
        assert_eq!(
            ops,
            [Op::Edit { at: 7, len: 2, path: "lib.rs".to_owned(), bytes: b"{ 1 }".to_vec() }]
        );
        assert_eq!(file.to_now(TextRange::new(10.into(), 12.into())), Some(TextRange::new(13.into(), 15.into())));
        assert_eq!(file.to_now(TextRange::new(3.into(), 4.into())), Some(TextRange::new(3.into(), 4.into())));
        assert_eq!(file.to_now(TextRange::new(8.into(), 9.into())), None);
    }

    #[test]
    fn reads_and_position_maps_allocate_nothing() {
        let mut file = Working::parsed("lib.rs", Edition::CURRENT, "fn a() {}\nfn b() {}\n");
        file.splice(TextRange::new(7.into(), 9.into()), "{ 1 }".to_owned()).unwrap();
        let read = blaze_alloc_count::assert_at_most(0, "a working file's reads", || {
            (
                file.text().len() + file.bytes().len(),
                file.to_now(TextRange::new(10.into(), 12.into())),
                file.to_base(13.into()),
                file.root().text_range(),
                file.base().text_range(),
            )
        });
        assert_eq!(read.0, 2 * 23);
    }

    #[test]
    fn the_text_is_the_trees_never_a_copy() {
        let text = syntax::Text::from("fn a() {}\n");
        let parse = SourceFile::parse_shared(text.clone(), Edition::CURRENT);
        let mut file = Working::new("lib.rs", Edition::CURRENT, text.len(), parse);
        assert_eq!(file.text().as_ptr(), text.as_ptr());
        file.splice(TextRange::new(7.into(), 9.into()), "{ 1 }".to_owned()).unwrap();
        assert_eq!(file.text(), "fn a() { 1 }\n");
        assert_eq!(file.base().file().text().as_ptr(), text.as_ptr());
    }

    #[test]
    fn the_appended_module_is_never_edited() {
        let parse = SourceFile::parse("fn a() {}\nmod __appended {}\n", Edition::CURRENT);
        let mut file = Working::new("lib.rs", Edition::CURRENT, 10, parse);
        assert!(file.splice(TextRange::new(8.into(), 12.into()), String::new()).is_err());
        file.splice(TextRange::new(0.into(), 0.into()), "// x\n".to_owned()).unwrap();
        assert_eq!(file.bytes(), "// x\nfn a() {}\n");
        assert_eq!(&file.text()[file.end..], "mod __appended {}\n");
    }
}
