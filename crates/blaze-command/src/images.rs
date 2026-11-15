//! The command's [`Images`]: where a program's images are kept and how the path the user runs
//! is made to name one (blaze-keyed `driver/program.rs`, "Durability", steps 3 and 6).
//!
//! Where things are (the path rule; nothing of the machine in a kept name):
//!
//! - an image: `<home>/v2/keyed/images/<kept manifest key, 64 hex>`, read-only (0o555): named
//!   by the key of the kept manifest that names it, O(1) to name, never a hash of its bytes;
//! - an image being made: `<image>.making.<pid>` beside it, renamed onto the image once synced
//!   (the pid only keeps two processes' half-made files apart; it is never kept);
//! - what a path was last placed as: `<home>/v2/keyed/images/placed/<program identity, 64 hex>`,
//!   the image's key and the path's stamp after the place ([`PLACED_MAGIC`]);
//! - the path the user runs: `<out_dir>/<name>`, the caller's; a placed file is staged beside it
//!   as `<path>.blaze-placing` and renamed over it.
//!
//! [`Images::make`]: a look at `to` and `from` (one stamp batch); `to` held is left as it is;
//! else a clone of `from` (`FICLONE` on Linux, `fclonefileat` on macOS: `blaze_core`'s
//! `Effect::Clone`) under the staged name, or, where the file system shares no blocks, a kernel
//! copy of its data extents (`Effect::CopyData`), said in a note ([`HomeImages::notes`]); with
//! no `from` (the host gives none for a `PatchBase::Empty` patch, and refuses a
//! `PatchBase::Clone` one with no earlier image before it gets here) an empty file, which a
//! patch's cut makes itself, so the old image is never read under a whole relink; then
//! the patch, cut to its length then each write at its offset, made read-only, synced, renamed
//! onto `to` (a durable rename). Then the answer.
//!
//! [`Images::place`]: one look (the path's stamp and its placed record, one batch). Equal to what
//! the last place left (the image's key, and the path's inode, size, times and mode): done. A
//! path written in place, replaced or deleted has another stamp and is placed again. Else the
//! image cloned beside the path (its own inode sharing the image's blocks, so the path and the
//! store never share a writable inode; the image was synced when made, so the clone forces no
//! write-back), given an executable's mode; where the file system clones nothing, a hard link
//! to the read-only image (said in a note: a user who makes the path writable and writes it in
//! place changes the store's image); where the path is on another volume than the home, a
//! kernel copy of its data (said in a note). Then renamed over the path, the path stamped again
//! and the record written.
//!
//! Floor (the host's `Images` doc): per request that links, one clone and the patch's bytes
//! and one sync; per answer, one look. Here: make is that plus one stamp batch and one remove of
//! a stale staged name; place on a no-op is one stat of the path (`statx`/`lstat`) and one read
//! of its 72-byte record, submitted together, and nothing written. Each call opens its own
//! driver (`blaze_core::native`, one kqueue or ring): a few system calls past the floor.
//!
//! Sans-IO: [`steps`] holds the two state machines over `blaze_core` effects; this file's
//! driver only submits, polls with a deadline and hands completions on (rules 14, 15, 18).

use core::alloc::Allocator;
use std::{
    alloc::Global,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_core::{Driver as _, Effect, FileName, lock::Lock, wait::Deadline};
use blaze_host::Images;
use blaze_ids::ContentId;
use blaze_keyed::{driver::TaskFailed, front::Patch};

use self::steps::{Make, Made, Place, Placed, Refusal, Steps};
use crate::CommandError;

/// The longest one wait for the file system may take with nothing completing: past a sync of
/// any real image, so only a hung call reaches it (rule 18).
pub const STEP_MOST: Duration = Duration::from_secs(120);

/// A path this command names is not absolute UTF-8 under `/`, which its driver's names are.
pub const NOT_ABSOLUTE: &str = "a program's images: a path is not absolute UTF-8 (the driver's names are under `/`)";
/// A directory the images or the path need could not be made.
pub const NO_DIR: &str = "a program's images: a directory could not be made";
/// The file system driver did not open.
pub const NO_DRIVER: &str = "a program's images: the file system driver did not open (`blaze_core::native`)";
/// A wait for the file system passed [`STEP_MOST`] with nothing completing.
pub const STALLED: &str = "a program's images: the file system answered nothing within the step's deadline (`images::STEP_MOST`)";
/// A program number past those the command named.
pub const NO_PROGRAM_PATH: &str = "a program's images: its number is no program the command named a path for";

/// The command's [`Images`] under `<home>/v2/keyed/images`.
pub struct HomeImages<A: Allocator + Clone> {
    images: FileName,
    programs: Vec<Placing, A>,
    /// What a make or a place said beyond its answer (a copy made, a refusal's paths), for the
    /// command to print.
    notes: Lock<Vec<Box<str, A>, A>>,
    plant: Plant,
    alloc: A,
}

/// One program's path and where its place is recorded.
struct Placing {
    path: FileName,
    beside: FileName,
    record: FileName,
}

/// A bug planted for a test's control. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A place whose record names the image trusts it without looking at the path: a certified
    /// answer places nothing, so a deleted path stays deleted.
    TrustsRecord,
}

