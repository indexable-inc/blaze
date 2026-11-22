//! The folders above the start folder (std.md, "Folders above the start folder"):
//! [`Fs::ancestors`] hands out an [`Ancestor`] per folder, nearest first, up to the granted
//! top. An ancestor is read-only by its type: it has `stat`, `read_all` and `list`, and the
//! name it reads by, blaze-core's [`blaze_core::Above`], is taken by `Effect::Above` alone, which
//! reads. No `Dir`, `File` or write effect can name a file above the start, and the only way to
//! make an `Above` is [`blaze_core::Ancestors::above`] on the driver's own answer, which refuses a
//! step past the granted top and a name that walks back down into the start.

use alloc::{string::String, vec::Vec};
use core::fmt;

use blaze_core::{Above, AboveOp, AboveRefused, Ancestors, Done, Effect};
use blaze_mem::Frame;

use super::{Fs, Meta};
use crate::{Plant, Stopped, cx::Cx, io::Io, mem::Bytes, path::Path};

/// One folder above the start folder, from [`Fs::ancestors`]: read-only.
#[derive(Clone)] // clone: authority held, refcounts
pub struct Ancestor {
    io: Io,
    ancestors: Ancestors,
    up: u8,
}

/// How a call on the folders above the start failed: what it read and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct AncestorError {
    /// The folder or file, as `..` steps from the start folder (`../../Cargo.toml`); empty for
    /// the ask of the folders themselves.
    pub path: String,
    pub kind: AncestorKind,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum AncestorKind {
    /// No such name: a step past the granted top, or one back down into the start.
    Refused(AboveRefused),
    /// Not there.
    NotFound,
    /// A file was asked to be listed, or a folder read.
    NotADirectory,
    IsADirectory,
    Stopped(Stopped),
    /// The driver said this.
    Driver(String),
}

impl fmt::Display for AncestorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() { "the folders above the start folder" } else { &self.path };
        match &self.kind {
            AncestorKind::Refused(why) => write!(f, "{path}: {why}"),
            AncestorKind::NotFound => write!(f, "{path}: not there"),
            AncestorKind::NotADirectory => write!(f, "{path}: not a folder"),
            AncestorKind::IsADirectory => write!(f, "{path}: a folder, not a file"),
            AncestorKind::Stopped(stopped) => write!(f, "{path}: {stopped}"),
            AncestorKind::Driver(said) => write!(f, "{path}: {said}"),
        }
    }
}

impl core::error::Error for AncestorError {}

impl Fs {
    /// The folders above the start folder, nearest first, up to the granted top (std.md,
    /// "Folders above the start folder"): the filesystem's root for a command, none for a
    /// program launched with a run spec. Asked of the driver here, once per call; nothing of
    /// it is read at start.
    pub fn ancestors(&self, cx: &Cx) -> Result<Vec<Ancestor>, AncestorError> {
        let ancestors = self.asked(cx)?;
        // clone: authority held, refcounts.
        Ok((1..=ancestors.depth()).map(|up| Ancestor { io: self.io.clone(), ancestors: ancestors.clone(), up }).collect())
    }

    /// The one folder `up` above the start folder: [`AboveRefused::PastTop`] past the granted
    /// top, [`AboveRefused::Root`] for 0.
    pub fn ancestor(&self, cx: &Cx, up: u8) -> Result<Ancestor, AncestorError> {
        let ancestors = self.asked(cx)?;
        let mut shown = String::new();
        for at in 0..up {
            shown.push_str(if at == 0 { ".." } else { "/.." });
        }
        ancestors.above(up, None).map_err(|why| AncestorError { path: shown, kind: AncestorKind::Refused(why) })?;
        // clone: authority held, a refcount.
        Ok(Ancestor { io: self.io.clone(), ancestors, up })
    }

    /// The driver's answer: the folders above, cut to the granted top.
    fn asked(&self, cx: &Cx) -> Result<Ancestors, AncestorError> {
        let io = &self.io;
        let most = match io.plant() {
            Plant::AncestorsUnbounded => u8::MAX,
            _ => io.above_most(),
        };
        let failed = |kind| AncestorError { path: String::new(), kind };
        match io.call(cx, |tag| Effect::Ancestors { most, tag }).map_err(|it| failed(AncestorKind::Stopped(it)))? {
            Done::Ancestors(ancestors) => Ok(ancestors),
            Done::Failed(error) => Err(failed(AncestorKind::Driver(error.0))),
            // alloc: an error's message.
            done => Err(failed(AncestorKind::Driver(alloc::format!("answered {done:?}")))),
        }
    }
}

