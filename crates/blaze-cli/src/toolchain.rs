//! The toolchain's shelf as this binary reads it (docs/v2/plan/design/toolchain.md, section 2):
//! `<home>/.blaze/v2/toolchain/`, one file per record named by its 64-hex digest. The home and
//! the start folder are driver facts `main` passes in; neither enters a key or a record. The
//! shelf is reached read-only: under the start folder through its `Dir`, else through the one
//! [`Ancestor`] that holds it (the nearest folder above the start that is above the shelf too),
//! confined to the shelf's folder by the one prefix every read is made under and by names that
//! are only ever 64 hex digits (`blaze_toolchain::shelf`). What is opened, and when, is
//! blaze-toolchain's: the manifest by `PINNED`, a pack only when a task needs a toolchain
//! crate's text.

use blaze_toolchain::shelf::Shelf;
use bstd::cx::Cx;
use bstd::fs::{Ancestor, Dir, Fs, ReadOnly};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

/// The shelf's folder beneath the home.
pub const SHELF: &str = ".blaze/v2/toolchain";

/// Why the shelf's folder could not be reached: the folder, as a person would look for it,
/// and what was said.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShelfError {
    pub place: String,
    pub said: String,
}

impl core::fmt::Display for ShelfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the toolchain's shelf {}: {}", self.place, self.said)
    }
}

impl core::error::Error for ShelfError {}

enum At {
    /// Beneath the start folder.
    Beneath(Dir<ReadOnly>),
    /// Beneath an ancestor of the start: every read is under `prefix`.
    Above { ancestor: Ancestor, prefix: String },
}

/// The shelf, opened once per command that needs it; files read into `frame`.
pub struct HomeShelf<'f> {
    place: String,
    at: At,
    cx: &'f Cx,
    frame: &'f Frame,
}

/// A file read from the shelf.
pub struct Held<'f>(Bytes<'f>);

impl core::fmt::Debug for Held<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} bytes from the shelf", self.0.len())
    }
}

impl AsRef<[u8]> for Held<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// The parts of an absolute path, `None` for one that is not absolute or has `.`/`..`.
fn parts(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    (!parts.iter().any(|p| *p == "." || *p == "..")).then_some(parts)
}

impl<'f> HomeShelf<'f> {
    /// The shelf under `home`, as seen from the command's start folder `start` (both absolute,
    /// as the driver gave them).
    pub fn open(cx: &'f Cx, fs: &Fs, start: &str, home: &str, frame: &'f Frame) -> Result<HomeShelf<'f>, ShelfError> {
        let place = format!("{}/{SHELF}", home.trim_end_matches('/'));
        let failed = |said: String| ShelfError { place: place.clone(), said };
        let (Some(start_parts), Some(shelf_parts)) = (parts(start), parts(&place)) else {
            return Err(failed(format!("the start folder `{start}` and the home must be absolute")));
        };
        let common = start_parts.iter().zip(&shelf_parts).take_while(|(a, b)| a == b).count();
        let below = shelf_parts[common..].join("/");
        let at = if common == start_parts.len() {
            let path = Path::new(&below).map_err(|e| failed(format!("{e}")))?;
            At::Beneath(fs.open_dir(cx, &path).map_err(|e| failed(format!("{e}")))?.read_only())
        } else {
            let up = u8::try_from(start_parts.len() - common).map_err(|_| failed("more than 255 folders above the start".into()))?;
            At::Above { ancestor: fs.ancestor(cx, up).map_err(|e| failed(format!("{e}")))?, prefix: below }
        };
        Ok(HomeShelf { place, at, cx, frame })
    }

    fn path(&self, name: &str) -> Result<Path, String> {
        let joined = match &self.at {
            At::Beneath(_) => name.to_string(),
            At::Above { prefix, .. } if prefix.is_empty() => name.to_string(),
            At::Above { prefix, .. } => format!("{prefix}/{name}"),
        };
        Path::new(&joined).map_err(|e| format!("{e}"))
    }
}

impl<'f> Shelf for HomeShelf<'f> {
    type Error = String;
    type Bytes = Held<'f>;

    fn place(&self) -> &str {
        &self.place
    }

    fn size(&mut self, name: &str) -> Result<Option<u64>, String> {
        let path = self.path(name)?;
        let meta = match &self.at {
            At::Beneath(dir) => dir.stat(self.cx, &path).map_err(|e| format!("{e}"))?,
            At::Above { ancestor, .. } => ancestor.stat(self.cx, &path).map_err(|e| format!("{e}"))?,
        };
        Ok(meta.filter(|m| !m.dir).map(|m| m.len))
    }

    fn read(&mut self, name: &str, size: u64) -> Result<Held<'f>, String> {
        let path = self.path(name)?;
        let mut bytes = Bytes::new_in(self.frame, "the toolchain's shelf");
        bytes.try_reserve(usize::try_from(size).map_err(|_| "a file past this machine's address space".to_string())?).map_err(|e| format!("{e}"))?;
        match &self.at {
            At::Beneath(dir) => dir.read_all(self.cx, &path, &mut bytes).map_err(|e| format!("{e}"))?,
            At::Above { ancestor, .. } => ancestor.read_all(self.cx, &path, &mut bytes).map_err(|e| format!("{e}"))?,
        };
        Ok(Held(bytes))
    }
}

#[cfg(test)]
mod tests {
    use blaze_toolchain::shelf::{self, Why};
    use blaze_toolchain::{ToolchainId, ToolchainView, record};
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    /// A shelf beneath the start folder opens the manifest named by its id, and one it lacks is
    /// named by its digest and folder.
    #[test]
    fn the_shelf_beneath_the_start_opens_its_manifest_and_names_what_it_lacks() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let fs = io.need_fs().expect("files");
        let manifest = record::write(&[], &[], blaze_own::Alloc::global()).expect("a record");
        let id = ToolchainView::id_of_bytes(&manifest);
        let path = Path::new(&format!("{SHELF}/{id}")).expect("a path");
        fs.root().publish(&cx, &path, Content::bytes(&manifest), Durable::No).expect("write");
        let mut home = HomeShelf::open(&cx, &fs, "/u/me", "/u/me", frame).expect("the shelf");
        let opened = shelf::open_manifest(&mut home, id).expect("the manifest");
        assert_eq!(opened.as_ref(), &manifest[..]);
        let other = ToolchainId([3; 32]);
        let error = shelf::open_manifest(&mut home, other).expect_err("not there");
        assert_eq!((error.why, error.place.as_str()), (Why::Absent, "/u/me/.blaze/v2/toolchain"));
    }
}