impl<A: Allocator + Clone> HomeImages<A> {
    /// The images under `keyed` (`<home>/v2/keyed`), and each program's path by its identity
    /// ([`blaze_keyed::program::ProgramPlan::program`]), in the driver's program order. Makes
    /// the images' directories and each path's (setup, once per command). Its paths and notes
    /// are in `alloc`.
    pub fn new(keyed: &Path, programs: &[(ContentId, PathBuf)], alloc: A) -> Result<HomeImages<A>, CommandError> {
        let dir = keyed.join("images");
        let placed = dir.join("placed");
        for at in [&dir, &placed] {
            make_dir(at)?;
        }
        let images = name_of(&dir)?;
        let mut out = Vec::with_capacity_in(programs.len(), alloc.clone());
        for (identity, path) in programs {
            if let Some(parent) = path.parent() {
                make_dir(parent)?;
            }
            let mut beside = path.as_os_str().to_owned();
            beside.push(".blaze-placing");
            out.push(Placing {
                path: name_of(path)?,
                beside: name_of(Path::new(&beside))?,
                record: name_of(&placed.join(hex(identity).as_str()))?,
            });
        }
        let notes = Lock::new("command.images.notes", Vec::new_in(alloc.clone()));
        Ok(HomeImages { images, programs: out, notes, plant: Plant::None, alloc })
    }

    /// What the makes and places said beyond their answers, each once, in order.
    pub fn notes(&self) -> Vec<Box<str, A>, A> {
        core::mem::replace(&mut *self.notes.lock(), Vec::new_in(self.alloc.clone()))
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    fn image(&self, id: &ContentId) -> FileName {
        joined(&self.images, hex(id).as_str())
    }

    fn note(&self, line: core::fmt::Arguments<'_>) {
        let mut text = blaze_mem::String::new_in(self.alloc.clone());
        blaze_mem::text::format_into(&mut text, line);
        self.notes.lock().push(text.into_boxed_str());
    }

    /// Runs `machine` to its end on a driver of its own: each wait bounded by [`STEP_MOST`] and
    /// named by `what` (rules 15, 18). A refusal's paths go to the notes; the task fails by its
    /// rule.
    fn drive<M: Steps>(&self, machine: &mut M, what: &'static str) -> Result<M::Out, TaskFailed> {
        let mut driver = blaze_core::native(Path::new("/"), 1).map_err(|error| {
            self.note(format_args!("{what}: {error} (rule: {NO_DRIVER})"));
            TaskFailed::Refused { what: NO_DRIVER }
        })?;
        // The driver's own vectors (`Driver::submit`, `Driver::poll`): the global heap.
        let mut effects: Vec<Effect> = Vec::new_in(Global);
        let mut done = Vec::new_in(Global);
        machine.start(&mut effects);
        loop {
            driver.submit(&mut effects);
            if driver.in_flight() == 0 {
                self.note(format_args!("{what}: nothing in flight and no answer (rule: {STALLED})"));
                return Err(TaskFailed::Refused { what: STALLED });
            }
            let until = Deadline::after(driver.now(), STEP_MOST);
            let got = {
                let _span = tracing::info_span!(target: "blaze", "images: waiting for the file system", what).entered();
                driver.poll(&mut done, until)
            };
            if got == 0 && until.passed(driver.now()) {
                self.note(format_args!("{what}: {STEP_MOST:?} with nothing completing (rule: {STALLED})"));
                return Err(TaskFailed::Refused { what: STALLED });
            }
            for completion in done.drain(..) {
                match machine.give(completion, &mut effects) {
                    Ok(Some(out)) => return Ok(out),
                    Ok(None) => {}
                    Err(Refusal { rule, what: why }) => {
                        self.note(format_args!("{what}: {why} (rule: {rule})"));
                        return Err(TaskFailed::Refused { what: rule });
                    }
                }
            }
        }
    }
}

impl<H: Allocator + Clone> Images for HomeImages<H> {
    fn make<A: Allocator>(&self, program: u32, from: Option<ContentId>, to: ContentId, patch: Option<&Patch<A>>) -> Result<(), TaskFailed> {
        let to_name = self.image(&to);
        let staged = joined(&self.images, &format!("{}.making.{}", hex(&to).as_str(), std::process::id()));
        let mut machine = Make::new(from.map(|from| self.image(&from)), to_name.clone(), staged, patch);
        let made = self.drive(&mut machine, "making a program's image")?;
        if made == Made::Copied {
            self.note(format_args!(
                "program {program}: image {to_name} copied, not cloned: the home's file system shares no blocks, so each link writes the whole image"
            ));
        }
        Ok(())
    }