impl Ancestor {
    /// How many folders above the start folder.
    pub fn up(&self) -> u8 {
        self.up
    }

    /// The name, in this folder, of the folder on the way down to the start.
    pub fn toward(&self) -> &str {
        let up = core::num::NonZeroU8::new(self.up).expect("an ancestor is at least one up");
        self.ancestors.toward(up).expect("made within the answered depth")
    }

    /// The name of `path` beneath this folder, or of the folder itself.
    fn name(&self, path: Option<&Path>) -> Result<Above, AncestorError> {
        // clone: the name, a refcount.
        self.ancestors.above(self.up, path.map(|it| it.name().clone())).map_err(|why| AncestorError {
            path: self.shown(path),
            kind: AncestorKind::Refused(why),
        })
    }

    fn shown(&self, path: Option<&Path>) -> String {
        let mut shown = String::new();
        for at in 0..self.up {
            shown.push_str(if at == 0 { ".." } else { "/.." });
        }
        if let Some(path) = path {
            shown.push('/');
            shown.push_str(path.as_str());
        }
        shown
    }

    fn read(&self, cx: &Cx, op: AboveOp, path: Option<&Path>) -> Result<Done, AncestorError> {
        let name = self.name(path)?;
        let failed = |kind| AncestorError { path: self.shown(path), kind };
        match self.io.call(cx, |tag| Effect::Above { op, name, tag }).map_err(|it| failed(AncestorKind::Stopped(it)))? {
            Done::Failed(error) => Err(failed(AncestorKind::Driver(error.0))),
            done => Ok(done),
        }
    }

    /// What is at `path` beneath this folder, or `None`.
    pub fn stat(&self, cx: &Cx, path: &Path) -> Result<Option<Meta>, AncestorError> {
        match self.read(cx, AboveOp::Stat, Some(path))? {
            Done::Stat(meta) => Ok(meta.as_ref().map(Meta::of)),
            // alloc: an error's message.
            done => Err(AncestorError { path: self.shown(Some(path)), kind: AncestorKind::Driver(alloc::format!("answered {done:?}")) }),
        }
    }

    /// The whole file at `path` beneath this folder added to `into`; how many bytes it added.
    pub fn read_all(&self, cx: &Cx, path: &Path, into: &mut Bytes<'_>) -> Result<usize, AncestorError> {
        let failed = |kind| AncestorError { path: self.shown(Some(path)), kind };
        match self.stat(cx, path)? {
            None => return Err(failed(AncestorKind::NotFound)),
            Some(Meta { dir: true, .. }) => return Err(failed(AncestorKind::IsADirectory)),
            Some(_) => {}
        }
        match self.read(cx, AboveOp::Read, Some(path))? {
            Done::Read(bytes) => {
                into.try_extend(&bytes).map_err(|it| failed(AncestorKind::Stopped(it)))?;
                Ok(bytes.len())
            }
            // alloc: an error's message.
            done => Err(failed(AncestorKind::Driver(alloc::format!("answered {done:?}")))),
        }
    }

    /// The names directly in this folder, sorted, each with its [`Meta`]. The folder on the
    /// way down to the start is among them, as a folder; reading beneath it is refused.
    pub fn list<'a>(&self, cx: &Cx, frame: &'a Frame) -> Result<Vec<(Path, Meta), &'a Frame>, AncestorError> {
        let failed = |kind| AncestorError { path: self.shown(None), kind };
        let listed = match self.read(cx, AboveOp::List, None)? {
            Done::Listed(listed) => listed,
            // alloc: an error's message.
            done => return Err(failed(AncestorKind::Driver(alloc::format!("answered {done:?}")))),
        };
        let mut found = Vec::new_in(frame);
        for (name, meta) in &listed {
            let path = Path::new(name.as_str()).map_err(|_| failed(AncestorKind::Driver(alloc::format!("listed {name:?}, no path"))))?;
            found.push((path, Meta::of(meta)));
        }
        Ok(found)
    }
}

impl fmt::Debug for Ancestor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ancestor({})", self.shown(None))
    }
}