    fn place(&self, program: u32, image: ContentId) -> Result<(), TaskFailed> {
        let placing = self.programs.get(program as usize).ok_or(TaskFailed::Refused { what: NO_PROGRAM_PATH })?;
        let mut machine = Place::new(
            self.image(&image),
            image,
            placing.path.clone(),
            placing.beside.clone(),
            placing.record.clone(),
            self.plant == Plant::TrustsRecord,
        );
        let placed = self.drive(&mut machine, "placing a program's path")?;
        match placed {
            Placed::InPlace | Placed::Cloned => {}
            Placed::Linked => self.note(format_args!(
                "/{}: a hard link to the home's read-only image, not a clone: the file system clones nothing; make it writable and write it in place, and the store's image changes",
                placing.path
            )),
            Placed::Copied => self.note(format_args!(
                "/{}: copied from the home, not cloned or linked: the path is on another volume than the home, so each place writes the whole image",
                placing.path
            )),
        }
        Ok(())
    }
}

/// `path` as a name under the driver's root `/`.
fn name_of(path: &Path) -> Result<FileName, CommandError> {
    path.to_str()
        .and_then(|it| it.strip_prefix('/'))
        .and_then(FileName::new)
        .ok_or_else(|| CommandError::Program { rule: NOT_ABSOLUTE, what: path.display().to_string().into_boxed_str() })
}

fn joined(dir: &FileName, name: &str) -> FileName {
    FileName::new(&format!("{dir}/{name}")).expect("a name joined under a valid name is valid")
}

fn make_dir(dir: &Path) -> Result<(), CommandError> {
    std::fs::create_dir_all(dir).map_err(|error| CommandError::File {
        path: dir.display().to_string().into_boxed_str(),
        what: format!("{error} (rule: {NO_DIR})").into_boxed_str(),
    })
}

/// An id's 64 lowercase hex digits, in a fixed buffer: no allocation.
struct Hex([u8; 64]);

impl Hex {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("hex digits are ASCII")
    }
}

fn hex(id: &ContentId) -> Hex {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (at, byte) in id.0.iter().enumerate() {
        out[2 * at] = DIGITS[usize::from(byte >> 4)];
        out[2 * at + 1] = DIGITS[usize::from(byte & 15)];
    }
    Hex(out)
}

/// The layout of a placed record: `magic "BZPL" | image key 32 B | the path's stamp (ino,
/// size, mtime, ctime as u64, mode as u32), little-endian`.
pub const PLACED_MAGIC: [u8; 4] = *b"BZPL";

/// The two state machines, over `blaze_core` effects: no IO here.
pub mod steps {
    #![blaze::sans_io]

    use core::alloc::Allocator;
    use std::{alloc::Global, sync::Arc};

    use blaze_core::{Completion, Done, Effect, FileName, Names, Stamp, Tag};
    use blaze_ids::ContentId;
    use blaze_keyed::front::Patch;

    use super::PLACED_MAGIC;

    /// A make's step did not complete as it must.
    pub const NOT_MADE: &str = "a program's image: a step of its make failed (the step and the file are named)";
    /// The image a make clones from is not held.
    pub const FROM_NOT_HELD: &str = "a program's image: the image it is made from is not in the home (rule: `from` not held is refused)";
    /// A patch write reaches past the patch's bytes.
    pub const PATCH_OUT_OF_BOUNDS: &str = "a program's image: a patch write names bytes past the patch's own";
    /// A place's step did not complete as it must.
    pub const NOT_PLACED: &str = "a program's path: a step of its place failed (the step and the file are named)";

    /// Why a machine stopped: the rule, and what it is about.
    pub struct Refusal {
        pub rule: &'static str,
        pub what: String,
    }

    /// A machine the driver runs: started once, then given each completion until it answers.
    pub trait Steps {
        type Out;
        fn start<E: Allocator>(&mut self, effects: &mut Vec<Effect, E>);
        fn give<E: Allocator>(&mut self, completion: Completion, effects: &mut Vec<Effect, E>) -> Result<Option<Self::Out>, Refusal>;
    }

    const LOOK: Tag = Tag(1);
    const RECORD: Tag = Tag(2);
    const STEP: Tag = Tag(3);

    /// How an image came to be.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Made {
        /// It was held already: left as it is.
        Held,
        /// A first image: an empty file and the whole patch.
        New,
        Cloned,
        Copied,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum MakeStep {
        Look,
        Clear,
        Base,
        Copy,
        Cut,
        Write,
        Mode,
        Sync,
        Rename,
    }

    /// One image made ([`super::Images::make`]).
    pub struct Make<'p, A: Allocator> {
        from: Option<FileName>,
        to: FileName,
        staged: FileName,
        patch: Option<&'p Patch<A>>,
        step: MakeStep,
        made: Made,
        writes_left: usize,
    }

    impl<'p, A: Allocator> Make<'p, A> {
        pub fn new(from: Option<FileName>, to: FileName, staged: FileName, patch: Option<&'p Patch<A>>) -> Make<'p, A> {
            Make { from, to, staged, patch, step: MakeStep::Look, made: Made::New, writes_left: 0 }
        }

        fn refuse(&self, rule: &'static str, why: impl core::fmt::Display) -> Refusal {
            Refusal { rule, what: format!("{:?} of {} (to {}): {why}", self.step, self.staged, self.to) }
        }

        /// After the base: the patch (cut, then each write), or straight to the mode.
        fn after_base<E: Allocator>(&mut self, effects: &mut Vec<Effect, E>) {
            match self.patch {
                Some(patch) => {
                    effects.push(Effect::Truncate { file: self.staged.clone(), len: patch.len, tag: STEP });
                    self.step = MakeStep::Cut;
                }
                None => self.mode(effects),
            }
        }

        fn mode<E: Allocator>(&mut self, effects: &mut Vec<Effect, E>) {
            effects.push(Effect::Mode { file: self.staged.clone(), mode: 0o555, tag: STEP });
            self.step = MakeStep::Mode;
        }
    }

    impl<A: Allocator> Steps for Make<'_, A> {
        type Out = Made;

        fn start<E: Allocator>(&mut self, effects: &mut Vec<Effect, E>) {
            let mut names = Names::with_capacity(2, 160);
            names.push(&self.to);
            if let Some(from) = &self.from {
                names.push(from);
            }
            effects.push(Effect::Stamp { files: Arc::new(names), tag: LOOK });
        }

        fn give<E: Allocator>(&mut self, completion: Completion, effects: &mut Vec<Effect, E>) -> Result<Option<Made>, Refusal> {
            match (self.step, completion.done) {
                (MakeStep::Look, Done::Stamped(stamps)) => {
                    if stamps.first().is_some_and(|it| !it.is_gone()) {
                        return Ok(Some(Made::Held));
                    }
                    if self.from.is_some() && stamps.get(1).is_none_or(Stamp::is_gone) {
                        return Err(self.refuse(FROM_NOT_HELD, format_args!("{}", self.from.as_ref().map_or("", FileName::as_str))));
                    }
                    // A staged name a killed make left.
                    effects.push(Effect::Remove { file: self.staged.clone(), tag: STEP });
                    self.step = MakeStep::Clear;
                }
                // The base: a clone of `from`, or with none (the host's `PatchBase::Empty`, or no
                // earlier image) an empty file. Under a patch the cut to its length makes that
                // file itself (`Effect::Truncate` makes a missing file), so no empty one first.
                (MakeStep::Clear, Done::Removed) => match (&self.from, self.patch) {
                    (Some(from), _) => {
                        effects.push(Effect::Clone { from: from.clone(), to: self.staged.clone(), tag: STEP });
                        self.step = MakeStep::Base;
                    }
                    (None, Some(_)) => self.after_base(effects),
                    (None, None) => {
                        effects.push(Effect::Truncate { file: self.staged.clone(), len: 0, tag: STEP });
                        self.step = MakeStep::Base;
                    }
                },
                (MakeStep::Base, Done::Cloned) => {
                    self.made = Made::Cloned;
                    self.after_base(effects);
                }
                (MakeStep::Base, Done::Truncated) => self.after_base(effects),
                // The one case a failure is not the end: a clone the file system cannot make
                // (it shares no blocks), for which the kernel copies the data extents; the
                // copy is said in a note, never quiet. A copy that fails too ends the make.
                (MakeStep::Base, Done::Failed(_)) if self.from.is_some() => {
                    let from = self.from.clone().expect("checked");
                    effects.push(Effect::CopyData { from, to: self.staged.clone(), tag: STEP });
                    self.step = MakeStep::Copy;
                }
                (MakeStep::Copy, Done::Copied { .. }) => {
                    self.made = Made::Copied;
                    self.after_base(effects);
                }
                (MakeStep::Cut, Done::Truncated) => {
                    let patch = self.patch.expect("a cut is of a patch");
                    for write in patch.writes.iter() {
                        let from = write.from as usize;
                        let bytes = patch.bytes.get(from..from + write.len as usize).ok_or_else(|| {
                            self.refuse(PATCH_OUT_OF_BOUNDS, format_args!("a write of {} bytes from {from}", write.len))
                        })?;
                        // The effect owns its bytes: one copy of the patch's.
                        effects.push(Effect::WriteAt { file: self.staged.clone(), at: write.at, bytes: bytes.to_vec(), tag: STEP });
                    }
                    self.writes_left = patch.writes.len();
                    self.step = MakeStep::Write;
                    if self.writes_left == 0 {
                        self.mode(effects);
                    }
                }
                (MakeStep::Write, Done::Written) => {
                    self.writes_left -= 1;
                    if self.writes_left == 0 {
                        self.mode(effects);
                    }
                }
                (MakeStep::Mode, Done::Moded) => {
                    effects.push(Effect::Sync { file: self.staged.clone(), tag: STEP });
                    self.step = MakeStep::Sync;
                }
                (MakeStep::Sync, Done::Synced) => {
                    effects.push(Effect::Rename { from: self.staged.clone(), to: self.to.clone(), tag: STEP });
                    self.step = MakeStep::Rename;
                }
                (MakeStep::Rename, Done::Renamed) => return Ok(Some(self.made)),
                (_, Done::Failed(error)) => return Err(self.refuse(NOT_MADE, error)),
                (_, other) => return Err(self.refuse(NOT_MADE, format_args!("an answer not of its step: {other:?}"))),
            }
            Ok(None)
        }
    }

    /// How a path came to name its image.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Placed {
        /// It named it already: nothing was written.
        InPlace,
        /// A clone of the image: its own inode, the image's blocks.
        Cloned,
        /// A hard link to the read-only image: the file system clones nothing.
        Linked,
        /// A kernel copy of the image's data: the path is on another volume.
        Copied,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum PlaceStep {
        Look,
        Clear,
        Clone,
        Link,
        Copy,
        Mode,
        Rename,
        Restamp,
        Record,
    }

    /// One path placed ([`super::Images::place`]).
    pub struct Place {
        image: FileName,
        id: ContentId,
        path: FileName,
        beside: FileName,
        record: FileName,
        step: PlaceStep,
        stamp: Option<Stamp>,
        recorded: Option<Vec<u8>>,
        placed: Placed,
        trusts_record: bool,
    }

    impl Place {
        pub fn new(image: FileName, id: ContentId, path: FileName, beside: FileName, record: FileName, trusts_record: bool) -> Place {
            Place {
                image,
                id,
                path,
                beside,
                record,
                step: PlaceStep::Look,
                stamp: None,
                recorded: None,
                placed: Placed::InPlace,
                trusts_record,
            }
        }

        fn refuse(&self, rule: &'static str, why: impl core::fmt::Display) -> Refusal {
            Refusal { rule, what: format!("{:?} of /{} (image {}): {why}", self.step, self.path, self.image) }
        }

        fn rename<E: Allocator>(&mut self, effects: &mut Vec<Effect, E>) {
            effects.push(Effect::Rename { from: self.beside.clone(), to: self.path.clone(), tag: STEP });
            self.step = PlaceStep::Rename;
        }
    }

    /// A placed record's length: magic, image key, four words and the mode.
    pub const PLACED_LEN: usize = 72;

    /// The record of `id` placed with the path at `stamp`, in a fixed buffer.
    pub fn encode(id: &ContentId, stamp: &Stamp) -> [u8; PLACED_LEN] {
        let mut out = [0u8; PLACED_LEN];
        out[..4].copy_from_slice(&PLACED_MAGIC);
        out[4..36].copy_from_slice(&id.0);
        for (at, word) in [stamp.ino, stamp.size, stamp.mtime_ns, stamp.ctime_ns].into_iter().enumerate() {
            out[36 + 8 * at..44 + 8 * at].copy_from_slice(&word.to_le_bytes());
        }
        out[68..].copy_from_slice(&stamp.mode.to_le_bytes());
        out
    }

    /// Whether `record` says `id` was placed: the image's key, and nothing of the path.
    fn names(record: &[u8], id: &ContentId) -> bool {
        record.len() == 72 && record[..4] == PLACED_MAGIC && record[4..36] == id.0
    }

    impl Steps for Place {
        type Out = Placed;

        fn start<E: Allocator>(&mut self, effects: &mut Vec<Effect, E>) {
            let mut names = Names::with_capacity(1, 160);
            names.push(&self.path);
            effects.push(Effect::Stamp { files: Arc::new(names), tag: LOOK });
            effects.push(Effect::ReadAll { file: self.record.clone(), tag: RECORD });
        }

        fn give<E: Allocator>(&mut self, completion: Completion, effects: &mut Vec<Effect, E>) -> Result<Option<Placed>, Refusal> {
            match (self.step, completion.tag, completion.done) {
                (PlaceStep::Look, LOOK, Done::Stamped(stamps)) => self.stamp = Some(stamps.first().copied().unwrap_or(Stamp::GONE)),
                (PlaceStep::Look, RECORD, Done::Read(bytes)) => self.recorded = Some(bytes),
                (PlaceStep::Clear, _, Done::Removed) => {
                    effects.push(Effect::Clone { from: self.image.clone(), to: self.beside.clone(), tag: STEP });
                    self.step = PlaceStep::Clone;
                }
                // A clone: the path's own inode, sharing the image's blocks, so a write to the
                // path never reaches the store. Given an executable's mode (a clone is a new
                // file's, 0o666 less the umask).
                (PlaceStep::Clone, _, Done::Cloned) => {
                    self.placed = Placed::Cloned;
                    effects.push(Effect::Mode { file: self.beside.clone(), mode: 0o755, tag: STEP });
                    self.step = PlaceStep::Mode;
                }
                // Where the file system clones nothing (ext4, tmpfs), a hard link to the image,
                // which is read-only (0o555), said in a note. Each step below is taken only
                // when the one above cannot be made, and each is said, never quiet.
                (PlaceStep::Clone, _, Done::Failed(_)) => {
                    effects.push(Effect::Link { from: self.image.clone(), to: self.beside.clone(), tag: STEP });
                    self.step = PlaceStep::Link;
                }
                (PlaceStep::Link, _, Done::Linked) => {
                    self.placed = Placed::Linked;
                    self.rename(effects);
                }
                // A link across volumes (the path's and the home's) is not made either: the
                // kernel copies the image's data instead, said in a note. A copy that fails too
                // ends the place.
                (PlaceStep::Link, _, Done::Failed(_)) => {
                    effects.push(Effect::CopyData { from: self.image.clone(), to: self.beside.clone(), tag: STEP });
                    self.step = PlaceStep::Copy;
                }
                (PlaceStep::Copy, _, Done::Copied { .. }) => {
                    self.placed = Placed::Copied;
                    effects.push(Effect::Mode { file: self.beside.clone(), mode: 0o755, tag: STEP });
                    self.step = PlaceStep::Mode;
                }
                (PlaceStep::Mode, _, Done::Moded) => self.rename(effects),
                (PlaceStep::Rename, _, Done::Renamed) => {
                    let mut names = Names::with_capacity(1, 160);
                    names.push(&self.path);
                    effects.push(Effect::Stamp { files: Arc::new(names), tag: STEP });
                    self.step = PlaceStep::Restamp;
                }
                (PlaceStep::Restamp, _, Done::Stamped(stamps)) => {
                    let stamp = stamps.first().copied().unwrap_or(Stamp::GONE);
                    if stamp.is_gone() {
                        return Err(self.refuse(NOT_PLACED, "the path is gone right after its rename"));
                    }
                    // The effect owns its bytes (`Effect::Write` holds a vector of the global heap).
                    let mut bytes = Vec::with_capacity_in(PLACED_LEN, Global);
                    bytes.extend_from_slice(&encode(&self.id, &stamp));
                    effects.push(Effect::Write { file: self.record.clone(), bytes, tag: STEP });
                    self.step = PlaceStep::Record;
                }
                (PlaceStep::Record, _, Done::Written) => return Ok(Some(self.placed)),
                (_, _, Done::Failed(error)) => return Err(self.refuse(NOT_PLACED, error)),
                (_, _, other) => return Err(self.refuse(NOT_PLACED, format_args!("an answer not of its step: {other:?}"))),
            }
            if self.step == PlaceStep::Look
                && let (Some(stamp), Some(recorded)) = (self.stamp, self.recorded.as_deref())
            {
                let in_place = match self.trusts_record {
                    true => names(recorded, &self.id),
                    false => !stamp.is_gone() && recorded == &encode(&self.id, &stamp)[..],
                };
                if in_place {
                    return Ok(Some(Placed::InPlace));
                }
                // A staged name a killed place left.
                effects.push(Effect::Remove { file: self.beside.clone(), tag: STEP });
                self.step = PlaceStep::Clear;
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_host::Images as _;
    use blaze_ids::ContentId;
    use blaze_keyed::front::{Patch, PatchBase, PatchWrite};

    use super::{HomeImages, Plant};

    /// The oracle: after an answer placed the program, its path holds the image's bytes.
    fn placed(path: &std::path::Path, bytes: &[u8]) -> Result<(), &'static str> {
        match std::fs::read(path) {
            Ok(read) if read == bytes => Ok(()),
            Ok(_) => Err("the path holds other bytes than the image the answer placed"),
            Err(_) => Err("the path is not there after the answer placed it"),
        }
    }

    /// A path deleted after a build is placed again by the next answer, from the image in the
    /// home, with no link step (no `make`); returns the oracle's verdict on that answer.
    fn deleted_then_answered(plant: Plant) -> Result<(), &'static str> {
        let root = std::env::temp_dir().join(format!("blaze-images-{}-{plant:?}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let path = root.join("target").join("hello");
        let program = ContentId::of("blaze-command images test", b"program");
        let image = ContentId::of("blaze-command images test", b"image");
        let mut images = HomeImages::new(&root.join("keyed"), &[(program, path.clone())], Global).expect("the images open");
        images.plant(plant);
        let bytes = b"\x7fELF a whole first image".to_vec();
        let mut writes = Vec::new_in(Global);
        writes.push(PatchWrite { at: 0, from: 0, len: bytes.len() as u32 });
        let mut patch_bytes = Vec::new_in(Global);
        patch_bytes.extend_from_slice(&bytes);
        let patch = Patch { base: PatchBase::Empty, len: bytes.len() as u64, writes, bytes: patch_bytes };

        images.make(0, None, image, Some(&patch)).expect("the first image is made");
        images.place(0, image).expect("the first answer places the path");
        placed(&path, &bytes).expect("the first place holds");

        std::fs::remove_file(&path).expect("the path is deleted");
        // The next answer is certified: no link step, so no make; only the place.
        images.place(0, image).expect("the certified answer places the path");
        let verdict = placed(&path, &bytes);
        _ = std::fs::remove_dir_all(&root);
        verdict
    }

    #[test]
    fn a_deleted_path_is_placed_again_by_a_certified_answer() {
        assert_eq!(deleted_then_answered(Plant::None), Ok(()));
        // The control: a place that trusts its record and does not look at the path leaves the
        // deleted path deleted, and the oracle says so by name.
        assert_eq!(deleted_then_answered(Plant::TrustsRecord), Err("the path is not there after the answer placed it"));
    }
}
